// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::panic::AssertUnwindSafe;
use std::ffi::OsString;
use std::io::Write;
use std::panic;

use clap::Parser;
use clap::error::ErrorKind;

use super::clean::clean;
use super::cli::{Cli, Command, SelectArgs};
use super::completions::completions;
use super::explain::explain;
use super::hints::hints_with_cargo;
use super::host::Host;
use super::list::list_with_cargo;
use super::merge::merge;
use super::run::{configure, run_session};
use super::suppress::suppress;
use super::unsuppress::unsuppress_with_cargo;
use crate::config::Config;
use crate::error::error;
use crate::report::Styler;

/// The multiple of each test binary's baseline duration a mutant is allowed when nothing says otherwise.
///
/// Tight on purpose: a mutant that hangs costs its whole budget, and that budget is paid once per
/// hang across the population, so a generous multiplier is one of the few ways a run can still take
/// far longer than the cost model predicts. The floor keeps a fast suite from reading scheduler
/// noise as a hang, and stall detection catches the common hangs long before the budget expires.
pub(super) const DEFAULT_TEST_TIMEOUT_MULTIPLIER: f64 = 1.5;

/// Exit code for a run in which every gate passed.
pub const EXIT_OK: i32 = 0;

/// Exit code for a usage error: bad arguments or bad configuration.
pub const EXIT_USAGE: i32 = 1;

/// Exit code for a run that completed but in which some gate failed.
pub const EXIT_GATE_FAILED: i32 = 2;

/// Exit code for a run that could not proceed.
pub const EXIT_CANNOT_PROCEED: i32 = 3;

/// Exit code for a bug in this tool: a panic that reached the top of the CLI.
///
/// Distinct from the three above because it is the one code that says nothing about the code under
/// test. A job that reads a `3` retries the build or reports a broken workspace; a job that reads
/// this should file a bug, and leaving a panic to Rust's default `101` puts it in the same bucket
/// as a test binary that aborted, which is a thing the tool is supposed to be able to observe.
///
/// The value follows `sysexits.h`'s `EX_SOFTWARE`.
pub const EXIT_INTERNAL: i32 = 70;

/// Runs the tool and returns the process exit code.
///
/// This returns rather than exits so that every path through the CLI, including the failure paths
/// and the exit codes themselves, is reachable from an ordinary integration test.
pub fn run<H: Host>(host: &mut H, args: impl IntoIterator<Item = impl Into<OsString> + Clone>) -> i32 {
    #[cfg(windows)]
    cargo_gamma_unsafe::job::suppress_error_dialogs();

    let notes = crate::notes::Run::new();
    let _notes = crate::notes::enter(Some(&notes));

    // A panic anywhere below here is a bug in this tool rather than a finding about the code under
    // test, and it is the only outcome the three ordinary codes cannot express. Caught here rather
    // than left to unwind out of `main` so that the distinction the documentation promises actually
    // reaches a caller: without this the process exits `101`, which a CI job cannot tell from a
    // test binary that aborted.
    //
    // The payload is dropped rather than reported. The default hook has already written the
    // message and the location to stderr by the time this runs, and `host` is not usable from
    // inside the unwind — its streams are exactly what a panic here may have left half-written.
    //
    // `AssertUnwindSafe` because `host` is a `&mut` and the compiler cannot know what state a panic
    // leaves it in. Nothing reads it afterwards: this branch returns the code and nothing else.
    match panic::catch_unwind(AssertUnwindSafe(|| dispatched(host, args))) {
        Ok(code) => code,
        Err(_payload) => EXIT_INTERNAL,
    }
}

