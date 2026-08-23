// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Running this run's own test binaries through `cargo nextest`.
//!
//! Nextest normally builds what it runs, which would put a cargo invocation between every mutant
//! and its verdict. It also accepts the two metadata files that describe an already-built tree, and
//! given those it runs the binaries and calls no cargo at all. Both are produced once, immediately
//! after the build, and then reused by every mutant for the rest of the run.
//!
//! What nextest is here for is isolation: it gives each test its own process. A suite that shares a
//! global, sets an environment variable, or installs a process-wide handler is red under a threaded
//! harness, and a red baseline stops a run before it judges anything. Such a tree cannot be measured
//! at all without this.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::Value;

use super::test_binary::TestBinary;
use super::workspace::Workspace;
use crate::error::error;
use crate::{HashMap, Result};

/// Keep Windows' command line below its practical limit, with room for metadata paths and test args.
const MAX_FILTERSET_BYTES: usize = 8 * 1024;

/// The metadata that lets nextest run a tree it did not build, and the ids it knows the binaries by.
#[derive(Debug, Clone)]
pub(super) struct Harness {
    /// Where the binaries nextest may run are described.
    binaries: Utf8PathBuf,

    /// Where the workspace those binaries came from is described.
    metadata: Utf8PathBuf,

    /// What nextest calls each binary, keyed by the path this run knows it by.
    ///
    /// A binary absent from this map is one nextest would not run, which is a disagreement about
    /// the tree rather than a fact about any mutant; [`Harness::id`] reports it as an error rather
    /// than silently running the whole suite in its place.
    ids: HashMap<Utf8PathBuf, String>,
}

impl Harness {
    /// Describes the built tree to nextest, so that no later run of it needs cargo.
    ///
    /// # Errors
    ///
    /// Returns an error if nextest is not installed or cannot enumerate the built tree.
    pub(super) fn prepare(work: &Workspace, binaries: &[TestBinary]) -> Result<Self> {
        let listing = work.capture_nextest_list(binaries)?;
        let ids = binary_ids(&listing);

        let paths = work.write_scratch("nextest-binaries.json", &listing)?;
        let metadata = work.write_scratch("nextest-metadata.json", &work.capture_cargo_metadata()?)?;

        let harness = Self {
            binaries: paths,
            metadata,
            ids,
        };

        // Checked here, once, rather than at the first mutant. A binary nextest does not know about
        // cannot be run through it, and discovering that a thousand mutants in would waste the whole
        // build; discovering it now costs nothing and names the binary.
        for binary in binaries {
            let _id = harness.id(&binary.path)?;
        }

        Ok(harness)
    }

    /// Builds the command that runs one binary's tests under nextest.
    ///
    /// `only` narrows the run to specific tests. A narrowed run keeps nextest's default of failing
    /// when nothing matched, because such a run *is* the binary as far as the verdict is concerned
    /// and an empty selection means the filterset and the built tree disagree — a fact about the
    /// run, not evidence the mutant survived. Nextest also intersects command-line filtersets with
    /// the `default-filter` in its own config, which the census cannot see because it lists tests by
    /// running the binary directly under libtest, so an empty intersection is reachable in ordinary
    /// use.
    ///
    /// # Errors
    ///
    /// Returns an error if nextest does not know the binary, which means the two disagree about what
    /// was built.
    pub(super) fn command(&self, work: &Workspace, binary: &TestBinary, only: &[&str]) -> Result<Command> {
        let id = self.id(&binary.path)?;
        let mut command = Command::new(nextest_binary());

        // Run from the root rather than the package directory: nextest sets each test's working
        // directory to its own package root, and pointing it at one package's directory would make
        // it resolve the whole workspace relative to that.
        let _ = command.current_dir(work.root().as_std_path());

        // Only this binary's tests. The run visits the reachable binaries one at a time and stops
        // at the first that convicts, and letting nextest run all of them would discard both that
        // ordering and the per-binary budget derived from it.
        let _ = command.args([
            "nextest",
            "run",
            "--binaries-metadata",
            self.binaries.as_str(),
            "--cargo-metadata",
            self.metadata.as_str(),
        ]);
        for filterset in filtersets(&id, only) {
            let _ = command.args(["-E", &filterset]);
        }
        // A startup failure is reported by the child, not by nextest. Pin captured failure output
        // so workspace configuration cannot hide the runtime marker and turn infrastructure
        // failure into a mutation kill.
        let _ = command.args(["--failure-output", "immediate-final"]);

        // A test target that defines no tests is ordinary — a `src/bin` with no `#[test]` in it
        // still gets a harness — and nextest treats an empty run as an error by default. Run
        // directly, such a binary reports no tests and exits clean, and it must mean the same
        // thing here.
        //
        // Only for a whole-binary run. A narrowed run stands in for the binary and its absence of
        // failures is read as a verdict, so suppressing the empty-run exit code there would turn
        // "the filterset matched nothing" into "the mutant survived".
        if only.is_empty() {
            let _ = command.args(["--no-tests", "pass"]);
        }

        if !work.test_arguments().is_empty() {
            let _ = command.arg("--");
            let _ = command.args(work.test_arguments());
        }

        Ok(command)
    }

