// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]

//! End-to-end tests of the command-line surface, driven through a fake host.

use std::fs;
use std::process::{Command, Stdio};

use camino::Utf8PathBuf;
use cargo_gamma_lib::testing::{Sink, gamma_base, run};
use tempfile::TempDir;

/// Exit code for a run in which every gate passed.
const EXIT_OK: i32 = 0;

/// Exit code for a usage error.
const EXIT_USAGE: i32 = 1;

/// Builds a throwaway single-package workspace containing `source` as its library.
fn workspace(source: &str) -> TempDir {
    let dir = TempDir::new().expect("could not create a temporary directory");
    let root = dir.path();

    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"subject\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .expect("could not write the manifest");

    fs::create_dir_all(root.join("src")).expect("could not create src");
    fs::write(root.join("src/lib.rs"), source).expect("could not write the library");

    dir
}

/// Creates a registry-free stand-in for the runtime dependency before cargo-gamma redirects it.
///
/// The stub deliberately omits `gamma_rt::embedded`: the fixture only compiles after the
/// dependency is redirected to cargo-gamma's real vendored runtime with `embedding` preserved.
fn runtime_stub(root: &std::path::Path) {
    let runtime = root.join("runtime-stub");

    fs::create_dir_all(runtime.join("src")).expect("could not create the runtime stub");
    fs::write(
        runtime.join("Cargo.toml"),
        "[package]\nname = \"cargo-gamma-rt\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n\
         [features]\nembedding = []\n",
    )
    .expect("could not write the runtime stub manifest");
    fs::write(runtime.join("src/lib.rs"), "").expect("could not write the runtime stub library");
}

fn scratch_base(dir: &TempDir) -> Utf8PathBuf {
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");

    gamma_base(&root)
}

/// Reproduces the cache-owner marker validated by `exec::workspace`.
///
/// Production stores the canonical workspace root, so the fixture must use the same path form and
/// marker name for seeded caches to be accepted as belonging to this workspace.
fn mark_cache_owner(dir: &TempDir, base: &Utf8PathBuf) {
    let owner = fs::canonicalize(dir.path()).expect("the workspace path can be resolved");
    let owner = Utf8PathBuf::from_path_buf(owner).expect("the resolved workspace path is UTF-8");

    fs::write(base.join(".cargo-gamma-owner"), owner.as_str()).expect("could not write the cache owner");
}

/// Runs the tool against a directory and returns the exit code and captured host.
fn invoke(dir: &TempDir, args: &[&str]) -> (i32, Sink) {
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let mut command = vec!["cargo-gamma".to_owned(), "gamma".to_owned()];

    command.extend(args.iter().map(|arg| (*arg).to_owned()));

    // `explain` reads only the registry, so it takes no directory.
    if args.first() != Some(&"explain") {
        command.push("--dir".to_owned());
        command.push(path.to_string());
    }

    let mut host = Sink::default();
    let code = run(&mut host, command);

    (code, host)
}

const SUBJECT: &str = "
/// Returns whether the value is in range.
pub fn in_range(value: i32, limit: i32) -> bool {
    value < limit
}

/// Adds a margin.
pub fn with_margin(value: i32) -> i32 {
    value + 10
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_work() {
        assert!(in_range(1, 2));
    }
}
";

#[test]
fn listing_mutants_reports_the_expected_operators() {
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["list", "mutants"]);
    let output = host.out();

    assert_eq!(code, EXIT_OK, "{}", host.err());
    assert!(output.contains("relational.lt_to_le"), "{output}");
    assert!(output.contains("arith.add_to_sub"), "{output}");
}

#[test]
fn configured_trait_implementation_exclusions_suppress_their_mutants() {
    let dir = workspace(
        "struct Subject(i32);

         impl core::fmt::Debug for Subject {
             fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                 let value = self.0 + 1;
                 write!(formatter, \"{value}\")
             }
         }

         impl core::fmt::Display for Subject {
             fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                 let value = self.0 - 1;
                 write!(formatter, \"{value}\")
             }
         }

         pub fn outside(value: i32) -> i32 { value * 2 }
        ",
    );
    fs::write(dir.path().join("gamma.toml"), "exclude-trait-impls = [\"Debug\", \"Display\"]\n").expect("could not write gamma.toml");

    let (unfiltered_code, unfiltered_host) = invoke(&dir, &["list", "mutants", "--no-config"]);
    let (filtered_code, filtered_host) = invoke(&dir, &["list", "mutants"]);
    let unfiltered = unfiltered_host.out();
    let filtered = filtered_host.out();

    assert_eq!(unfiltered_code, EXIT_OK, "{}", unfiltered_host.err());
    assert_eq!(filtered_code, EXIT_OK, "{}", filtered_host.err());
    assert!(unfiltered.contains("self.0 + 1"), "{unfiltered}");
    assert!(unfiltered.contains("self.0 - 1"), "{unfiltered}");
    assert!(!unfiltered.contains("[suppressed: config]"), "{unfiltered}");
    assert!(
        filtered
            .lines()
            .any(|line| line.contains("self.0 + 1") && line.contains("[suppressed: config]")),
        "{filtered}"
    );
    assert!(
        filtered
            .lines()
            .any(|line| line.contains("self.0 - 1") && line.contains("[suppressed: config]")),
        "{filtered}"
    );
    assert!(
        filtered
            .lines()
            .any(|line| line.contains("value * 2") && !line.contains("[suppressed:")),
        "{filtered}"
    );
}