/// Runs the tool, letting a panic escape.
///
/// Split out of [`run`] only so that the catch has a single call to wrap, which keeps the `?`-less
/// early returns below readable.
fn dispatched<H: Host>(host: &mut H, args: impl IntoIterator<Item = impl Into<OsString> + Clone>) -> i32 {
    let normalized = normalize(args);

    let cli = match Cli::try_parse_from(normalized) {
        Ok(cli) => cli,

        Err(cause) => {
            // clap renders help and version to stdout and errors to stderr, matching cargo.
            let is_help = matches!(cause.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion);

            let text = cause.render().ansi().to_string();

            if is_help {
                let _ = write!(host.output(), "{text}");
                return EXIT_OK;
            }

            let _ = write!(host.error(), "{text}");
            return EXIT_USAGE;
        }
    };

    let styler = Styler::new(cli.color.resolve(host.is_terminal()));

    let code = match dispatch(host, cli, styler) {
        Ok(code) => code,

        Err(cause) => {
            let code = if cause.is_usage() { EXIT_USAGE } else { EXIT_CANNOT_PROCEED };
            let label = styler.error("error:");
            let mut stream = host.error();

            let _ = writeln!(stream, "{label} {cause}");

            code
        }
    };

    say_notes(host, styler);

    code
}

/// Says whatever was raised from below the output seam, through the `Host` like everything else.
///
/// Said after the command rather than as it happens: workers and low-level file publishers can
/// discover a warning while the progress display owns the terminal. Said on the way out of a
/// failed command too, because a warning about incomplete output is often the context that makes
/// the failure legible.
fn say_notes<H: Host>(host: &mut H, styler: Styler) {
    let notes = crate::notes::drain();

    if notes.is_empty() {
        return;
    }

    let label = styler.warning();
    let mut stream = host.error();

    for note in notes {
        let _ = writeln!(stream, "{label} {note}");
    }
}

/// Strips the argument cargo inserts when invoking a subcommand.
///
/// Invoked as `cargo gamma ...`, the process sees `["cargo-gamma", "gamma", ...]`. Invoked
/// directly as `cargo-gamma ...` it does not. Both must work, so drop a second argument that is
/// exactly `gamma` and nothing else.
fn normalize(args: impl IntoIterator<Item = impl Into<OsString> + Clone>) -> Vec<OsString> {
    let mut normalized: Vec<OsString> = args.into_iter().map(Into::into).collect();

    if normalized.get(1).is_some_and(|entry| entry == "gamma") {
        let _ = normalized.remove(1);
    }

    if !normalized.is_empty() && implies_run(normalized.get(1..).unwrap_or_default()) {
        normalized.insert(1, "run".into());
    }

    normalized
}

/// The top-level options that may legitimately appear before a subcommand.
///
/// Each takes one value, which has to be stepped over when looking for the subcommand.
const GLOBAL_OPTIONS: [&str; 2] = ["--color", "--progress"];

/// Whether `args` is a bare `run` invocation with the word `run` left off.
///
/// The rule is deliberately shallow: after stepping over the global options, an argument that
/// begins with a dash cannot be a subcommand, so `run` is what was meant. Anything else is left
/// exactly as written, including a misspelled subcommand — clap's "did you mean" is far more useful
/// there than an unexpected-argument error from a `run` the user never asked for.
fn implies_run(args: &[OsString]) -> bool {
    let mut rest = args;

    while let Some(first) = rest.first().and_then(|entry| entry.to_str()) {
        if GLOBAL_OPTIONS
            .iter()
            .any(|option| first.strip_prefix(option).is_some_and(|rest| rest.starts_with('=')))
        {
            rest = &rest[1..];
        } else if GLOBAL_OPTIONS.contains(&first) {
            // The value is skipped along with the option, or a `--color never merge` would look
            // like it begins with the word `never`.
            rest = rest.get(2..).unwrap_or_default();
        } else {
            break;
        }
    }

    let Some(first) = rest.first().and_then(|entry| entry.to_str()) else {
        // Nothing at all means a default run, which is the shortest path into the tool.
        return true;
    };

    // Help and version are answered by the top-level parser; routing them through `run` would print
    // that subcommand's page instead of the overview the user asked for.
    first.starts_with('-') && !matches!(first, "-h" | "--help" | "-V" | "--version")
}