    /// A harness that knows the given path-to-id pairs, without asking nextest anything.
    #[cfg(test)]
    pub(super) fn fake(ids: &[(&str, &str)]) -> Self {
        Self {
            binaries: Utf8PathBuf::from("/t/binaries.json"),
            metadata: Utf8PathBuf::from("/t/metadata.json"),
            ids: ids.iter().map(|(path, id)| (Utf8PathBuf::from(*path), (*id).to_owned())).collect(),
        }
    }

    /// What nextest calls the binary at this path.
    fn id(&self, path: &Utf8Path) -> Result<String> {
        self.ids.get(path).cloned().ok_or_else(|| {
            error!(
                "`cargo nextest` does not know the test binary `{path}` that this run built.\n\
                 Run without `--nextest` to run the binaries directly instead."
            )
        })
    }
}

/// Reads the path-to-id map out of a nextest binaries listing.
///
/// Everything else in the listing — platforms, build script outputs, target directories — is
/// nextest's own business and is passed back to it untouched.
fn binary_ids(listing: &str) -> HashMap<Utf8PathBuf, String> {
    let mut ids = HashMap::default();

    let Ok(message) = serde_json::from_str::<Value>(listing) else {
        return ids;
    };

    let Some(binaries) = message.get("rust-binaries").and_then(Value::as_object) else {
        return ids;
    };

    for entry in binaries.values() {
        let path = entry.get("binary-path").and_then(Value::as_str);
        let id = entry.get("binary-id").and_then(Value::as_str);

        if let (Some(path), Some(id)) = (path, id) {
            let _replaced = ids.insert(Utf8PathBuf::from(path), id.to_owned());
        }
    }

    ids
}

/// Turns a binary id into the filterset atom that matches exactly that binary and nothing else.
///
/// The `=` prefix is nextest's exact-match operator. It matters that this is exact rather than the
/// default substring match: one binary's id is regularly a prefix of another's — `serde` and
/// `serde::integration` — and a substring match would run a binary this mutant was not being judged
/// against, against a budget apportioned for a different one.
///
/// Ids are built from a package name and a target name, so they carry `::` and `/`, which need no
/// escaping. The characters that would end the atom are escaped anyway, since nothing checks that a
/// future cargo keeps target names as narrow as they are today.
fn matcher(id: &str) -> String {
    let escaped = id.replace('\\', "\\\\").replace(')', "\\)").replace(',', "\\,");

    format!("={escaped}")
}

/// Narrows a binary when the selection is safe to express on a command line.
///
/// The selected tests form one parenthesized union beneath the binary constraint. Multiple `-E`
/// arguments are not a substitute: nextest combines them as separate filters rather than as the
/// single union this probe needs. Windows still cannot launch an arbitrarily long command, so an
/// oversized selection runs the whole binary instead.
fn filtersets(id: &str, only: &[&str]) -> Vec<String> {
    let binary = format!("binary_id({})", matcher(id));

    if only.is_empty() {
        return vec![binary];
    }

    let tests = only
        .iter()
        .map(|name| format!("test({})", matcher(name)))
        .collect::<Vec<_>>()
        .join(" or ");
    let narrowed = format!("{binary} and ({tests})");

    if narrowed.len() > MAX_FILTERSET_BYTES {
        vec![binary]
    } else {
        vec![narrowed]
    }
}

/// The nextest executable.
///
/// Invoked directly rather than through `cargo nextest`, so that a run whose binaries are already
/// built never needs cargo on the path at all.
fn nextest_binary() -> String {
    "cargo-nextest".to_owned()
}

/// Exit codes from nextest that describe the test run rather than a failure to perform one.
///
/// Nextest reports far more than pass and fail, and the difference matters: a code saying the tests
/// ran and some failed is a verdict about the mutant, whereas a code saying nextest could not start
/// is a fact about this machine that must not be recorded as a kill.
pub(super) const TEST_RUN_FAILED: i32 = 100;

/// Nextest could not create the test list it needs to start the selected run.
pub(super) const TEST_LIST_CREATION_FAILED: i32 = 104;