#[test]
fn an_unmatched_mutant_exclusion_is_a_usage_error() {
    let dir = workspace("pub fn outside(value: i32) -> i32 { value * 2 }");
    fs::write(dir.path().join("gamma.toml"), "exclude-trait-impls = [\"Debgu\"]\n").expect("could not write gamma.toml");

    let (code, host) = invoke(&dir, &["list", "mutants"]);
    let error = host.err();

    assert_eq!(code, EXIT_USAGE, "{}", host.out());
    assert!(error.contains("matched no trait implementations"), "{error}");
    assert!(error.contains("Debgu"), "{error}");
}

#[test]
fn listing_mutants_reports_the_file_line_and_column() {
    let dir = workspace(SUBJECT);
    let (_, host) = invoke(&dir, &["list", "mutants"]);
    let output = host.out();

    assert!(output.contains("src/lib.rs:4:5"), "{output}");
}

#[test]
fn test_modules_are_not_mutated() {
    let dir = workspace(SUBJECT);
    let (_, host) = invoke(&dir, &["list", "mutants"]);
    let output = host.out();

    assert!(!output.contains("ranges_work"), "{output}");
    assert!(!output.contains("literal.int_to_zero]") || !output.contains("assert"), "{output}");
}

#[test]
fn doc_comments_are_not_reported_as_string_literals() {
    let dir = workspace(SUBJECT);
    let (_, host) = invoke(&dir, &["list", "mutants"]);
    let output = host.out();

    assert!(!output.contains("Returns whether the value is in range"), "{output}");
}

#[test]
fn selecting_one_operator_excludes_the_others() {
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["list", "mutants", "--mutators", "relational"]);
    let output = host.out();

    assert_eq!(code, EXIT_OK, "{}", host.err());
    assert!(output.contains("relational."), "{output}");
    assert!(!output.contains("arith."), "{output}");
}

#[test]
fn a_negated_selector_carves_out_of_a_family() {
    let dir = workspace(SUBJECT);
    let (_, host) = invoke(&dir, &["list", "mutants", "--mutators", "relational,!relational.lt_to_le"]);
    let output = host.out();

    assert!(output.contains("relational.lt_to_gt"), "{output}");
    assert!(!output.contains("relational.lt_to_le"), "{output}");
}

#[test]
fn an_unknown_selector_is_a_usage_error() {
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["list", "mutants", "--mutators", "relationl"]);

    assert_eq!(code, EXIT_USAGE, "{}", host.out());
    assert!(host.err().contains("did you mean `relational`"), "{}", host.err());
}

#[test]
fn an_out_of_range_shard_is_a_usage_error() {
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["list", "mutants", "--shard-count", "2", "--shard-index", "5"]);

    assert_eq!(code, EXIT_USAGE, "{}", host.out());
    assert!(host.err().contains("out of range"), "{}", host.err());
}

#[test]
fn shards_partition_the_mutants_exactly() {
    let dir = workspace(SUBJECT);
    let (_, whole) = invoke(&dir, &["list", "mutants"]);
    let total = whole.out().lines().count();

    let parts: usize = (0..3)
        .map(|index| {
            let (code, host) = invoke(
                &dir,
                &["list", "mutants", "--shard-count", "3", "--shard-index", &index.to_string()],
            );

            assert_eq!(code, EXIT_OK, "{}", host.err());
            host.out().lines().count()
        })
        .sum();

    assert_eq!(parts, total, "sharding lost or duplicated mutants");
}

#[test]
fn shard_membership_is_deterministic() {
    let dir = workspace(SUBJECT);
    let (_, first) = invoke(&dir, &["list", "mutants", "--shard-count", "3", "--shard-index", "1"]);
    let (_, second) = invoke(&dir, &["list", "mutants", "--shard-count", "3", "--shard-index", "1"]);

    assert_eq!(first.out(), second.out());
}