/// Re-runs this process inside a delegated cgroup when that is what memory control is missing.
///
/// Placed here, ahead of every command that executes tests, because a relaunch has to happen before
/// any work is done: the point is for the new process to do the run, and a process that has already
/// built a scratch tree would either duplicate it or hand over half-finished state.
///
/// Returns the exit code of the relaunched run when there was one, and `None` when the run should
/// continue in this process — which covers both "no relaunch was needed" and "no relaunch was
/// possible". The distinction between those two is deliberately not made here. When it was not
/// needed there is nothing to say, and when it was not possible the run continues to
/// `admit_memory_control`, which already explains the absence of a ceiling and already decides
/// whether that is an error or a degradation based on whether the user asked for one.
#[cfg(target_os = "linux")]
fn relaunch_for_memory_control<H: Host>(host: &H, args: &super::cli::RunArgs) -> Option<i32> {
    use crate::exec::relaunch;

    if !host.may_replace_process() || args.measure.no_relaunch || relaunch::relaunched() {
        return None;
    }

    // Asking the host first means a machine that already delegates a cgroup — the ordinary case on
    // a desktop Linux session — never spawns a second process to discover it did not need one.
    if !super::run::memory_policy(args).measuring() || crate::exec::memory_support().is_ok() {
        return None;
    }

    relaunch::relaunch().ok().flatten()
}

/// Runs the parsed command.
///
/// The configuration file is folded into the arguments here rather than inside each command, so
/// there is exactly one place where precedence between the file and the command line is decided —
/// and exactly one place where the settings that can arrive half from each side are checked.
pub(super) fn dispatch<H: Host>(host: &mut H, cli: Cli, styler: Styler) -> crate::Result<i32> {
    match cli.command {
        Command::Run(mut args) => {
            configure(host, &mut args, styler)?;
            check_shard(&args.select)?;

            #[cfg(target_os = "linux")]
            if let Some(code) = relaunch_for_memory_control(host, &args) {
                return Ok(code);
            }

            run_session(host, &args, cli.progress, styler)
        }

        Command::List(mut args) => {
            let config = Config::resolve(&args.select)?;
            let cargo = config.cargo_options();
            config.apply_selection(&mut args.select)?;
            check_shard(&args.select)?;
            list_with_cargo(host, &args, styler, &cargo)
        }

        Command::Explain(args) => explain(host, &args),
        Command::Suppress(mut args) => {
            configure(host, &mut args.run, styler)?;
            check_shard(&args.run.select)?;

            #[cfg(target_os = "linux")]
            if let Some(code) = relaunch_for_memory_control(host, &args.run) {
                return Ok(code);
            }

            suppress(host, &args, cli.progress, styler)
        }

        Command::Unsuppress(mut args) => {
            let config = Config::resolve(&args.select)?;
            let cargo = config.cargo_options();
            config.apply_selection(&mut args.select)?;
            check_shard(&args.select)?;
            unsuppress_with_cargo(host, &args, styler, &cargo)
        }

        Command::Merge(args) => merge(host, &args, styler),

        Command::Hints(mut args) => {
            // Before the merge, so that a shard count sitting in the committed configuration for
            // the benefit of the matrix does not stop the one job that promotes the artifact.
            refuse_shard(&args.select)?;

            let config = Config::resolve(&args.select)?;
            let cargo = config.cargo_options();
            config.apply_selection(&mut args.select)?;
            hints_with_cargo(host, &args, styler, &cargo)
        }

        Command::Clean(args) => clean(host, &args, styler),

        Command::Completions(args) => Ok(completions(host, &args)),
    }
}

