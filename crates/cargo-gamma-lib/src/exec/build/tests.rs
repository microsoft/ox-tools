// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::fmt::Write as _;
use core::ops::Range;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::{fs, io, thread};

use camino::Utf8Path;
use serde_json::Value;

use super::complaints::Diagnostic;
use super::invoke::{
    OutputLimits, Stream, compile, drained, finish_readers, is_progress, read_pipe, read_pipe_with_limits, rendered_diagnostic,
    spawn_failure, supervise, supervise_with_limits,
};
use super::messages::dep_files;
use super::*;
use crate::discover::TargetFile;
use crate::schema::Position;

/// The ordinals [`blame`] attributed, without the error codes it attributed them under.
///
/// Every one of these tests predates the census and asks only which mutants were named, so
/// they say so directly rather than each spelling out a code they do not care about.
fn ordinals_blamed(stdout: &str, root: &Utf8Path, guards: &Guards) -> HashSet<u32> {
    blame(stdout, root, guards).into_keys().collect()
}

fn artifact_message(path: &Utf8Path) -> String {
    serde_json::json!({
        "reason": "compiler-artifact",
        "filenames": [path.as_str()],
    })
    .to_string()
}

/// The bar is what a reader watches during the long silences, so it has to be recognised.
#[test]
fn cargos_progress_bar_is_told_apart_from_the_things_it_wanted_to_say() {
    assert!(is_progress("    Building [====>    ] 4/17: serde_core, quote"));
    assert!(is_progress("   Compiling [=>       ] 1/9: syn"));
    assert!(is_progress("\u{1b}[1;36m    Building\u{1b}[0m [====>    ] 4/17: serde_core, quote"));

    assert!(!is_progress("   Compiling serde v1.0.229"));
    assert!(!is_progress(
        "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 6.14s"
    ));
    assert!(!is_progress(""));
}

/// The escape valve exists to reproduce what running cargo directly would have shown, so it
/// passes the compiler's own rendering through untouched, warnings included.
#[test]
fn a_diagnostic_is_surfaced_the_way_the_compiler_rendered_it() {
    let message = |level: &str| {
        format!(
            r#"{{"reason":"compiler-message","message":{{"level":"{level}","message":"mismatched types","rendered":"error[E0308]: mismatched types\n --> src/lib.rs:2:5\n"}}}}"#
        )
    };

    assert_eq!(
        rendered_diagnostic(&message("error")).as_deref(),
        Some("error[E0308]: mismatched types\n --> src/lib.rs:2:5\n")
    );

    assert!(rendered_diagnostic(&message("warning")).is_some());
}