#[test]
fn file_filters_narrow_the_scan() {
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["list", "mutants", "--file", "**/lib.rs"]);

    assert_eq!(code, EXIT_OK, "{}", host.err());
    assert!(!host.out().trim().is_empty(), "{}", host.out());
}

#[test]
fn a_file_pattern_matching_nothing_is_a_usage_error() {
    // Silently reporting no mutants and exiting zero reads in CI exactly like a clean run, so a
    // typo in a checked-in pattern could hide a whole crate from mutation testing indefinitely.
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["list", "mutants", "--file", "nothing_matches.rs"]);

    assert_eq!(code, EXIT_USAGE, "{}", host.err());
    assert!(host.err().contains("no source file matches"), "{}", host.err());
}

#[test]
fn excluded_files_are_not_scanned() {
    let dir = workspace(SUBJECT);
    let (_, host) = invoke(&dir, &["list", "mutants", "--exclude-file", "lib.rs"]);

    assert!(host.out().trim().is_empty(), "{}", host.out());
}

#[test]
fn json_output_is_valid_json() {
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["list", "mutants", "--json"]);

    assert_eq!(code, EXIT_OK, "{}", host.err());

    let parsed: serde_json::Value = serde_json::from_str(&host.out()).expect("output was not valid JSON");

    assert!(parsed.as_array().is_some_and(|items| !items.is_empty()));
}

#[test]
fn json_mutants_carry_a_stable_id() {
    let dir = workspace(SUBJECT);
    let (_, first) = invoke(&dir, &["list", "mutants", "--json"]);
    let (_, second) = invoke(&dir, &["list", "mutants", "--json"]);

    let left: serde_json::Value = serde_json::from_str(&first.out()).expect("not JSON");
    let right: serde_json::Value = serde_json::from_str(&second.out()).expect("not JSON");

    let ids = |value: &serde_json::Value| -> Vec<String> {
        value
            .as_array()
            .expect("not an array")
            .iter()
            .map(|item| item["id"].as_str().expect("no id").to_owned())
            .collect()
    };

    let identifiers = ids(&left);

    assert!(!identifiers.is_empty());
    assert_eq!(identifiers, ids(&right));
}

#[test]
fn listing_files_reports_the_library() {
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["list", "files"]);

    assert_eq!(code, EXIT_OK, "{}", host.err());
    assert!(host.out().contains("src/lib.rs"), "{}", host.out());
}

#[test]
fn listing_ops_marks_the_enabled_set() {
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["list", "mutators"]);
    let output = host.out();

    assert_eq!(code, EXIT_OK, "{}", host.err());
    assert!(output.contains("relational.lt_to_le"), "{output}");
    assert!(output.contains("* = enabled by the current selection"), "{output}");
}

#[test]
fn a_run_reports_what_it_found() {
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["run", "--dry-run"]);

    assert_eq!(code, EXIT_OK, "{}", host.err());
    assert!(host.out().contains("mutants in"), "{}", host.out());
}

#[test]
fn test_execution_optimization_is_explicit_and_conflicts_with_whole_binaries() {
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["run", "--dry-run", "--optimize-test-execution"]);

    assert_eq!(code, EXIT_OK, "{}", host.err());

    let (conflict_code, conflict) = invoke(&dir, &["run", "--dry-run", "--optimize-test-execution", "--whole-test-binaries"]);

    assert_eq!(conflict_code, EXIT_USAGE, "{}", conflict.err());
    assert!(conflict.err().contains("--optimize-test-execution"), "{}", conflict.err());
    assert!(conflict.err().contains("--whole-test-binaries"), "{}", conflict.err());
}

#[test]
fn a_default_measured_run_skips_and_reports_the_census() {
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["run", "--jobs", "1"]);

    assert_eq!(code, EXIT_OK, "{}", host.err());

    let diagnostics =
        fs::read_to_string(dir.path().join("target/cargo-gamma/gamma-diagnostics.json")).expect("the measured run writes diagnostics");
    let diagnostics: serde_json::Value = serde_json::from_str(&diagnostics).expect("diagnostics are JSON");
    let phases = &diagnostics["phases"];

    assert_eq!(phases["censusStatus"], "disabled", "{diagnostics}");
    assert!(
        phases.get("census").is_none(),
        "a disabled census must perform no measured work: {diagnostics}"
    );
}

