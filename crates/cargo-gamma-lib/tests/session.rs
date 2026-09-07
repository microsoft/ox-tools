// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]

//! End-to-end mutation sessions against real crates.
//!
//! These tests drive the whole pipeline: copy the tree, vendor the guard runtime, build one
//! instrumented binary, measure the baseline and run every mutant. They invoke a real `cargo`, so
//! they are slower than the rest of the suite, but they are the only coverage that proves the
//! encoding in `schema.rs` actually compiles and that a verdict means what it claims.

use std::sync::Arc;
use std::{fs, thread};

use camino::Utf8PathBuf;
use cargo_gamma_lib::internals::exec::gamma_base;
use cargo_gamma_lib::run;
use cargo_gamma_lib::testing::Sink;
use tempfile::TempDir;

/// Exit code for a run in which every gate passed.
const EXIT_OK: i32 = 0;

/// Exit code for a usage error, which is what a rejected option produces.
const EXIT_USAGE: i32 = 1;

fn scratch_base(dir: &TempDir) -> Utf8PathBuf {
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");

    gamma_base(&root, None)
}

/// A subject whose comparison is asserted exactly and whose side effect is not.
const SUBJECT: &str = "
pub fn is_adult(age: u32) -> bool {
    age >= 18
}

#[derive(Default)]
pub struct Log {
    entries: Vec<u32>,
}

impl Log {
    pub fn record(&mut self, value: u32) {
        self.entries.push(value);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boundary_is_pinned() {
        assert!(!is_adult(17));
        assert!(is_adult(18));
        assert!(is_adult(19));
    }

    #[test]
    fn recording_does_not_panic() {
        let mut log = Log::default();

        log.record(1);
    }
}
";

/// A crate whose only mutant cannot compile, so `suppress --eligible unviable` has something to write.
///
/// A trait object is what makes the mutant unviable and keeps it so. It names a capability rather
/// than a type, so there is no value to put inside the `Some`, and the family falls back on
/// `Default::default()`, which cannot compile here. A reference to a concrete type would not do:
/// those are served by leaking a box, and the mutant would build.
const UNVIABLE: &str = "
pub fn lookup() -> Option<&'static dyn core::fmt::Debug> {
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_builds() {
        assert!(super::lookup().is_none());
    }
}
";

/// Builds a throwaway crate containing `source`.
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

/// Builds a custom test harness whose nextest enumeration fails after mutation starts.
fn enumeration_failure(persistent: bool) -> TempDir {
    let dir = workspace(
        "
pub fn answer() -> u32 {
    42
}
",
    );
    let root = dir.path();

    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"subject\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [[test]]\nname = \"enumeration\"\npath = \"tests/enumeration.rs\"\nharness = false\n\n[dependencies]\n",
    )
    .expect("could not write the manifest");
    fs::create_dir_all(root.join("tests")).expect("could not create tests");
    fs::write(
        root.join("tests/enumeration.rs"),
        format!(
            r#"
fn main() {{
    let listing = std::env::args().any(|argument| argument == "--list");
    let active = std::env::var_os("GAMMA_ACTIVE").is_some();
    let marker = std::path::Path::new("persistent-104.marker");

    if listing && active {{
        if {persistent} {{
            std::fs::write(marker, "").expect("could not mark the active enumeration failure");
        }}
        eprintln!("enumeration failed while the mutant was active");
        std::process::exit(104);
    }}

    if listing && marker.exists() {{
        eprintln!("enumeration still failed with no mutant active");
        std::process::exit(104);
    }}

    if listing {{
        println!("enumeration::checks_answer: test");
    }} else {{
        assert_eq!(subject::answer(), 42);
    }}
}}
"#
        ),
    )
    .expect("could not write the custom test harness");

    dir
}

/// Builds a crate whose second module is behind a feature that is off by default.
///
/// The module holds a mutant of a kind that could never compile if it were built at all, so a run
/// that reports it as anything other than unbuilt has either compiled code it should not have or
/// judged code no compiler ever read.
fn conditional() -> TempDir {
    let dir = workspace(SUBJECT);
    let root = dir.path();

    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"subject\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[features]\nextra = []\n\n[dependencies]\n",
    )
    .expect("could not write the manifest");

    fs::write(
        root.join("src/lib.rs"),
        format!("{SUBJECT}\n#[cfg(feature = \"extra\")]\npub mod extra;\n"),
    )
    .expect("could not write the library");

    fs::write(
        root.join("src/extra.rs"),
        "pub fn width(name: &String) -> usize {\n    name.len()\n}\n",
    )
    .expect("could not write the conditional module");

    dir
}

/// Builds a two-package workspace in which nothing links `island`.
///
/// `island` opts its lib target out of testing, so the build produces no test binary for it, and
/// `mainland` does not depend on it. No test that exists can reach the island's code.
fn archipelago() -> TempDir {
    let dir = TempDir::new().expect("could not create a temporary directory");
    let root = dir.path();

    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"mainland\", \"island\"]\nresolver = \"2\"\n",
    )
    .expect("could not write the workspace manifest");

    for (name, extra) in [("mainland", ""), ("island", "\n[lib]\ntest = false\n")] {
        let package = root.join(name);

        fs::create_dir_all(package.join("src")).expect("could not create src");
        fs::write(
            package.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n{extra}"),
        )
        .expect("could not write the manifest");
        fs::write(package.join("src/lib.rs"), SUBJECT).expect("could not write the library");
    }

    dir
}

/// Whether these tests are themselves running inside a mutation run.
///
/// Each of these drives a real cargo build. Nested inside a scratch tree that has already been
/// instrumented, that build fails for reasons that have nothing to do with any mutant, which turns
/// into a red baseline and stops the run before it starts. `CARGO_GAMMA` is set on every test
/// process precisely so a suite that shells out to cargo can step aside.
fn nested() -> bool {
    std::env::var_os("CARGO_GAMMA").is_some()
}

/// Steps aside when this suite is running inside a mutation run, saying so where it can be seen.
///
/// The early return has to stay — a nested cargo build inside an instrumented tree fails for
/// reasons that have nothing to do with any mutant — but a silent one is worse than the problem it
/// avoids. Forty-odd tests reporting success without executing means a self-run of this tool over
/// itself presents a score built on tests that did nothing, and nothing anywhere says so.
///
/// The announcement is written to the file descriptor rather than through `eprintln!` because the
/// harness captures the macros and only replays what a *failing* test printed. A step-aside is not
/// a failure, so its record would be discarded by the one mechanism that exists to preserve it.
/// Stable Rust has no way for a running test to report itself skipped, so a line on stderr and a
/// running count is the strongest signal available.
macro_rules! step_aside_if_nested {
    () => {
        if nested() {
            let _announced = ::std::io::Write::write_all(
                &mut ::std::io::stderr(),
                format!(
                    "stepping aside ({}): {} runs a real cargo build, which cannot nest inside an \
                     instrumented tree\n",
                    STEPPED_ASIDE.fetch_add(1, ::core::sync::atomic::Ordering::Relaxed) + 1,
                    ::std::thread::current().name().unwrap_or("a session test"),
                )
                .as_bytes(),
            );

            return;
        }
    };
}