/// Nextest's code for a run that matched no tests at all.
pub(super) const NO_TESTS_RUN: i32 = 4;

/// Extracts the first failing test name from nextest's output.
///
/// Nextest prints a `FAIL` line per failing test carrying the binary id and the test path. Only the
/// first is read: it is the one that convicted the mutant, and nextest cancels the run after it.
///
/// The name is borrowed from `output` rather than owned, because the streaming caller looks at every
/// line a test process writes and keeps almost none of them.
pub(super) fn first_failure(output: &str) -> Option<&str> {
    for line in output.lines() {
        let trimmed = line.trim_start();

        let Some(rest) = trimmed.strip_prefix("FAIL ").map(str::trim_start) else {
            continue;
        };

        // `[   0.024s] (1/2) crate::binary test::name` — the duration and the counter are progress
        // reporting, and the name is what is left once they are dropped.
        let after_time = rest.split_once(']').map_or(rest, |(_before, after)| after).trim_start();
        let after_count = after_time.split_once(')').map_or(after_time, |(_before, after)| after).trim_start();

        // The binary id and the test path are separated by a space; the test path is what names the
        // test a reader can go and run.
        let name = after_count.split_once(' ').map_or(after_count, |(_binary, test)| test);

        if !name.is_empty() {
            return Some(name.trim());
        }
    }

    None
}

#[cfg(all(test, not(miri)))]
mod fuzz {
    use super::first_failure;
    use crate::testing::{spliced, token};

    /// A nextest failure announcement is found whatever surrounds it.
    ///
    /// Nextest's line carries a duration and a counter that this parser drops positionally, by
    /// splitting on `]` and `)`. Both of those characters can also appear in a test path or in a
    /// test's own output, which is exactly the kind of collision worth throwing random input at.
    #[test]
    fn a_nextest_failure_is_never_lost_among_arbitrary_output() {
        bolero::check!()
            .with_type::<(Vec<String>, String, String, usize)>()
            .for_each(|(noise, binary, name, at)| {
                let line = format!("        FAIL [   0.024s] (1/2) {} {}", token(binary), token(name));
                let output = spliced(noise, &line, *at);

                assert!(first_failure(&output).is_some(), "the failure was lost in {output:?}");
            });
    }