#[test]
fn clean_deletes_only_the_current_workspaces_cached_data() {
    let dir = workspace(SUBJECT);
    let base = scratch_base(&dir);
    let report = dir.path().join("target/cargo-gamma/gamma-report.json");

    fs::create_dir_all(&base).expect("cache");
    mark_cache_owner(&dir, &base);
    fs::create_dir(base.join("workspace")).expect("cached workspace");
    fs::write(base.join("last-gamma-run.json"), "{}").expect("run record");
    fs::create_dir_all(report.parent().expect("report parent")).expect("report directory");
    fs::write(&report, "{}").expect("published report");

    let (code, host) = invoke(&dir, &["clean"]);

    assert_eq!(code, EXIT_OK, "{}", host.err());
    assert!(host.err().contains("Cleaned"), "{}", host.err());
    assert!(!base.join("workspace").exists());
    assert!(!base.join("last-gamma-run.json").exists());
    assert!(report.exists(), "published reports must survive cache cleaning");
}

#[test]
fn a_measured_run_journals_every_testing_verdict_in_the_cache_directory() {
    let dir = workspace(SUBJECT);
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let production = cargo_gamma_lib::testing::production_gamma_base(&root);

    assert!(
        !production.exists(),
        "the fixture started with a production-cache entry at `{production}`"
    );

    let (code, host) = invoke(&dir, &["run", "--whole-test-binaries", "--jobs", "1"]);

    assert_eq!(code, EXIT_OK, "{}", host.err());

    let base = scratch_base(&dir);
    let path = base.join("gamma-progress.log");
    let text = fs::read_to_string(&path).unwrap_or_else(|cause| panic!("could not read `{path}`: {cause}"));

    assert!(base.starts_with(&root), "the test cache escaped its workspace: `{base}`");
    assert!(base.join(".cargo-gamma-owner").exists(), "the private cache was not claimed");
    assert!(
        !production.exists(),
        "the run wrote into the user's production cache at `{production}`"
    );
    assert!(text.contains("killed"), "{text}");
    assert!(text.contains("SURVIVED"), "{text}");
    assert!(!text.contains('\x1b'), "the log contains terminal escapes: {text:?}");
}

#[test]
fn cache_and_artifact_directories_are_independent() {
    let dir = workspace(SUBJECT);
    let cache = dir.path().join(".gamma-cache");
    let artifacts = dir.path().join("published");
    let (code, host) = invoke(
        &dir,
        &[
            "run",
            "--whole-test-binaries",
            "--jobs",
            "1",
            "--cache-dir",
            cache.to_str().expect("UTF-8 cache path"),
            "--artifact-dir",
            artifacts.to_str().expect("UTF-8 artifact path"),
        ],
    );

    assert_eq!(code, EXIT_OK, "{}", host.err());
    assert!(cache.join("last-gamma-run.json").exists(), "cache state was not redirected");
    assert!(cache.join("gamma-progress.log").exists(), "progress journal was not redirected");

    for name in [
        "gamma-report.json",
        "gamma-report.html",
        "gamma-report.sarif",
        "gamma-perf-advice.md",
        "gamma-diagnostics.json",
    ] {
        assert!(artifacts.join(name).exists(), "missing `{name}`");
        assert!(!cache.join(name).exists(), "`{name}` leaked into the cache");
    }
}

#[test]
fn a_run_with_no_subcommand_behaves_like_run() {
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["--dry-run"]);

    assert_eq!(code, EXIT_OK, "{}", host.err());
    assert!(host.out().contains("mutants in"), "{}", host.out());
}

#[test]
fn results_go_to_stdout_and_progress_goes_to_stderr() {
    // A user piping `list` into another program must not receive progress chatter in the pipe.
    let dir = workspace(SUBJECT);
    let (_, host) = invoke(&dir, &["list", "mutants"]);

    assert!(!host.out().is_empty());
    assert!(!host.out().contains("Scanning"), "{}", host.out());
}

#[test]
fn a_terminal_host_is_colorized() {
    let dir = workspace(SUBJECT);
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let mut host = Sink::default().terminal(100);

    let code = run(
        &mut host,
        [
            "cargo-gamma",
            "gamma",
            "run",
            "--dry-run",
            "--color",
            "always",
            "--dir",
            path.as_str(),
        ],
    );

    assert_eq!(code, EXIT_OK, "{}", host.err());
    assert!(host.out().contains('\x1b'), "{:?}", host.out());
}

#[test]
fn a_piped_host_is_not_colorized() {
    let dir = workspace(SUBJECT);
    let (_, host) = invoke(&dir, &["run", "--dry-run"]);

    assert!(!host.out().contains('\x1b'), "{:?}", host.out());
}