/// Steps aside when a test needs the optional `cargo-nextest` executable.
macro_rules! step_aside_without_nextest {
    () => {
        if !std::process::Command::new("cargo-nextest")
            .arg("nextest")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            let _announced = ::std::io::Write::write_all(
                &mut ::std::io::stderr(),
                format!(
                    "stepping aside ({}): {} requires cargo-nextest, which is not installed\n",
                    STEPPED_ASIDE.fetch_add(1, ::core::sync::atomic::Ordering::Relaxed) + 1,
                    ::std::thread::current().name().unwrap_or("a session test"),
                )
                .as_bytes(),
            );

            return;
        }
    };
}

/// How many tests in this file have stepped aside so far.
static STEPPED_ASIDE: ::core::sync::atomic::AtomicUsize = ::core::sync::atomic::AtomicUsize::new(0);

/// Runs a session against `dir` and returns the exit code and everything the tool printed.
fn session(dir: &TempDir, args: &[&str]) -> (i32, String) {
    let (code, out, err) = session_on(Sink::default(), dir, args);

    (code, format!("{out}{err}"))
}

/// Runs a session under the default case-level reachability policy.
///
/// Kept separate because most tests in this file exercise unrelated behavior and run concurrently;
/// making all fifty of them census at once creates a process storm no real invocation produces.
fn censused_session(dir: &TempDir, args: &[&str]) -> (i32, String) {
    let (code, out, err) = session_on_with(Sink::default(), dir, args, false);

    (code, format!("{out}{err}"))
}

/// Runs a session on a given host, keeping the two streams apart.
fn session_on(host: Sink, dir: &TempDir, args: &[&str]) -> (i32, String, String) {
    session_on_with(host, dir, args, true)
}

fn session_on_with(mut host: Sink, dir: &TempDir, args: &[&str], whole_test_binaries: bool) -> (i32, String, String) {
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let mut command = vec![
        "cargo-gamma".to_owned(),
        "gamma".to_owned(),
        "run".to_owned(),
        "--jobs".to_owned(),
        "1".to_owned(),
    ];

    command.extend(args.iter().map(|arg| (*arg).to_owned()));
    if whole_test_binaries && !args.contains(&"--whole-test-binaries") {
        command.push("--whole-test-binaries".to_owned());
    }
    command.push("--dir".to_owned());
    command.push(path.to_string());

    let code = run(&mut host, command);

    (code, host.out(), host.err())
}

#[test]
fn an_asserted_boundary_catches_its_mutant() {
    step_aside_if_nested!();
    let dir = workspace(SUBJECT);
    let (code, output) = session(&dir, &["--mutators", "relational"]);

    assert_eq!(code, EXIT_OK, "{output}");

    // `>=` becoming `>` moves the boundary onto 18, which the test pins, so it must be caught.
    assert!(!output.contains("SURVIVED src/lib.rs:3:5"), "{output}");
    assert!(output.contains("0 survived,"), "{output}");
}

/// The run record accelerates a second run without carrying its test verdicts.
#[test]
fn a_second_run_reestablishes_the_first_runs_kills() {
    step_aside_if_nested!();
    let dir = workspace(SUBJECT);
    let (first, output) = session(&dir, &["--mutators", "relational"]);

    assert_eq!(first, EXIT_OK, "{output}");

    let record = scratch_base(&dir).join("last-gamma-run.json");
    let text = fs::read_to_string(&record).expect("the run record should have been written");

    assert!(!text.contains("\"outcome\":\"killed\""), "{text}");

    // With `--incremental no`, the record and its checked killer hints are ignored.
    let (cold, output) = session(&dir, &["--mutators", "relational", "--incremental", "no"]);

    assert_eq!(cold, EXIT_OK, "{output}");
    assert!(!output.contains("Iterating"), "a cold run must not adopt a kill: {output}");

    // The default build cache may try the prior killer first, but it runs the mutant again.
    let (second, output) = session(&dir, &["--mutators", "relational"]);

    assert_eq!(second, EXIT_OK, "{output}");
    assert!(output.contains("0 survived,"), "{output}");
    assert!(!output.contains("Iterating"), "a kill must never be carried: {output}");
}

fn incremental_args(cache: Option<&str>) -> Vec<&str> {
    let mut args = vec!["--mutators", "relational", "--incremental", "build"];

    if let Some(cache) = cache {
        args.extend(["--cache-dir", cache]);
    }

    args
}

fn assert_incremental_cache_lock_is_held(cache: Option<&str>) {
    step_aside_if_nested!();
    let dir = Arc::new(workspace(SUBJECT));
    let args = incremental_args(cache);
    let (warmed, output) = session(&dir, &args);

    assert_eq!(warmed, EXIT_OK, "{output}");

    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("workspace path is not UTF-8");
    let mut adopted = cargo_gamma_lib::testing::hold_after_cache_adoption(root.clone());
    let first_dir = Arc::clone(&dir);
    let first_cache = cache.map(str::to_owned);
    let first = thread::spawn(move || session(&first_dir, &incremental_args(first_cache.as_deref())));

    let adopted_lock = adopted.wait();

    let (blocked, output) = session(&dir, &args);

    assert_ne!(blocked, EXIT_OK, "{output}");
    assert!(output.contains("already using"), "{output}");

    let mut preparing = cargo_gamma_lib::testing::hold_during_workspace_preparation(root);
    adopted.release();
    let preparing_lock = preparing.wait();

    assert_eq!(
        preparing_lock, adopted_lock,
        "workspace preparation must receive the lock pair claimed before cache adoption"
    );

    let (blocked, output) = session(&dir, &args);

    assert_ne!(blocked, EXIT_OK, "{output}");
    assert!(output.contains("already using"), "{output}");

    preparing.release();

    let (first_code, output) = first.join().expect("the paused incremental command should finish after release");

    assert_eq!(first_code, EXIT_OK, "{output}");

    let (reacquired, output) = session(&dir, &args);

    assert_eq!(reacquired, EXIT_OK, "{output}");
}

#[test]
fn an_incremental_command_holds_the_default_cache_lock_from_adoption_through_preparation() {
    assert_incremental_cache_lock_is_held(None);
}

#[test]
fn an_incremental_command_holds_a_redirected_cache_lock_from_adoption_through_preparation() {
    let cache = TempDir::new().expect("could not create the redirected cache");
    let cache = cache.path().to_str().expect("cache path is not UTF-8").to_owned();

    assert_incremental_cache_lock_is_held(Some(&cache));
}

#[test]
fn an_unasserted_side_effect_leaves_a_survivor() {
    step_aside_if_nested!();
    let dir = workspace(SUBJECT);
    let (code, output) = session(&dir, &["--mutators", "stmt"]);

    assert_eq!(code, EXIT_OK, "{output}");

    // Nothing observes the recorded entry, so deleting the push cannot fail a test.
    assert!(output.contains("SURVIVED"), "{output}");
    assert!(output.contains("self.entries.push"), "{output}");
}

#[test]
fn a_failing_score_gate_fails_the_run() {
    step_aside_if_nested!();
    let dir = workspace(SUBJECT);
    let (code, output) = session(&dir, &["--mutators", "stmt", "--min-score", "100"]);

    assert_ne!(code, EXIT_OK, "{output}");
    assert!(output.contains("below the required"), "{output}");
}