/// Everything else on the stream is cargo talking about its own progress, not a diagnostic.
#[test]
fn a_line_that_is_not_a_compiler_message_yields_no_diagnostic() {
    assert!(rendered_diagnostic(r#"{"reason":"compiler-artifact","target":{"name":"serde"}}"#).is_none());
    assert!(rendered_diagnostic("    Building [====>    ] 4/17: serde").is_none());
    assert!(rendered_diagnostic("").is_none());
}

/// A message cargo did not render carries nothing a reader could act on.
#[test]
fn a_message_without_a_rendering_yields_nothing() {
    let bare = r#"{"reason":"compiler-message","message":{"level":"error","message":"could not find `nope`","spans":[]}}"#;

    assert!(rendered_diagnostic(bare).is_none());
    assert!(rendered_diagnostic(r#"{"reason":"compiler-message","message":{"level":"error","rendered":"   "}}"#).is_none());
}

/// Cargo draws its bar with carriage returns and no newlines, so a reader that split only on
/// newlines would see one enormous line at the very end and nothing at all while it mattered.
#[test]
fn the_reader_splits_on_carriage_returns_as_well_as_newlines() {
    let (sender, lines) = mpsc::sync_channel(64);
    let text = "one\rtwo\nthree\r\nfour";

    let collected = read_pipe(io::Cursor::new(text), Stream::Prose, &sender)
        .expect("spawn reader")
        .join()
        .expect("reader");

    drop(sender);

    let seen: Vec<String> = lines.into_iter().map(|(_, line)| line).collect();

    assert_eq!(seen, ["one", "two", "three", "four"]);
    assert_eq!(collected.text, text.as_bytes());
    assert!(collected.complete, "a stream read to its end is the whole of it");
    assert!(collected.within_limits, "short output stays within the normal output limit");
}

/// A pipe that fails part way through is not a stream that ended.
///
/// `supervise` refuses a build whose readers did not finish, because a truncated JSON stream loses
/// artifacts and a run that lost them reports the test binaries it could not find as ones that do
/// not exist, and excuses the mutants in files it could not see as never built. A reader that
/// stopped on an `EIO` and handed back what it had would walk in through the one door that guard
/// does not watch: the thread is finished, so the bytes look like the whole of what cargo said.
#[test]
fn a_pipe_that_fails_part_way_is_not_read_as_the_end_of_the_stream() {
    /// Yields one chunk and then refuses, the way a pipe whose writer died mid-stream does.
    struct Faltering(bool);

    impl io::Read for Faltering {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0 {
                return Err(io::Error::other("the read a test asked to fail"));
            }

            self.0 = true;

            let said = b"{\"reason\":\"compiler-artifact\"}\n";
            buf[..said.len()].copy_from_slice(said);

            Ok(said.len())
        }
    }

    let (sender, _lines) = mpsc::sync_channel(64);
    let reader = read_pipe(Faltering(false), Stream::Json, &sender).expect("spawn reader");

    assert!(
        drained(Some(reader), Instant::now() + Duration::from_secs(5)).is_none(),
        "a reader that stopped on an error must not pass its bytes off as the whole stream"
    );
}

/// A small configurable limit lets the failure path be tested without allocating megabytes.
#[test]
fn a_build_past_its_output_limit_fails_without_retaining_the_excess() {
    let directory = crate::testing::workdir("build-output-limit-");
    let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("utf8");
    let work = Workspace::adopt(root.clone(), root.join("target"));
    let mut command = Command::new(crate::testing::helper_binary_path().as_std_path());

    let _configured = command
        .arg(crate::testing::directive("print:the compiler said more than sixteen bytes"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let error = supervise_with_limits(
        command,
        &work,
        Some(Duration::from_secs(30)),
        &mut crate::testing::Recorder::default(),
        OutputLimits {
            retained: 16,
            line: 64,
            backlog: 1,
        },
    )
    .expect_err("output above the configured limit is not parsed as a complete build");

    assert!(
        error
            .to_string()
            .contains("configured 16-byte retained or 64-byte per-line build-output limit"),
        "{error}"
    );
}

/// A small retained cap records truncation while continuing to drain the pipe.
#[test]
fn an_over_limit_pipe_keeps_only_its_configured_prefix() {
    let (sender, _lines) = mpsc::sync_channel(4);
    let pipe = read_pipe_with_limits(
        io::Cursor::new("0123456789\n"),
        Stream::Prose,
        &sender,
        OutputLimits {
            retained: 4,
            line: 16,
            backlog: 4,
        },
    )
    .expect("spawn reader")
    .join()
    .expect("reader");

    assert_eq!(pipe.text, b"0123");
    assert!(pipe.complete, "the pipe was still drained to EOF");
    assert!(!pipe.within_limits, "the capped output is explicitly marked incomplete");
}

#[test]
fn reader_thread_creation_failure_is_reported_as_incomplete_output() {
    let (sender, lines) = mpsc::sync_channel(1);
    let _refused = crate::exec::faults::arm(crate::exec::faults::Fault::Thread);

    let failure = read_pipe(io::Cursor::new("cargo output"), Stream::Prose, &sender).expect_err("thread creation was asked to fail");
    drop(sender);
    let mut events = crate::testing::Recorder::default();
    let (stdout, _stderr) = finish_readers(
        Some(Err(failure)),
        None,
        &lines,
        &mut events,
        Instant::now() + Duration::from_secs(1),
    );

    assert!(stdout.is_none(), "a reader that could not be created was treated as complete");
}

/// Finishing readers continues narration, so a bounded channel never turns collection into a
/// post-build deadlock.
#[test]
fn finishing_readers_drains_a_backpressured_narration_channel() {
    let (sender, lines) = mpsc::sync_channel(0);
    let stdout = read_pipe_with_limits(
        io::Cursor::new("first\nsecond\n"),
        Stream::Prose,
        &sender,
        OutputLimits {
            retained: 64,
            line: 64,
            backlog: 0,
        },
    );
    let mut events = crate::testing::Recorder::default();

    let (stdout, stderr) = finish_readers(Some(stdout), None, &lines, &mut events, Instant::now() + Duration::from_secs(5));

    assert_eq!(stdout.expect("the reader was drained").text, b"first\nsecond\n");
    assert!(stderr.is_some(), "an absent stderr pipe is an empty complete stream");
}

use crate::ops::collect::Shape;

/// The diagnostic from a build that could not be made to compile, or a panic if it did.
fn stuck_reason(convergence: Convergence) -> String {
    match convergence {
        Convergence::Built(_stdout) => panic!("the build was expected not to compile"),
        Convergence::Stuck(reason) => reason.to_string(),
    }
}

/// A workspace holding one trivial crate, so a real cargo invocation is cheap.
fn trivial_workspace(prefix: &str) -> (tempfile::TempDir, Workspace) {
    let dir = crate::testing::workdir(prefix);
    let root = Utf8PathBuf::from_path_buf(dir.path().join("src")).expect("utf8");

    fs::create_dir_all(root.join("src").as_std_path()).expect("src");
    fs::write(
        root.join("Cargo.toml").as_std_path(),
        "[package]\nname = \"trivial\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
    )
    .expect("manifest");
    fs::write(root.join("src/lib.rs").as_std_path(), "pub const A: i32 = 1;\n").expect("lib");

    let target = Utf8PathBuf::from_path_buf(dir.path().join("target")).expect("utf8");
    let work = Workspace::adopt(root, target);

    (dir, work)
}

/// A build that fails for a reason no guard explains stops rather than looping forever.
#[test]
fn a_build_that_no_guard_explains_stops_with_the_compiler_output() {
    let (_dir, work) = trivial_workspace("build-unattributed-");

    // Broken source that has nothing to do with instrumentation: no mutant can be withdrawn
    // to make it compile, so withdrawing forever would be an infinite loop.
    fs::write(
        work.root.join("src/lib.rs").as_std_path(),
        "pub const A: i32 = \"not an integer\";\n",
    )
    .expect("lib");

    let plan = empty_plan(&work);
    let limits = BuildLimits::default();
    let convergence = Converger::default()
        .converge(
            &work,
            &plan,
            None,
            &["build", "--tests", "--keep-going"],
            limits,
            &mut crate::testing::Recorder::default(),
        )
        .expect("the build ran, however badly");

    assert!(stuck_reason(convergence).contains("could not be attributed"));
}

/// A build that fails before the compiler is reached says so, rather than blaming a mutant.
///
/// The two ways a build can fail unattributably are not the same failure, and the message that
/// serves one misleads about the other. `a_build_that_no_guard_explains_stops_with_the_compiler
/// _output` covers the case where rustc ran and complained about something no guard explains.
/// This one covers the case where rustc was never reached at all: cargo gave up on a build
/// script, so the JSON stream carries no diagnostics and there is nothing to attribute. Telling
/// the reader the tree "does not compile" there sends them hunting for a broken mutant that was
/// never generated.
#[test]
fn a_build_that_never_reached_the_compiler_says_so_rather_than_blaming_the_tree() {
    let (_dir, work) = trivial_workspace("build-no-diagnostics-");

    // A build script that panics fails the build with an exit status and a message on stderr,
    // and with no rustc diagnostic anywhere in the JSON stream.
    fs::write(
        work.root.join("build.rs").as_std_path(),
        "fn main() { panic!(\"the build script refused\"); }\n",
    )
    .expect("build script");

    let plan = empty_plan(&work);
    let convergence = Converger::default()
        .converge(
            &work,
            &plan,
            None,
            &["build", "--tests", "--keep-going"],
            BuildLimits::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("the build ran, however badly");
    let reason = stuck_reason(convergence);

    assert!(reason.contains("the compiler reported nothing"), "{reason}");
    assert!(
        !reason.contains("does not compile"),
        "the message for a tree rustc rejected must not be reused here: {reason}"
    );
    assert!(
        reason.contains("the build script refused"),
        "cargo's own words are the only account of this failure: {reason}"
    );
}

/// A narrowed build that fails is retried across the whole workspace before giving up.
#[test]
fn a_narrowed_build_that_fails_is_retried_across_the_whole_workspace() {
    let (_dir, work) = trivial_workspace("build-widen-");
    let mut plan = empty_plan(&work);
    let select = vec!["no-such-package".to_owned()];

    // Cargo rejects the selection outright, which is exactly the shape of failure the widen
    // path exists for: the narrowing is at fault, not the code being built.
    let build = Converger::default()
        .finish(
            &work,
            &mut plan,
            Some(&select),
            BuildLimits::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("widening to the whole workspace must succeed");

    assert!(build.widened, "the build should have reported that it widened");
}

/// Examples remain part of the compilation oracle even though they are never test binaries.
#[test]
fn the_final_build_still_compiles_examples() {
    let (_dir, work) = trivial_workspace("build-example-");

    fs::create_dir_all(work.root.join("examples").as_std_path()).expect("examples");
    fs::write(
        work.root.join("examples/broken.rs").as_std_path(),
        "fn main() { let _: i32 = \"not an integer\"; }\n",
    )
    .expect("example");

    let mut plan = empty_plan(&work);
    let build = Converger::default()
        .finish(
            &work,
            &mut plan,
            Some(&["trivial".to_owned()]),
            BuildLimits::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("the build reports the example failure");

    assert!(
        build.stuck.is_some(),
        "a broken example must not disappear from the compilation oracle"
    );
    assert!(build.binaries.is_empty(), "a failed compilation produces no runnable oracle");
}

/// A workspace of two members, one of which does not compile and is not being mutated.
fn split_workspace(prefix: &str) -> (tempfile::TempDir, Workspace) {
    let dir = crate::testing::workdir(prefix);
    let root = Utf8PathBuf::from_path_buf(dir.path().join("src")).expect("utf8");

    fs::create_dir_all(root.join("good/src").as_std_path()).expect("good");
    fs::create_dir_all(root.join("broken/src").as_std_path()).expect("broken");
    fs::write(
        root.join("Cargo.toml").as_std_path(),
        "[workspace]\nmembers = [\"good\", \"broken\"]\nresolver = \"3\"\n",
    )
    .expect("workspace manifest");

    for member in ["good", "broken"] {
        fs::write(
            root.join(member).join("Cargo.toml").as_std_path(),
            format!("[package]\nname = \"{member}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n"),
        )
        .expect("member manifest");
    }

    fs::write(root.join("good/src/lib.rs").as_std_path(), "pub const A: i32 = 1;\n").expect("good lib");
    fs::write(
        root.join("broken/src/lib.rs").as_std_path(),
        "pub const B: i32 = \"gamma-broken-marker\";\n",
    )
    .expect("broken lib");

    let target = Utf8PathBuf::from_path_buf(dir.path().join("target")).expect("utf8");
    let work = Workspace::adopt(root, target);

    (dir, work)
}

/// A workspace whose selected member compiles only when cargo unifies features over every member.
///
/// `leaf` guards its only item behind an optional feature that nothing in `leaf` activates; `app`
/// depends on `leaf` and turns it on. A build of `leaf` alone therefore fails, and a build of both
/// succeeds — which is exactly the trap a narrowed build falls into and no mutant is responsible
/// for.
fn unified_workspace(prefix: &str) -> (tempfile::TempDir, Workspace) {
    let dir = crate::testing::workdir(prefix);
    let root = Utf8PathBuf::from_path_buf(dir.path().join("src")).expect("utf8");

    fs::create_dir_all(root.join("app/src").as_std_path()).expect("app");
    fs::create_dir_all(root.join("leaf/src").as_std_path()).expect("leaf");
    fs::write(
        root.join("Cargo.toml").as_std_path(),
        "[workspace]\nmembers = [\"app\", \"leaf\"]\nresolver = \"3\"\n",
    )
    .expect("workspace manifest");

    fs::write(
        root.join("app/Cargo.toml").as_std_path(),
        "[package]\nname = \"app\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
         [dependencies]\nleaf = { path = \"../leaf\", features = [\"wide\"] }\n",
    )
    .expect("app manifest");
    fs::write(
        root.join("leaf/Cargo.toml").as_std_path(),
        "[package]\nname = \"leaf\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[features]\nwide = []\n",
    )
    .expect("leaf manifest");

    fs::write(
        root.join("leaf/src/lib.rs").as_std_path(),
        "#[cfg(feature = \"wide\")]\npub const WIDE: i32 = 1;\n\n\
         pub fn value() -> i32 {\n    WIDE\n}\n",
    )
    .expect("leaf lib");
    fs::write(
        root.join("app/src/lib.rs").as_std_path(),
        "pub fn value() -> i32 {\n    leaf::value()\n}\n",
    )
    .expect("app lib");

    let target = Utf8PathBuf::from_path_buf(dir.path().join("target")).expect("utf8");
    let work = Workspace::adopt(root, target);

    (dir, work)
}

/// The preflight says which scope proved the tree, not merely that some scope did.
///
/// A narrow failure that the whole workspace survives is a statement about the *selection*: the
/// tree compiles under cargo's feature unification and does not compile under a subset of it.
/// Answering `Ok(())` and dropping which scope answered leaves the run free to narrow again.
#[test]
fn a_check_that_only_the_whole_workspace_passes_says_so() {
    let (_dir, work) = unified_workspace("build-preflight-unified-");
    let plan = empty_plan(&work);
    let select = vec!["leaf".to_owned()];

    let cleared = Converger::preflight(
        &work,
        &plan,
        Some(&select),
        &select,
        BuildLimits::default(),
        &mut crate::testing::Recorder::default(),
    )
    .expect("the workspace compiles when its features are unified");

    assert!(
        cleared.whole_workspace,
        "the check widened to succeed and then reported nothing about it"
    );
    assert!(cleared.dropped.is_empty(), "{:?}", cleared.dropped);
}

/// A converger told the tree only builds whole does not narrow again.
///
/// Without this, the staged build reruns the failure the preflight already attributed to the
/// selection, the rollback loop blames whichever mutants it can, and valid mutants are settled as
/// `NotBuilt` — withheld from the denominator and quietly raising the score.
#[test]
fn a_whole_workspace_requirement_survives_into_the_staged_builds() {
    let (_dir, work) = unified_workspace("build-stage-unified-");
    let mut plan = empty_plan(&work);
    let stage = vec!["leaf".to_owned()];

    let mut narrowing = Converger::default();

    assert!(
        narrowing
            .stage(
                &work,
                &mut plan,
                &stage,
                BuildLimits::default(),
                &mut crate::testing::Recorder::default(),
            )
            .expect("the build runs")
            .is_some(),
        "the fixture is meant to be one a narrowed build cannot compile"
    );

    let mut whole = Converger::default();

    whole.require_whole_workspace();

    assert!(
        whole
            .stage(
                &work,
                &mut plan,
                &stage,
                BuildLimits::default(),
                &mut crate::testing::Recorder::default(),
            )
            .expect("the build runs")
            .is_none(),
        "the scope the preflight validated was not carried into the staged build"
    );
}

/// A package nobody asked to mutate, and which does not compile, must not stop the whole run.
///
/// Both wider checks fail on it and neither says anything about the code the caller asked
/// about. Refusing to run on that evidence turns somebody else's broken crate into a tool that
/// cannot be used at all, when narrowing by hand would have worked — which is a flag the caller
/// had no reason to know they needed.
#[test]
fn a_broken_package_nobody_is_mutating_is_dropped_rather_than_failing_the_run() {
    let (_dir, work) = split_workspace("build-preflight-retreat-");
    let plan = empty_plan(&work);
    let select = vec!["broken".to_owned(), "good".to_owned()];
    let mutating = vec!["good".to_owned()];

    let dropped = Converger::preflight(
        &work,
        &plan,
        Some(&select),
        &mutating,
        BuildLimits::default(),
        &mut crate::testing::Recorder::default(),
    )
    .expect("the package being mutated compiles on its own");

    // Named, because the run is about to stop building and running their tests, and a mutant
    // they would have killed will be reported as a survivor.
    assert_eq!(dropped.dropped, vec!["broken".to_owned()]);
    assert!(
        !dropped.whole_workspace,
        "a retreat narrows the scope, so it cannot be reporting that only the whole workspace built"
    );
}

/// The retreat is not a way to run over code that does not compile.
///
/// A failure in the package being mutated survives every narrowing there is, so the run stops
/// exactly as it did before — otherwise the preflight would stop being the thing that makes
/// later compiler errors attributable to a mutant.
#[test]
fn a_broken_package_that_is_being_mutated_still_stops_the_run() {
    let (_dir, work) = split_workspace("build-preflight-noretreat-");

    fs::write(
        work.root.join("good/src/lib.rs").as_std_path(),
        "pub const A: i32 = \"gamma-selected-marker\";\n",
    )
    .expect("good lib");

    let plan = empty_plan(&work);
    let select = vec!["broken".to_owned(), "good".to_owned()];
    let mutating = vec!["good".to_owned()];

    let error = Converger::preflight(
        &work,
        &plan,
        Some(&select),
        &mutating,
        BuildLimits::default(),
        &mut crate::testing::Recorder::default(),
    )
    .expect_err("no narrowing makes the selected package compile");

    assert!(error.to_string().contains("gamma-selected-marker"), "{error}");
}

/// When both preflight checks fail, the caller hears about the packages they selected.
///
/// The wider check is only ever asked whether feature unification explains the narrow failure.
/// Reporting its diagnostics instead would answer "your tree does not compile" with errors from
/// a crate the caller never chose to mutate, which is the wrong thing to send them to fix.
#[test]
fn a_preflight_that_fails_both_ways_reports_the_selected_packages_errors() {
    let (_dir, work) = trivial_workspace("build-preflight-both-");

    fs::write(
        work.root.join("src/lib.rs").as_std_path(),
        "pub const A: i32 = \"gamma-narrow-marker\";\n",
    )
    .expect("lib");

    let plan = empty_plan(&work);
    let select = vec!["trivial".to_owned()];

    let error = Converger::preflight(
        &work,
        &plan,
        Some(&select),
        &select,
        BuildLimits::default(),
        &mut crate::testing::Recorder::default(),
    )
    .expect_err("neither check can succeed");

    assert!(error.to_string().contains("gamma-narrow-marker"), "{error}");
}

/// A whole-workspace build that fails is reported as it stands, with nothing to widen to.
#[test]
fn a_whole_workspace_build_that_fails_is_reported_rather_than_retried() {
    let (_dir, work) = trivial_workspace("build-nowiden-");

    fs::write(
        work.root.join("src/lib.rs").as_std_path(),
        "pub const A: i32 = \"not an integer\";\n",
    )
    .expect("lib");

    let mut plan = empty_plan(&work);

    // There was no narrowing to blame, so there is no second build to try: retrying the same
    // command would only spend the time again to reach the same answer.
    let build = Converger::default()
        .finish(
            &work,
            &mut plan,
            None,
            BuildLimits::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("the build ran, however badly");

    let stuck = build.stuck.expect("the build could not be made to compile");

    assert!(stuck.reason.contains("could not be attributed"), "{}", stuck.reason);
    assert!(build.binaries.is_empty(), "a build that never compiled has nothing to run");
}

/// A rollback loop that keeps hitting compile errors it can attribute has to stop somewhere,
/// or an author who mistypes `--rollback-rounds` far too low would watch the tool spin without
/// ever explaining why: the limit is reported by name, with the withdrawal history that led to
/// it, rather than the run simply hanging.
#[test]
fn hitting_the_rollback_round_limit_is_reported_rather_than_retried_forever() {
    let (_dir, work) = trivial_workspace("build-rollback-limit-");

    // A guard around a `const` initializer cannot compile: the compiler cannot call a function
    // while evaluating a constant, whatever the guard would have chosen between. That failure
    // is squarely inside the guard's own span, so it is exactly the kind of round this loop is
    // meant to withdraw and retry — which is what makes it a safe way to force a limit to bite
    // deterministically rather than relying on a build that never converges at all.
    let text = "pub const A: i32 = 1;\n";
    let start = text.find('1').expect("the literal is in the fixture");
    let mutant = Mutant {
        span: start..start + 1,
        replacement: "2".to_owned().into(),
        ..mutant()
    };
    let mut plan = empty_plan(&work);
    plan.files.push(target_file(&work.root, "src/lib.rs"));
    plan.mutants.push(mutant);

    let limits = BuildLimits {
        timeout: None,
        multiplier: None,
        rollback_rounds: 1,
    };

    let convergence = Converger::default()
        .converge(
            &work,
            &plan,
            None,
            &["build", "--keep-going"],
            limits,
            &mut crate::testing::Recorder::default(),
        )
        .expect("a guard around a const initializer cannot compile");

    assert!(stuck_reason(convergence).contains("rollback"));
}

/// A stage that cannot be converged gives up on its own mutants instead of ending the run, and
/// records them as never built rather than as unviable.
///
/// The two are not the same claim. `unviable` says the compiler looked at this mutant and
/// refused it; `notbuilt` says nobody ever asked. A run that could not converge has made the
/// second claim about every mutant in the failing build, and dressing that up as the first
/// would put a verdict on mutants the tool never judged.
#[test]
fn a_stage_that_cannot_be_converged_abandons_its_mutants_as_never_built() {
    let (_dir, work) = trivial_workspace("build-stage-abandons-");
    let mut plan = unguardable_plan(&work, 1);

    plan.mutants[0].package = ("trivial".to_owned()).into();

    // One round only, so the guard the first round blames exhausts the budget rather than being
    // withdrawn and retried. That is the rollback-limit half of giving up.
    let limits = BuildLimits {
        timeout: None,
        multiplier: None,
        rollback_rounds: 1,
    };
    let mut converger = Converger::default();

    let abandoned = converger
        .stage(
            &work,
            &mut plan,
            &["trivial".to_owned()],
            limits,
            &mut crate::testing::Recorder::default(),
        )
        .expect("the stage ran, however badly")
        .expect("a budget of one round cannot survive a failing round");

    assert_eq!(abandoned.ordinals, vec![1]);

    // The advice that would otherwise reach the user as the run's dying words has to reach them as a
    // diagnostic instead: whether the counts were falling or flat is the only thing that
    // decides whether raising `--rollback-rounds` would have helped. With a budget of one
    // round the series is this one round, so it has to be there rather than being omitted for
    // never having been withdrawn.
    assert!(abandoned.reason.contains("rollback rounds"), "{}", abandoned.reason);
    assert!(
        abandoned.reason.contains("Mutants blamed in the last rounds of this build: 1"),
        "{}",
        abandoned.reason
    );

    assert_eq!(plan.mutants[0].outcome, Outcome::NotBuilt);
    assert!(plan.mutants[0].note.is_some(), "the mutant should say why it never ran");

    // And the count the run reports as unviable does not swallow them: nothing here was
    // blamed on a mutant.
    converger.settle(&mut plan);

    assert_eq!(plan.mutants[0].outcome, Outcome::NotBuilt, "settling must not overwrite it");
}

/// The run carries on after a stage it could not converge, because giving up on that stage's
/// mutants puts its sources back exactly as the preflight check found them.
///
/// This is what makes continuing defensible rather than optimistic: the next build is asked of
/// a tree with no guards left in the offending package, which is the tree that was already
/// proved to compile.
#[test]
fn a_stage_the_run_gave_up_on_leaves_a_tree_the_next_build_can_still_compile() {
    let (_dir, work) = trivial_workspace("build-stage-carries-on-");
    let mut plan = unguardable_plan(&work, 1);

    plan.mutants[0].package = ("trivial".to_owned()).into();

    let limits = BuildLimits {
        timeout: None,
        multiplier: None,
        rollback_rounds: 1,
    };
    let mut converger = Converger::default();
    let packages = ["trivial".to_owned()];

    let _abandoned = converger
        .stage(&work, &mut plan, &packages, limits, &mut crate::testing::Recorder::default())
        .expect("the stage ran, however badly")
        .expect("a budget of one round cannot survive a failing round");

    let again = converger
        .stage(&work, &mut plan, &packages, limits, &mut crate::testing::Recorder::default())
        .expect("the stage ran");

    assert!(again.is_none(), "the tree compiles once the abandoned mutants are out of it");
}

#[test]
fn an_unattributed_failure_is_isolated_to_the_mutant_that_provably_breaks_the_build() {
    let (_dir, work) = trivial_workspace("build-isolates-unattributed-");
    let text = "pub fn bad() -> i32 { 1 }\n\
                pub fn good(x: i32) -> i32 { x + 1 }\n";
    let pristine = work.root.parent().expect("tree parent").join("pristine");

    fs::create_dir_all(pristine.as_std_path()).expect("pristine");
    fs::write(work.root.join("src/lib.rs").as_std_path(), text).expect("working source");
    fs::write(pristine.join("lib.rs").as_std_path(), text).expect("pristine source");

    let bad_start = text.find("{ 1 }").expect("bad body") + 2;
    let good_start = text.find("x + 1").expect("addition");
    let mut bad = mutant();
    bad.ordinal = 1;
    bad.span = bad_start..bad_start + 1;
    bad.original = "1".into();
    bad.replacement = "()".into();
    bad.item_path = ("trivial::bad".to_owned()).into();
    bad.package = ("trivial".to_owned()).into();
    let mut good = mutant();
    good.ordinal = 2;
    good.span = good_start..good_start + "x + 1".len();
    good.original = "x + 1".into();
    good.replacement = "x - 1".into();
    good.item_path = ("trivial::good".to_owned()).into();
    good.package = ("trivial".to_owned()).into();

    let mut plan = empty_plan(&work);
    plan.files.push(TargetFile {
        path: Utf8PathBuf::from("src/lib.rs"),
        absolute: pristine.join("lib.rs"),
        package: "trivial".to_owned(),
    });
    plan.mutants.extend([bad, good]);
    fs::create_dir_all(work.root.join("gamma-rt/src").as_std_path()).expect("runtime source");
    fs::write(
        work.root.join("gamma-rt/Cargo.toml").as_std_path(),
        "[package]\nname = \"cargo-gamma-rt\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
         [lib]\nname = \"gamma_rt\"\n",
    )
    .expect("runtime manifest");
    fs::write(
        work.root.join("gamma-rt/src/lib.rs").as_std_path(),
        "pub const fn a(_: u32) -> bool { false }\n",
    )
    .expect("runtime library");
    work.link_runtime("trivial", &plan.files).expect("runtime linked");

    let isolated = Converger::default()
        .isolate(
            &work,
            &plan,
            Some(&["trivial".to_owned()]),
            &["build", "--keep-going"],
            BuildLimits::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("isolation builds ran")
        .expect("the bad mutant was isolated");

    assert!(matches!(isolated, Isolation::Blamed(ordinals) if ordinals == vec![1]));
}

/// Mutants that compile alone but fail only together settle to `NotBuilt`, not `CompileError`.
///
/// When `isolate` narrows an unattributed failure to an interaction inside one item it returns
/// [`Isolation::Item`], and convergence abandons those ordinals so [`Converger::settle`] records
/// them as never judged rather than accusing honest mutants of being unviable. No cheap real
/// fixture makes two mutations compile apart and fail together, so the proof build is scripted
/// through the test-only `subset_oracle`: each mutant compiles on its own, only the pair fails.
#[test]
fn an_item_only_interaction_settles_its_mutants_to_not_built() {
    #[expect(
        clippy::unnecessary_wraps,
        reason = "the oracle matches subset_fails's Option<bool> verdict, where None is an indeterminate build"
    )]
    fn only_the_pair_fails(active: &HashSet<u32>) -> Option<bool> {
        Some(active.contains(&1) && active.contains(&2))
    }

    let (_dir, work) = trivial_workspace("build-isolates-interaction-");

    let mut plan = empty_plan(&work);
    plan.mutants = vec![Mutant { ordinal: 1, ..mutant() }, Mutant { ordinal: 2, ..mutant() }];

    let mut converger = Converger {
        subset_oracle: Some(only_the_pair_fails),
        ..Converger::default()
    };

    let isolated = converger
        .isolate(
            &work,
            &plan,
            None,
            &["build", "--keep-going"],
            BuildLimits::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("the scripted proof builds ran")
        .expect("the interaction was isolated to its item");

    assert!(
        matches!(&isolated, Isolation::Item(ordinals) if *ordinals == vec![1, 2]),
        "an interaction with no single culprit must be an item isolation, not a blamed one"
    );

    // Convergence withdraws every isolated ordinal but abandons only an `Item` one, and abandoning
    // is what makes `settle` record it as never built rather than as compiler-rejected.
    let ordinals = match &isolated {
        Isolation::Blamed(ordinals) | Isolation::Item(ordinals) => ordinals.clone(),
    };
    for ordinal in &ordinals {
        let _ = converger.withdrawn.insert(*ordinal);
    }
    if let Isolation::Item(ordinals) = &isolated {
        converger.abandoned.extend(ordinals.iter().copied());
    }

    converger.settle(&mut plan);

    assert_eq!(
        plan.mutants[0].outcome,
        Outcome::NotBuilt,
        "an item-only interaction is never judged"
    );
    assert_eq!(
        plan.mutants[1].outcome,
        Outcome::NotBuilt,
        "an item-only interaction is never judged"
    );
}

/// A plan holding one mutant of the trivial fixture's `const`, which cannot be guarded and so
/// costs exactly one rollback round to withdraw.
fn unguardable_plan(work: &Workspace, ordinal: u32) -> Plan {
    let text = "pub const A: i32 = 1;\n";
    let start = text.find('1').expect("the literal is in the fixture");

    // Each round instruments the tree from `absolute`, so the pristine copy has to sit outside
    // the tree the rounds rewrite: reading it back out of the scratch tree would have a round
    // treat the previous round's guards as the original source, and nothing would converge.
    let pristine = work.root.parent().expect("the tree has a parent").join("pristine");

    fs::create_dir_all(pristine.as_std_path()).expect("pristine");
    fs::write(pristine.join("lib.rs").as_std_path(), text).expect("pristine source");

    let mut plan = empty_plan(work);

    plan.files.push(TargetFile {
        path: Utf8PathBuf::from("src/lib.rs"),
        absolute: pristine.join("lib.rs"),
        package: "trivial".to_owned(),
    });
    plan.mutants.push(Mutant {
        ordinal,
        span: start..start + 1,
        replacement: "2".to_owned().into(),
        ..mutant()
    });

    plan
}

/// `--rollback-rounds` caps the rounds one build may spend converging, and a run performs
/// several builds. Charging them all against one counter means a stage that converged normally
/// can leave the build that actually decides the run with no budget at all — which surfaces as
/// a rollback-limit failure on a tree that was converging perfectly well.
#[test]
fn each_build_gets_the_whole_round_budget_rather_than_what_earlier_builds_left() {
    let (_dir, work) = trivial_workspace("build-budget-per-build-");
    let limits = BuildLimits {
        timeout: None,
        multiplier: None,
        rollback_rounds: 2,
    };
    let mut converger = Converger::default();
    let first = unguardable_plan(&work, 1);

    let first_built = converger
        .converge(
            &work,
            &first,
            None,
            &["build", "--keep-going"],
            limits,
            &mut crate::testing::Recorder::default(),
        )
        .expect("one withdrawal is enough to make the tree compile");

    assert!(matches!(first_built, Convergence::Built(_)), "the tree must converge");

    assert_eq!(converger.rounds, 2, "one failed round and one that succeeded");

    // A second build, with a mutant the first one never saw, costs a round of its own. The
    // whole budget has to be available to it.
    let second = unguardable_plan(&work, 2);

    let second_built = converger
        .converge(
            &work,
            &second,
            None,
            &["build", "--keep-going"],
            limits,
            &mut crate::testing::Recorder::default(),
        )
        .expect("the second build must get a budget of its own");

    assert!(matches!(second_built, Convergence::Built(_)), "the tree must converge");

    assert_eq!(converger.rounds, 2, "the counter is per build, not per run");
    assert_eq!(converger.total_rounds, 4, "the run still reports what it spent in total");

    // The withdrawal set is the one thing that is deliberately shared: a mutant already known
    // not to compile stays withdrawn for the rest of the run.
    assert_eq!(converger.withdrawn(), 2, "withdrawals carry across builds");
}

/// The limit error reads a series of withdrawal counts and gives falling-or-flat advice from
/// it, so the series has to describe the build that just failed. Counts left over from earlier
/// builds would have it describe work the reader is not being told about.
#[test]
fn the_rollback_limit_error_describes_only_the_build_that_hit_it() {
    let (_dir, work) = trivial_workspace("build-limit-series-");
    let mut converger = Converger {
        rounds: 9,
        total_rounds: 9,
        per_round: vec![41],
        ..Converger::default()
    };
    let limits = BuildLimits {
        timeout: None,
        multiplier: None,
        rollback_rounds: 1,
    };
    let plan = unguardable_plan(&work, 1);

    let error = stuck_reason(
        converger
            .converge(
                &work,
                &plan,
                None,
                &["build", "--keep-going"],
                limits,
                &mut crate::testing::Recorder::default(),
            )
            .expect("a budget of one round cannot survive a failing round"),
    );

    assert!(error.contains("1 of the 1 rollback rounds"), "{error}");
    assert!(!error.contains("41 blamed during this build"), "{error}");
    assert!(!error.contains("last rounds of this build: 41"), "{error}");
    assert!(!error.contains("10 of"), "{error}");
}

/// A cargo that cannot even be spawned fails inside `run_cargo`, and that failure has to climb
/// back out through `converge` rather than being swallowed as an ordinary build failure to
/// withdraw mutants over: retrying a build that can never start would spin forever, and the
/// person running it deserves to be told cargo itself could not be launched, not that every
/// mutant in the tree is somehow unviable.
#[test]
fn converging_when_the_tree_is_missing_reports_the_failure_rather_than_looping() {
    let work = Workspace::adopt(
        Utf8PathBuf::from("/gamma/definitely/not/a/directory"),
        Utf8PathBuf::from("/gamma/definitely/not/a/directory/target"),
    );
    let plan = empty_plan(&work);

    let error = Converger::default()
        .converge(
            &work,
            &plan,
            None,
            &["build", "--keep-going"],
            BuildLimits::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect_err("cargo cannot be spawned in a directory that does not exist");

    assert!(error.to_string().contains("disappeared"), "{error}");
}

/// A mutant whose span no longer fits the file it names is an internal error whether it is
/// discovered by calling the instrumenter directly or, as here, by driving a whole round of
/// `converge`: the round has to stop and report it rather than treating the file as though it
/// simply failed to compile, or the bug would surface as a baffling rollback instead of the
/// named internal error it actually is.
#[test]
fn converging_a_mutant_whose_span_no_longer_fits_the_file_reports_the_internal_error() {
    let (_dir, work) = trivial_workspace("build-converge-missing-guard-");
    let text = fs::read_to_string(work.root.join("src/lib.rs").as_std_path()).expect("lib");

    let mutant = Mutant {
        span: text.len() + 10..text.len() + 11,
        ..mutant()
    };
    let mut plan = empty_plan(&work);
    plan.files.push(target_file(&work.root, "src/lib.rs"));
    plan.mutants.push(mutant);

    let error = Converger::default()
        .converge(
            &work,
            &plan,
            None,
            &["build", "--keep-going"],
            BuildLimits::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect_err("the span is out of range");

    assert!(error.to_string().contains("no guard was emitted"), "{error}");
}

/// `stage` builds only what one stage needs, and a failure there has to surface exactly like
/// any other build failure would: this function had never been called by a unit test at all,
/// so nothing stood between it compiling and it actually reporting the right thing when the
/// tree it is given does not build.
#[test]
fn a_stage_that_fails_to_compile_reports_the_failure() {
    let (_dir, work) = trivial_workspace("build-stage-fails-");

    fs::write(
        work.root.join("src/lib.rs").as_std_path(),
        "pub const A: i32 = \"not an integer\";\n",
    )
    .expect("lib");

    let mut plan = empty_plan(&work);
    let abandoned = Converger::default()
        .stage(
            &work,
            &mut plan,
            &["trivial".to_owned()],
            BuildLimits::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("the stage ran, however badly")
        .expect("the package does not compile");

    assert!(abandoned.reason.contains("could not be attributed"), "{}", abandoned.reason);
}

/// Narrowing to a package is abandoned when even the widened, whole-workspace build fails: the
/// narrowing was never the problem, the code itself does not compile, and reporting the
/// original narrow-build error rather than the widened one keeps the message about the
/// selection that was actually asked for instead of an unrelated whole-workspace retry.
#[test]
fn widening_to_the_whole_workspace_that_also_fails_reports_the_original_narrow_error() {
    let (_dir, work) = trivial_workspace("build-widen-fails-");

    fs::write(
        work.root.join("src/lib.rs").as_std_path(),
        "pub const A: i32 = \"not an integer\";\n",
    )
    .expect("lib");

    let mut plan = empty_plan(&work);
    let select = vec!["trivial".to_owned()];

    let stuck = Converger::default()
        .finish(
            &work,
            &mut plan,
            Some(&select),
            BuildLimits::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("the build ran, however badly")
        .stuck
        .expect("neither the narrowed nor the widened build compiles");

    assert!(stuck.reason.contains("could not be attributed"), "{}", stuck.reason);
}

/// A mutant whose span no longer fits the file it names is dropped by the instrumenter rather
/// than spliced at a wrong offset, but it is still "live" as far as withdrawal bookkeeping is
/// concerned. If that mismatch were left unchecked, the mutant would run with nothing in the
/// tree to make it behave differently and be recorded as a survivor — a wrong verdict rather
/// than a missing one — so this invariant is caught and reported as the internal error it is.
#[test]
fn a_live_mutant_whose_span_no_longer_fits_the_file_is_an_internal_error_rather_than_a_silent_survivor() {
    let (_dir, work) = trivial_workspace("build-missing-guard-");
    let text = fs::read_to_string(work.root.join("src/lib.rs").as_std_path()).expect("lib");

    let mutant = Mutant {
        span: text.len() + 10..text.len() + 11,
        ..mutant()
    };
    let mut plan = empty_plan(&work);
    plan.files.push(target_file(&work.root, "src/lib.rs"));
    plan.mutants.push(mutant);

    let error = Splices::default()
        .instrument(&work, &plan, &HashSet::default())
        .expect_err("the span is out of range");

    assert!(error.to_string().contains("no guard was emitted"), "{error}");
}

/// Every file the survey found is read while instrumenting, whether or not it has a live
/// mutant of its own, because a withdrawn file still has to be rewritten back to its original
/// text. A file that has since become unreadable — moved, deleted, permissions revoked between
/// the survey and the build — must stop the round with a named error rather than silently
/// dropping the file from the instrumented tree, which would leave stale instrumented text
/// behind for a mutant nothing further ever rewrites.
#[test]
fn a_survey_file_that_can_no_longer_be_read_stops_instrumentation_with_a_named_error() {
    let (_dir, work) = trivial_workspace("build-unreadable-file-");
    let mut plan = empty_plan(&work);

    plan.files.push(target_file(&work.root, "src/gone.rs"));

    let error = Splices::default()
        .instrument(&work, &plan, &HashSet::default())
        .expect_err("the file was never created");

    assert!(error.to_string().contains("could not read"), "{error}");
}

/// Two live mutants whose spans overlap without one nesting inside the other cannot both be
/// spliced into the same text, and the instrumenter reports that ambiguity as an error rather
/// than guessing an order. That failure has to be visible from `instrument_tree`'s own call
/// site, not merely from the schema module's tests, because a caller reading a stack trace here
/// needs to see the tree walk that actually produced it.
#[test]
fn overlapping_mutant_spans_are_reported_as_an_ambiguous_splice() {
    let (_dir, work) = trivial_workspace("build-overlap-");
    let text = "fn f(a: i32, b: i32, c: i32) -> i32 { a + b + c }";

    fs::write(work.root.join("src/lib.rs").as_std_path(), text).expect("lib");

    let left = text.find("a + b").expect("the fixture contains this text");
    let right = text.find("b + c").expect("the fixture contains this text");

    let mut plan = empty_plan(&work);
    plan.files.push(target_file(&work.root, "src/lib.rs"));
    plan.mutants.push(Mutant {
        ordinal: 1,
        span: left..left + "a + b".len(),
        ..mutant()
    });
    plan.mutants.push(Mutant {
        ordinal: 2,
        span: right..right + "b + c".len(),
        ..mutant()
    });

    let error = Splices::default()
        .instrument(&work, &plan, &HashSet::default())
        .expect_err("the spans overlap without nesting");

    assert!(error.to_string().contains("overlap"), "{error}");
}

/// Staged discovery appends mutants in package order, then the baseline sorts them into report
/// order. The splice cache stores vector positions, so the sort has to invalidate those positions
/// before a changed withdrawal set makes any file dirty again.
#[test]
fn sorting_a_staged_plan_reindexes_mutants_before_the_baseline_splice() {
    let (_dir, work) = trivial_workspace("build-sort-reindexes-");
    let a_text = "pub fn a() -> bool { true }\n";
    let b_text = "pub fn b() -> bool { true }\n";

    fs::write(work.root.join("src/a.rs").as_std_path(), a_text).expect("a");
    fs::write(work.root.join("src/b.rs").as_std_path(), b_text).expect("b");

    let mut plan = empty_plan(&work);
    plan.files
        .extend([target_file(&work.root, "src/a.rs"), target_file(&work.root, "src/b.rs")]);

    let mut b = mutant();
    b.file = Utf8PathBuf::from("src/b.rs").into();
    b.span = b_text.find("true").expect("true")..b_text.find("true").expect("true") + 4;
    plan.mutants.push(b);

    let mut splices = Splices::default();
    let _guards = splices
        .instrument(&work, &plan, &HashSet::default())
        .expect("the first stage is instrumented");

    let mut a = mutant();
    a.ordinal = 2;
    a.file = Utf8PathBuf::from("src/a.rs").into();
    a.span = a_text.find("true").expect("true")..a_text.find("true").expect("true") + 4;
    plan.mutants.push(a);

    let _guards = splices
        .instrument(&work, &plan, &HashSet::default())
        .expect("the second stage is instrumented");

    plan.sort();
    splices.plan_reordered();

    let guards = splices
        .instrument(&work, &plan, &HashSet::from_iter([1]))
        .expect("the baseline withdrawal reindexes the sorted plan");

    assert_eq!(guards.get(&2).map(|(path, _guard)| path.as_str()), Some("src/a.rs"));
    assert!(!guards.contains_key(&1), "the withdrawn b mutant has no guard");
}

/// A mutant's file has to already exist in the scratch tree for its instrumented text to be
/// written back over it: the copy step is what puts it there, so a `TargetFile` naming a path
/// the copy never created is a bug in the survey rather than something worth silently creating
/// a brand new file for, which is why the write is refused with the same error `overwrite`
/// reports for that case anywhere else it is called from.
#[test]
fn a_mutants_file_the_copy_never_created_reports_the_write_failure() {
    let (_dir, work) = trivial_workspace("build-uncopied-destination-");
    let mut plan = empty_plan(&work);

    // The source is genuinely readable, so the read at the top of the loop succeeds; only the
    // later write, against a destination the copy never produced, is meant to fail here.
    plan.files.push(TargetFile {
        path: Utf8PathBuf::from("src/never_copied.rs"),
        absolute: work.root.join("src/lib.rs"),
        package: "trivial".to_owned(),
    });

    let error = Splices::default()
        .instrument(&work, &plan, &HashSet::default())
        .expect_err("the destination was never copied");

    assert!(error.to_string().contains("which the copy did not create"), "{error}");
}

/// A plan with no mutants and no files, rooted in the given workspace.
fn target_file(root: &Utf8Path, path: &str) -> TargetFile {
    TargetFile {
        path: Utf8PathBuf::from(path),
        absolute: root.join(path),
        package: "trivial".to_owned(),
    }
}

fn empty_plan(work: &Workspace) -> Plan {
    Plan {
        skipped: Vec::new(),
        digests: HashMap::default(),
        root: work.root.clone(),
        files: Vec::new(),
        mutants: Vec::new(),
        suppressed: 0,
        idle: Vec::new(),
        sharded_out: 0,
        settled_out: 0,
        reach: HashMap::default(),
        specs: HashMap::default(),
    }
}

/// A build that outstays its budget is killed rather than waited out.
#[test]
fn a_build_that_outstays_its_budget_is_stopped() {
    let (_dir, work) = trivial_workspace("build-budget-");

    // Zero budget: the deadline has passed before the first poll, so the child is killed on the
    // very first pass through the wait loop.
    let outcome = compile(
        &work,
        &["check".to_owned()],
        Some(Duration::ZERO),
        &mut crate::testing::Recorder::default(),
    )
    .expect("spawn");

    assert!(outcome.is_none(), "a build past its budget should report no output");
}

/// A build stopped by its budget reports no output at all, so the caller can say so.
#[test]
fn a_build_stopped_by_its_budget_reports_no_stdout() {
    let (_dir, work) = trivial_workspace("build-nostdout-");
    let limits = BuildLimits {
        timeout: Some(Duration::ZERO),
        multiplier: None,
        rollback_rounds: 0,
    };

    let compiled = run_cargo(
        &work,
        &empty_plan(&work),
        &["check"],
        None,
        limits,
        None,
        &mut crate::testing::Recorder::default(),
    )
    .expect("spawn");

    assert!(!compiled.succeeded);
    assert!(compiled.stdout.is_none());
}

/// And converging on such a build stops with an error naming the budget rather than looping.
#[test]
fn converging_on_a_build_that_never_finishes_stops_with_the_budget() {
    let (_dir, work) = trivial_workspace("build-converge-budget-");
    let plan = Plan {
        skipped: Vec::new(),
        digests: HashMap::default(),
        root: work.root.clone(),
        files: Vec::new(),
        mutants: Vec::new(),
        suppressed: 0,
        idle: Vec::new(),
        sharded_out: 0,
        settled_out: 0,
        reach: HashMap::default(),
        specs: HashMap::default(),
    };
    let limits = BuildLimits {
        timeout: Some(Duration::ZERO),
        multiplier: None,
        rollback_rounds: 0,
    };

    let error = Converger::default()
        .converge(&work, &plan, None, &["check"], limits, &mut crate::testing::Recorder::default())
        .expect_err("the build never finishes");

    assert!(error.to_string().contains("was still running"), "{error}");
    assert!(error.to_string().contains("--build-timeout"), "{error}");
}

/// A build inside its budget is collected through the same polling wait.
#[test]
fn a_build_inside_its_budget_is_collected() {
    let (_dir, work) = trivial_workspace("build-collected-");

    let outcome = compile(
        &work,
        &["--version".to_owned()],
        Some(Duration::from_mins(2)),
        &mut crate::testing::Recorder::default(),
    )
    .expect("spawn")
    .expect("cargo should finish well inside two minutes");

    assert!(outcome.status.success(), "{outcome:?}");
}

/// A build stopped for outstaying its budget takes everything it started with it.
///
/// Cargo is the root of a tree, not a process: `rustc`, build scripts, and whatever those start.
/// A kill aimed at cargo alone leaves that tree compiling against the same scratch directory the
/// run is about to reuse, on the cores the next attempt needs — and holding the write ends of the
/// pipes it inherited, which is what turns a build the budget cut off into a collection that never
/// ends.
///
/// The descendant here is the shape that survives: spawned by the build, holding its pipes, and
/// due to write a file well after the budget runs out. Reaching that write is what a surviving
/// subtree looks like from outside.
#[test]
fn a_build_stopped_by_its_budget_takes_its_descendants_with_it() {
    crate::testing::within(crate::testing::WATCHDOG, "a build with a descendant", || {
        let started = crate::testing::workdir("build-descendant-");
        let root = Utf8PathBuf::from_path_buf(started.path().to_path_buf()).expect("a UTF-8 scratch path");
        let (running, survived) = (root.join("running"), root.join("survived"));

        let mut command = Command::new(crate::testing::helper_binary_path().as_std_path());

        let _configured = command
            .arg(crate::testing::directive(format_args!(
                "spawn:touch:{running}|sleep:3000|touch:{survived}"
            )))
            .arg(crate::testing::directive("sleep:30000"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let work = Workspace::adopt(root.clone(), root.join("target"));

        let outcome = supervise(
            command,
            &work,
            Some(Duration::from_millis(750)),
            &mut crate::testing::Recorder::default(),
        )
        .expect("the build was supervised");

        assert!(outcome.is_none(), "a build past its budget reports no output");
        assert!(
            running.as_std_path().exists(),
            "the descendant never started, so this test proves nothing about killing it"
        );

        // Past the descendant's sleep, so anything still alive has had its chance to write.
        thread::sleep(Duration::from_millis(3500));

        assert!(
            !survived.as_std_path().exists(),
            "the build's descendant outlived the kill that was aimed at the build"
        );
    });
}

/// A build that finished is collected without waiting for whatever it left running.
///
/// Cargo exiting says nothing about what it started: a build script that leaves a daemon behind
/// leaves it holding the two pipes this run is reading, and end of file arrives when the *last*
/// holder lets go. Joining the readers first would wait for the survivor rather than for the
/// build, which is a hang with a build that succeeded sitting in it.
///
/// That the output arrives at all is the proof: a pipe still held by a live descendant cannot
/// reach end of file, so a reader that finished is one whose survivors are gone.
#[test]
fn a_finished_build_is_collected_without_waiting_for_its_survivors() {
    crate::testing::within(crate::testing::WATCHDOG, "a build with a survivor", || {
        let started = crate::testing::workdir("build-survivor-");
        let root = Utf8PathBuf::from_path_buf(started.path().to_path_buf()).expect("a UTF-8 scratch path");
        let running = root.join("running");

        let mut command = Command::new(crate::testing::helper_binary_path().as_std_path());

        let _configured = command
            .arg(crate::testing::directive(format_args!("spawn:touch:{running}|sleep:20000")))
            .arg(crate::testing::directive(format_args!("wait-file:{running}|5000")))
            .arg(crate::testing::directive("print:the build said this"))
            .arg(crate::testing::directive("exit:0"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let work = Workspace::adopt(root.clone(), root.join("target"));
        let began = Instant::now();

        let output = supervise(
            command,
            &work,
            Some(Duration::from_secs(45)),
            &mut crate::testing::Recorder::default(),
        )
        .expect("the build was supervised")
        .expect("the build finished well inside its budget");

        assert!(output.status.success(), "{output:?}");
        assert!(
            running.as_std_path().exists(),
            "the survivor never started, so this test proves nothing about outliving it"
        );
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "collecting the build waited for the survivor holding its pipes"
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("the build said this"),
            "what the build itself said has to survive the sweep: {output:?}"
        );
    });
}

/// A cargo that cannot be spawned at all names the tree it was to run in.
#[test]
fn a_missing_scratch_tree_is_named_rather_than_blamed_on_cargo() {
    let work = Workspace::adopt(
        Utf8PathBuf::from("/gamma/definitely/not/a/directory"),
        Utf8PathBuf::from("/gamma/definitely/not/a/directory/target"),
    );

    let error = compile(&work, &["--version".to_owned()], None, &mut crate::testing::Recorder::default()).expect_err("no such directory");

    assert!(error.to_string().contains("disappeared"), "{error}");
}

#[test]
fn a_cargo_that_cannot_be_found_names_the_program_and_where_it_came_from() {
    let root = tempfile::TempDir::new().expect("temp");
    let root = Utf8PathBuf::from_path_buf(root.path().to_path_buf()).expect("utf8");
    let work = Workspace::adopt(root.clone(), root.join("target"));

    let cause = io::Error::from(io::ErrorKind::NotFound);
    let error = spawn_failure("gamma-no-such-cargo-binary", &work, cause);

    assert!(error.to_string().contains("gamma-no-such-cargo-binary"), "{error}");
    assert!(error.to_string().contains("CARGO"), "{error}");
}

fn at(line: u32, column: u32) -> Position {
    Position::new(line, column).expect("a position written into a test is one-based by construction")
}

fn guard(site: Range<Position>, mutated: Option<Range<Position>>) -> Guard {
    Guard { site, mutated }
}

fn span(file: &str, line_start: u32, column_start: u32, line_end: u32, column_end: u32, primary: bool) -> Value {
    serde_json::json!({
        "file_name": file,
        "line_start": line_start,
        "column_start": column_start,
        "line_end": line_end,
        "column_end": column_end,
        "is_primary": primary,
    })
}

fn compiler_message(spans: &[Value]) -> String {
    serde_json::json!({
        "reason": "compiler-message",
        "message": {
            "level": "error",
            "rendered": "error: boom\n",
            "spans": spans,
        },
    })
    .to_string()
}

/// A diagnostic that names a code and keeps half of what it knows in its notes, which is the
/// shape every borrow-checker error has.
fn coded_message(code: &str, primary: &[Value], notes: &[Value]) -> String {
    serde_json::json!({
        "reason": "compiler-message",
        "message": {
            "level": "error",
            "code": { "code": code },
            "rendered": "error: boom\n",
            "spans": primary,
            "children": [{ "level": "note", "spans": notes }],
        },
    })
    .to_string()
}

fn mutant() -> Mutant {
    Mutant {
        id: "deadbeefcafe".to_owned().into(),
        ordinal: 1,
        file: (Utf8PathBuf::from("src/lib.rs")).into(),
        package: ("pkg".to_owned()).into(),
        span: 0..1,
        line: 7,
        end_line: 7,
        column: 3,
        mutator: ("lit.true_to_false".to_owned()).into(),
        item_path: ("pkg::f".to_owned()).into(),
        occurrence: 0,
        replacement_index: 0,
        original: "true".to_owned().into(),
        replacement: "false".to_owned().into(),
        shape: Shape::Expr,
        outcome: Outcome::Pending,
        suppression: None,
        expectation: None,
        test_timeout_multiplier: None,
        elapsed_ms: 0,
        killed_by: None,
        note: None,
    }
}

/// The failure this tier exists for, taken from a real tree: a deleted `continue` makes a path
/// that could not be reached statically reachable, so a value moved earlier is now seen to be
/// used again — and rustc reports that at the use, at the move and at the reinitialization,
/// none of which is where the guard sits.
#[test]
fn a_move_error_is_blamed_on_the_deletion_that_changed_which_paths_exist() {
    let mut guards = Guards::default();

    // The deleted `continue`, at line 396: inside the region the diagnostic talks about, but
    // named by none of its spans.
    let _ = guards.insert(7, (Utf8PathBuf::from("src/codegen.rs"), guard(at(396, 9)..at(396, 17), None)));

    // A substitution in the same region. It cannot have changed reachability, so it is not the
    // one to withdraw while a deletion is available.
    let _ = guards.insert(
        8,
        (
            Utf8PathBuf::from("src/codegen.rs"),
            guard(at(400, 5)..at(400, 9), Some(at(400, 5)..at(400, 9))),
        ),
    );

    let stdout = coded_message(
        "E0382",
        &[span("src/codegen.rs", 432, 9, 432, 13, true)],
        &[
            span("src/codegen.rs", 372, 5, 372, 20, false),
            span("src/codegen.rs", 383, 9, 383, 13, false),
        ],
    );

    let blamed = ordinals_blamed(&stdout, Utf8Path::new(""), &guards);

    assert_eq!(blamed, HashSet::from_iter([7]), "the deletion is the only reachability change");
}

/// A run can report how many mutants would not compile; without the code it cannot say whether
/// that number is a mutator emitting ill-typed code or the borrow checker objecting to the
/// schema, and those want opposite remedies.
#[test]
fn a_withdrawal_remembers_the_error_code_that_caused_it() {
    let mut guards = Guards::default();

    let _ = guards.insert(
        7,
        (
            Utf8PathBuf::from("src/lib.rs"),
            guard(at(10, 5)..at(10, 9), Some(at(10, 5)..at(10, 9))),
        ),
    );

    let stdout = coded_message("E0308", &[span("src/lib.rs", 10, 5, 10, 9, true)], &[]);

    assert_eq!(
        blame(&stdout, Utf8Path::new(""), &guards).get(&7).map(String::as_str),
        Some("E0308")
    );
}

#[test]
fn a_secondary_span_intersecting_generated_text_blames_that_mutant() {
    let mut guards = Guards::default();
    let _ = guards.insert(
        7,
        (
            Utf8PathBuf::from("src/lib.rs"),
            guard(at(10, 5)..at(10, 45), Some(at(10, 20)..at(10, 25))),
        ),
    );

    let stdout = compiler_message(&[span("src/lib.rs", 12, 5, 12, 20, true), span("src/lib.rs", 10, 5, 10, 40, false)]);

    assert_eq!(ordinals_blamed(&stdout, Utf8Path::new(""), &guards), HashSet::from_iter([7]));
}

#[test]
fn a_secondary_span_on_innocent_original_text_blames_nothing() {
    let mut guards = Guards::default();
    let _ = guards.insert(
        7,
        (
            Utf8PathBuf::from("src/lib.rs"),
            guard(at(10, 5)..at(10, 45), Some(at(10, 20)..at(10, 25))),
        ),
    );

    let stdout = compiler_message(&[span("src/lib.rs", 12, 5, 12, 20, true), span("src/lib.rs", 10, 30, 10, 40, false)]);

    assert!(ordinals_blamed(&stdout, Utf8Path::new(""), &guards).is_empty());
}

/// One unviable mutant can draw a four-figure count of follow-on diagnostics, so a census that
/// tallies rows rather than distinct mutants overstates its answer by an order of magnitude —
/// and the pair that leads the list is the one a reader will act on.
#[test]
fn the_census_counts_mutants_rather_than_diagnostics_and_leads_with_the_densest_pair() {
    let mut converger = Converger::default();
    let plan = plan_of(&[(1, "lit.true_to_false"), (2, "lit.true_to_false"), (3, "expr.delete")]);

    let _ = converger.census.insert(1, "E0308".to_owned());
    let _ = converger.census.insert(2, "E0308".to_owned());
    let _ = converger.census.insert(3, "E0382".to_owned());

    // A second sighting of a mutant already in the census is what a follow-on diagnostic looks
    // like, and must not count twice.
    let _ = converger.census.insert(1, "E0308".to_owned());

    assert_eq!(
        converger.tally(&plan),
        vec![
            Withdrawal {
                code: "E0308".to_owned(),
                mutator: "lit.true_to_false".to_owned(),
                mutants: 2,
            },
            Withdrawal {
                code: "E0382".to_owned(),
                mutator: "expr.delete".to_owned(),
                mutants: 1,
            },
        ]
    );
}

/// A plan with one mutant per ordinal and mutator, for the census.
fn plan_of(entries: &[(u32, &str)]) -> Plan {
    Plan {
        skipped: Vec::new(),
        digests: HashMap::default(),
        root: Utf8PathBuf::new(),
        files: Vec::new(),
        mutants: entries
            .iter()
            .map(|(ordinal, mutator)| Mutant {
                ordinal: *ordinal,
                mutator: ((*mutator).to_owned()).into(),
                ..mutant()
            })
            .collect(),
        suppressed: 0,
        idle: Vec::new(),
        sharded_out: 0,
        settled_out: 0,
        reach: HashMap::default(),
        specs: HashMap::default(),
    }
}

/// The gate is the whole safety of the tier. An error whose position means something must not
/// reach it, or a tree that simply does not compile would have innocent mutants withdrawn from
/// it one round at a time.
#[test]
fn an_error_that_is_not_flow_sensitive_is_never_blamed_by_region() {
    let mut guards = Guards::default();

    let _ = guards.insert(7, (Utf8PathBuf::from("src/codegen.rs"), guard(at(396, 9)..at(396, 17), None)));

    // An unresolved import is positional, and it is exactly what a tree with a broken feature
    // selection reports. Nothing here is a mutant's doing.
    let stdout = coded_message("E0432", &[span("src/codegen.rs", 432, 9, 432, 13, true)], &[]);

    assert!(ordinals_blamed(&stdout, Utf8Path::new(""), &guards).is_empty());

    // Neither is an error with no code at all, which carries nothing to gate on.
    let uncoded = compiler_message(&[span("src/codegen.rs", 432, 9, 432, 13, true)]);

    assert!(ordinals_blamed(&uncoded, Utf8Path::new(""), &guards).is_empty());
}

/// With no deletion in the region there is nothing better to go on, and losing a few mutants
/// that would have compiled is a far smaller loss than losing the run.
#[test]
fn a_region_with_no_deletion_falls_back_to_every_guard_in_it() {
    let mut guards = Guards::default();

    let _ = guards.insert(
        3,
        (
            Utf8PathBuf::from("src/codegen.rs"),
            guard(at(400, 5)..at(400, 9), Some(at(400, 5)..at(400, 9))),
        ),
    );

    // Outside the region the diagnostic spans, so it is not a candidate however blunt the
    // fallback is.
    let _ = guards.insert(
        4,
        (
            Utf8PathBuf::from("src/codegen.rs"),
            guard(at(900, 5)..at(900, 9), Some(at(900, 5)..at(900, 9))),
        ),
    );

    let stdout = coded_message(
        "E0499",
        &[span("src/codegen.rs", 432, 9, 432, 13, true)],
        &[span("src/codegen.rs", 383, 9, 383, 13, false)],
    );

    assert_eq!(ordinals_blamed(&stdout, Utf8Path::new(""), &guards), HashSet::from_iter([3]));
}

/// The positional tiers are still the better answer when they have one, since they name a
/// single mutant rather than a region's worth of them.
#[test]
fn a_diagnostic_that_lands_on_a_guard_is_still_blamed_on_that_guard_alone() {
    let mut guards = Guards::default();

    let _ = guards.insert(
        5,
        (
            Utf8PathBuf::from("src/codegen.rs"),
            guard(at(432, 9)..at(432, 13), Some(at(432, 9)..at(432, 13))),
        ),
    );

    let _ = guards.insert(6, (Utf8PathBuf::from("src/codegen.rs"), guard(at(396, 9)..at(396, 17), None)));

    let stdout = coded_message(
        "E0382",
        &[span("src/codegen.rs", 432, 9, 432, 13, true)],
        &[span("src/codegen.rs", 383, 9, 383, 13, false)],
    );

    assert_eq!(ordinals_blamed(&stdout, Utf8Path::new(""), &guards), HashSet::from_iter([5]));
}

#[test]
fn diagnostics_are_read_from_the_json_stream() {
    // Diagnostics arrive on stdout as JSON; stderr holds only a summary, so a failure report
    // built from stderr would say nothing about what actually went wrong.
    let stdout = concat!(
        r#"{"reason":"compiler-message","message":{"level":"error","rendered":"error[E0308]: boom"}}"#,
        "\n",
        r#"{"reason":"compiler-message","message":{"level":"warning","rendered":"just a warning"}}"#,
        "\n",
        r#"{"reason":"compiler-message"}"#,
        "\n",
        r#"{"reason":"compiler-message","message":{}}"#,
        "\n",
        r#"{"reason":"compiler-artifact","profile":{"test":true},"executable":"/tmp/x"}"#,
        "\n",
        // Cargo interleaves its own plain-text chatter with the JSON stream.
        "warning: unused manifest key\n",
    );
    let rendered = diagnostics(stdout).into_iter().map(|found| found.rendered).collect::<String>();

    assert!(rendered.contains("E0308"));
    assert!(!rendered.contains("just a warning"));
    assert!(!rendered.contains("unused manifest key"));
}

/// An error-level compiler message with no `rendered` field is not something real `rustc`
/// emits, but a future cargo could add an error kind that omits it, and treating a missing
/// field as an empty string rather than skipping it silently would make the caller's report
/// look like `rustc` said nothing at all, which is worse than the diagnostic being absent.
#[test]
fn an_error_message_with_no_rendered_text_contributes_nothing_rather_than_panicking() {
    let stdout = r#"{"reason":"compiler-message","message":{"level":"error"}}"#;

    assert!(diagnostics(stdout).is_empty());
}

/// The report quotes whole diagnostics from the front. Keeping the *last* N lines instead would
/// open the report partway through a snippet, with no error line above it to say what the
/// underlines were pointing at.
#[test]
fn a_long_diagnostic_list_keeps_the_first_errors_whole_and_counts_the_rest() {
    let rendered: Vec<Diagnostic> = (0..8)
        .map(|index| reported(None, &format!("error[E{index:04}]: something\n --> src/lib.rs:{index}\n")))
        .collect();

    let shown = leading(&rendered, 3);

    assert!(shown.starts_with("error[E0000]"), "{shown}");
    assert!(shown.contains("error[E0002]"), "{shown}");
    assert!(!shown.contains("error[E0003]"), "{shown}");
    assert!(shown.contains("and 5 further errors not shown"), "{shown}");
}

#[test]
fn a_diagnostic_list_within_the_limit_is_quoted_whole_and_counts_nothing() {
    let rendered = vec![reported(None, "error[E0001]: something\n")];

    assert_eq!(leading(&rendered, 3), "error[E0001]: something\n");
}

/// The errors quoted first are the ones in the code the caller chose to mutate. A reverse
/// dependency pulled in because its tests form part of the oracle can bury the caller's own
/// crate under errors that are consequences of it.
#[test]
fn errors_in_the_mutated_packages_are_quoted_before_anyone_elses() {
    let mut found = vec![
        reported(Some("/w/other/Cargo.toml"), "error: in the reverse dependency\n"),
        reported(Some("/w/mine/Cargo.toml"), "error: in my own crate\n"),
        reported(None, "error: from nowhere in particular\n"),
    ];

    prioritize(&mut found, &core::iter::once("/w/mine/Cargo.toml".to_owned()).collect());

    assert!(leading(&found, 1).starts_with("error: in my own crate\n"), "{}", leading(&found, 1));

    // The rest keep the order the compiler emitted them in.
    assert!(
        leading(&found, 3).contains("reverse dependency\nerror: from nowhere"),
        "{}",
        leading(&found, 3)
    );
}

fn reported(manifest: Option<&str>, rendered: &str) -> Diagnostic {
    Diagnostic {
        manifest: manifest.map(ToOwned::to_owned),
        rendered: rendered.to_owned(),
    }
}

#[test]
fn diagnostic_spans_inside_mutated_text_name_that_mutant_exactly() {
    let mut guards = Guards::default();
    let file = Utf8PathBuf::from("src/lib.rs");

    let _old = guards.insert(1, (file.clone(), guard(at(10, 1)..at(20, 1), Some(at(12, 5)..at(12, 10)))));
    let _old = guards.insert(2, (file, guard(at(10, 1)..at(20, 1), None)));

    // Exact mutated-branch hits are trusted before the broader guarded site, or shared sites
    // would withdraw innocent neighbors.
    let blamed = ordinals_blamed(
        &compiler_message(&[span("src/lib.rs", 12, 6, 12, 8, true)]),
        Utf8Path::new("/work"),
        &guards,
    );

    assert_eq!(blamed, HashSet::from_iter([1]));
}

#[test]
fn a_diagnostic_inside_a_guard_blames_the_innermost_enclosing_site() {
    let mut guards = Guards::default();

    for ordinal in [1, 2, 3] {
        let site = if ordinal == 1 { at(1, 1)..at(50, 1) } else { at(10, 1)..at(15, 1) };
        let _old = guards.insert(ordinal, (Utf8PathBuf::from("src/lib.rs"), guard(site, None)));
    }

    // When a deletion or type change breaks copied text inside the guard, the narrowest site
    // is the least destructive rollback candidate, with ties kept together.
    let blamed = ordinals_blamed(
        &compiler_message(&[span("src/lib.rs", 12, 2, 12, 4, true)]),
        Utf8Path::new("/work"),
        &guards,
    );

    assert_eq!(blamed, HashSet::from_iter([2, 3]));
}

#[test]
fn a_diagnostic_that_encloses_guards_keeps_only_the_smallest_reported_region() {
    let mut guards = Guards::default();

    let _old = guards.insert(1, (Utf8PathBuf::from("src/lib.rs"), guard(at(20, 1)..at(21, 1), None)));
    let _old = guards.insert(2, (Utf8PathBuf::from("src/lib.rs"), guard(at(40, 1)..at(41, 1), None)));

    let stdout = compiler_message(&[span("src/lib.rs", 1, 1, 100, 1, false), span("src/lib.rs", 35, 1, 45, 1, false)]);

    // Borrow-checker errors can land on a construct containing a guard; the smallest such
    // diagnostic is blamed so a broad fallback does not mask a narrower one.
    let blamed = ordinals_blamed(&stdout, Utf8Path::new("/work"), &guards);

    assert_eq!(blamed, HashSet::from_iter([2]));
}

/// When a wider enclosing diagnostic is processed after a narrower one has already claimed a
/// guard, the wider one must not overwrite that narrower attribution: preferring the smaller
/// region is the whole point of keeping "the smallest reported region", and a widen-then-keep
/// bug here would silently blame — and withdraw — the wrong, unrelated guard instead.
#[test]
fn a_wider_enclosing_diagnostic_processed_after_a_narrower_one_does_not_replace_its_attribution() {
    let mut guards = Guards::default();

    let _old = guards.insert(1, (Utf8PathBuf::from("src/lib.rs"), guard(at(10, 1)..at(11, 1), None)));
    let _old = guards.insert(2, (Utf8PathBuf::from("src/lib.rs"), guard(at(60, 1)..at(61, 1), None)));

    // The narrow span (covering only guard 1) comes first in the message; the much wider span
    // (covering both guards) comes second. Message spans are read in order, so this
    // deterministically exercises the case where a later, wider candidate must be discarded
    // rather than take over from an earlier, narrower one.
    let stdout = compiler_message(&[span("src/lib.rs", 5, 1, 15, 1, false), span("src/lib.rs", 1, 1, 100, 1, false)]);

    let blamed = ordinals_blamed(&stdout, Utf8Path::new("/work"), &guards);

    assert_eq!(blamed, HashSet::from_iter([1]));
}

#[test]
fn diagnostics_are_matched_by_suffix_when_cargo_spells_paths_differently() {
    let mut guards = Guards::default();

    let _old = guards.insert(
        1,
        (
            Utf8PathBuf::from("crates/pkg/src/lib.rs"),
            guard(at(5, 1)..at(6, 1), Some(at(5, 5)..at(5, 10))),
        ),
    );

    // Cargo can report an absolute or otherwise differently-rooted path; suffix matching keeps
    // those diagnostics attributable rather than losing the whole run.
    let blamed = ordinals_blamed(
        &compiler_message(&[span("/elsewhere/crates/pkg/src/lib.rs", 5, 6, 5, 8, true)]),
        Utf8Path::new("/scratch/tree"),
        &guards,
    );

    assert_eq!(blamed, HashSet::from_iter([1]));
}

#[test]
fn non_error_messages_and_malformed_spans_are_ignored_for_ordinals_blamed() {
    let mut guards = Guards::default();

    let _old = guards.insert(1, (Utf8PathBuf::from("src/lib.rs"), guard(at(1, 1)..at(2, 1), None)));
    let stdout = [
        "not json".to_owned(),
        serde_json::json!({"reason": "compiler-artifact"}).to_string(),
        serde_json::json!({"reason": "compiler-message", "message": {"level": "warning"}}).to_string(),
        serde_json::json!({"reason": "compiler-message", "message": {"level": "error"}}).to_string(),
        serde_json::json!({
            "reason": "compiler-message",
            "message": {"level": "error", "spans": [{}, {"file_name": "src/lib.rs"}]},
        })
        .to_string(),
    ]
    .join("\n");

    // Ignoring incomplete compiler output is safer than guessing and withdrawing unrelated
    // mutants.
    assert!(ordinals_blamed(&stdout, Utf8Path::new("/work"), &guards).is_empty());
}

/// A span with a line but no column is exactly as unusable as one missing everything: reading a
/// half-formed position and treating it as real would blame a guard at some arbitrary column,
/// which is worse than simply not attributing the diagnostic at all.
#[test]
fn a_span_with_a_line_but_no_column_is_not_read_as_a_position() {
    let mut guards = Guards::default();

    let _old = guards.insert(1, (Utf8PathBuf::from("src/lib.rs"), guard(at(1, 1)..at(2, 1), None)));
    let span = serde_json::json!({
        "file_name": "src/lib.rs",
        "line_start": 1,
        "line_end": 1,
        "column_end": 2,
        "is_primary": true,
    });
    let stdout = compiler_message(&[span]);

    assert!(ordinals_blamed(&stdout, Utf8Path::new("/work"), &guards).is_empty());
}

/// A span whose start is complete but whose end is missing a field is just as unreadable as one
/// missing its start: the diagnostic names no coherent region at all, so it must be ignored
/// rather than blamed against whatever guard happens to sit near its start.
#[test]
fn a_span_with_a_complete_start_but_no_end_is_not_read_as_a_position() {
    let mut guards = Guards::default();

    let _old = guards.insert(1, (Utf8PathBuf::from("src/lib.rs"), guard(at(1, 1)..at(2, 1), None)));
    let span = serde_json::json!({
        "file_name": "src/lib.rs",
        "line_start": 1,
        "column_start": 1,
        "line_end": 1,
        "is_primary": true,
    });
    let stdout = compiler_message(&[span]);

    assert!(ordinals_blamed(&stdout, Utf8Path::new("/work"), &guards).is_empty());
}

#[test]
fn build_errors_are_formatted_without_running_cargo() {
    let timeout = Converger::build_timeout_error(Duration::from_secs(2)).to_string();
    let stdout = compiler_message(&[span("src/lib.rs", 1, 1, 1, 2, true)]);
    let (_dir, mut work) = trivial_workspace("build-errors-");

    work.leak = true;

    let unattributed = Converger::unattributed_build_error(&work, &stdout, "").to_string();
    let limited = Converger::rollback_limit_error(32, 32, &[9, 5, 2], &work, &stdout).to_string();
    let missing = Converger::missing_guard_error(&mutant()).to_string();

    // These messages are the user's only explanation of build failures that happen before any
    // test binary can run, so the pure formatting paths are kept under test.
    assert!(timeout.contains("after 2s"));
    assert!(unattributed.contains("could not be attributed"));
    assert!(limited.contains("32 of the 32 rollback rounds"), "{limited}");
    assert!(limited.contains("16 blamed during this build"), "{limited}");
    assert!(limited.contains("9, 5, 2"), "{limited}");

    // The series ends with the round the limit stopped, so a budget of one round still has a
    // count to report and still gets the falling-or-flat advice. Reading the series without
    // that round would leave this message empty and claim the last round found nothing.
    let single = Converger::rollback_limit_error(1, 1, &[41], &work, &stdout).to_string();

    assert!(single.contains("1 of the 1 rollback rounds"), "{single}");
    assert!(single.contains("41 blamed during this build"), "{single}");
    assert!(single.contains("blamed in the last rounds of this build: 41"), "{single}");
    assert!(single.contains("If those counts are falling"), "{single}");
    assert!(missing.contains("no guard was emitted"));
    assert!(missing.contains("src/lib.rs:7"));

    // A leaked tree is still there to be read, so the message names it.
    assert!(unattributed.contains(work.root.as_str()), "{unattributed}");

    // One that was not leaked is gone by the time the message is read, and sending someone to
    // a path that does not exist reads as a second bug on top of the one being reported.
    work.leak = false;

    let swept = Converger::unattributed_build_error(&work, &stdout, "").to_string();

    assert!(swept.contains("--leak-dirs"), "{swept}");
    assert!(!swept.contains(work.root.as_str()), "{swept}");
}

#[test]
fn a_build_that_reported_nothing_is_not_described_as_a_compile_failure() {
    // Cargo rejects a bad invocation — an ambiguous `--package`, an unknown feature — before it
    // compiles anything, so the failure arrives with an empty JSON stream. Calling that "does
    // not compile" sends the reader hunting for a broken mutant that was never generated.
    let (_dir, work) = trivial_workspace("silent-build-");
    let stderr = "   Compiling tonic v0.14.0\n\
                  error: failed to run custom build command for `codegen v0.1.0`\n\n\
                  Caused by:\n  \
                    process didn't exit successfully: exit status: 101\n";
    let message = Converger::unattributed_build_error(&work, "", stderr).to_string();

    assert!(message.contains("the compiler reported nothing"), "{message}");
    assert!(!message.contains("does not compile"), "{message}");

    // The whole point: the cause is on stderr and nowhere else, so it has to be shown.
    assert!(message.contains("failed to run custom build command"), "{message}");
    assert!(message.contains("Caused by"), "{message}");

    // Cargo narrates its progress on the same stream, and a cold build narrates thousands of
    // lines. Those must not bury the two that matter.
    assert!(!message.contains("Compiling tonic"), "{message}");
}

#[test]
fn a_failure_with_nothing_on_either_stream_says_so_rather_than_showing_a_blank() {
    // Printing an empty block reads as though the message itself is broken.
    let (_dir, work) = trivial_workspace("silent-both-");
    let message = Converger::unattributed_build_error(&work, "", "   Compiling x v0.1.0\n").to_string();

    assert!(message.contains("cargo said nothing on stderr either"), "{message}");
}

#[test]
fn cargo_progress_is_told_apart_from_a_word_that_merely_starts_the_same_way() {
    // `Compiling` is progress; `Compilation failed` is not, and dropping it would hide the
    // only line that explains the failure.
    let kept = complaints("   Compiling x v0.1.0\nCompilationfailed for a reason\n");

    assert!(kept.contains("Compilationfailed"), "{kept}");
    assert!(!kept.contains("Compiling x"), "{kept}");
}

/// A progress verb is only progress when it is followed by a space or by nothing at all; a line
/// that merely shares the verb's letters as a prefix, with real text immediately butted up
/// against it, is not cargo narrating a step and has to be kept — dropping it on the strength of
/// the prefix alone would risk hiding a genuine complaint that happened to start the same way.
#[test]
fn a_word_sharing_a_progress_verbs_prefix_but_continuing_without_a_space_is_kept() {
    let kept = complaints("Compilingx and y do not unify\n");

    assert!(kept.contains("Compilingx and y do not unify"), "{kept}");
}

/// Cargo draws its progress bar with carriage returns and no newlines, so a whole build's worth
/// of redraws arrives as a single newline-terminated line. Splitting only on newlines would
/// hand someone whose build just failed one enormous bar with the error buried inside it.
#[test]
fn a_carriage_return_progress_bar_does_not_bury_the_error_it_was_drawn_around() {
    let bar = "    Building [==>      ] 2/17: serde_core\r\
               \u{1b}[1;36m    Building\u{1b}[0m [====>    ] 4/17: quote, syn\r\
               \u{1b}[1;36m   Compiling\u{1b}[0m [======>  ] 9/17: trivial\r";
    let stderr =
        format!("   Compiling serde v1.0.229\n{bar}\rerror: linking with `cc` failed: exit status: 1\r{bar}\n    Building [=>] 1/2\r");

    let kept = complaints(&stderr);

    assert_eq!(kept, "error: linking with `cc` failed: exit status: 1\n", "{kept}");
}

/// Cargo pads its own diagnostics with a blank leading line; keeping it would show the reader
/// an empty first line before the actual complaint, which reads as though the message itself
/// is truncated or broken.
#[test]
fn a_leading_blank_line_on_stderr_is_dropped_rather_than_kept() {
    let kept = complaints("\nerror: something is wrong\n");

    assert_eq!(kept, "error: something is wrong\n");
}

/// Dep-info from an earlier run with different features is the whole reason this is keyed off
/// the build's own artifact messages, so it has to be shown that the stale file is ignored.
#[test]
fn only_the_dep_info_belonging_to_this_build_is_read() {
    let dir = crate::testing::workdir("dep-files-");
    let deps = Utf8PathBuf::from_path_buf(dir.path().join("deps")).expect("utf8");

    fs::create_dir_all(deps.as_std_path()).expect("deps");
    fs::write(deps.join("mine-aaaa.d").as_std_path(), "x: src/kept.rs\n").expect("mine");
    fs::write(deps.join("stale-bbbb.d").as_std_path(), "x: src/gone.rs\n").expect("stale");

    let stdout = artifact_message(&deps.join("libmine-aaaa.rmeta"));

    let compiled = compiled_sources(&stdout, Utf8Path::new("/nowhere")).expect("read something");

    assert!(compiled.contains(Utf8Path::new("src/kept.rs")), "{compiled:?}");
    assert!(!compiled.contains(Utf8Path::new("src/gone.rs")), "{compiled:?}");
}

/// A dependency path with an escaped space is one path, not two.
///
/// `.d` files are makefile fragments, so a space inside a path is written `\ `. Read as a
/// separator it produces two fragments that match nothing the survey found, the source looks as
/// though the compiler never opened it, and every mutant in it is excused as unbuilt — silently,
/// because the set still agrees with the survey about every space-free file beside it.
#[test]
fn a_dependency_path_containing_an_escaped_space_is_read_as_one_path() {
    let dir = crate::testing::workdir("dep-spaces-");
    let deps = Utf8PathBuf::from_path_buf(dir.path().join("deps")).expect("utf8");

    fs::create_dir_all(deps.as_std_path()).expect("deps");
    fs::write(deps.join("mine-aaaa.d").as_std_path(), "x.rmeta: src/lib.rs src/my\\ file.rs\n").expect("mine");

    let stdout = artifact_message(&deps.join("libmine-aaaa.rmeta"));

    let compiled = compiled_sources(&stdout, Utf8Path::new("/nowhere")).expect("read something");

    assert!(compiled.contains(Utf8Path::new("src/lib.rs")), "{compiled:?}");
    assert!(compiled.contains(Utf8Path::new("src/my file.rs")), "{compiled:?}");
    assert_eq!(compiled.len(), 2, "the escaped space split the path: {compiled:?}");
}

/// A backslash that is not escaping whitespace is a Windows separator and stays in the path.
///
/// The two emitters escape the space and nothing else, so unescaping every backslash would turn
/// `C:\src\lib.rs` into `C:srclib.rs` and lose every dependency on that platform — a far larger
/// hole than the one closed by handling the space at all.
#[test]
fn a_backslash_that_is_not_an_escape_survives_into_the_path() {
    let dir = crate::testing::workdir("dep-separators-");
    let deps = Utf8PathBuf::from_path_buf(dir.path().join("deps")).expect("utf8");

    fs::create_dir_all(deps.as_std_path()).expect("deps");
    fs::write(deps.join("mine-aaaa.d").as_std_path(), "x.rmeta: src\\my\\ file.rs\n").expect("mine");

    let stdout = artifact_message(&deps.join("libmine-aaaa.rmeta"));

    let compiled = compiled_sources(&stdout, Utf8Path::new("/nowhere")).expect("read something");

    // The separator is normalised the way every other dep-info path is, so what a Windows run
    // compares against the survey is the same shape as what a Unix run does.
    assert!(compiled.contains(Utf8Path::new("src/my file.rs")), "{compiled:?}");
}

/// A mutant in a file whose path contains a space is not excused as never built.
///
/// This is the consequence the tokenising exists to prevent, and the reason it cannot be caught
/// downstream: the whole-set agreement check passes as soon as any one space-free source is
/// recognised, so a misread path is withdrawn one file at a time with nothing in the report saying
/// so. `NotBuilt` is excluded from the score, so those mutants leave the denominator silently.
#[test]
fn a_mutant_in_a_file_whose_path_contains_a_space_is_not_excused_as_unbuilt() {
    let dir = crate::testing::workdir("withdraw-spaces-");
    let deps = Utf8PathBuf::from_path_buf(dir.path().join("deps")).expect("utf8");

    fs::create_dir_all(deps.as_std_path()).expect("deps");
    fs::write(deps.join("mine-aaaa.d").as_std_path(), "x.rmeta: src/lib.rs src/my\\ file.rs\n").expect("mine");

    let stdout = artifact_message(&deps.join("libmine-aaaa.rmeta"));
    let compiled = compiled_sources(&stdout, Utf8Path::new("/nowhere")).expect("read something");

    let target = |path: &str| TargetFile {
        path: Utf8PathBuf::from(path),
        absolute: Utf8PathBuf::from("/nowhere").join(path),
        package: "pkg".to_owned(),
    };

    let mutant_in = |path: &str, ordinal: u32| Mutant {
        ordinal,
        file: (Utf8PathBuf::from(path)).into(),
        ..mutant()
    };

    let mut plan = plan_of(&[]);

    plan.files = vec![target("src/lib.rs"), target("src/my file.rs"), target("src/absent.rs")];
    plan.mutants = vec![
        mutant_in("src/lib.rs", 1),
        mutant_in("src/my file.rs", 2),
        mutant_in("src/absent.rs", 3),
    ];

    withdraw_uncompiled(&mut plan, &compiled);

    assert_eq!(plan.mutants[0].outcome, Outcome::Pending, "the plain file was withdrawn");
    assert_eq!(plan.mutants[1].outcome, Outcome::Pending, "the spaced file was withdrawn");

    // The withdrawal still has to happen for a file the compiler genuinely never opened, or the
    // test would pass just as well against a version that concluded nothing at all.
    assert_eq!(plan.mutants[2].outcome, Outcome::NotBuilt, "an uncompiled file was kept");
}

/// A compiled set that names not one surveyed file makes `withdraw_uncompiled` conclude nothing.
///
/// The whole-set agreement guard is what stands between a dep-info spelling regression and a
/// flattering perfect score: if `compiled_sources`/`normalize_separators`/escaping ever spelled
/// paths differently from the survey, every path would miss, every pending mutant would be excused
/// as `NotBuilt`, and the run would report approximately 100% with no sign anything went wrong. When the
/// compiled set is wholly disjoint from `plan.files`, the tool does not understand it, so it must
/// keep its hands off rather than withdraw the lot. Deleting the guard flips both mutants to
/// `NotBuilt`.
#[test]
fn a_compiled_set_disjoint_from_the_survey_withdraws_nothing() {
    let target = |path: &str| TargetFile {
        path: Utf8PathBuf::from(path),
        absolute: Utf8PathBuf::from("/nowhere").join(path),
        package: "pkg".to_owned(),
    };
    let mutant_in = |path: &str, ordinal: u32| Mutant {
        ordinal,
        file: (Utf8PathBuf::from(path)).into(),
        ..mutant()
    };

    let mut plan = plan_of(&[]);

    plan.files = vec![target("src/lib.rs"), target("src/main.rs")];
    plan.mutants = vec![mutant_in("src/lib.rs", 1), mutant_in("src/main.rs", 2)];

    // Every path names something the survey never produced, so nothing intersects `plan.files`.
    let mut compiled = HashSet::default();
    let _ = compiled.insert(Utf8PathBuf::from("build/generated.rs"));
    let _ = compiled.insert(Utf8PathBuf::from("vendor/other.rs"));

    withdraw_uncompiled(&mut plan, &compiled);

    assert_eq!(
        plan.mutants[0].outcome,
        Outcome::Pending,
        "a disjoint compiled set must not excuse a mutant"
    );
    assert_eq!(
        plan.mutants[1].outcome,
        Outcome::Pending,
        "a disjoint compiled set must not excuse a mutant"
    );
}

/// An unreadable tree must yield no opinion at all. Returning an empty set instead would mark
/// every mutant in the run as unbuilt and report a perfect score for a run that tested nothing.
#[test]
fn dep_info_that_cannot_be_read_yields_no_conclusion() {
    let stdout = r#"{"reason":"compiler-artifact","filenames":["/nowhere/deps/libmine-aaaa.rmeta"]}"#;

    assert!(compiled_sources(stdout, Utf8Path::new("/nowhere")).is_none());
}

/// A `.d` path that cannot be read as text — because it turned out to be a directory, not a
/// file — must not abort discovery of every other unit's dep-info; only that one entry's
/// contribution is lost.
#[test]
fn a_dep_info_path_that_cannot_be_read_as_text_is_skipped() {
    let dir = crate::testing::workdir("dep-not-a-file-");
    let deps = Utf8PathBuf::from_path_buf(dir.path().join("deps")).expect("utf8");

    fs::create_dir_all(deps.as_std_path()).expect("deps");

    // A directory happens to share the name a dep-info file would have; reading it as text
    // fails the same way an unreadable file would.
    fs::create_dir_all(deps.join("mine-aaaa.d").as_std_path()).expect("a directory standing in for the dep file");

    let stdout = artifact_message(&deps.join("libmine-aaaa.rmeta"));
    let compiled = compiled_sources(&stdout, Utf8Path::new("/nowhere"));

    // Nothing could be read at all, so the honest answer is "no opinion", not an empty set.
    assert!(compiled.is_none(), "{compiled:?}");
}

/// A dep-info line naming no rule at all — no `:` in it — carries nothing this can act on;
/// tolerating it keeps the rest of the file's entries readable instead of failing the whole
/// parse on one stray or corrupted line.
#[test]
fn a_dep_info_line_without_a_rule_separator_is_ignored() {
    let dir = crate::testing::workdir("dep-no-colon-");
    let deps = Utf8PathBuf::from_path_buf(dir.path().join("deps")).expect("utf8");

    fs::create_dir_all(deps.as_std_path()).expect("deps");
    fs::write(deps.join("mine-aaaa.d").as_std_path(), "just some text with no colon\n").expect("mine");

    let stdout = artifact_message(&deps.join("libmine-aaaa.rmeta"));
    let compiled = compiled_sources(&stdout, Utf8Path::new("/nowhere")).expect("the file was read");

    assert!(compiled.is_empty(), "{compiled:?}");
}

/// Cargo's JSON stream can be interleaved with plain text on some platforms; a line that fails
/// to parse must be skipped rather than aborting discovery of every artifact that follows it.
#[test]
fn a_line_that_is_not_json_does_not_stop_dep_file_discovery() {
    let dir = crate::testing::workdir("dep-files-bad-json-");
    let deps = Utf8PathBuf::from_path_buf(dir.path().join("deps")).expect("utf8");

    fs::create_dir_all(deps.as_std_path()).expect("deps");
    fs::write(deps.join("mine-aaaa.d").as_std_path(), "x: src/kept.rs\n").expect("mine");

    let stdout = format!("not json at all\n{}\n", artifact_message(&deps.join("libmine-aaaa.rmeta")));

    assert_eq!(dep_files(&stdout), vec![deps.join("mine-aaaa.d")]);
}

/// Only `compiler-artifact` messages name a compiled unit; a message of a different reason —
/// cargo emits many kinds on the same stream — carries no dep-info to look for and must not be
/// mistaken for one.
#[test]
fn messages_with_a_different_reason_are_not_read_as_artifacts() {
    let stdout = r#"{"reason":"build-script-executed","filenames":["/gamma/deps/libmine-aaaa.rmeta"]}"#;

    assert!(dep_files(stdout).is_empty());
}

/// A filename with no file stem — such as one that is only an extension — cannot be paired
/// with a hash, so it has to be skipped instead of panicking or fabricating one.
#[test]
fn a_filename_with_no_parent_or_stem_is_skipped() {
    let stdout = r#"{"reason":"compiler-artifact","filenames":[".rlib"]}"#;

    assert!(dep_files(stdout).is_empty());
}

/// An entirely empty filename has neither a parent directory nor a name to hash from; cargo
/// would never really emit one, but a malformed or truncated JSON stream should still be
/// tolerated rather than panicking on a path with nothing in it.
#[test]
fn a_completely_empty_filename_is_skipped() {
    let stdout = r#"{"reason":"compiler-artifact","filenames":[""]}"#;

    assert!(dep_files(stdout).is_empty());
}

/// A directory that vanished between the build finishing and this scan running must not abort
/// the whole discovery pass; the rest of the units still have dep-info worth reading.
#[test]
fn a_dep_info_directory_that_does_not_exist_is_skipped() {
    let stdout = r#"{"reason":"compiler-artifact","filenames":["/gamma/definitely/not/a/directory/libmine-aaaa.rmeta"]}"#;

    assert!(dep_files(stdout).is_empty());
}

/// A filesystem is not obliged to hand back valid UTF-8 names, even though cargo's own output
/// always is; an entry that fails that conversion has to be skipped rather than making the
/// whole discovery pass panic partway through a directory listing.
#[test]
#[cfg(unix)]
fn a_directory_entry_with_a_non_utf8_name_is_skipped_without_panicking() {
    use std::os::unix::ffi::OsStrExt;

    let dir = crate::testing::workdir("dep-files-non-utf8-");
    let deps = Utf8PathBuf::from_path_buf(dir.path().join("deps")).expect("utf8");

    fs::create_dir_all(deps.as_std_path()).expect("deps");
    fs::write(deps.join("mine-aaaa.d").as_std_path(), "x: src/kept.rs\n").expect("mine");

    let invalid_name = std::ffi::OsStr::from_bytes(b"mine-\xFF\xFE.d");
    fs::write(dir.path().join("deps").join(invalid_name), "x: src/ignored.rs\n").expect("non-utf8 entry");

    let stdout = artifact_message(&deps.join("libmine-aaaa.rmeta"));
    let found = dep_files(&stdout);

    assert_eq!(found, vec![deps.join("mine-aaaa.d")], "{found:?}");
}

/// Dep-info spells its paths absolutely for some units and relatively for others, and mutants
/// are only ever named relative to the tree, so both spellings have to arrive the same way.
#[test]
fn absolute_and_relative_dependency_paths_both_land_relative_to_the_tree() {
    let dir = crate::testing::workdir("dep-paths-");
    let deps = Utf8PathBuf::from_path_buf(dir.path().join("deps")).expect("utf8");

    fs::create_dir_all(deps.as_std_path()).expect("deps");
    fs::write(deps.join("mine-aaaa.d").as_std_path(), "x: /tree/src/one.rs src/two.rs\n").expect("mine");

    let stdout = artifact_message(&deps.join("mine-aaaa.rlib"));
    let compiled = compiled_sources(&stdout, Utf8Path::new("/tree")).expect("read something");

    assert!(compiled.contains(Utf8Path::new("src/one.rs")), "{compiled:?}");
    assert!(compiled.contains(Utf8Path::new("src/two.rs")), "{compiled:?}");
}

/// A diagnostic full of escapes must survive the borrowed decode intact.
///
/// The decode names the fields it wants so serde can skip the rest of a megabyte-scale stream
/// without building it, and the strings that are only ever compared point into the line. A
/// `rendered` diagnostic is the one field that cannot: it is all newlines and quotes, so it
/// needs unescaping and a plain `&str` could not hold it. This is what pins the `Cow`.
#[test]
fn a_rendered_diagnostic_survives_the_escapes_it_arrives_with() {
    let line = "{\"reason\": \"compiler-message\", \"message\": {\"level\": \"error\", \"rendered\": \"error[E0308]: mismatched types\\n  --> src/x.rs:1:5\\n   |\\n   = note: \\\"quoted\\\"\\n\"}}";

    let rendered = rendered_diagnostic(line).expect("the line carries a rendered diagnostic");

    assert!(rendered.contains('\n'), "the newlines are real ones: {rendered:?}");
    assert!(rendered.contains("= note: \"quoted\""), "the quotes came through: {rendered:?}");
    assert!(rendered.starts_with("error[E0308]"));
}

/// A second round rewrites the files it changed, and leaves every other file exactly as it was.
///
/// Between two rollback rounds only the files whose mutants were withdrawn can differ, so
/// re-reading and re-splicing the rest produces byte-identical text at a cost that multiplies
/// by the round count. The proof has to be observable rather than incidental: a sentinel is
/// written over an untouched file's instrumented copy between the rounds, and it is still there
/// afterwards precisely because the second round never wrote that file. The file whose mutant
/// was withdrawn is rewritten in the same round, which is what stops this from being a test
/// that a round does nothing at all.
#[test]
fn a_second_round_rewrites_only_the_files_it_withdrew_from() {
    let dir = crate::testing::workdir("build-incremental-splice-");
    let origin = Utf8PathBuf::from_path_buf(dir.path().join("origin")).expect("utf8");
    let root = Utf8PathBuf::from_path_buf(dir.path().join("copy")).expect("utf8");

    let source = "pub const A: i32 = 1;\n";

    for base in [&origin, &root] {
        fs::create_dir_all(base.join("src").as_std_path()).expect("src");
        fs::write(base.join("src/a.rs").as_std_path(), source).expect("a");
        fs::write(base.join("src/b.rs").as_std_path(), source).expect("b");
    }

    let target = Utf8PathBuf::from_path_buf(dir.path().join("target")).expect("utf8");
    let work = Workspace::adopt(root.clone(), target);
    let mut plan = empty_plan(&work);

    for name in ["src/a.rs", "src/b.rs"] {
        plan.files.push(TargetFile {
            path: Utf8PathBuf::from(name),
            absolute: origin.join(name),
            package: "trivial".to_owned(),
        });
    }

    plan.mutants.push(Mutant {
        ordinal: 1,
        file: (Utf8PathBuf::from("src/a.rs")).into(),
        span: 19..20,
        ..mutant()
    });
    plan.mutants.push(Mutant {
        ordinal: 2,
        file: (Utf8PathBuf::from("src/b.rs")).into(),
        span: 19..20,
        ..mutant()
    });

    let mut splices = Splices::default();
    let first = splices
        .instrument(&work, &plan, &HashSet::default())
        .expect("the first round splices");

    assert_eq!(first.len(), 2, "both mutants were guarded");

    let sentinel = "// this round never touched me\n";

    fs::write(root.join("src/b.rs").as_std_path(), sentinel).expect("sentinel");

    // The first round blamed the mutant in `a.rs`, so only that file changes.
    let second = splices
        .instrument(&work, &plan, &HashSet::from_iter([1]))
        .expect("the second round splices");

    assert_eq!(
        fs::read_to_string(root.join("src/b.rs").as_std_path()).expect("b"),
        sentinel,
        "the untouched file was re-spliced"
    );

    assert_eq!(
        fs::read_to_string(root.join("src/a.rs").as_std_path()).expect("a"),
        source,
        "the withdrawn file was not put back"
    );

    // The guard for the file that did not change is still reported, since the text holding it
    // is still in the tree and a diagnostic can still land in it.
    assert!(second.contains_key(&2), "{second:?}");
    assert!(!second.contains_key(&1), "{second:?}");
}

/// A workspace linked against the real guard runtime, so a spliced guard can actually compile.
///
/// [`trivial_workspace`] deliberately is not: every mutant spliced into it fails, which is what
/// the convergence tests want. A test of build *ordering* needs the opposite — a population the
/// compiler genuinely divides in two — because the whole claim being tested is that the hint
/// decides only which half the compiler is shown first, and never which half a mutant lands in.
///
/// The runtime is vendored beside the crate and excluded from the workspace, which is what the run
/// itself does and for the same reason: a path dependency inside a workspace directory but absent
/// from its member list makes cargo refuse to build.
fn guarded_workspace(prefix: &str) -> (tempfile::TempDir, Workspace) {
    let dir = crate::testing::workdir(prefix);
    let root = Utf8PathBuf::from_path_buf(dir.path().join("src")).expect("utf8");
    let runtime = root.join("gamma-rt");

    fs::create_dir_all(root.join("src").as_std_path()).expect("src");
    fs::create_dir_all(runtime.join("src").as_std_path()).expect("runtime src");

    fs::write(
        runtime.join("Cargo.toml").as_std_path(),
        "[package]\nname = \"cargo-gamma-rt\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n\
         [lib]\nname = \"gamma_rt\"\npath = \"src/lib.rs\"\n\n[workspace]\n",
    )
    .expect("runtime manifest");

    // The tool's own runtime sources rather than stand-ins, so a change to the guard's signature
    // breaks this fixture rather than leaving it testing a shape nothing generates any more.
    for (name, source) in gamma_rt::embedded::SOURCES {
        fs::write(runtime.join("src").join(name).as_std_path(), source).expect("runtime source");
    }

    fs::write(
        root.join("Cargo.toml").as_std_path(),
        "[package]\nname = \"trivial\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
         [dependencies]\ngamma_rt = { path = \"gamma-rt\", package = \"cargo-gamma-rt\" }\n\n\
         [workspace]\nexclude = [\"gamma-rt\"]\n",
    )
    .expect("manifest");

    fs::write(root.join("src/lib.rs").as_std_path(), "pub const A: i32 = 1;\n").expect("lib");

    let target = Utf8PathBuf::from_path_buf(dir.path().join("target")).expect("utf8");
    let work = Workspace::adopt(root, target);

    (dir, work)
}

/// A workspace whose library holds `unviable` const items and `viable` function bodies.
///
/// The split matters: a guard is a call into the runtime, so splicing one into a `const`
/// initializer cannot compile — a const initializer may not call a function — while splicing one
/// into a function body compiles perfectly well. That gives a fixture with two populations whose
/// fate the compiler decides for real, which is what a test of build *ordering* needs: nothing here
/// may be settled by the hint itself.
fn probe_plan(work: &Workspace, unviable: usize, viable: usize) -> Plan {
    let mut text = String::new();

    for index in 0..unviable {
        let _ = writeln!(text, "pub const C{index}: i32 = 1;");
    }

    for index in 0..viable {
        let _ = writeln!(text, "pub fn f{index}() -> i32 {{ let value = 1; value }}");
    }

    let pristine = work.root.parent().expect("the tree has a parent").join("pristine");

    fs::create_dir_all(pristine.as_std_path()).expect("pristine");
    fs::write(pristine.join("lib.rs").as_std_path(), &text).expect("pristine source");
    fs::write(work.root.join("src/lib.rs").as_std_path(), &text).expect("tree source");

    let mut plan = empty_plan(work);

    plan.files.push(TargetFile {
        path: Utf8PathBuf::from("src/lib.rs"),
        absolute: pristine.join("lib.rs"),
        package: "trivial".to_owned(),
    });

    // Every `1` in the fixture is a mutation site, and they appear in source order: the constants
    // first, then the function bodies. Ordinals and ids follow that order, so a test can name
    // either population by index without depending on how the plan is built.
    let mut offset = 0;

    for index in 0..unviable.saturating_add(viable) {
        // Anchored on the assignment rather than on the digit, because the item names carry digits
        // of their own and a bare search would splice the guard into `C1` instead of its value.
        let start = text
            .get(offset..)
            .and_then(|rest| rest.find("= 1"))
            .expect("every item holds a literal")
            + offset
            + 2;

        offset = start + 1;

        plan.mutants.push(Mutant {
            id: format!("mutant-{index}").into(),
            ordinal: u32::try_from(index).expect("the fixture is small") + 1,
            span: start..start + 1,
            replacement: "2".to_owned().into(),
            ..mutant()
        });
    }

    plan
}

/// The ids of the mutants at the given indices, which is the form a hint takes.
fn hinted(plan: &Plan, indices: impl IntoIterator<Item = usize>) -> HashSet<crate::model::MutantId> {
    indices.into_iter().map(|index| plan.mutants[index].id.clone()).collect()
}

/// The mutants' outcomes after a build, keyed by ordinal, which is the population a hint may not move.
fn population(plan: &Plan) -> Vec<(u32, Outcome)> {
    let mut outcomes: Vec<(u32, Outcome)> = plan.mutants.iter().map(|mutant| (mutant.ordinal, mutant.outcome)).collect();

    outcomes.sort_unstable_by_key(|(ordinal, _outcome)| *ordinal);
    outcomes
}

/// A stale hint may reorder the build and nothing else.
///
/// This is the whole safety claim of the tier. The record whose unviability produced these hints is
/// rejected for context — it is never consulted as a filter — so the only way it may be allowed to
/// act is on what the compiler is shown first. Two runs over the same tree, one guided and one not,
/// therefore have to reach the same verdict for every mutant; if the guided one differs anywhere,
/// the hint has moved the population and the tier is unsafe at any speed.
#[test]
fn a_context_mismatch_changes_the_order_and_never_the_population() {
    let (_blind_dir, blind_work) = guarded_workspace("build-probe-blind-");
    let mut blind_plan = probe_plan(&blind_work, 5, 2);

    let blind = Converger::default()
        .finish(
            &blind_work,
            &mut blind_plan,
            None,
            BuildLimits::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("the unguided build converges");

    let (_guided_dir, guided_work) = guarded_workspace("build-probe-guided-");
    let mut guided_plan = probe_plan(&guided_work, 5, 2);

    let guided = Converger::guided(hinted(&guided_plan, 0..5))
        .finish(
            &guided_work,
            &mut guided_plan,
            None,
            BuildLimits::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("the guided build converges");

    assert_eq!(
        population(&blind_plan),
        population(&guided_plan),
        "a hint that changes any verdict is filtering, not ordering"
    );

    assert_eq!(blind.withdrawn, guided.withdrawn, "the same mutants must be withdrawn either way");
    assert_eq!(blind.ordering.rounds, 0, "an unguided build takes no probe round");
    assert_eq!(guided.ordering.offered, 5, "every hinted mutant was front-loaded");
    assert_eq!(guided.ordering.confirmed, 5, "the compiler agreed with every hint");
    assert_eq!(guided.ordering.rounds, 1, "one probe round, taken once");
}

/// A hint that is wrong costs its round and leaves the mutant exactly where it was.
///
/// The dangerous failure is the silent one: a mutant named by a stale record that would compile
/// perfectly well today, quietly withheld and never judged, leaves the denominator and flatters the
/// score. Here every hint is wrong by construction — the hinted mutants are the ones in function
/// bodies, which compile — so nothing may be withdrawn, and the run must settle them normally.
#[test]
fn a_hint_the_compiler_disagrees_with_leaves_its_mutant_live_and_judged() {
    let (_dir, work) = guarded_workspace("build-probe-wrong-");
    let mut plan = probe_plan(&work, 0, 5);

    let build = Converger::guided(hinted(&plan, 0..5))
        .finish(
            &work,
            &mut plan,
            None,
            BuildLimits::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("the build converges");

    assert_eq!(build.ordering.offered, 5, "the hints were taken at face value and probed");
    assert_eq!(build.ordering.confirmed, 0, "no hint was confirmed, because none was right");
    assert_eq!(build.withdrawn, 0, "a wrong hint must never withdraw a mutant");

    for mutant in &plan.mutants {
        assert_eq!(
            mutant.outcome,
            Outcome::Pending,
            "a hinted mutant that compiles has to be left for the sweep to judge: {mutant:?}"
        );
    }
}

/// A hint the compiler agrees with is confirmed by the compiler, not by the hint.
#[test]
fn a_hint_the_compiler_agrees_with_is_settled_by_the_compiler() {
    let (_dir, work) = guarded_workspace("build-probe-right-");
    let mut plan = probe_plan(&work, 5, 1);

    let build = Converger::guided(hinted(&plan, 0..5))
        .finish(
            &work,
            &mut plan,
            None,
            BuildLimits::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("the build converges");

    assert_eq!(build.ordering.confirmed, 5, "every hinted mutant was refused by the compiler");

    for mutant in plan.mutants.iter().take(5) {
        assert_eq!(mutant.outcome, Outcome::CompileError, "{mutant:?}");
    }

    assert_eq!(
        plan.mutants[5].outcome,
        Outcome::Pending,
        "the unhinted mutant compiles and is judged"
    );
}

/// Too few hints do not buy a round, so no round is spent on them.
#[test]
fn a_handful_of_hints_is_not_worth_a_probe_round() {
    let (_dir, work) = guarded_workspace("build-probe-floor-");
    let mut plan = probe_plan(&work, 3, 1);

    let build = Converger::guided(hinted(&plan, 0..3))
        .finish(
            &work,
            &mut plan,
            None,
            BuildLimits::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("the build converges");

    assert_eq!(build.ordering.rounds, 0, "a probe below the floor costs a build and buys nothing");
    assert_eq!(build.ordering.offered, 0, "nothing was front-loaded");
    assert_eq!(build.withdrawn, 3, "the ordinary rounds still find every unviable mutant");
}

/// The probe is taken once per mutant for the whole run, however many builds ask for it.
#[test]
fn no_mutant_is_probed_twice_however_many_builds_run() {
    let (_dir, work) = guarded_workspace("build-probe-once-");
    let mut plan = probe_plan(&work, 5, 1);
    let mut converger = Converger::guided(hinted(&plan, 0..5));

    let staged = converger
        .converge(
            &work,
            &plan,
            None,
            &["build", "--keep-going"],
            BuildLimits::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("the staged build runs");

    assert!(
        matches!(staged, Convergence::Built(_)),
        "the tree converges once the consts are out"
    );

    let build = converger
        .finish(
            &work,
            &mut plan,
            None,
            BuildLimits::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("the deciding build converges");

    assert_eq!(
        build.ordering.rounds, 1,
        "the second build must not re-probe what the first already did"
    );
    assert_eq!(build.ordering.offered, 5, "each hinted mutant is offered exactly once");
}

/// The probe set is derived from the plan, so the same tree and the same hints probe the same thing.
///
/// Iterating the hint set directly would make the order — and so the reported counts and any future
/// tie-break — depend on hash iteration order, which differs between processes.
#[test]
fn the_probe_set_is_ordered_by_the_plan_rather_than_by_the_hint_set() {
    let (_dir, work) = guarded_workspace("build-probe-order-");
    let plan = probe_plan(&work, 5, 2);

    let forwards = Converger::guided(hinted(&plan, 0..5));
    let backwards = Converger::guided(hinted(&plan, (0..5).rev()));

    let (first, deferred) = forwards.probe_sets(&plan, None);
    let (second, also_deferred) = backwards.probe_sets(&plan, None);

    assert_eq!(first, vec![1, 2, 3, 4, 5], "the candidates follow the plan's ordinals");
    assert_eq!(first, second, "the hint set's own order may not reach the build");
    assert_eq!(deferred, also_deferred);

    let held: Vec<u32> = {
        let mut ordinals: Vec<u32> = deferred.into_iter().collect();
        ordinals.sort_unstable();
        ordinals
    };

    assert_eq!(held, vec![6, 7], "only the unhinted mutants are held back from the probe round");
}

/// A probe round only defers mutants in the packages the build actually compiles.
///
/// Deferring a mutant outside the selection buys nothing — it contributes no diagnostic to this
/// build — and costs a rewrite of a file a later stage is about to want instrumented again.
#[test]
fn a_probe_leaves_mutants_outside_the_selection_where_they_are() {
    let (_dir, work) = guarded_workspace("build-probe-selection-");
    let mut plan = probe_plan(&work, 5, 2);

    for mutant in plan.mutants.iter_mut().skip(5) {
        mutant.package = "elsewhere".to_owned().into();
    }

    let converger = Converger::guided(hinted(&plan, 0..5));
    let select = vec![plan.mutants[0].package.to_string()];
    let (candidates, deferred) = converger.probe_sets(&plan, Some(&select));

    assert_eq!(candidates, vec![1, 2, 3, 4, 5]);
    assert!(deferred.is_empty(), "another package's mutants stay spliced: {deferred:?}");
}