#[test]
fn explaining_a_mutator_describes_how_to_suppress_it() {
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["explain", "relational.lt_to_le"]);
    let output = host.out();

    assert_eq!(code, EXIT_OK, "{}", host.err());
    assert!(output.contains("replace < with <="), "{output}");
    assert!(output.contains("// #[gamma::skip(relational.lt_to_le)]"), "{output}");
}

#[test]
fn explaining_an_unknown_subject_is_a_usage_error() {
    let dir = workspace(SUBJECT);
    let (code, _) = invoke(&dir, &["explain", "not_a_mutator"]);

    assert_eq!(code, EXIT_USAGE);
}

#[test]
fn help_goes_to_stdout_and_exits_zero() {
    let mut host = Sink::default();
    let code = run(&mut host, ["cargo-gamma", "gamma", "--help"]);

    assert_eq!(code, EXIT_OK);
    assert!(host.out().contains("mutation testing"), "{}", host.out());
    assert!(host.err().is_empty(), "{}", host.err());
}

#[test]
fn an_unknown_flag_goes_to_stderr_and_exits_one() {
    let mut host = Sink::default();
    let code = run(&mut host, ["cargo-gamma", "gamma", "--not-a-flag"]);

    assert_eq!(code, EXIT_USAGE);
    assert!(!host.err().is_empty());
    assert!(host.out().is_empty(), "{}", host.out());
}

#[test]
fn the_tool_works_when_invoked_directly_rather_than_through_cargo() {
    let mut host = Sink::default();
    let code = run(&mut host, ["cargo-gamma", "--help"]);

    assert_eq!(code, EXIT_OK);
    assert!(host.out().contains("mutation testing"), "{}", host.out());
}

#[test]
fn a_directory_that_is_not_a_workspace_fails_without_panicking() {
    let dir = TempDir::new().expect("could not create a temporary directory");
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let mut host = Sink::default();

    let code = run(&mut host, ["cargo-gamma", "gamma", "list", "mutants", "--dir", path.as_str()]);

    assert_ne!(code, EXIT_OK);
    assert!(host.err().contains("cargo metadata"), "{}", host.err());
}

#[test]
fn a_file_that_does_not_parse_names_the_file() {
    let dir = workspace("pub fn broken( {");
    let (code, host) = invoke(&dir, &["list", "mutants"]);

    assert_ne!(code, EXIT_OK);
    assert!(host.err().contains("could not parse"), "{}", host.err());
    assert!(host.err().contains("lib.rs"), "{}", host.err());
}

#[test]
fn an_empty_library_yields_no_mutants() {
    let dir = workspace("");
    let (code, host) = invoke(&dir, &["list", "mutants"]);

    assert_eq!(code, EXIT_OK, "{}", host.err());
    assert!(host.out().trim().is_empty());
}

/// A redirected runtime dependency must keep the `features` it already carried.
///
/// `cargo-gamma-lib`'s own manifest depends on the runtime with `features = ["embedding"]`, which
/// gates a real API (`gamma_rt::embedded`). Redirecting that dependency to the run's vendored copy
/// used to rebuild it as a bare `{ package, path }` table, silently dropping the feature and
/// breaking any package — this crate included — whose code needs it.
#[test]
fn a_redirected_runtime_dependency_keeps_the_feature_gating_its_own_api() {
    let dir = TempDir::new().expect("could not create a temporary directory");
    let root = dir.path();

    runtime_stub(root);

    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"subject\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\ngamma_rt = { package = \"cargo-gamma-rt\", path = \"runtime-stub\", features = [\"embedding\"] }\n",
    )
    .expect("could not write the manifest");

    fs::create_dir_all(root.join("src")).expect("could not create src");
    fs::write(
        root.join("src/lib.rs"),
        "pub fn embedded_source_count() -> usize {\n    gamma_rt::embedded::SOURCES.len()\n}\n",
    )
    .expect("could not write the library");

    let (code, host) = invoke(&dir, &["run", "--whole-test-binaries", "--jobs", "1"]);

    assert_eq!(code, EXIT_OK, "{}", host.err());
}