    /// Arbitrary output never panics the reader and never names an empty test.
    ///
    /// An empty name is the failure mode that matters here rather than a panic: the parser reaches
    /// it by dropping everything up to the last separator it recognizes, so a malformed line can
    /// leave nothing behind, and a caller told the test is named `""` cannot act on it.
    #[test]
    fn arbitrary_output_is_read_without_panicking() {
        bolero::check!().with_type::<String>().for_each(|output| {
            if let Some(name) = first_failure(output) {
                assert!(!name.is_empty(), "an empty test name was reported for {output:?}");
                assert!(!name.contains('\n'), "a test name spans lines: {name:?}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listing_yields_the_id_each_binary_is_known_by() {
        let listing = r#"{"rust-binaries":{"nxspike":{"binary-id":"nxspike","binary-path":"/t/deps/nxspike-abc","kind":"lib"}}}"#;
        let ids = binary_ids(listing);

        assert_eq!(ids.get(Utf8Path::new("/t/deps/nxspike-abc")).map(String::as_str), Some("nxspike"));
    }

    /// A listing that is not JSON, or carries no binaries, must produce an empty map rather than
    /// panic — the caller turns an absent id into a diagnostic naming the binary.
    #[test]
    fn a_listing_that_says_nothing_yields_no_ids() {
        assert!(binary_ids("not json").is_empty());
        assert!(binary_ids("{}").is_empty());
        assert!(binary_ids(r#"{"rust-binaries":{"x":{"binary-id":"x"}}}"#).is_empty());
    }

    /// Exactness is the point: `serde` must not match `serde::integration`, which shares its prefix.
    #[test]
    fn a_binary_id_becomes_an_exact_filterset_atom() {
        assert_eq!(matcher("crate::bin/name"), "=crate::bin/name");
        assert_eq!(matcher("plain"), "=plain");
    }

    /// Target names cannot contain these today, so this only has to hold if that ever changes.
    #[test]
    fn a_character_that_would_end_the_atom_is_escaped() {
        assert_eq!(matcher("odd)name"), "=odd\\)name");
        assert_eq!(matcher("odd,name"), "=odd\\,name");
        assert_eq!(matcher("back\\slash"), "=back\\\\slash");
    }

    #[test]
    fn a_long_test_selection_runs_the_whole_binary() {
        let long = "x".repeat(MAX_FILTERSET_BYTES);

        assert_eq!(filtersets("nxspike", &[&long]), ["binary_id(=nxspike)"]);
    }

    #[test]
    fn selected_tests_form_one_parenthesized_union() {
        assert_eq!(
            filtersets("nxspike", &["tests::one", "tests::two"]),
            ["binary_id(=nxspike) and (test(=tests::one) or test(=tests::two))"]
        );
    }

    #[test]
    fn the_first_failing_test_is_named_from_nextest_output() {
        let output = "        FAIL [   0.024s] (1/2) nxspike tests::fails_when_asked\n\
                              FAIL [   0.030s] (2/2) nxspike tests::also_fails\n";

        assert_eq!(first_failure(output), Some("tests::fails_when_asked"));
    }

    #[test]
    fn output_with_no_failure_line_names_nothing() {
        assert_eq!(first_failure("        PASS [   0.012s] (1/1) nxspike tests::works"), None);
        assert_eq!(first_failure(""), None);
    }

    /// The scan must not stop at the first line that is not a failure, or a suite that reports
    /// anything at all before its failure would convict a mutant without naming what caught it.
    #[test]
    fn a_failure_is_found_past_lines_that_are_not_failures() {
        let output = "    Starting 2 tests across 1 binary\n\
                              PASS [   0.010s] (1/2) nxspike tests::works\n\
                              FAIL [   0.024s] (2/2) nxspike tests::fails_when_asked\n";

        assert_eq!(first_failure(output), Some("tests::fails_when_asked"));
    }

    #[test]
    fn a_known_binary_resolves_to_the_id_nextest_gave_it() {
        let harness = Harness::fake(&[("/t/deps/nxspike-abc", "nxspike")]);

        assert_eq!(harness.id(Utf8Path::new("/t/deps/nxspike-abc")).unwrap(), "nxspike");
    }

    /// A binary nextest never listed cannot be run through it, and running the whole suite in its
    /// place would judge the mutant against tests it was never apportioned a budget for. The error
    /// names the binary and the way out.
    #[test]
    fn a_binary_nextest_never_listed_is_an_error_that_names_it() {
        let harness = Harness::fake(&[("/t/deps/nxspike-abc", "nxspike")]);
        let failure = harness.id(Utf8Path::new("/t/deps/stranger-def")).unwrap_err().to_string();

        assert!(failure.contains("/t/deps/stranger-def"), "{failure}");
        assert!(failure.contains("without `--nextest`"), "{failure}");
    }

    /// The argv nextest is handed has to describe the built tree, this binary alone, and nothing
    /// else — every part of it is load-bearing, so the whole of it is asserted.
    #[test]
    #[cfg(unix)]
    fn the_command_names_the_metadata_and_this_binary_alone() {
        let (_scratch, mut work) = crate::testing::shell_workspace("nextest-command", "exit 0");

        work.set_test_args(vec!["--nocapture".to_owned()]);

        let harness = Harness::fake(&[("/t/deps/nxspike-abc", "nxspike")]);

        let command = harness
            .command(&work, &crate::testing::test_binary("/t/deps/nxspike-abc"), &[])
            .expect("a known binary yields a command");
        let args: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();

        assert_eq!(command.get_program(), "cargo-nextest");
        assert_eq!(
            command.get_current_dir().map(std::path::Path::to_path_buf),
            Some(work.root().as_std_path().to_path_buf())
        );
        assert_eq!(
            args,
            vec![
                "nextest",
                "run",
                "--binaries-metadata",
                "/t/binaries.json",
                "--cargo-metadata",
                "/t/metadata.json",
                "-E",
                "binary_id(=nxspike)",
                "--failure-output",
                "immediate-final",
                "--no-tests",
                "pass",
                "--",
                "--nocapture",
            ]
        );
    }

    /// With no test arguments to pass on there is no `--` either, because nextest reads a trailing
    /// `--` with nothing after it as an empty argument to hand each test.
    #[test]
    #[cfg(unix)]
    fn a_command_with_no_test_arguments_ends_at_the_filterset() {
        let (_scratch, mut work) = crate::testing::shell_workspace("nextest-bare", "exit 0");

        work.set_test_args(Vec::new());

        let harness = Harness::fake(&[("/t/deps/nxspike-abc", "nxspike")]);
        let command = harness
            .command(&work, &crate::testing::test_binary("/t/deps/nxspike-abc"), &[])
            .expect("a known binary yields a command");
        let args: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();

        assert_eq!(args.last().map(String::as_str), Some("pass"));
        assert!(!args.iter().any(|arg| arg == "--"), "{args:?}");
    }

    /// A binary nextest never listed must be refused before a command is built for it, rather than
    /// producing a filterset that matches nothing and a run that convicts nobody.
    #[test]
    #[cfg(unix)]
    fn a_command_for_a_binary_nextest_never_listed_is_refused() {
        let (_scratch, work) = crate::testing::shell_workspace("nextest-stranger", "exit 0");
        let harness = Harness::fake(&[("/t/deps/nxspike-abc", "nxspike")]);

        let failure = harness
            .command(&work, &crate::testing::test_binary("/t/deps/stranger-def"), &[])
            .unwrap_err()
            .to_string();

        assert!(failure.contains("/t/deps/stranger-def"), "{failure}");
    }

    /// A run narrowed to one test keeps the binary in the filterset and adds the test to it.
    ///
    /// Keeping `binary_id` is what makes the pair exact: a test path is only unique within a
    /// binary, so filtering on the name alone would run every same-named test in the workspace and
    /// stop the probe from being one test.
    #[test]
    #[cfg(unix)]
    fn a_command_narrowed_to_one_test_filters_on_the_binary_and_the_test() {
        let (_scratch, mut work) = crate::testing::shell_workspace("nextest-filtered", "exit 0");

        work.set_test_args(Vec::new());

        let harness = Harness::fake(&[("/t/deps/nxspike-abc", "nxspike")]);
        let command = harness
            .command(&work, &crate::testing::test_binary("/t/deps/nxspike-abc"), &["tests::parses"])
            .expect("a known binary yields a command");
        let args: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();

        assert!(
            args.contains(&"binary_id(=nxspike) and (test(=tests::parses))".to_owned()),
            "{args:?}"
        );
    }

    /// Nextest intersects a command-line filterset with the `default-filter` in its own config, and
    /// the census cannot see that filter because it lists a binary's tests by running it directly
    /// under libtest. An empty intersection with `--no-tests pass` in force exits 0, and a narrowed
    /// run's absence of failures is read as a verdict — so the mutant is reported as a survivor
    /// though nothing ran. Keeping nextest's default `fail` turns that into exit code 4, which
    /// `settle` reads as a fact about the run.
    #[test]
    #[cfg(unix)]
    fn a_narrowed_command_does_not_suppress_the_empty_run_exit_code() {
        let (_scratch, mut work) = crate::testing::shell_workspace("nextest-narrowed-no-tests", "exit 0");

        work.set_test_args(Vec::new());

        let harness = Harness::fake(&[("/t/deps/nxspike-abc", "nxspike")]);
        let command = harness
            .command(&work, &crate::testing::test_binary("/t/deps/nxspike-abc"), &["tests::parses"])
            .expect("a known binary yields a command");
        let args: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();

        assert!(!args.iter().any(|arg| arg == "--no-tests"), "{args:?}");

        // A whole-binary run still needs it: a target with no tests at all is ordinary.
        let whole = harness
            .command(&work, &crate::testing::test_binary("/t/deps/nxspike-abc"), &[])
            .expect("a known binary yields a command");
        let whole: Vec<_> = whole.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();

        assert!(whole.windows(2).any(|pair| pair == ["--no-tests", "pass"]), "{whole:?}");
    }

    /// A census names every test that reaches a mutant, and nextest has to be asked for all of
    /// them at once. The group of names is combined with the binary by `and` rather than by `or`, or a
    /// name that also exists in another binary would drag that binary's tests into a run budgeted
    /// for this one.
    #[test]
    #[cfg(unix)]
    fn a_command_narrowed_to_several_tests_asks_for_any_of_them_within_the_one_binary() {
        let (_scratch, mut work) = crate::testing::shell_workspace("nextest-several", "exit 0");

        work.set_test_args(Vec::new());

        let harness = Harness::fake(&[("/t/deps/nxspike-abc", "nxspike")]);
        let command = harness
            .command(
                &work,
                &crate::testing::test_binary("/t/deps/nxspike-abc"),
                &["tests::parses", "tests::rejects"],
            )
            .expect("a known binary yields a command");
        let args: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();

        assert!(
            args.contains(&"binary_id(=nxspike) and (test(=tests::parses) or test(=tests::rejects))".to_owned()),
            "{args:?}"
        );
    }

    /// A test name carrying the characters a filterset gives meaning to is escaped, not interpolated.
    ///
    /// A name is a Rust path and cannot hold a bracket, but it reaches here from a harness's output
    /// rather than from the compiler, and a filterset that fails to parse would abandon a run over
    /// a cache entry.
    #[test]
    #[cfg(unix)]
    fn a_test_name_holding_filterset_syntax_is_escaped() {
        assert_eq!(matcher("odd)name,here"), "=odd\\)name\\,here");
    }
}