/// Checks the settings that can arrive half from the command line and half from the file.
///
/// The shard count and index are one such setting, and the only one: the count belongs in the
/// committed configuration, because every job in a matrix has to agree on it, while the index
/// differs per job and arrives on the command line. Neither source holds the whole pair, so the
/// check cannot be made while the command line is parsed — which is why the pair is validated by
/// [`SelectArgs::shard`] rather than by clap.
///
/// Made here, immediately after the merge, so that no command that *uses* a shard can reach its work
/// without it. On `run` in particular that is ahead of the relaunch into a cgroup: a shard nobody
/// can honour should cost a second, not a second process and a build. `hints` is the one command
/// that uses no shard at all, and it refuses one outright rather than checking it — see
/// [`refuse_shard`].
fn check_shard(select: &SelectArgs) -> crate::Result<()> {
    let _shard = select.shard()?;

    Ok(())
}

/// Refuses the shard flags on the one command that deliberately ignores them.
///
/// `hints` promotes an artifact from the whole population, because a shard sees a fraction of it and
/// promoting from one would publish an almost-empty artifact while every other job in the matrix
/// overwrote it. That is the right behaviour, but accepting the flags and then ignoring them is not
/// how to have it: a user who adds `hints` to an existing sharded matrix step, where
/// `--shard-index ${{ matrix.i }}` is already on the line, would get exactly the race the design
/// avoids and no diagnostic at all. Refusing says which command they want instead.
fn refuse_shard(select: &SelectArgs) -> crate::Result<()> {
    if select.shard_count.is_none() && select.shard_index.is_none() {
        return Ok(());
    }

    Err(error!(
        "`hints` is deliberately unsharded: it promotes from the whole population, because a shard sees a fraction of it and every job in the matrix would race to overwrite the artifact. Drop `--shard-count` and `--shard-index`, and promote from one job rather than all of them"
    )
    .usage())
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use std::fs;
    use std::sync::Barrier;

    use super::*;
    use crate::testing::Sink as TestSink;

    struct ClosedOutput {
        err: Vec<u8>,
    }

    impl Host for ClosedOutput {
        fn output(&mut self) -> impl Write {
            crate::testing::Broken
        }

        fn error(&mut self) -> impl Write {
            &mut self.err
        }

        fn is_terminal(&self) -> bool {
            false
        }

        fn terminal_width(&self) -> Option<u16> {
            None
        }
    }

    #[test]
    fn a_closed_results_pipe_ends_a_listing_successfully_without_a_diagnostic() {
        crate::notes::alone(|| {
            let dir = crate_dir("dispatch-broken-pipe-", None);
            let root = dir.path().to_string_lossy().into_owned();
            let mut host = ClosedOutput { err: Vec::new() };

            let code = run(&mut host, ["cargo-gamma", "gamma", "list", "mutants", "--dir", &root]);

            assert_eq!(code, EXIT_OK);
            assert!(host.err.is_empty(), "{}", String::from_utf8_lossy(&host.err));
        });
    }

    /// A panic below the top of the CLI is reported as a bug in this tool, not as a finding.
    ///
    /// Regression: an escaping panic exited `101`, which is also what a test binary that aborted
    /// under the tool exits with, so a CI job could not tell "cargo-gamma is broken" from
    /// "the code under test is broken" — the one distinction the exit codes exist to draw.
    #[test]
    fn a_panic_inside_the_tool_is_reported_as_an_internal_error_rather_than_escaping() {
        let previous = panic::take_hook();

        // The default hook would write this deliberate panic's message and backtrace to the
        // suite's stderr, where it reads as a failure. Silenced only for the call below, and put
        // back immediately, because the hook is process-wide.
        panic::set_hook(Box::new(|_| {}));

        let code = run(&mut Exploding, ["cargo-gamma", "gamma", "list", "files"]);

        panic::set_hook(previous);

        assert_eq!(code, EXIT_INTERNAL, "a panic reached the caller as something other than a tool bug");
    }

    /// A host that panics the moment the CLI asks it anything.
    ///
    /// Stands in for any bug below [`run`]: what is being pinned is that the boundary catches, not
    /// that this particular question is dangerous.
    struct Exploding;

    impl Host for Exploding {
        fn output(&mut self) -> impl Write {
            Vec::new()
        }

        fn error(&mut self) -> impl Write {
            Vec::new()
        }

        fn is_terminal(&self) -> bool {
            panic!("a bug in the tool")
        }

        fn terminal_width(&self) -> Option<u16> {
            None
        }
    }

    /// A crate small enough to enumerate mutants in, with whatever configuration the test needs.
    fn crate_dir(name: &str, config: Option<&str>) -> tempfile::TempDir {
        let (dir, root) = crate::fixtures::crate_dir(name, "pub fn less(a: i32, b: i32) -> bool { a < b }\n");

        if let Some(text) = config {
            fs::write(root.join("gamma.toml"), text).expect("config");
        }

        dir
    }

    /// The split a CI matrix wants: the width is committed and the slice is per job. Clap cannot
    /// judge this before the file has been read, so the check runs on the merged values.
    #[test]
    fn a_count_from_the_file_and_an_index_from_the_command_line_run() {
        crate::notes::alone(|| {
            let dir = crate_dir("dispatch-shard-split-", Some("[shard]\ncount = 3\n"));
            let root = dir.path().to_string_lossy().into_owned();
            let mut host = crate::testing::Sink::default();

            let code = run(
                &mut host,
                ["cargo-gamma", "gamma", "list", "mutants", "--dir", &root, "--shard-index", "1"],
            );

            assert_eq!(code, EXIT_OK, "{}", host.err());
        });
    }

    /// And the half pair that nothing completes is refused, rather than quietly running the whole
    /// population under a name that says it ran a third of it.
    #[test]
    fn a_shard_count_with_nothing_to_complete_it_is_a_usage_error() {
        crate::notes::alone(|| {
            let dir = crate_dir("dispatch-shard-half-", Some("[shard]\ncount = 3\n"));
            let root = dir.path().to_string_lossy().into_owned();
            let mut host = crate::testing::Sink::default();

            let code = run(&mut host, ["cargo-gamma", "gamma", "list", "mutants", "--dir", &root]);

            assert_eq!(code, EXIT_USAGE, "{}", host.out());
            assert!(host.err().contains("--shard-index"), "{}", host.err());
            assert!(host.out().is_empty(), "a rejected shard listed mutants anyway: {}", host.out());
        });
    }

    /// The same, typed on the command line: parsing accepts it now, and the effective-value check
    /// is what refuses it.
    #[test]
    fn half_a_shard_on_the_command_line_is_refused_by_the_command_rather_than_the_parser() {
        crate::notes::alone(|| {
            let dir = crate_dir("dispatch-shard-cli-half-", None);
            let root = dir.path().to_string_lossy().into_owned();
            let mut host = crate::testing::Sink::default();

            let code = run(
                &mut host,
                ["cargo-gamma", "gamma", "list", "mutants", "--dir", &root, "--shard-index", "1"],
            );

            assert_eq!(code, EXIT_USAGE, "{}", host.out());
            assert!(host.err().contains("--shard-count"), "{}", host.err());
        });
    }

    /// The command that promotes an artifact from the whole population has no use for a shard, and
    /// accepting the flags anyway would let someone add it to a sharded matrix step and get every
    /// job overwriting the same file — the race the design already refuses to run.
    #[test]
    fn a_sharded_hints_is_refused_rather_than_quietly_promoting_from_everything() {
        crate::notes::alone(|| {
            let dir = crate_dir("dispatch-shard-hints-", None);
            let root = dir.path().to_string_lossy().into_owned();
            let mut host = crate::testing::Sink::default();

            let code = run(
                &mut host,
                [
                    "cargo-gamma",
                    "gamma",
                    "hints",
                    "--dir",
                    &root,
                    "--shard-index",
                    "1",
                    "--shard-count",
                    "4",
                ],
            );

            assert_eq!(code, EXIT_USAGE, "{}", host.out());
            assert!(host.err().contains("deliberately unsharded"), "{}", host.err());
        });
    }

    /// A `gamma.toml` selection key reaches a discovery command.
    ///
    /// `list mutators` narrows to what the file selects, which is only possible if the `List` arm
    /// folds the file's `mutators` in with `apply_selection` before listing. Drop that call and the
    /// default family leaks through, marking `relational.lt_to_gt` enabled again — so this is a
    /// direct, behavioural pin on the arm rather than an inspection of its source.
    #[test]
    fn a_config_selection_key_narrows_a_listing() {
        crate::notes::alone(|| {
            let dir = crate_dir("dispatch-config-list-", Some("mutators = [\"relational.lt_to_le\"]\n"));
            let root = dir.path().to_string_lossy().into_owned();
            let mut host = crate::testing::Sink::default();

            let code = run(&mut host, ["cargo-gamma", "gamma", "list", "mutators", "--json", "--dir", &root]);
            assert_eq!(code, EXIT_OK, "{}", host.err());

            let entries: Vec<serde_json::Value> = serde_json::from_str(&host.out()).expect("the listing is JSON");
            let enabled = |name: &str| {
                entries
                    .iter()
                    .find(|entry| entry["name"] == name)
                    .and_then(|entry| entry["enabled"].as_bool())
            };

            assert_eq!(enabled("relational.lt_to_le"), Some(true), "{}", host.out());
            assert_eq!(enabled("relational.lt_to_gt"), Some(false), "{}", host.out());
        });
    }

    /// Exactly the discovery commands fold `gamma.toml` selection keys in before discovery;
    /// `explain` deliberately does not.
    ///
    /// A `packages` key in the file, together with `--workspace` on the command line, is a
    /// contradiction that only `apply_selection` (through `validate_effective`) catches — clap sees
    /// only the flag. So a command that reaches that diagnostic is one that folded the file's
    /// selection in, and one that runs clean is one that did not. Removing `apply_selection` from
    /// the `list`, `unsuppress`, or `hints` arm, or adding it to `explain`, flips exactly one of
    /// these assertions. It stops at the merge, before any build, so it stays a cheap unit test.
    #[test]
    fn only_the_discovery_commands_apply_config_selection() {
        crate::notes::alone(|| {
            let dir = crate_dir("dispatch-config-selection-", Some("packages = [\"subject\"]\n"));
            let root = dir.path().to_string_lossy().into_owned();

            for command in ["list", "unsuppress", "hints"] {
                let mut host = crate::testing::Sink::default();
                let code = run(&mut host, ["cargo-gamma", "gamma", command, "--workspace", "--dir", &root]);

                assert_eq!(code, EXIT_USAGE, "`{command}` did not fold in the file's selection: {}", host.err());
                assert!(host.err().contains("packages"), "`{command}`: {}", host.err());
                assert!(host.err().contains("workspace"), "`{command}`: {}", host.err());
            }

            // `explain` resolves a named subject, not a selection, so the file never reaches it and
            // the whole `relational` family is still explained.
            let mut host = crate::testing::Sink::default();
            let code = run(&mut host, ["cargo-gamma", "gamma", "explain", "relational"]);

            assert_eq!(code, EXIT_OK, "explain applied config selection: {}", host.err());
            assert!(host.out().contains("relational.lt_to_le"), "{}", host.out());
            assert!(host.out().contains("relational.lt_to_gt"), "{}", host.out());
        });
    }

    /// An unsatisfiable shard has to be caught before the run does anything, which for `run` means
    /// before the build and before this process would relaunch itself into a cgroup.
    #[test]
    fn a_run_with_an_impossible_shard_stops_before_it_builds() {
        crate::notes::alone(|| {
            let dir = crate_dir("dispatch-shard-run-", Some("[shard]\ncount = 2\n"));
            let root = dir.path().to_string_lossy().into_owned();
            let mut host = crate::testing::Sink::default();

            let code = run(&mut host, ["cargo-gamma", "gamma", "run", "--dir", &root, "--shard-index", "9"]);

            assert_eq!(code, EXIT_USAGE, "{}", host.out());
            assert!(host.err().contains("out of range"), "{}", host.err());
        });
    }

    /// The whole point of the shell scripts is that they reach the results stream.
    #[test]
    fn completions_are_dispatched_to_the_results_stream() {
        crate::notes::alone(|| {
            let mut host = crate::testing::Sink::default();

            let code = run(&mut host, ["cargo-gamma", "gamma", "completions", "bash"]);

            assert_eq!(code, EXIT_OK);
            assert!(host.out().contains("cargo-gamma"), "{}", host.out());
        });
    }

    /// A diagnostic raised from below the output seam still comes out through the `Host`.
    ///
    /// This is what the seam buys: the two places that have something to say and no `Host` in reach
    /// — the cargo command line being assembled, and a verdict being reached on a worker thread —
    /// — would otherwise write to `stderr` themselves, bypassing the colour, the width and the
    /// progress display that everything else respects, and staying invisible to a test like this one.
    #[test]
    fn a_note_raised_below_the_seam_is_said_through_the_host() {
        crate::notes::alone(|| {
            let mut host = crate::testing::Sink::default();

            crate::notes::note("something worth saying");

            say_notes(&mut host, Styler::new(false));

            assert!(host.err().contains("something worth saying"), "{}", host.err());
            assert!(host.err().contains("warning"), "{}", host.err());
            assert!(host.out().is_empty(), "a diagnostic reached the results stream: {}", host.out());
        });
    }

    /// Every note is said, not only the first: they describe different events.
    #[test]
    fn every_pending_note_is_said() {
        crate::notes::alone(|| {
            let mut host = crate::testing::Sink::default();

            crate::notes::note("the first");
            crate::notes::note("the second");

            say_notes(&mut host, Styler::new(false));

            assert_eq!(host.err().lines().count(), 2, "{}", host.err());
        });
    }

    /// Saying them is what hands them over, so a second command does not repeat the first's.
    #[test]
    fn a_note_is_said_once_and_not_again_by_the_next_command() {
        crate::notes::alone(|| {
            let mut first = crate::testing::Sink::default();
            let mut second = crate::testing::Sink::default();

            crate::notes::note("said once");

            say_notes(&mut first, Styler::new(false));
            say_notes(&mut second, Styler::new(false));

            assert!(first.err().contains("said once"), "{}", first.err());
            assert!(second.err().is_empty(), "{}", second.err());
        });
    }

    #[test]
    fn concurrent_hosts_receive_only_the_notes_from_their_own_runs() {
        let first = crate::notes::Run::new();
        let second = crate::notes::Run::new();
        let ready = Barrier::new(2);

        std::thread::scope(|scope| {
            let first = first.clone();
            let first_ready = &ready;
            let left = scope.spawn(move || {
                let _notes = crate::notes::enter(Some(&first));
                let mut host = TestSink::default();

                crate::notes::note("first run");
                let _ready = first_ready.wait();
                say_notes(&mut host, Styler::new(false));

                String::from_utf8(host.err).expect("note output is UTF-8")
            });

            let second = second.clone();
            let second_ready = &ready;
            let right = scope.spawn(move || {
                let _notes = crate::notes::enter(Some(&second));
                let mut host = TestSink::default();

                crate::notes::note("second run");
                let _ready = second_ready.wait();
                say_notes(&mut host, Styler::new(false));

                String::from_utf8(host.err).expect("note output is UTF-8")
            });

            let left = left.join().expect("first run");
            let right = right.join().expect("second run");

            assert!(left.contains("first run"), "{left}");
            assert!(!left.contains("second run"), "{left}");
            assert!(right.contains("second run"), "{right}");
            assert!(!right.contains("first run"), "{right}");
        });
    }

    /// A run with nothing to add says nothing, rather than an empty label on an empty line.
    #[test]
    fn a_command_that_raised_no_note_writes_nothing() {
        crate::notes::alone(|| {
            let mut host = crate::testing::Sink::default();

            say_notes(&mut host, Styler::new(false));

            assert!(host.err().is_empty(), "{}", host.err());
        });
    }

    #[test]
    fn cargos_inserted_argument_is_stripped() {
        let normalized = normalize(["cargo-gamma", "gamma", "list"]);

        assert_eq!(normalized, vec!["cargo-gamma", "list"]);
    }

    #[test]
    fn direct_invocation_is_left_alone() {
        let normalized = normalize(["cargo-gamma", "list"]);

        assert_eq!(normalized, vec!["cargo-gamma", "list"]);
    }

    #[test]
    fn only_the_second_argument_named_gamma_is_stripped() {
        let normalized = normalize(["cargo-gamma", "list", "gamma"]);

        assert_eq!(normalized, vec!["cargo-gamma", "list", "gamma"]);
    }

    #[test]
    fn an_empty_argument_list_does_not_panic() {
        assert!(normalize(Vec::<String>::new()).is_empty());
    }

    #[test]
    fn a_bare_invocation_implies_run() {
        assert_eq!(normalize(["cargo-gamma", "gamma"]), vec!["cargo-gamma", "run"]);
    }

    #[test]
    fn a_leading_option_implies_run() {
        // The top level accepts no options of its own beyond the two globals, so an option here can
        // only have been meant for `run`.
        assert_eq!(
            normalize(["cargo-gamma", "gamma", "--mutators", "relational"]),
            vec!["cargo-gamma", "run", "--mutators", "relational"]
        );
    }

    #[test]
    fn a_named_subcommand_is_not_second_guessed() {
        for command in ["run", "list", "explain", "suppress", "merge", "help"] {
            assert_eq!(normalize(["cargo-gamma", "gamma", command]), vec!["cargo-gamma", command]);
        }
    }

    #[test]
    fn a_misspelled_subcommand_is_left_for_clap_to_diagnose() {
        // Wrapping it in `run` would turn "did you mean `merge`?" into an unexpected-value error
        // about a subcommand the user did name.
        assert_eq!(normalize(["cargo-gamma", "gamma", "mrege"]), vec!["cargo-gamma", "mrege"]);
    }

    #[test]
    fn help_and_version_stay_at_the_top_level() {
        for flag in ["-h", "--help", "-V", "--version"] {
            assert_eq!(normalize(["cargo-gamma", "gamma", flag]), vec!["cargo-gamma", flag]);
        }
    }

    #[test]
    fn a_global_option_before_a_subcommand_is_stepped_over() {
        assert_eq!(
            normalize(["cargo-gamma", "gamma", "--color", "never", "merge", "a.json"]),
            vec!["cargo-gamma", "--color", "never", "merge", "a.json"]
        );
        assert_eq!(
            normalize(["cargo-gamma", "gamma", "--progress=never", "merge", "a.json"]),
            vec!["cargo-gamma", "--progress=never", "merge", "a.json"]
        );
    }

    #[test]
    fn a_global_option_before_no_subcommand_still_implies_run() {
        assert_eq!(
            normalize(["cargo-gamma", "gamma", "--color", "never", "--mutators", "stmt"]),
            vec!["cargo-gamma", "run", "--color", "never", "--mutators", "stmt"]
        );
    }

    #[test]
    fn a_dangling_global_option_does_not_panic() {
        assert_eq!(
            normalize(["cargo-gamma", "gamma", "--color"]),
            vec!["cargo-gamma", "run", "--color"]
        );
    }

    #[test]
    fn a_global_option_with_value_before_run_options_still_implies_run() {
        assert_eq!(
            normalize(["cargo-gamma", "gamma", "--progress=never", "--mutators", "relational"]),
            vec!["cargo-gamma", "run", "--progress=never", "--mutators", "relational"]
        );
    }
}