/// A member that inherits the runtime dependency with `workspace = true` must keep the `features`
/// declared by the workspace's own `[workspace.dependencies]` entry — those settings live in a
/// different manifest than the one `link_runtime` edits, and used to be dropped once that entry
/// was rebuilt as a bare `{ package, path }` table, breaking any member whose code gates on them.
#[test]
fn a_workspace_inherited_runtime_dependency_keeps_the_feature_gating_its_own_api() {
    let dir = TempDir::new().expect("could not create a temporary directory");
    let root = dir.path();

    runtime_stub(root);

    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"subject\"]\nresolver = \"2\"\n\n\
         [workspace.dependencies]\ngamma_rt = { package = \"cargo-gamma-rt\", path = \"runtime-stub\", features = [\"embedding\"] }\n",
    )
    .expect("could not write the workspace manifest");

    fs::create_dir_all(root.join("subject/src")).expect("could not create the member's src");
    fs::write(
        root.join("subject/Cargo.toml"),
        "[package]\nname = \"subject\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\ngamma_rt = { workspace = true }\n",
    )
    .expect("could not write the member manifest");
    fs::write(
        root.join("subject/src/lib.rs"),
        "pub fn embedded_source_count() -> usize {\n    gamma_rt::embedded::SOURCES.len()\n}\n",
    )
    .expect("could not write the member's library");

    let (code, host) = invoke(&dir, &["run", "--whole-test-binaries", "--jobs", "1"]);

    assert_eq!(code, EXIT_OK, "{}", host.err());
}

/// The ids and files of every mutant the tool finds in a workspace, in the order it reports them.
fn population(dir: &TempDir) -> Vec<(String, String)> {
    let (code, host) = invoke(dir, &["list", "mutants", "--json"]);

    assert_eq!(code, EXIT_OK, "{}", host.err());

    let listed: serde_json::Value = serde_json::from_str(&host.out()).expect("the listing is JSON");

    listed
        .as_array()
        .expect("the listing is an array")
        .iter()
        .map(|mutant| {
            (
                mutant["id"].as_str().expect("every mutant has an id").to_owned(),
                mutant["file"].as_str().expect("every mutant names its file").to_owned(),
            )
        })
        .collect()
}