#[test]
fn a_suppressed_mutant_is_never_run() {
    step_aside_if_nested!();
    let source = SUBJECT.replace(
        "pub fn is_adult(age: u32) -> bool {",
        "// #[gamma::skip(relational)]\npub fn is_adult(age: u32) -> bool {",
    );
    let dir = workspace(&source);
    let (code, output) = session(&dir, &["--mutators", "relational"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("none tested"), "{output}");

    // Nothing was left to run, so the session must not have paid for a build at all.
    assert!(!output.contains("built once"), "{output}");
}

#[test]
fn a_red_baseline_is_reported_rather_than_measured() {
    step_aside_if_nested!();
    let source = format!("{SUBJECT}\n#[test]\nfn always_fails() {{ panic!(\"nope\"); }}\n");
    let dir = workspace(&source);
    let (code, output) = session(&dir, &["--mutators", "relational"]);

    assert_ne!(code, EXIT_OK, "{output}");
    assert!(output.contains("the baseline could not be measured"), "{output}");
    assert!(output.contains("test `always_fails` failed"), "{output}");
    assert!(output.contains("baseline-failure.json"), "{output}");
    assert!(output.contains("gamma-diagnostics.json"), "{output}");
    assert!(!output.contains("nope"), "{output}");
}

#[test]
fn a_file_whose_every_mutant_is_unviable_still_converges() {
    step_aside_if_nested!();
    // `Some(Default::default())` cannot compile for a trait object, so the mutant has to be
    // withdrawn. Withdrawing the only mutant in a file must not leave the previous round's
    // instrumented copy in the tree, or the offending guard survives its own withdrawal and the
    // build can never be made to succeed.
    let dir = workspace(UNVIABLE);
    let (code, output) = session(&dir, &["--mutators", "fn_value.some_default", "--show-unviable"]);

    assert_eq!(code, EXIT_OK, "{output}");

    // The summary line no longer counts unviable mutants, so the listing `--show-unviable` opts
    // into is what proves the withdrawal happened rather than the build failing outright.
    assert!(output.contains("[fn_value.some_default]"), "{output}");
    assert!(output.contains("none tested"), "{output}");
}

#[test]
fn the_reporters_write_a_conformant_document_and_a_self_contained_page() {
    step_aside_if_nested!();
    let dir = workspace(SUBJECT);
    let artifacts = dir.path().join("artifacts");
    let json = artifacts.join("gamma-report.json");
    let html = artifacts.join("gamma-report.html");
    let (code, output) = session(
        &dir,
        &[
            "--mutators",
            "relational",
            "--artifact-dir",
            artifacts.to_str().expect("path is not UTF-8"),
        ],
    );

    assert_eq!(code, EXIT_OK, "{output}");

    let document: serde_json::Value = serde_json::from_str(&fs::read_to_string(&json).expect("the JSON report was not written"))
        .expect("the JSON report is not valid JSON");

    assert_eq!(document["schemaVersion"], "2");
    assert_eq!(document["framework"]["name"], "cargo-gamma");

    let files = document["files"].as_object().expect("files is an object");
    let mutants = files["src/lib.rs"]["mutants"].as_array().expect("mutants is an array");

    assert!(!mutants.is_empty(), "the report has no mutants");
    assert!(files["src/lib.rs"]["source"].as_str().is_some_and(|s| s.contains("is_adult")));

    // Connect the verdict the run reached to the string the artifact exports. The boundary tests
    // pin `is_adult` on both sides, so every relational mutant of `age >= 18` is killed — and the
    // serialized `status` must say `Killed`, not merely be *some* valid status. Without this the
    // console score and the report could disagree silently: a killed mutant could export under the
    // wrong verdict and a clean run would render catastrophic in the artifact, or the reverse.
    assert!(
        mutants.iter().any(|mutant| mutant["status"] == "Killed"),
        "no killed mutant was exported with status Killed: {mutants:?}"
    );

    // The page has to survive being opened from a file:// URL with no network, so the viewer and
    // the payload both have to be in it, and nothing may be fetched.
    let page = fs::read_to_string(&html).expect("the HTML report was not written");

    assert!(page.contains("<mutation-test-report-app"), "the custom element is missing");
    assert!(!page.contains("cdn.jsdelivr.net"), "the offline page references a CDN");
    assert!(page.len() > 200_000, "the viewer was not inlined: {} bytes", page.len());
}

#[test]
fn the_config_file_is_honoured_by_a_real_run() {
    step_aside_if_nested!();
    // The unit tests prove the merge; this proves the merged values actually reach the session,
    // which is the part that silently does nothing if the wiring is wrong.
    let dir = workspace(SUBJECT);

    fs::write(dir.path().join("gamma.toml"), "mutators = [\"stmt\"]\nmin-score = 100.0\n").expect("could not write the config");

    let (code, output) = session(&dir, &[]);

    // `stmt` leaves a survivor and the configured gate demands a perfect score, so a run that
    // ignored the file would pass with the default operator set and no gate at all.
    assert_ne!(code, EXIT_OK, "{output}");
    assert!(output.contains("below the required"), "{output}");
}

#[test]
fn a_misspelled_config_key_stops_the_run() {
    step_aside_if_nested!();
    let dir = workspace(SUBJECT);

    fs::write(dir.path().join("gamma.toml"), "op = [\"stmt\"]\n").expect("could not write the config");

    let (code, output) = session(&dir, &[]);

    assert_ne!(code, EXIT_OK, "{output}");
    assert!(output.contains("unknown field"), "{output}");
}

#[test]
fn a_foreign_config_is_reported_as_unread() {
    step_aside_if_nested!();
    let dir = workspace(SUBJECT);

    fs::create_dir_all(dir.path().join(".cargo")).expect("could not create .cargo");
    fs::write(dir.path().join(".cargo/mutants.toml"), "exclude_re = [\"impl Debug\"]\n").expect("could not write the foreign config");

    let (code, output) = session(&dir, &["--dry-run", "--progress", "always"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains(".cargo/mutants.toml is not supported or read"), "{output}");
    let note = output
        .find(".cargo/mutants.toml is not supported or read")
        .expect("foreign-config note");
    let analyzing = output.find("Analyzing the workspace").expect("analysis phase");

    assert!(note < analyzing, "{output}");
}

#[test]
fn suppressing_writes_a_directive_that_actually_suppresses_the_mutant() {
    step_aside_if_nested!();
    let dir = workspace(UNVIABLE);
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let mut host = Sink::default();
    let code = run(
        &mut host,
        vec![
            "cargo-gamma".to_owned(),
            "gamma".to_owned(),
            "suppress".to_owned(),
            "--eligible".to_owned(),
            "unviable".to_owned(),
            "--mutators".to_owned(),
            "fn_value.some_default".to_owned(),
            "--dir".to_owned(),
            path.to_string(),
        ],
    );

    let output = format!("{}{}", host.out(), host.err());

    assert_eq!(code, EXIT_OK, "{output}");

    let source = fs::read_to_string(dir.path().join("src/lib.rs")).expect("could not read the source");

    assert!(source.contains("// #[gamma::skip(fn_value.some_default"), "{source}");
    assert!(
        source.contains("written by cargo gamma suppress"),
        "the directive must say who wrote it"
    );

    // The written directive has to be one the tool itself honours; verification inside `suppress`
    // asserts that, and this asserts the verification was not vacuous.
    let (code, output) = session(&dir, &["--mutators", "fn_value.some_default", "--dry-run"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("none tested"), "{output}");
}

#[test]
fn suppressing_a_dry_run_prints_a_diff_and_changes_nothing() {
    step_aside_if_nested!();
    let dir = workspace(UNVIABLE);
    let before = fs::read_to_string(dir.path().join("src/lib.rs")).expect("could not read the source");
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let mut host = Sink::default();
    let code = run(
        &mut host,
        vec![
            "cargo-gamma".to_owned(),
            "gamma".to_owned(),
            "suppress".to_owned(),
            "--dry-run-suppress".to_owned(),
            "--eligible".to_owned(),
            "unviable".to_owned(),
            "--mutators".to_owned(),
            "fn_value.some_default".to_owned(),
            "--dir".to_owned(),
            path.to_string(),
        ],
    );

    assert_eq!(code, EXIT_OK, "{}{}", host.out(), host.err());
    assert!(host.out().contains('+'), "{}", host.out());
    assert!(host.out().contains("gamma::skip"), "{}", host.out());

    let after = fs::read_to_string(dir.path().join("src/lib.rs")).expect("could not read the source");

    assert_eq!(before, after, "a dry run must not touch the source");
}

#[test]
fn suppressing_refuses_to_touch_a_survivor() {
    step_aside_if_nested!();
    // The guarantee the whole feature rests on, asserted through the CLI rather than the parser,
    // because the parser is not what a user reaches for.
    let dir = workspace(SUBJECT);
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let mut host = Sink::default();
    let code = run(
        &mut host,
        vec![
            "cargo-gamma".to_owned(),
            "gamma".to_owned(),
            "suppress".to_owned(),
            "--eligible".to_owned(),
            "missed".to_owned(),
            "--dir".to_owned(),
            path.to_string(),
        ],
    );

    assert_ne!(code, EXIT_OK, "{}", host.err());
    assert!(host.err().contains("gap in the test suite"), "{}", host.err());
}

#[test]
fn two_shards_merge_into_one_score() {
    step_aside_if_nested!();

    // The end-to-end justification for sharding: two nights of partial runs have to add up to one
    // answer, or the feature only halves the work without ever producing a number.
    let dir = workspace(SUBJECT);
    let mut reports = Vec::new();

    for index in 0..2 {
        let artifacts = dir.path().join(format!("shard-{index}-artifacts"));
        let path = artifacts.join("gamma-report.json");
        let (code, output) = session(
            &dir,
            &[
                "--mutators",
                "relational",
                "--shard-count",
                "2",
                "--shard-index",
                &index.to_string(),
                "--artifact-dir",
                artifacts.to_str().expect("path is not UTF-8"),
            ],
        );

        assert_eq!(code, EXIT_OK, "{output}");
        reports.push(path);
    }

    let merged = dir.path().join("merged.json");
    let mut host = Sink::default();
    let mut command = vec!["cargo-gamma".to_owned(), "gamma".to_owned(), "merge".to_owned()];

    for path in &reports {
        command.push(path.to_str().expect("path is not UTF-8").to_owned());
    }

    command.push("--json-report".to_owned());
    command.push(merged.to_str().expect("path is not UTF-8").to_owned());

    let code = run(&mut host, command);
    let output = format!("{}{}", host.out(), host.err());

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("2 of 2 shards seen"), "{output}");
    assert!(output.contains("never tested"), "{output}");

    // The merged population must be the union, not either half.
    let document: serde_json::Value = serde_json::from_str(&fs::read_to_string(&merged).expect("the merged report was not written"))
        .expect("the merged report is not valid JSON");

    let count = document["files"]["src/lib.rs"]["mutants"]
        .as_array()
        .expect("mutants is an array")
        .len();

    let (code, single) = session(&dir, &["--mutators", "relational", "--dry-run"]);

    assert_eq!(code, EXIT_OK, "{single}");

    let whole = single
        .lines()
        .find_map(|line| line.trim_start().strip_prefix("Summary "))
        .and_then(|line| line.split_whitespace().next())
        .and_then(|count| count.parse::<usize>().ok())
        .expect("the plan reports how many mutants it found");

    assert_eq!(count, whole, "the merged population must be the whole population");
}

#[test]
fn a_merged_score_gate_can_fail_the_build() {
    step_aside_if_nested!();

    let dir = workspace(SUBJECT);
    let artifacts = dir.path().join("shard-artifacts");
    let path = artifacts.join("gamma-report.json");
    let (code, output) = session(
        &dir,
        &[
            "--mutators",
            "stmt",
            "--artifact-dir",
            artifacts.to_str().expect("path is not UTF-8"),
        ],
    );

    assert_eq!(code, EXIT_OK, "{output}");

    let mut host = Sink::default();
    let code = run(
        &mut host,
        vec![
            "cargo-gamma".to_owned(),
            "gamma".to_owned(),
            "merge".to_owned(),
            path.to_str().expect("path is not UTF-8").to_owned(),
            "--min-score".to_owned(),
            "100".to_owned(),
        ],
    );

    assert_ne!(code, EXIT_OK, "{}", host.err());
    assert!(host.err().contains("merged mutation score"), "{}", host.err());
}

#[test]
fn estimating_projects_the_rest_of_the_run_and_then_runs_it() {
    step_aside_if_nested!();
    let dir = workspace(SUBJECT);
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let mut host = Sink::default();
    let code = run(
        &mut host,
        vec![
            "cargo-gamma".to_owned(),
            "gamma".to_owned(),
            "run".to_owned(),
            "--estimate".to_owned(),
            "--mutators".to_owned(),
            "relational".to_owned(),
            // Phase lines belong to the progress display, so asserting on them needs it on.
            "--progress".to_owned(),
            "always".to_owned(),
            "--dir".to_owned(),
            path.to_string(),
        ],
    );

    let output = format!("{}{}", host.out(), host.err());

    assert_eq!(code, EXIT_OK, "{output}");

    // The projection rests on the fixed cost, so it cannot be printed before that is paid.
    assert!(output.contains("Baseline"), "{output}");
    assert!(output.contains("Estimate"), "{output}");
    assert!(output.contains("worst case"), "{output}");

    // One line, not a block: the build and baseline it would otherwise repeat are on the screen
    // immediately above it.
    let estimate = output
        .lines()
        .find(|line| line.contains("Estimate"))
        .expect("the estimate line is missing");

    assert!(estimate.contains("worst case"), "the estimate must fit one line: {estimate}");

    // And unlike the subcommand it replaced, it carries on and actually tests the mutants.
    assert!(output.contains("Summary"), "{output}");
    assert!(output.contains("2 mutants ("), "{output}");
}

#[test]
fn no_estimate_is_printed_unless_it_was_asked_for() {
    step_aside_if_nested!();
    let dir = workspace(SUBJECT);
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let mut host = Sink::default();
    let code = run(
        &mut host,
        vec![
            "cargo-gamma".to_owned(),
            "gamma".to_owned(),
            "run".to_owned(),
            "--mutators".to_owned(),
            "relational".to_owned(),
            "--dir".to_owned(),
            path.to_string(),
        ],
    );

    let output = format!("{}{}", host.out(), host.err());

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(!output.contains("Estimate"), "{output}");
}

#[test]
fn the_estimate_survives_being_piped() {
    // It is the one line the user explicitly asked for, so suppressing it along with the progress
    // display when stdout is not a terminal would defeat the flag in exactly the setting — a CI
    // log — where knowing the remaining cost matters most.
    step_aside_if_nested!();
    let dir = workspace(SUBJECT);
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let mut host = Sink::default();
    let code = run(
        &mut host,
        vec![
            "cargo-gamma".to_owned(),
            "gamma".to_owned(),
            "run".to_owned(),
            "--estimate".to_owned(),
            "--mutators".to_owned(),
            "relational".to_owned(),
            "--progress".to_owned(),
            "never".to_owned(),
            "--dir".to_owned(),
            path.to_string(),
        ],
    );

    let output = format!("{}{}", host.out(), host.err());

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("Estimate"), "{output}");
}

/// Builds a crate whose only oracle for the boundary lives in a named integration target.
///
/// The library carries no unit tests at all, so `tests/pinned.rs` is the one thing that can convict
/// the relational mutant. Taking that target out of the oracle has to turn a caught mutant into a
/// survivor, which is what makes the effect of the option observable rather than asserted about
/// its own plumbing.
fn split_oracle() -> TempDir {
    let dir = TempDir::new().expect("could not create a temporary directory");
    let root = dir.path();

    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"subject\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .expect("could not write the manifest");

    fs::create_dir_all(root.join("src")).expect("could not create src");
    fs::write(root.join("src/lib.rs"), "pub fn is_adult(age: u32) -> bool {\n    age >= 18\n}\n").expect("could not write the library");

    fs::create_dir_all(root.join("tests")).expect("could not create tests");
    fs::write(
        root.join("tests/pinned.rs"),
        "#[test]\nfn the_boundary_is_pinned() {\n    assert!(!subject::is_adult(17));\n    assert!(subject::is_adult(18));\n}\n",
    )
    .expect("could not write the integration test");

    dir
}

#[test]
fn excluding_a_test_target_takes_it_out_of_the_oracle() {
    step_aside_if_nested!();
    let dir = split_oracle();

    // The control: the integration target is present, so the boundary is pinned and nothing gets
    // past it. Without this half, the assertion below would also pass on a crate that never had a
    // working oracle in the first place.
    let (code, output) = session(&dir, &["--mutators", "relational"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("0 survived,"), "{output}");

    let (code, output) = session(&dir, &["--mutators", "relational", "--exclude-test", "pinned"]);

    assert_eq!(code, EXIT_OK, "{output}");

    // Nothing is caught any more. What is left of the oracle is the lib's own unit-test binary,
    // which announces no tests at all, so these mutants are reported as uncovered rather than as
    // survivors: there is no test that missed them.
    assert!(output.contains("0 killed"), "{output}");
    assert!(output.contains("2 uncovered"), "{output}");
    assert!(output.contains("1 test target not consulted"), "{output}");
}

#[test]
fn including_only_the_unit_tests_leaves_the_integration_target_out() {
    step_aside_if_nested!();
    let dir = split_oracle();
    let (code, output) = session(&dir, &["--mutators", "relational", "--include-test", "subject"]);

    assert_eq!(code, EXIT_OK, "{output}");

    // The unit-test binary is all that is left, and it announces no tests, so nothing here could
    // have convicted anything — uncovered rather than missed.
    assert!(output.contains("0 killed"), "{output}");
    assert!(output.contains("2 uncovered"), "{output}");
}

#[test]
fn a_test_pattern_that_names_no_target_stops_the_run() {
    step_aside_if_nested!();
    let dir = split_oracle();

    // A misspelled exclusion silently widens the oracle, so mutants the missing target would have
    // let through are reported as caught and the score reads better than the suite deserves. That
    // is indistinguishable in CI from a run that went well, which is why it is fatal.
    let (code, output) = session(&dir, &["--mutators", "relational", "--exclude-test", "pinnd"]);

    assert_eq!(code, EXIT_USAGE, "{output}");
    assert!(output.contains("pinnd"), "{output}");
}

/// A whole run through nextest reaches the same verdicts as a run through the binaries directly.
///
/// This is the only test that exercises the metadata capture and the filterset construction against
/// a real `cargo nextest`, which is what makes it worth the cost of a second full session: those are
/// an agreement with another tool's command line, and nothing but that tool can check it.
///
/// Runtime-analysis configurations do not all install `cargo-nextest`, so the test stands down
/// loudly when that external executable is unavailable.
#[test]
fn a_run_through_nextest_reaches_the_same_verdicts_as_one_through_the_binaries() {
    step_aside_if_nested!();
    step_aside_without_nextest!();

    let dir = split_oracle();
    let (direct, direct_output) = session(&dir, &["--mutators", "relational"]);
    let (through, through_output) = session(&dir, &["--mutators", "relational", "--nextest"]);

    assert_eq!(direct, EXIT_OK, "{direct_output}");
    assert_eq!(through, EXIT_OK, "{through_output}");
    assert!(through_output.contains("0 survived,"), "{through_output}");
}

#[test]
fn nextest_enumeration_failure_caused_only_by_a_mutant_kills_it() {
    step_aside_if_nested!();
    step_aside_without_nextest!();
    let dir = enumeration_failure(false);
    let (code, output) = session(&dir, &["--mutators", "fn_value", "--nextest", "--incremental", "no"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("2 killed"), "{output}");

    // The raw output nextest's fake harness printed while enumeration failed is still worth an
    // operator's attention, so it must reach the console this run just produced...
    assert!(
        output.contains("enumeration failed while the mutant was active"),
        "the local diagnostic dropped the raw output an operator would need to debug this: {output}"
    );

    // ...but it must never be published: that same text comes from a test process and could as
    // easily have been an inherited credential, and the durable report is what an unrelated reader
    // — or an upload pipeline — sees long after this run's console is gone.
    let report = fs::read_to_string(dir.path().join("target/cargo-gamma/gamma-report.json")).expect("could not read the report");
    assert!(report.contains("could not enumerate tests"), "{report}");
    assert!(
        !report.contains("enumeration failed while the mutant was active"),
        "raw nextest output reached the durable report: {report}"
    );
}

#[test]
fn persistent_nextest_enumeration_failure_aborts_the_run() {
    step_aside_if_nested!();
    step_aside_without_nextest!();
    let dir = enumeration_failure(true);
    let (code, output) = session(&dir, &["--mutators", "fn_value", "--nextest", "--incremental", "no"]);

    assert_ne!(code, EXIT_OK, "{output}");
    assert!(output.contains("cannot be trusted to judge anything further"), "{output}");
    assert!(output.contains("code 104"), "{output}");
    assert!(output.contains("enumeration still failed with no mutant active"), "{output}");
}

/// Patterns that each name a real target but together leave none is a usage error, not a run.
///
/// Nothing left to run means every mutant survives unopposed, and the report would read as a total
/// collapse of the test suite rather than as the filters having eaten it.
#[test]
fn filters_that_between_them_leave_no_test_target_stop_the_run() {
    step_aside_if_nested!();
    let dir = split_oracle();
    let (code, output) = session(
        &dir,
        &["--mutators", "relational", "--include-test", "subject", "--exclude-test", "subject"],
    );

    assert_eq!(code, EXIT_USAGE, "{output}");
    assert!(output.contains("no test target"), "{output}");
}

#[test]
fn the_advise_surface_that_became_a_flag_is_gone() {
    // `advise` was a run that also diagnosed, and `--yields` was half of that diagnosis. Both are
    // now part of every run's artifact set, so the analysis is spelled once and easy to share.
    for argv in [
        vec!["run".to_owned(), "--advise".to_owned()],
        vec!["run".to_owned(), "--yields".to_owned()],
        vec!["advise".to_owned()],
    ] {
        let mut host = Sink::default();
        let mut full = vec!["cargo-gamma".to_owned(), "gamma".to_owned()];

        full.extend(argv.clone());

        assert_eq!(run(&mut host, full), EXIT_USAGE, "{argv:?} was accepted");
    }
}

#[test]
fn advice_is_written_as_markdown_and_carries_the_family_table() {
    step_aside_if_nested!();
    let dir = workspace(SUBJECT);
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let artifacts = path.join("artifacts");
    let advice = artifacts.join("gamma-perf-advice.md");
    let mut host = Sink::default();
    let code = run(
        &mut host,
        vec![
            "cargo-gamma".to_owned(),
            "gamma".to_owned(),
            "run".to_owned(),
            "--artifact-dir".to_owned(),
            artifacts.to_string(),
            "--mutators".to_owned(),
            "relational,stmt".to_owned(),
            "--dir".to_owned(),
            path.to_string(),
        ],
    );

    let output = format!("{}{}", host.out(), host.err());

    assert_eq!(code, EXIT_OK, "{output}");

    let text = fs::read_to_string(&advice).expect("the advice file was not written");

    assert!(text.starts_with("# Mutation testing advice"), "{text}");
    assert!(text.contains("## Contents"), "{text}");
    assert!(text.contains("## This run"), "{text}");
    assert!(text.contains("| Family |"), "{text}");
    assert!(text.contains("`relational`"), "{text}");

    // Every entry in the table of contents must land on a heading that is actually in the file.
    // A table of contents whose links do not resolve is worse than none, because it is only found
    // to be broken by someone who already had to scroll.
    let slug = |heading: &str| -> String {
        heading
            .chars()
            .filter(|character| character.is_alphanumeric() || *character == ' ' || *character == '-')
            .map(|character| if character == ' ' { '-' } else { character.to_ascii_lowercase() })
            .collect()
    };

    let headings: Vec<String> = text
        .lines()
        .filter_map(|line| line.strip_prefix("## ").or_else(|| line.strip_prefix("### ")))
        .map(slug)
        .collect();

    for line in text.lines().filter(|line| line.trim_start().starts_with("- [")) {
        let anchor = line
            .split("](#")
            .nth(1)
            .expect("a contents entry links somewhere")
            .trim_end_matches(')');

        assert!(
            headings.contains(&anchor.to_owned()),
            "the contents entry `{anchor}` has no heading in:\n{text}"
        );
    }

    // A tiny healthy crate must not be told its two files are each half the population.
    assert!(!text.contains("hot-file"), "{text}");

    // The diagnosis is a document now, so it must not also be dumped on the console.
    assert!(!output.contains("survivors/cpu-h"), "{output}");
}

#[test]
fn the_job_summary_carries_the_advice() {
    // The summary panel is the artifact a team reads every morning; a score with no diagnosis
    // beside it is the reason a nightly run gets ignored.
    step_aside_if_nested!();
    let dir = workspace(SUBJECT);
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let summary = path.join("summary.md");
    let mut host = Sink::default().with_env("GITHUB_STEP_SUMMARY", summary.as_str());
    let code = run(
        &mut host,
        vec![
            "cargo-gamma".to_owned(),
            "gamma".to_owned(),
            "run".to_owned(),
            "--annotations".to_owned(),
            "github".to_owned(),
            "--mutators".to_owned(),
            "relational".to_owned(),
            "--dir".to_owned(),
            path.to_string(),
        ],
    );

    assert_eq!(code, EXIT_OK, "{}", host.err());

    let text = fs::read_to_string(&summary).expect("the job summary was not written");

    assert!(text.contains("## Mutation testing"), "{text}");
    assert!(text.contains("### Findings"), "{text}");
    assert!(text.contains("| Family |"), "{text}");

    // The panel owns the heading and has just stated the score, so the fragment must not open a
    // level-one title beneath it or repeat the verdict table above it.
    assert!(!text.contains("# Mutation testing advice"), "{text}");
    assert!(!text.contains("## This run"), "{text}");
}

/// A crate whose tests print far more than a pipe will hold.
///
/// A pipe is about 64 KB. Before the output was drained concurrently, a binary like this blocked
/// forever in `write` while the run waited for it to exit — which the baseline reported as a
/// ten-minute stall, and which a mutant would have been recorded as a timeout for. A timeout counts
/// as detected, so a chatty test could silently turn a survivor into a passing score.
const CHATTY: &str = "
pub fn is_adult(age: u32) -> bool {
    age >= 18
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boundary_is_pinned_loudly() {
        for index in 0..40_000 {
            println!(\"line {index} of a test that has a great deal to say about nothing at all\");
        }

        assert!(!is_adult(17));
        assert!(is_adult(18));
    }
}
";

#[test]
fn a_test_that_outprints_the_pipe_does_not_deadlock() {
    step_aside_if_nested!();
    let dir = workspace(CHATTY);
    let (code, output) = session(&dir, &["--mutators", "relational"]);

    assert_eq!(code, EXIT_OK, "{output}");

    // Both mutants break the assertion, so libtest dumps the captured output of a failing test —
    // megabytes of it — down the pipe. The verdict has to be the real one, reached promptly, rather
    // than the timeout a blocked pipe would have produced once the budget expired.
    assert!(
        output.contains("2 mutants (2 killed, 0 survived, 0 timed out, 0 out of memory, 0 uncovered => 100.0%)"),
        "{output}"
    );
}

/// A crate whose mutant makes a loop run forever.
///
/// `drain` terminates only because the condition eventually goes false. Relaxing `>` to `>=` makes
/// it true for every `u64`, and because the body saturates rather than overflowing, the loop spins
/// instead of panicking. That is precisely the mutant the stall detector exists for: the process
/// stays alive and busy while producing no output at all, so nothing but silence distinguishes it
/// from a test that is merely slow.
const HANGS: &str = "
pub fn drain(mut remaining: u64) -> u64 {
    let mut steps = 0_u64;

    while remaining > 0 {
        remaining = remaining.saturating_sub(1);
        steps = steps.wrapping_add(1);
    }

    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draining_terminates() {
        assert_eq!(drain(3), 3);
    }
}
";

#[test]
fn a_runaway_mutant_is_reported_as_stalled() {
    step_aside_if_nested!();
    let dir = workspace(HANGS);
    let (code, output) = session(&dir, &["--mutators", "relational.gt_to_ge", "--minimum-test-timeout", "120"]);

    assert_eq!(code, EXIT_OK, "{output}");

    // The mutant hangs, so it must be reported as detected rather than as a survivor.
    assert!(output.contains("0 survived"), "{output}");

    // The report says silence, rather than the two-minute timeout, stopped the mutant. Which of the
    // two forms appears depends on whether the harness had finished announcing a test before it
    // went quiet; here the only test is the one that hangs, so there is nothing to name and saying
    // so is the honest answer.
    assert!(output.contains("TIMEOUT"), "{output}");
    assert!(output.contains("stalled"), "{output}");
    assert!(output.contains("1 timed out,"), "{output}");
}

#[test]
fn stall_detection_can_be_turned_off() {
    step_aside_if_nested!();
    let dir = workspace(SUBJECT);
    let (code, output) = session(&dir, &["--mutators", "relational", "--no-stall-detection"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(!output.contains("Hangs"), "{output}");
}

#[test]
fn a_mutant_no_test_can_reach_is_reported_uncovered() {
    step_aside_if_nested!();
    let dir = archipelago();
    let (code, output) = session(&dir, &["--workspace", "--mutators", "relational"]);

    assert_eq!(code, EXIT_OK, "{output}");

    // The same mutant exists in both packages. The one the mainland's tests compile is caught; the
    // island's cannot be reached by any binary the build produced, which is a stronger statement
    // than "survived" and must not be reported as one.
    assert!(
        output.contains("0 survived,"),
        "the mainland's own tests must still kill its mutants: {output}"
    );

    // An uncovered mutant costs score without being a survivor, so it is counted on its own rather
    // than folded into the missed total a reader would go looking for an assertion for.
    assert!(
        output.contains("4 mutants (2 killed, 0 survived, 0 timed out, 0 out of memory, 2 uncovered => 50.0%)"),
        "{output}"
    );

    // Uncovered is a stronger statement than survived, so the island's mutants must not be listed
    // among the ones a test ran and failed to notice.
    assert!(!output.contains("SURVIVED"), "{output}");
}

#[test]
fn a_survivor_reaches_the_diff_and_the_security_tab() {
    step_aside_if_nested!();
    let dir = workspace(SUBJECT);
    let artifacts = Utf8PathBuf::from_path_buf(dir.path().join("artifacts")).expect("path is not UTF-8");
    let sarif = artifacts.join("gamma-report.sarif");
    let summary_path = dir.path().join("summary.md");
    let host = Sink::default()
        .with_env("GITHUB_ACTIONS", "true")
        .with_env("GITHUB_STEP_SUMMARY", summary_path.to_str().expect("path is not UTF-8"));

    let (code, out, err) = session_on(host, &dir, &["--mutators", "stmt", "--artifact-dir", artifacts.as_str()]);

    assert_eq!(code, EXIT_OK, "{out}{err}");

    // Deleting the `push` survives, because the test only asserts that recording does not panic.
    assert!(out.contains("::warning file=src/lib.rs,"), "{out}");
    assert!(out.contains("entries"), "{out}");

    let log: serde_json::Value = serde_json::from_str(&fs::read_to_string(&sarif).expect("the sarif log")).expect("valid json");

    assert_eq!(log["version"], "2.1.0");

    let results = log["runs"][0]["results"].as_array().expect("results");

    assert!(!results.is_empty(), "a survivor must reach the log");
    assert_eq!(results[0]["level"], "note");
    assert_eq!(
        results[0]["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
        "src/lib.rs"
    );

    let summary = fs::read_to_string(&summary_path).expect("the job summary");

    assert!(summary.contains("## Mutation testing"), "{summary}");
    assert!(summary.contains("**Score"), "{summary}");
}

#[test]
fn nothing_is_annotated_outside_a_runner() {
    step_aside_if_nested!();
    let dir = workspace(SUBJECT);

    // The default is `auto`, and a developer at a terminal must not have workflow commands printed
    // into the results stream they are piping somewhere.
    let (code, out, err) = session_on(Sink::default(), &dir, &["--mutators", "stmt"]);

    assert_eq!(code, EXIT_OK, "{out}{err}");
    assert!(!out.contains("::warning"), "{out}");
}

#[test]
fn the_hidden_diag_flag_dumps_what_the_run_measured_about_itself() {
    step_aside_if_nested!();
    let dir = workspace(SUBJECT);

    let (code, output) = session(&dir, &["--dry-run", "--diag"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("── diag ─"), "{output}");
    assert!(output.contains("by mutator"), "{output}");

    // A dry run built nothing, so there is nothing to say about a build or a baseline.
    assert!(!output.contains("baseline "), "{output}");
}

#[test]
fn the_diag_flag_stays_out_of_the_help_it_is_not_for_users_of_the_tool() {
    step_aside_if_nested!();
    let mut host = Sink::default();
    let code = run(&mut host, ["cargo-gamma", "gamma", "run", "--help"].map(str::to_owned).to_vec());
    let text = format!("{}{}", host.out(), host.err());
    let bare = text.split("--diag").skip(1).any(|rest| !rest.starts_with('-'));

    assert_eq!(code, EXIT_OK, "{text}");

    // The prose dump is a tool-author's instrument. The bundle is part of every artifact set.
    assert!(!bare, "{text}");
    assert!(text.contains("--artifact-dir"), "{text}");
    assert!(text.contains("--diag-names"), "{text}");
}

#[test]
fn a_real_run_that_finds_no_mutants_says_so_rather_than_reporting_a_perfect_score() {
    step_aside_if_nested!();
    let dir = workspace(SUBJECT);

    // `--exclude` leaves nothing to mutate, so the run still copies, builds and baselines but has
    // no population to judge. Reporting 100% over an empty set would be the most misleading answer
    // the tool could give, so it says plainly that it generated nothing.
    let (code, output) = session(&dir, &["--exclude-file", "**/*.rs"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("no mutants were generated"), "{output}");
}

#[test]
fn leaking_the_scratch_tree_reports_where_it_was_kept() {
    step_aside_if_nested!();
    let dir = workspace(SUBJECT);

    // `--leak-dirs` promises that the instrumented copy remains inspectable even when a run cannot
    // settle, and the path has to be printed or the option leaves the user with a workspace they
    // cannot find.
    let (code, output) = session(&dir, &["--mutators", "relational", "--leak-dirs"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("Kept"), "{output}");

    let tree = scratch_base(&dir).join("workspace");

    assert!(tree.exists(), "the scratch tree should have been kept at {tree}");
}

#[test]
fn skipping_the_baseline_says_so_and_still_judges_the_mutants() {
    step_aside_if_nested!();
    let dir = workspace(SUBJECT);

    // Measuring the baseline is the largest fixed cost of a run, and a user who already knows the
    // suite is green can skip it. The run then has no measured time to scale a timeout from, so it
    // has to say the measurement was skipped rather than report a baseline of zero.
    let (code, output) = session(&dir, &["--mutators", "relational", "--no-baseline"]);

    // Without a measured baseline there is no elapsed time to scale a timeout from, so the run
    // falls back to the configured floor and still has to reach a verdict on every mutant.
    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("2 killed"), "{output}");
}

#[test]
fn a_default_run_completes_whether_or_not_the_host_can_bound_memory() {
    step_aside_if_nested!();
    // Memory control is on by default, and most hosts a test runs on cannot provide it: a CI
    // container without cgroup delegation, or macOS at all. A default nobody asked for must not be
    // able to stop a run, so this asserts the run finishes and produces a score either way — and
    // that whichever path was taken, the fact is stated rather than left to be discovered later.
    let dir = workspace(SUBJECT);
    let (code, output) = session(&dir, &["--mutators", "relational"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("Summary"), "{output}");

    let bounded = !output.contains("not bounded on this host");

    // The summary always carries the count, so a reader can distinguish memory exhaustion from an
    // assertion kill without having to know whether enforcement was available.
    assert!(output.contains("out of memory"), "{output}");

    if !bounded {
        assert!(output.contains("Memory"), "the note has to name itself so it can be searched for");
    }
}

#[test]
fn asking_for_memory_control_that_cannot_be_delivered_is_an_error() {
    step_aside_if_nested!();
    // The inverse of the test above, and the reason `Demand` exists. Someone who passed `--memory`
    // wants a guarantee, so silently running without one is the single outcome that could cost them
    // the machine they were protecting.
    if cargo_gamma_lib::internals::exec::memory::support().is_ok() {
        return;
    }

    let dir = workspace(SUBJECT);
    let (code, output) = session(&dir, &["--mutators", "relational", "--memory", "enforce"]);

    assert_ne!(code, EXIT_OK, "{output}");
    assert!(output.contains("not available here"), "{output}");
}

#[test]
fn a_mutant_in_code_the_feature_set_excludes_is_not_reported_as_a_survivor() {
    step_aside_if_nested!();
    // The bug this pins down was silent and expensive: mutants behind an inactive `#[cfg]` were
    // generated, compiled away to nothing, killed by no test and reported as survivors, so a run
    // could name a page of unfixable failures and quote a score tens of points below the truth.
    let dir = conditional();
    let (code, output) = session(&dir, &["--mutators", "expr"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(
        !output.contains("SURVIVED src/extra.rs"),
        "an unbuilt mutant was blamed on the tests: {output}"
    );

    // They are accounted for by `-V` and `--diag` rather than on the console, which stays about
    // what the tests did and did not catch.
    assert!(!output.contains("not built"), "{output}");
    assert!(!output.contains("Features"), "{output}");
}

#[test]
fn turning_the_feature_on_brings_the_same_code_back_into_the_run() {
    step_aside_if_nested!();
    // The other half of the pair. Excusing a mutant is only correct while the compiler really is
    // ignoring its file; a rule that quietly excused it either way would hide real gaps in the
    // suite, which is a worse failure than the one it replaced.
    let dir = conditional();
    let (code, output) = session(&dir, &["--mutators", "expr", "--features", "extra"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(!output.contains("not built"), "{output}");
}

/// A crate whose two functions are reached by one test each, plus a third that reaches neither.
///
/// The shape is what makes a census observable end to end: without one, every mutant runs all three
/// tests, and there is no way to tell a narrowed run from a full one by its verdicts. With one, the
/// mutant in `orphan` is reached by nothing at all, and a run that reports it as *uncovered* rather
/// than *surviving* can only have measured that.
const CENSUSED: &str = "
pub fn is_adult(age: u32) -> bool {
    age >= 18
}

pub fn orphan(age: u32) -> bool {
    age >= 21
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boundary_is_pinned() {
        // Make sampling economical under coverage without slowing samples or mutant runs.
        if std::env::var_os(\"GAMMA_ACTIVE\").is_none() && std::env::var_os(\"GAMMA_CENSUS\").is_none() {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        assert!(!is_adult(17));
        assert!(is_adult(18));
    }

    #[test]
    fn something_unrelated() {
        assert_eq!(1 + 1, 2);
    }
}
";

#[test]
fn a_census_still_catches_every_mutant_the_full_sweep_caught() {
    step_aside_if_nested!();
    // The property that matters most: narrowing may only remove tests that cannot convict. A kill
    // lost here would be a survivor invented out of nothing, which is the one direction this must
    // never be wrong in.
    let dir = workspace(CENSUSED);
    let (code, output) = censused_session(&dir, &["--mutators", "relational", "--incremental", "no"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(!output.contains("SURVIVED src/lib.rs:3:5"), "{output}");
}

#[test]
fn a_site_reached_by_most_tests_runs_the_whole_binary_instead_of_becoming_uncovered() {
    step_aside_if_nested!();
    // Past half the tests, the census deliberately declines to name them because running the
    // binary whole is cheaper. That "run whole" answer must not be collapsed into the distinct
    // "no test reaches this site" answer.
    let dir = workspace(
        "
pub fn answer() -> Result<u32, ()> {
    Ok(42)
}

#[cfg(test)]
mod tests {
    #[test]
    fn checks_the_answer() {
        assert_eq!(super::answer(), Ok(42));
    }
}
",
    );
    let (code, output) = censused_session(&dir, &["--mutators", "fn_value.ok", "--incremental", "no"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("0 uncovered"), "{output}");
    assert!(output.contains("2 killed"), "{output}");
}

#[test]
fn a_census_reports_code_no_test_reaches_as_uncovered_rather_than_surviving() {
    step_aside_if_nested!();
    // `orphan` is linked by the test binary, so the run has no way to excuse it and calls its
    // mutants survivors — which sends the reader to strengthen assertions that do not exist. A
    // census establishes that no test executes the line, so they are reported as uncovered, and
    // they are never run at all.
    let dir = workspace(CENSUSED);
    let (censused, output) = censused_session(&dir, &["--mutators", "relational", "--incremental", "no"]);

    assert_eq!(censused, EXIT_OK, "{output}");
    assert!(output.contains("2 uncovered"), "{output}");
    assert!(!output.contains("2 survived"), "{output}");

    // The same crate with case-level selection disabled, to prove the difference is the census and
    // not the crate.
    let plain = workspace(CENSUSED);
    let (code, output) = session(
        &plain,
        &["--mutators", "relational", "--incremental", "no", "--whole-test-binaries"],
    );

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("2 survived"), "{output}");
    assert!(!output.contains("2 uncovered"), "{output}");
}

/// A crate with a test that convicts every mutant while reaching none of them.
///
/// `objects_to_any_mutant` executes no instrumented code, so a census puts it in no mutant's list
/// and it never runs during the sweep. Run anyway, it fails whenever a mutant is active and credits
/// the suite with catching one it never looked at — which makes it a precise oracle for whether the
/// narrowing happened, rather than a timing measurement that would be flaky by construction.
///
/// `shout`'s value is reached by `the_boundary_is_pinned` but asserted by nothing, so it is a
/// genuine survivor that only the poison test can hide.
const POISONED: &str = "
pub fn is_adult(age: u32) -> bool {
    age >= 18
}

pub fn shout(name: &str) -> String {
    format!(\"{name}!\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boundary_is_pinned() {
        // Keep the baseline long enough that case-level sampling repays its launch cost even under
        // coverage instrumentation, without slowing either the census samples or mutant runs.
        if std::env::var_os(\"GAMMA_ACTIVE\").is_none() && std::env::var_os(\"GAMMA_CENSUS\").is_none() {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        assert!(!is_adult(17));
        assert!(is_adult(18));

        let _unasserted = shout(\"quiet\");
    }

    #[test]
    fn objects_to_any_mutant() {
        assert!(std::env::var(\"GAMMA_ACTIVE\").is_err(), \"a mutant was active\");
    }
}
";

#[test]
fn a_census_leaves_out_the_tests_that_reach_none_of_a_mutant_s_code() {
    step_aside_if_nested!();
    // Without a census every mutant runs the whole binary, so the test that objects to any mutant
    // at all convicts the survivor in `shout` and the run reports a clean sweep it never made.
    let plain = workspace(POISONED);
    let (code, output) = session(&plain, &["--mutators", "fn_value", "--incremental", "no", "--whole-test-binaries"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("0 survived,"), "{output}");

    // With one, only the tests that reach `shout` run, the objector never does, and the survivor it
    // was hiding is reported.
    let dir = workspace(POISONED);
    let (code, output) = censused_session(&dir, &["--mutators", "fn_value", "--incremental", "no"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("SURVIVED src/lib.rs"), "{output}");
}