/// Writes a run record naming the first mutant unviable and the second one killed by a named test.
///
/// Written as text rather than through the tool so that the on-disk shape of the record is pinned
/// by something outside the code that produces it. A promotion that silently stopped reading a
/// field would otherwise still pass every test it has.
fn commit_workspace(dir: &TempDir) {
    let git = |arguments: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git should start")
    };

    assert!(git(&["init", "--quiet"]).success());
    assert!(git(&["add", "Cargo.toml", "src"]).success());
    assert!(
        git(&[
            "-c",
            "user.name=cargo-gamma",
            "-c",
            "user.email=cargo-gamma@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ])
        .success()
    );
}

fn seed_record(dir: &TempDir, population: &[(String, String)]) {
    commit_workspace(dir);
    let base = scratch_base(dir);

    fs::create_dir_all(&base).expect("could not create the scratch base");
    mark_cache_owner(dir, &base);

    let (unviable, file) = population.first().expect("the fixture yields mutants");
    let (killed, _elsewhere) = population.get(1).expect("the fixture yields more than one mutant");

    let record = serde_json::json!({
        "version": 9,
        "context": {
            "features": "f",
            "profile": "p",
            "rustflags": "r",
            "extra": "e",
            "toolchain": "t",
            "tool": "v",
        },
        "files": [{
            "path": file,
            "digest": "0",
            "size": 0,
            "mutants": [
                { "id": unviable, "outcome": "CompileError" },
            ],
        }],
        "hints": {
            killed.clone(): { "package": "subject", "target": "lib", "test": "tests::ranges_work" },
        },
    });

    fs::write(
        base.join("last-gamma-run.json"),
        serde_json::to_string(&record).expect("the record serializes"),
    )
    .expect("could not write the record");
}

/// The promoted artifact carries the two score-neutral tiers and refuses everything else.
#[test]
fn promoting_hints_writes_only_what_cannot_move_a_score() {
    let dir = workspace(SUBJECT);
    let population = population(&dir);

    seed_record(&dir, &population);

    let (code, host) = invoke(&dir, &["hints"]);

    assert_eq!(code, EXIT_OK, "{}", host.err());

    let written = fs::read_to_string(dir.path().join("gamma-hints.yaml")).expect("the artifact should have been written");
    let hints: yaml_serde::Value = yaml_serde::from_str(&written).expect("the artifact is YAML");

    assert_eq!(hints["version"].as_u64(), Some(3), "{written}");
    assert!(
        hints["tool"].as_str().is_some_and(|tool| tool.starts_with("cargo-gamma ")),
        "the artifact has to say what wrote it: {written}"
    );
    assert_eq!(hints["context"].as_mapping().expect("context").len(), 2, "{written}");
    assert_eq!(hints["context"]["repo_sha"].as_str().map(str::len), Some(40), "{written}");
    assert_eq!(hints["context"]["generated_on"].as_str().map(str::len), Some(10), "{written}");

    let files = hints["files"].as_sequence().expect("the artifact groups source files");
    let entries = files
        .iter()
        .flat_map(|file| file["mutants"].as_sequence().expect("the file group lists mutants"))
        .collect::<Vec<_>>();

    assert_eq!(entries.len(), 2, "one unviable mutant and one probe: {written}");

    assert!(
        entries.iter().any(|entry| entry["unviable"] == true),
        "the build-order tier is missing: {written}"
    );
    assert!(
        files.iter().any(|file| {
            file["killers"]
                .as_sequence()
                .expect("the file group interns killers")
                .iter()
                .any(|killer| killer["test"] == "tests::ranges_work")
        }),
        "the probe tier is missing: {written}"
    );

    // A killer hint must not become a verdict in the promoted artifact.
    assert!(!written.contains("Killed"), "a verdict reached the artifact: {written}");
    assert!(!written.contains("Survived"), "a verdict reached the artifact: {written}");
}

/// A preview says what would be promoted and writes nothing.
#[test]
fn previewing_a_promotion_writes_no_file() {
    let dir = workspace(SUBJECT);
    let population = population(&dir);

    seed_record(&dir, &population);

    let (code, host) = invoke(&dir, &["hints", "--dry-run"]);

    assert_eq!(code, EXIT_OK, "{}", host.err());
    assert!(host.err().contains("would carry"), "{}", host.err());
    assert!(
        !dir.path().join("gamma-hints.yaml").exists(),
        "a preview must not write the artifact"
    );
}

/// Regenerating an artifact from an unchanged record produces the same bytes and says so.
///
/// Order stability is what makes a generated file reviewable in version control. A population in
/// the tens of thousands regenerated on a schedule would otherwise produce a diff nobody can read,
/// and an unreviewable file in a repository is a liability rather than an asset.
#[test]
fn regenerating_an_unchanged_artifact_changes_no_bytes() {
    let dir = workspace(SUBJECT);
    let population = population(&dir);

    seed_record(&dir, &population);

    let (first_code, first_host) = invoke(&dir, &["hints"]);

    assert_eq!(first_code, EXIT_OK, "{}", first_host.err());

    let first = fs::read_to_string(dir.path().join("gamma-hints.yaml")).expect("the artifact exists");

    let (second_code, host) = invoke(&dir, &["hints"]);
    let second = fs::read_to_string(dir.path().join("gamma-hints.yaml")).expect("the artifact still exists");

    assert_eq!(second_code, EXIT_OK, "{}", host.err());
    assert_eq!(first, second, "the artifact is not byte-stable across regeneration");
    assert!(host.err().contains("Unchanged"), "{}", host.err());
}

#[test]
fn no_op_promotion_preserves_a_current_artifact_without_generalized_hints() {
    let dir = workspace(SUBJECT);
    let artifact = concat!(
        "version: 3\n",
        "tool: cargo-gamma older\n",
        "context:\n",
        "  repo_sha: old\n",
        "  generated_on: '2026-09-16'\n",
        "files: []\n",
    );
    let path = dir.path().join("gamma-hints.yaml");
    fs::write(&path, artifact).expect("current hints are writable");

    let (code, host) = invoke(&dir, &["hints"]);

    assert_eq!(code, EXIT_OK, "{}", host.err());
    assert_eq!(fs::read_to_string(path).expect("current hints remain readable"), artifact);
    assert!(host.err().contains("Unchanged"), "{}", host.err());
}

/// A record that learned nothing promotes nothing, and says so rather than writing an empty file.
#[test]
fn promoting_without_a_record_writes_nothing_and_explains_why() {
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["hints"]);

    assert_eq!(code, EXIT_OK, "{}", host.err());
    assert!(host.err().contains("nothing to promote"), "{}", host.err());
    assert!(!dir.path().join("gamma-hints.yaml").exists());
}

/// An empty legacy generation still needs migration even when the latest run learned nothing.
#[test]
fn promoting_empty_legacy_hints_writes_yaml_and_removes_json() {
    let dir = workspace(SUBJECT);
    commit_workspace(&dir);
    let legacy = serde_json::json!({
        "version": 1,
        "tool": "cargo-gamma legacy",
        "context": {
            "features": "f",
            "profile": "p",
            "rustflags": "r",
            "extra": "e",
            "toolchain": "t",
            "tool": "v",
        },
        "mutants": [],
    });
    fs::write(
        dir.path().join("gamma-hints.json"),
        serde_json::to_vec(&legacy).expect("legacy hints serialize"),
    )
    .expect("legacy hints are writable");

    let (code, host) = invoke(&dir, &["hints"]);

    assert_eq!(code, EXIT_OK, "{}", host.err());
    assert!(host.err().contains("Wrote"), "{}", host.err());
    assert!(dir.path().join("gamma-hints.yaml").exists());
    assert!(!dir.path().join("gamma-hints.json").exists());
}

/// Explicit replacement writes a valid empty generation instead of retaining unknown bytes.
#[test]
fn replacing_an_unknown_artifact_without_new_hints_still_writes() {
    let dir = workspace(SUBJECT);
    commit_workspace(&dir);
    fs::write(dir.path().join("gamma-hints.yaml"), "unknown: artifact\n").expect("unknown hints are writable");

    let (code, host) = invoke(&dir, &["hints", "--replace"]);
    let written = fs::read_to_string(dir.path().join("gamma-hints.yaml")).expect("replacement hints exist");
    let hints: yaml_serde::Value = yaml_serde::from_str(&written).expect("replacement hints are YAML");

    assert_eq!(code, EXIT_OK, "{}", host.err());
    assert!(host.err().contains("Wrote"), "{}", host.err());
    assert_eq!(hints["version"].as_u64(), Some(3), "{written}");
    assert!(hints["files"].as_sequence().expect("file groups").is_empty(), "{written}");
}

/// A partial selection preserves absences, while a complete replacement deliberately retires them.
#[test]
fn incremental_promotion_preserves_unproven_stale_hints_until_replacement() {
    let dir = workspace(SUBJECT);
    let population = population(&dir);

    seed_record(&dir, &population);
    let (first_code, first_host) = invoke(&dir, &["hints"]);

    assert_eq!(first_code, EXIT_OK, "{}", first_host.err());
    let before = fs::read_to_string(dir.path().join("gamma-hints.yaml")).expect("the first generation");

    // The mutants keep their identities only as long as the code they name does. Rewriting the
    // library retires every one of them, but the default mutator preset is not complete enough to
    // prove that every absent id belongs to its selection.
    fs::write(dir.path().join("src/lib.rs"), "pub fn unrelated() -> u8 { 7 }\n").expect("could not rewrite the library");

    let (code, host) = invoke(&dir, &["hints"]);
    let preserved = fs::read_to_string(dir.path().join("gamma-hints.yaml")).expect("the preserved generation");

    assert_eq!(code, EXIT_OK, "{}", host.err());
    assert_eq!(preserved, before);
    assert!(host.err().contains("preserved"), "{}", host.err());

    let (replacement_code, replacement) = invoke(&dir, &["hints", "--replace"]);
    let replaced = fs::read_to_string(dir.path().join("gamma-hints.yaml")).expect("the replacement generation");
    let yaml: yaml_serde::Value = yaml_serde::from_str(&replaced).expect("replacement YAML");

    assert_eq!(replacement_code, EXIT_OK, "{}", replacement.err());
    assert!(yaml["files"].as_sequence().expect("file groups").is_empty(), "{replaced}");
    assert!(replacement.err().contains("removed"), "{}", replacement.err());
}

/// A corrupt artifact is ignored by automatic consumers but protected from incremental promotion.
///
/// The file lives in version control, so it will be merged badly, truncated by a failed checkout,
/// and written by a version that does not exist yet. A run that failed over any of that would have
/// turned an optimization into a dependency. Promotion is explicit, so it must refuse to discard
/// unknown knowledge unless replacement was requested.
#[test]
fn a_corrupt_artifact_costs_runs_only_and_requires_explicit_replacement() {
    let dir = workspace(SUBJECT);

    fs::write(dir.path().join("gamma-hints.yaml"), "{ not yaml at all").expect("could not write the artifact");

    let (code, host) = invoke(&dir, &["list", "mutants"]);

    assert_eq!(code, EXIT_OK, "{}", host.err());
    assert!(host.out().contains("relational.lt_to_le"), "{}", host.out());

    let population = population(&dir);

    seed_record(&dir, &population);

    let (promoted, promotion) = invoke(&dir, &["hints"]);

    assert_ne!(promoted, EXIT_OK, "incremental promotion replaced unknown knowledge");
    assert!(promotion.err().contains("use `--replace`"), "{}", promotion.err());
    assert_eq!(
        fs::read_to_string(dir.path().join("gamma-hints.yaml")).expect("the corrupt artifact remains"),
        "{ not yaml at all"
    );

    let (replaced, replacement) = invoke(&dir, &["hints", "--replace"]);

    assert_eq!(replaced, EXIT_OK, "{}", replacement.err());

    let written = fs::read_to_string(dir.path().join("gamma-hints.yaml")).expect("the artifact was rewritten");

    assert!(yaml_serde::from_str::<yaml_serde::Value>(&written).is_ok(), "{written}");
}
