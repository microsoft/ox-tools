// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Running cargo, and turning its two output streams into something a reader can follow.

use core::fmt::Write as _;
use core::ops::ControlFlow;
use core::time::Duration;
use std::io::{self, Read};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use cargo_gamma_process::{MemoryRequest, ProcessTree, prepare};
use tick::{SimpleClock, Stopwatch};

use super::super::cargo_options::BuildLimits;
use super::super::events::Events;
#[cfg(test)]
use super::super::faults::{self, Fault};
use super::super::rustc_replay::PROGRESS_WIDTH;
use super::super::workspace::Workspace;
use super::messages::cargo_message;
#[cfg(any(test, feature = "internals"))]
use super::native_test_util::SCRIPTED_COMPILER;
use crate::Result;
use crate::discover::Plan;
use crate::error::{Error, error};
use crate::report::encode_controls;

const CARGO_PROGRESS_WHEN: &str = "always";

/// Explains a failed cargo spawn.
///
/// Platforms do not agree on which error a missing working directory produces, so the directory
/// is probed to tell that case apart from a missing program. Naming the program matters because it
/// is not always `cargo` from `PATH`: an inherited `CARGO` wins, and a stale one left over from an
/// earlier shell points at a binary that no longer exists.
pub(super) fn spawn_failure(program: &str, work: &Workspace, cause: io::Error) -> Error {
    let program = encode_controls(program);
    let root = encode_controls(work.root.as_str());

    if !work.root.as_std_path().is_dir() {
        return error!("the scratch tree at `{root}` disappeared while it was being built").caused_by(cause);
    }

    error!(
        "could not run `{program}` in `{root}`. Cargo is taken from the `CARGO` environment variable when it is set, and from `PATH` otherwise"
    )
    .caused_by(cause)
}

/// Runs one cargo build, stopping it if it outstays its budget.
///
/// Returns `None` when the budget ran out, having killed the build and everything it started.
/// Waiting for cargo to finish and complaining afterwards would report a slow build accurately and
/// a hung one never: the whole run rests on this single compile, and there is no test harness
/// behind it to notice that nothing is happening.
///
/// The pipes are drained on their own threads. A build produces megabytes of JSON, and a pipe
/// holds about sixty-four kilobytes, so a caller that waits for exit before reading deadlocks
/// against a compiler blocked writing to it — which would look exactly like the hang this is meant
/// to catch.
pub(super) fn compile(work: &Workspace, args: &[String], budget: Option<Duration>, events: &mut dyn Events) -> Result<Option<Output>> {
    compile_with_replay(work, args, budget, events, false)
}

fn compile_with_replay(
    work: &Workspace,
    args: &[String],
    budget: Option<Duration>,
    events: &mut dyn Events,
    selective: bool,
) -> Result<Option<Output>> {
    let mut command = work.cargo();

    let _command = command
        .env("CARGO_TERM_PROGRESS_WHEN", CARGO_PROGRESS_WHEN)
        .env("CARGO_TERM_PROGRESS_WIDTH", PROGRESS_WIDTH.to_string())
        .args(args)
        .stderr(Stdio::piped())
        .stdout(Stdio::piped());

    work.configure_compiler_capture(&mut command, selective);

    #[cfg(any(test, feature = "internals"))]
    if let Some(output) =
        SCRIPTED_COMPILER.with_borrow_mut(|runner| runner.as_mut().map(|runner| runner(work, args, budget, events, &command)))
    {
        return output;
    }

    supervise(command, work, budget, events)
}

/// Runs an already-configured cargo command under containment, and collects what it said.
///
/// Split from [`compile`] so that a test can supervise a command of its own choosing. The cargo
/// that `compile` builds comes from the environment, and the behaviour worth testing here — what
/// happens to a build's descendants when its budget runs out — needs a program that deliberately
/// leaves one behind, which no real cargo invocation can be asked to do.
///
/// The build is contained for the same reason a test binary is. Cargo is the root of a tree:
/// `rustc`, build scripts, and whatever those start in turn. Killing cargo alone leaves that tree
/// compiling — burning the cores the next attempt needs, writing into the scratch tree the run is
/// about to reuse, and, because every one of them inherited the two pipes below, holding open the
/// write ends that the readers are waiting to see closed. A build killed for hanging would then be
/// followed by a collection that hangs, which is the failure the budget exists to prevent, arriving
/// one step later.
///
/// No accounting is asked for. This is the boundary that can be killed, and nothing here reads a
/// peak: the memory a build uses is `rustc`'s business, and a ceiling on it would fail builds the
/// user never asked to be bounded.
pub(super) fn supervise(command: Command, work: &Workspace, budget: Option<Duration>, events: &mut dyn Events) -> Result<Option<Output>> {
    supervise_with_limits(command, work, budget, events, OUTPUT_LIMITS)
}

/// Runs a build with explicit limits for retained and narrated output.
#[cfg_attr(coverage_nightly, coverage(off))]
pub(super) fn supervise_with_limits(
    command: Command,
    work: &Workspace,
    budget: Option<Duration>,
    events: &mut dyn Events,
    limits: OutputLimits,
) -> Result<Option<Output>> {
    let program = command.get_program().to_string_lossy().into_owned();
    let root = encode_controls(work.root.as_str());

    let prepared = prepare(command, MemoryRequest::default()).map_err(|reason| {
        let raw_reason = reason.to_string();
        let reason = encode_controls(&raw_reason);

        error!("the cargo build in `{root}` could not be contained: {reason}")
    })?;

    let spawned = prepared.spawn().map_err(|failure| {
        let (cause, _prepared) = failure.into_parts();

        spawn_failure(&program, work, cause)
    })?;

    let subtree = match ProcessTree::adopt(spawned) {
        Ok(subtree) => subtree,
        Err(reason) => {
            finish_build(events);
            let raw_reason = reason.to_string();
            let reason = encode_controls(&raw_reason);

            return Err(containment_error(&root, &reason));
        }
    };

    supervise_tree(subtree, work, budget, events, limits)
}

/// Supervises an already-adopted build tree with bounded output.
#[cfg_attr(coverage_nightly, coverage(off))]
fn supervise_tree(
    mut subtree: ProcessTree,
    work: &Workspace,
    budget: Option<Duration>,
    events: &mut dyn Events,
    limits: OutputLimits,
) -> Result<Option<Output>> {
    let root = encode_controls(work.root.as_str());
    let (sender, lines) = mpsc::sync_channel(limits.backlog);

    let stdout = subtree
        .take_stdout()
        .map(|pipe| read_pipe_with_limits(pipe, Stream::Json, &sender, limits));
    let stderr = subtree
        .take_stderr()
        .map(|pipe| read_pipe_with_limits(pipe, Stream::Prose, &sender, limits));

    // Release the supervisor's sender after handing narration to the readers.
    // #[gamma::skip(call.replace_with_default, call_result.default, stmt.delete_call, tag = "equivalent", reason = "Narration uses nonblocking receives and reader joins, not channel disconnection; the call result is unit.")]
    close_narration_channel(sender);

    let outcome = wait_for_build(
        &mut subtree,
        &root,
        budget,
        &SimpleClock::new_system(),
        || {
            // #[gamma::skip(call_result.default, tag = "equivalent", reason = "default_after still narrates every line; this closure discards the returned boolean.")]
            let _narrated = narrate(&lines, events);
        },
        || thread::sleep(BUILD_POLL_INTERVAL),
    );

    // Bounded, and reached on every path out of the loop above: the build and everything it
    // started have been killed by now, so the pipes close and the readers finish at once — unless
    // something escaped the containment, in which case waiting for them without a limit would hang
    // the run on the exact process the kill was supposed to have collected.
    //
    // Released first on every one of those paths, since this wait is the long one and a slot still
    // naming a reaped pid names whatever the kernel hands that id to next.
    debug_assert!(subtree.released(), "the containment is released before the output is drained");

    let grace = Instant::now() + DRAIN_GRACE;
    let (said, printed) = finish_readers(stdout, stderr, &lines, events, grace);

    // #[gamma::skip(call_result.default, reason = "This call returns (); default_after performs the original side effects and substitutes only the identical unit value.", tag = "equivalent")]
    finish_build(events);

    let Some(status) = outcome? else {
        return Ok(None);
    };

    // Not reported as a build that produced no diagnostics, which is what an empty stream reads as
    // to everything downstream: a partial JSON stream loses artifacts, and a run that lost the
    // artifacts of a build reports the tests it could not find as ones that do not exist. Both ways
    // of losing it are refused here — a reader still blocked on a pipe something else holds open,
    // and one that stopped on a read that failed — because the difference is invisible in the bytes.
    let (stdout, stderr) = complete_pipes(&root, said, printed)?;

    if !stdout.within_limits || !stderr.within_limits {
        return Err(output_limit_error(&root, limits));
    }

    Ok(Some(Output {
        status,
        stdout: stdout.text,
        stderr: stderr.text,
    }))
}

pub(super) trait BuildProcess {
    fn observe(&mut self) -> io::Result<Option<ExitStatus>>;
    fn terminate_tree(&mut self) -> io::Result<()>;
}

impl BuildProcess for ProcessTree {
    #[cfg_attr(coverage_nightly, coverage(off))]
    // #[gamma::skip(fn_value.ok, tag = "native-boundary", reason = "This adapter forwards native process observation; replacing it with Ok(None) stalls real process supervision. Deterministic wait-policy tests supply scripted observations instead of spawning processes.")]
    fn observe(&mut self) -> io::Result<Option<ExitStatus>> {
        Self::observe(self)
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn terminate_tree(&mut self) -> io::Result<()> {
        collect(self)
    }
}

pub(super) fn wait_for_build(
    subtree: &mut impl BuildProcess,
    root: &str,
    budget: Option<Duration>,
    clock: &SimpleClock,
    mut narrate: impl FnMut(),
    mut pause: impl FnMut(),
) -> Result<Option<ExitStatus>> {
    let started = Stopwatch::new(clock);
    loop {
        narrate();
        match subtree.observe() {
            Ok(Some(status)) => return Ok(Some(status)),
            Ok(None) => {
                if budget.is_some_and(|budget| started.elapsed() >= budget) {
                    return subtree.terminate_tree().map(|()| None).map_err(|cause| {
                        error!("cargo in `{root}` could not be terminated after its build budget expired").caused_by(cause)
                    });
                }
                pause();
            }
            Err(cause) => {
                return match subtree.terminate_tree() {
                    Ok(()) => Err(wait_error(root, cause)),
                    Err(cleanup) => Err(cleanup_error(root, cleanup)),
                };
            }
        }
    }
}

/// Ends a build that will not be waited out, and reaps it.
///
/// The order is the whole content of this function, which is why it is one. The group is signalled
/// while its leader is still unreaped, so the id being signalled cannot yet have been handed to
/// another spawn; the watch slot is given back next, while the pid it names is still this child's;
/// and only then is the pid freed. Reaping first would put an allocation the run does not control
/// between the two, and a stray `killpg` in a run this wide lands on a test binary and is scored as
/// a kill that never happened.
///
/// The ordinary-exit path uses the same order through [`ProcessTree::observe`], which observes an exit
/// without reaping it before sweeping and finally waiting.
fn collect(subtree: &mut ProcessTree) -> io::Result<()> {
    #[cfg(unix)]
    debug_assert!(
        !subtree.released(),
        "the subtree is signalled while it still holds its leader and its watch slot"
    );

    subtree.terminate().map(|_status| ())
}

/// Turns cargo's two streams into the handful of things a reader needs told while a build runs.
///
/// Compiler diagnostics go to `build_output`, which is the escape valve's channel and is silent
/// unless `--show-build` asked for it. That is deliberate: during an instrumented build a
/// compiler error is the mechanism rather than a fault. The tree was checked before any mutant
/// was applied, so an error here was introduced by one, and the rollback loop is already about
/// to withdraw it and rebuild. Showing it would present the tool's normal operation as a
/// failure — and it reads as one, because these arrive in the first seconds of a run and name
/// files the caller never chose to mutate.
///
/// They are rendered whole rather than condensed, because the only reason to ask for them is to
/// read them: `--show-build` exists to reproduce what a bare cargo invocation would have shown,
/// and cargo puts the snippet and the underlines in `rendered`. Nothing else carries them —
/// under `--message-format=json` the prose stream has only cargo's own summary lines — so
/// without this the escape valve would show a build with no diagnostics in it at all.
///
/// A call consumes at most [`NARRATION_BATCH`] lines. That keeps an output flood from monopolizing
/// the supervising thread and postponing its build deadline; the bounded channel retains the rest
/// until the next pass.
pub(super) fn narrate(lines: &Receiver<(Stream, String)>, events: &mut dyn Events) -> usize {
    let wanted = events.wants_build_output();
    let mut narrated = 0;

    for (stream, line) in lines.try_iter().take(NARRATION_BATCH) {
        narrated += 1;

        match stream {
            Stream::Prose if is_progress(&line) => events.build_progress(line.trim_end()),
            Stream::Prose => events.build_output(line.trim_end()),

            Stream::Json => {
                if !wanted {
                    continue;
                }

                if let Some(rendered) = rendered_diagnostic(&line) {
                    events.build_output(rendered.trim_end());
                }
            }
        }
    }

    narrated
}

/// Whether a line is cargo's progress bar rather than something it wanted to say.
///
/// Matched on the gauge rather than on the word, so that the check does not rest on cargo's choice
/// of verb. A line that is not recognised is treated as prose, which shows it only when asked for;
/// the failure is a bar that does not appear, never a garbled display.
///
/// The styling is stripped first because cargo colors this line when it is told to, which puts an
/// escape sequence in front of the verb and between the verb and the gauge.
pub(super) fn is_progress(line: &str) -> bool {
    let plain = crate::report::unstyled(line);
    let trimmed = plain.trim_start();

    trimmed.starts_with("Building [") || trimmed.starts_with("Compiling [")
}

/// The compiler's own rendering of a diagnostic, for a line of cargo's JSON stream that carries one.
///
/// Returned whole, snippet and underlines and all, because the only caller shows it to someone who
/// asked to see the build. Warnings are included for the same reason: `--show-build` is meant to
/// reproduce what running cargo directly would have printed, and cargo prints them.
pub(super) fn rendered_diagnostic(line: &str) -> Option<String> {
    let message = cargo_message(line)?;

    if message.reason != "compiler-message" {
        return None;
    }

    let rendered = message.message?.rendered?;

    if rendered.trim().is_empty() {
        None
    } else {
        Some(rendered.into_owned())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod final_survivor_tests {
    use super::*;

    #[derive(Default)]
    struct Narration {
        output: Vec<String>,
    }

    impl Events for Narration {
        fn phase(&mut self, _: &str, _: &str) {}
        fn mutant(&mut self, _: &crate::model::Mutant) {}
        fn build_output(&mut self, line: &str) {
            self.output.push(line.to_owned());
        }
    }

    #[test]
    fn nov21_narration_yields_at_the_batch_boundary_without_losing_the_next_line() {
        let (sender, lines) = mpsc::sync_channel(NARRATION_BATCH + 1);
        for index in 0..=NARRATION_BATCH {
            sender.send((Stream::Prose, index.to_string())).unwrap();
        }
        close_narration_channel(sender);
        let mut events = Narration::default();
        assert_eq!(narrate(&lines, &mut events), NARRATION_BATCH);
        assert_eq!(events.output.len(), NARRATION_BATCH);
        assert_eq!(narrate(&lines, &mut events), 1);
        assert_eq!(events.output.last(), Some(&NARRATION_BATCH.to_string()));
        assert_eq!(narrate(&lines, &mut events), 0);
    }

    #[test]
    fn nov21_an_empty_connected_narration_channel_yields_and_accepts_later_lines() {
        let (sender, lines) = mpsc::sync_channel(1);
        let mut events = Narration::default();
        assert_eq!(narrate(&lines, &mut events), 0);
        sender.send((Stream::Prose, "later".into())).unwrap();
        assert_eq!(narrate(&lines, &mut events), 1);
        assert_eq!(events.output, ["later"]);
        assert_eq!(narrate(&lines, &mut events), 0);
        drop(sender);
        assert_eq!(narrate(&lines, &mut events), 0);
    }

    #[test]
    fn pending_reader_deadline_keeps_output_narrated_before_refusing_the_join() {
        let (sender, lines) = mpsc::sync_channel(1);
        sender.send((Stream::Prose, "compiler output before deadline".into())).unwrap();
        let mut events = Narration::default();
        assert!(
            drain_pending_readers(
                &lines,
                &mut events,
                || (false, true),
                || true,
                |_| panic!("an expired deadline must not back off"),
            )
            .is_break()
        );
        assert_eq!(events.output, ["compiler output before deadline"]);
    }

    #[test]
    fn either_pending_reader_requires_narration_before_the_drain_deadline() {
        for finished in [(false, true), (true, false), (false, false)] {
            let (sender, lines) = mpsc::sync_channel(1);
            sender.send((Stream::Prose, "pending compiler output".into())).unwrap();
            let mut events = Narration::default();
            assert!(
                drain_pending_readers(
                    &lines,
                    &mut events,
                    || finished,
                    || true,
                    |_| panic!("an expired drain never joins or pauses a live reader"),
                )
                .is_break()
            );
            assert_eq!(events.output, ["pending compiler output"]);
        }
    }

    #[test]
    fn nov21_pending_readers_apply_the_exact_deadline_and_one_backoff_per_live_poll() {
        for before_deadline in [false, true] {
            let (release, wait) = mpsc::channel::<()>();
            let (ended, done) = mpsc::channel();
            let reader = thread::spawn(move || {
                let _released = wait.recv();
                let _finished = ended.send(());
                Pipe {
                    text: Vec::new(),
                    complete: true,
                    within_limits: true,
                }
            });
            let (_sender, lines) = mpsc::sync_channel(1);
            let mut events = Narration::default();
            let deadline = Instant::now() + BUILD_POLL_INTERVAL;
            let mut polls = 0;
            let mut pauses = Vec::new();
            let (stdout, stderr) = finish_readers_with(
                Some(Ok(reader)),
                None,
                &lines,
                &mut events,
                deadline,
                || {
                    polls += 1;
                    assert!(polls <= 2, "an exact deadline must end draining, not poll indefinitely");
                    if before_deadline && polls == 1 {
                        deadline.checked_sub(BUILD_POLL_INTERVAL).unwrap()
                    } else {
                        deadline
                    }
                },
                |duration| pauses.push(duration),
            );
            drop(release);
            done.recv().unwrap();
            assert!(stdout.is_none());
            assert!(stderr.is_none());
            assert_eq!(polls, if before_deadline { 2 } else { 1 });
            assert_eq!(pauses, if before_deadline { vec![BUILD_POLL_INTERVAL] } else { Vec::new() });
        }
    }

    #[test]
    fn nov21_silent_json_does_not_hide_later_prose_and_finished_readers_drain_every_batch() {
        let (sender, lines) = mpsc::sync_channel(NARRATION_BATCH + 3);
        sender.send((Stream::Json, r#"{"reason":"compiler-message"}"#.into())).unwrap();
        for index in 0..NARRATION_BATCH + 2 {
            sender.send((Stream::Prose, format!("line {index}"))).unwrap();
        }
        close_narration_channel(sender);
        let mut events = Narration::default();
        let (stdout, stderr) = finish_readers(None, None, &lines, &mut events, Instant::now());
        assert_eq!(events.output.len(), NARRATION_BATCH + 2);
        assert_eq!(events.output.first().map(String::as_str), Some("line 0"));
        assert_eq!(events.output.last(), Some(&format!("line {}", NARRATION_BATCH + 1)));
        assert!(stdout.unwrap().complete);
        assert!(stderr.unwrap().complete);
    }

    #[test]
    fn nov21_an_expired_drain_budget_still_applies_when_only_stderr_is_pending() {
        // Keep the reader pending on entry; narration releases it so an incorrect post-narration
        // join cannot hang this deadline regression.
        struct ReleaseReader(Option<mpsc::Sender<()>>);
        impl Events for ReleaseReader {
            fn phase(&mut self, _: &str, _: &str) {}
            fn mutant(&mut self, _: &crate::model::Mutant) {}
            fn build_output(&mut self, _: &str) {
                self.0.take().unwrap().send(()).unwrap();
            }
        }
        let (release, wait) = mpsc::channel();
        let (ended, done) = mpsc::channel();
        let reader = thread::spawn(move || {
            wait.recv().unwrap();
            ended.send(()).unwrap();
            Pipe {
                text: b"stderr".to_vec(),
                complete: true,
                within_limits: true,
            }
        });
        let (sender, lines) = mpsc::sync_channel(1);
        sender.send((Stream::Prose, "release reader".into())).unwrap();
        drop(sender);
        let mut events = ReleaseReader(Some(release));
        let deadline = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        let (stdout, stderr) = finish_readers(None, Some(Ok(reader)), &lines, &mut events, deadline);
        assert!(events.0.is_none(), "the queued reader output must be narrated before expiry");
        assert!(stdout.is_none());
        assert!(stderr.is_none());
        done.recv().unwrap();
    }

    #[test]
    fn missing_diagnostic_fields_are_absent_not_panics() {
        for line in [
            "not JSON",
            r#"{"reason":"compiler-message"}"#,
            r#"{"reason":"compiler-message","message":{}}"#,
            r#"{"reason":"compiler-message","message":{"rendered":" \n\t"}}"#,
        ] {
            assert_eq!(rendered_diagnostic(line), None);
        }
        assert_eq!(
            rendered_diagnostic(r#"{"reason":"compiler-message","message":{"rendered":"error: detail\n"}}"#),
            Some("error: detail\n".into())
        );
    }

    #[test]
    fn an_oversized_narrated_line_invalidates_output_even_when_retained_bytes_fit() {
        let limits = OutputLimits {
            retained: 64,
            line: 3,
            backlog: 8,
        };
        for (text, within_limits, narrated) in [
            (b"abc\n".as_slice(), true, vec!["abc"]),
            (b"abcd\nok\n".as_slice(), false, vec!["ok"]),
            (b"abcd".as_slice(), false, Vec::new()),
        ] {
            let (sender, receiver) = mpsc::sync_channel(8);
            let reader = read_pipe_with_limits(io::Cursor::new(text.to_vec()), Stream::Prose, &sender, limits).unwrap();
            drop(sender);
            let pipe = reader.join().unwrap();
            assert_eq!(pipe.text, text);
            assert!(pipe.complete);
            assert_eq!(pipe.within_limits, within_limits);
            assert_eq!(receiver.try_iter().map(|(_, text)| text).collect::<Vec<_>>(), narrated);
        }
    }
}

/// How often a running build is checked for having finished.
///
/// A build is measured in seconds at best, so a coarse poll costs nothing and keeps an otherwise
/// idle thread from spinning against a compiler that wants the core.
pub(super) const BUILD_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Bounds every allocation a build's two output readers can retain or queue.
#[derive(Clone, Copy, Debug)]
pub(super) struct OutputLimits {
    /// Maximum bytes retained from one stream for parsing after the build.
    pub(super) retained: usize,

    /// Maximum bytes copied into one narrated line.
    pub(super) line: usize,

    /// Maximum narrated lines awaiting the build supervisor.
    pub(super) backlog: usize,
}

/// The normal build-output budget, per stream.
const OUTPUT_LIMITS: OutputLimits = OutputLimits {
    retained: 256 * 1024 * 1024,
    line: 1024 * 1024,
    backlog: 64,
};

/// The number of lines one supervision pass narrates before it re-checks the build.
const NARRATION_BATCH: usize = 128;

/// What one output reader finished with.
#[derive(Debug)]
pub(super) struct Pipe {
    pub(super) text: Vec<u8>,
    pub(super) complete: bool,
    pub(super) within_limits: bool,
}

/// Drains one pipe on its own thread, publishing each line as it arrives.
///
/// Bytes are retained only up to [`OutputLimits::retained`]. Everything downstream needs a whole
/// stream, so crossing that limit marks the build output unusable rather than passing a prefix to
/// artifact discovery. The channel is a second view for narration, and its synchronous capacity
/// supplies backpressure instead of retaining an unbounded queue.
///
/// Lines are split on carriage returns as well as newlines: cargo redraws its progress bar by
/// returning to the start of the line, so a bar that has been redrawn a thousand times is one
/// newline-terminated line and is worth nothing to a reader who only sees it at the end.
///
/// Whether the stream really ended and stayed within its limits travels with the bytes. A read
/// failure or limit crossing leaves a prefix indistinguishable from a quiet build to artifact
/// discovery, so [`supervise`] refuses both rather than parsing a truncated stream.
#[cfg(test)]
pub(super) fn read_pipe<R: Read + Send + 'static>(
    pipe: R,
    stream: Stream,
    sink: &SyncSender<(Stream, String)>,
) -> io::Result<JoinHandle<Pipe>> {
    read_pipe_with_limits(pipe, stream, sink, OUTPUT_LIMITS)
}

/// Drains one pipe with a caller-selected output limit.
pub(super) fn read_pipe_with_limits<R: Read + Send + 'static>(
    mut pipe: R,
    stream: Stream,
    sink: &SyncSender<(Stream, String)>,
    limits: OutputLimits,
) -> io::Result<JoinHandle<Pipe>> {
    let sink = sink.clone();

    #[cfg(test)]
    if faults::fired(Fault::Thread) {
        return Err(io::Error::other("the reader thread a test asked to fail"));
    }

    thread::Builder::new()
        // #[gamma::skip(all, tag = "non-contract", reason = "The private build-reader name is diagnostic metadata and is never read by supervision or output parsing.")]
        .name("cargo-gamma-build-output".to_owned())
        .spawn(move || {
            let mut text = Vec::new();
            // #[gamma::skip(literal.int_increment, reason = "read buffer capacity changes chunk boundaries only; retained bytes and line splitting are explicitly chunk-independent")]
            let mut buffer = [0_u8; 8192];
            let mut line = Vec::new();
            let mut complete = true;
            let mut within_limits = true;
            let mut line_limited = false;

            loop {
                let read = match pipe.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => read,
                    Err(cause) => {
                        // Retried rather than given up on, since an interrupted read has not lost
                        // anything: nothing was taken out of the pipe, and the next read gets it.
                        if cause.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }

                        // #[gamma::skip(assign_value.default, reason = "`complete` is bool, whose `Default::default()` is exactly false")]
                        complete = false;

                        break;
                    }
                };

                let room = limits.retained.saturating_sub(text.len());
                let kept = read.min(room);

                text.extend_from_slice(&buffer[..kept]);

                if kept < read {
                    // #[gamma::skip(assign_value.default, reason = "`within_limits` is bool, whose `Default::default()` is exactly false")]
                    within_limits = false;
                }

                for byte in &buffer[..read] {
                    if matches!(*byte, b'\n' | b'\r') {
                        if !line_limited
                            && !line.is_empty()
                            && let Ok(line) = str::from_utf8(&line)
                        {
                            // The bounded synchronous channel makes a fast compiler wait for the
                            // supervisor to narrate output, rather than retaining an unbounded queue.
                            let _sent = sink.send((stream, line.to_owned()));
                        }

                        line.clear();
                        line_limited =
                            // #[gamma::skip(assign_value.default, tag = "equivalent", reason = "The default value of bool is exactly false.")]
                            false;

                        continue;
                    }

                    if line.len() < limits.line {
                        line.push(*byte);
                    } else {
                        // A line's owned copy is part of the queue bound. Refuse a result built from
                        // output that cannot be held within that bound, while continuing to drain.
                        line_limited = true;
                        // #[gamma::skip(assign_value.default, reason = "`within_limits` is bool, whose `Default::default()` is exactly false")]
                        within_limits = false;
                    }
                }
            }

            if !line_limited
                && !line.is_empty()
                && let Ok(line) = str::from_utf8(&line)
            {
                let _ = sink.send((stream, line.to_owned()));
            }

            Pipe {
                text,
                complete,
                within_limits,
            }
        })
}

/// Which of cargo's two streams a line arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stream {
    /// The JSON stream, carrying compiler messages and artifacts.
    Json,

    /// Cargo's own prose: what it is compiling, and how far along it is.
    Prose,
}

// #[gamma::skip(fn_value.continue_default, tag = "timeout", reason = "Returning before draining the bounded output channel leaves reader threads blocked in send while their caller joins them.")]
fn drain_pending_readers(
    lines: &Receiver<(Stream, String)>,
    events: &mut dyn Events,
    mut finished: impl FnMut() -> (bool, bool),
    mut expired: impl FnMut() -> bool,
    mut pause: impl FnMut(Duration),
) -> ControlFlow<()> {
    while !matches!(finished(), (true, true)) {
        let _narrated =
            // #[gamma::skip(call_result.default, tag = "equivalent", reason = "This loop ignores the narration count; default_after preserves the narration side effects.")]
            narrate(lines, events);
        if expired() {
            return ControlFlow::Break(());
        }
        // #[gamma::skip(stmt.delete_call, tag = "timeout", reason = "Removing the polling pause busy-spins while output readers are pending and exhausts the campaign's test budget.")]
        pause(BUILD_POLL_INTERVAL);
    }
    ControlFlow::Continue(())
}

/// Narrates while waiting for bounded output readers to finish.
///
/// A reader can legitimately be blocked on the synchronous channel when a compiler is noisy.
/// Draining that channel here is therefore part of joining it: waiting for the thread without
/// narrating would deadlock at a full, deliberately bounded backlog.
#[cfg_attr(coverage_nightly, coverage(off))]
pub(super) fn finish_readers(
    stdout: Option<io::Result<JoinHandle<Pipe>>>,
    stderr: Option<io::Result<JoinHandle<Pipe>>>,
    lines: &Receiver<(Stream, String)>,
    events: &mut dyn Events,
    deadline: Instant,
) -> (Option<Pipe>, Option<Pipe>) {
    finish_readers_with(stdout, stderr, lines, events, deadline, Instant::now, thread::sleep)
}

pub(super) fn finish_readers_with(
    stdout: Option<io::Result<JoinHandle<Pipe>>>,
    stderr: Option<io::Result<JoinHandle<Pipe>>>,
    lines: &Receiver<(Stream, String)>,
    events: &mut dyn Events,
    deadline: Instant,
    mut now: impl FnMut() -> Instant,
    mut pause: impl FnMut(Duration),
) -> (Option<Pipe>, Option<Pipe>) {
    let finished = |reader: &Option<io::Result<JoinHandle<Pipe>>>| {
        reader
            .as_ref()
            // #[gamma::skip(result.is_err_to_is_ok, tag = "timeout", reason = "the campaign stalled pipe supervision when failed reader creation was no longer treated as finished")]
            .is_none_or(|reader| reader.as_ref().is_err() || reader.as_ref().is_ok_and(JoinHandle::is_finished))
    };

    if drain_pending_readers(
        lines,
        events,
        || (finished(&stdout), finished(&stderr)),
        || now() >= deadline,
        &mut pause,
    )
    .is_break()
    {
        return
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "The default of this pair of Options is exactly (None, None).")]
            (None, None);
    }

    while narrate(lines, events) == NARRATION_BATCH {}

    let stdout = stdout.map(|handle| {
        let handle = handle.ok()?;
        handle.join().ok()
    });
    let stderr = stderr.map(|handle| {
        let handle = handle.ok()?;
        handle.join().ok()
    });

    (
        stdout.unwrap_or_else(|| {
            Some(Pipe {
                text: Vec::new(),
                complete: true,
                within_limits: true,
            })
        }),
        stderr.unwrap_or_else(|| {
            Some(Pipe {
                text: Vec::new(),
                complete: true,
                within_limits: true,
            })
        }),
    )
}

/// How long the readers are given once the build and everything it started have been killed.
///
/// Long enough that a machine under load still closes its pipes in time, and short enough that a
/// descendant which escaped the containment costs the run seconds rather than the whole build.
const DRAIN_GRACE: Duration = Duration::from_secs(5);

/// What one cargo invocation produced.
#[derive(Debug)]
pub(super) struct Compiled {
    pub(super) succeeded: bool,

    /// Actual termination status and Cargo arguments, for failures without compiler errors.
    pub(super) failure_context: String,

    /// Cargo's JSON stream, or `None` if the build was stopped for outstaying its budget.
    pub(super) stdout: Option<String>,

    /// What cargo said on stderr.
    ///
    /// Kept because the JSON stream only carries what the *compiler* said, and a build can fail
    /// without the compiler ever being reached: a build script that panics, a missing native
    /// library, an unresolvable dependency, an ambiguous package. Those failures are explained on
    /// stderr and nowhere else, and discarding it left the reader with a build that failed for no
    /// stated reason.
    pub(super) stderr: String,
}

/// Runs one cargo command in the tree under the build budget.
#[cfg_attr(coverage_nightly, coverage(off))]
pub(super) fn run_cargo(
    work: &Workspace,
    plan: &Plan,
    verb: &[&str],
    select: Option<&[String]>,
    limits: BuildLimits,
    first_round: Option<Duration>,
    events: &mut dyn Events,
) -> Result<Compiled> {
    run_cargo_with_replay::<false>(work, plan, verb, select, limits, first_round, events)
}

/// Keeps Cargo's complete invocation and scheduler, reusing only closed successful rustc units.
///
/// Uncaptured, changed, failed, and unsupported units execute normally under the same containment
/// and budget as a broad build. Schema differences are discovered from actual dep-info inputs on
/// every invocation, not merely from the most recently written subset.
pub(super) fn run_cargo_selective(
    work: &Workspace,
    plan: &Plan,
    verb: &[&str],
    select: Option<&[String]>,
    limits: BuildLimits,
    first_round: Option<Duration>,
    events: &mut dyn Events,
) -> Result<Compiled> {
    run_cargo_with_replay::<true>(work, plan, verb, select, limits, first_round, events)
}

fn run_cargo_with_replay<const SELECTIVE: bool>(
    work: &Workspace,
    plan: &Plan,
    verb: &[&str],
    select: Option<&[String]>,
    limits: BuildLimits,
    first_round: Option<Duration>,
    events: &mut dyn Events,
) -> Result<Compiled> {
    if SELECTIVE && verb.first() != Some(&"check") {
        super::super::proof_context::invalidate(work);
    }
    let mut args: Vec<String> = verb.iter().map(|arg| (*arg).to_owned()).collect();

    args.push("--message-format=json".to_owned());

    match select {
        Some(packages) => {
            for package in packages {
                args.push("--package".to_owned());
                args.push(plan.spec(&work.root, package));
            }
        }
        None => args.push("--workspace".to_owned()),
    }

    extend_build_arguments(work, &mut args);

    let output = if SELECTIVE {
        compile_with_replay(work, &args, limits.budget(first_round), events, true)?
    } else {
        compile(work, &args, limits.budget(first_round), events)?
    };
    let Some(output) = output else {
        return Ok(Compiled {
            succeeded: false,
            failure_context: String::new(),
            stdout: None,
            stderr: String::new(),
        });
    };

    // Cargo's JSON stream can run to many megabytes, and it is valid UTF-8 in every case that
    // matters, so the bytes are taken over rather than copied.
    let stdout = match String::from_utf8(output.stdout) {
        Ok(text) => text,
        Err(invalid) => String::from_utf8_lossy(invalid.as_bytes()).into_owned(),
    };

    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if SELECTIVE && output.status.success() {
        super::super::proof_context::seal(work, plan, &args, &stdout);
    }
    let mut failure_context = format!("Cargo finished with {}", output.status);
    #[cfg(windows)]
    if let Some(code) = output.status.code() {
        let _ = write!(failure_context, " (0x{:08X})", code.cast_unsigned());
    }
    let _ = write!(failure_context, ".\n\nCargo Arguments\n{args:?}");

    Ok(Compiled {
        succeeded: output.status.success(),
        failure_context,
        stdout: Some(stdout),
        stderr,
    })
}

fn finish_build(events: &mut dyn Events) {
    events.build_finished();
}

// #[gamma::skip(fn_value.unit, tag = "equivalent", reason = "The owned sender is dropped on function return even without the explicit drop statement.")]
fn close_narration_channel(sender: SyncSender<(Stream, String)>) {
    drop(sender);
}

fn containment_error(root: &str, reason: &str) -> Error {
    error!("the cargo build in `{root}` could not be contained: {reason}")
}

fn wait_error(root: &str, cause: io::Error) -> Error {
    error!("could not wait for cargo in `{root}`").caused_by(cause)
}

fn cleanup_error(root: &str, cause: io::Error) -> Error {
    error!("cargo in `{root}` could not be terminated after it stopped being observable").caused_by(cause)
}

fn incomplete_output_error(root: &str) -> Error {
    error!("cargo in `{root}` finished, but its output could not be read to the end, so what it built could not be read")
}

fn output_limit_error(root: &str, limits: OutputLimits) -> Error {
    error!(
        "cargo in `{root}` exceeded the configured {}-byte retained or {}-byte per-line build-output limit, so its truncated output could not be trusted",
        limits.retained, limits.line
    )
}

fn complete_pipes(root: &str, stdout: Option<Pipe>, stderr: Option<Pipe>) -> Result<(Pipe, Pipe)> {
    let (Some(stdout), Some(stderr)) = (stdout, stderr) else {
        return Err(incomplete_output_error(root));
    };
    if !(stdout.complete && stderr.complete) {
        return Err(incomplete_output_error(root));
    }
    Ok((stdout, stderr))
}

fn extend_build_arguments(work: &Workspace, args: &mut Vec<String>) {
    work.cargo.extend_build_args(args);
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod mutation_tests {
    use std::collections::VecDeque;
    use std::ffi::OsString;
    use std::io::Cursor;

    use cargo_gamma_process::faults::{self as process_faults, Fault as ProcessFault};

    use super::*;
    use crate::model::Mutant;

    #[test]
    fn compiler_command_seam_observes_capture_configuration_for_broad_and_selective_builds() {
        use crate::exec::rustc_replay::snapshot_io::{self, Script};
        use crate::exec::rustc_replay::{COMMAND_VAR, REPLAY_VAR, ROOT_VAR, TARGET_VAR};

        let root = camino::Utf8PathBuf::from(if cfg!(windows) {
            r"C:\scripted-command"
        } else {
            "/scripted-command"
        });
        let target = root.join("target");
        let captures = target.join("captures");
        for ready in [false, true] {
            let mut work = Workspace::adopt(root.clone(), target.clone());
            work.set_rustc_captures(ready.then(|| captures.clone()));
            let plan = Plan {
                root: root.clone(),
                files: Vec::new(),
                mutants: Vec::new(),
                suppressed: 0,
                idle: Vec::new(),
                sharded_out: 0,
                settled_out: 0,
                digests: crate::HashMap::default(),
                skipped: Vec::new(),
                reach: crate::HashMap::default(),
                specs: crate::HashMap::default(),
            };
            let script = Script::default();
            snapshot_io::with(script, || {
                for selective in [false, true] {
                    let expected_root = root.clone();
                    let expected_target = target.clone();
                    let expected_captures = captures.clone();
                    let calls = std::rc::Rc::new(std::cell::Cell::new(0));
                    let observed = std::rc::Rc::clone(&calls);
                    super::super::native_test_util::with_compiler_command(
                        move |_, args, _, _, command| {
                            observed.set(observed.get() + 1);
                            assert_eq!(
                                command.get_args().map(OsString::from).collect::<Vec<_>>(),
                                args.iter().map(OsString::from).collect::<Vec<_>>()
                            );
                            let environment = command.get_envs().collect::<std::collections::BTreeMap<_, _>>();
                            assert_eq!(
                                environment.get(std::ffi::OsStr::new("CARGO_TERM_PROGRESS_WHEN")).copied().flatten(),
                                Some(std::ffi::OsStr::new("always"))
                            );
                            let identity = environment[std::ffi::OsStr::new(COMMAND_VAR)];
                            assert_eq!(identity.is_some(), ready);
                            assert_eq!(environment[std::ffi::OsStr::new(REPLAY_VAR)].is_some(), ready && selective);
                            assert!(
                                environment.get(std::ffi::OsStr::new("RUSTC_WRAPPER")).copied().flatten().is_some(),
                                "the lint-capping wrapper is independent of capture readiness"
                            );
                            if ready {
                                assert_eq!(
                                    environment[std::ffi::OsStr::new("RUSTC_WRAPPER")],
                                    Some(expected_root.join("cargo-gamma.exe").as_os_str())
                                );
                                assert_eq!(environment[std::ffi::OsStr::new(ROOT_VAR)], Some(expected_root.as_os_str()));
                                assert_eq!(environment[std::ffi::OsStr::new(TARGET_VAR)], Some(expected_target.as_os_str()));
                            }
                            if ready && selective {
                                assert_eq!(environment[std::ffi::OsStr::new(REPLAY_VAR)], identity);
                                let _directory =
                                    snapshot_io::read_dir(expected_captures.join("replay").join(identity.unwrap().to_str().unwrap()))
                                        .unwrap();
                            }
                            Ok(None)
                        },
                        || {
                            let result = if selective {
                                run_cargo_selective(
                                    &work,
                                    &plan,
                                    &["check"],
                                    None,
                                    BuildLimits::default(),
                                    None,
                                    &mut Recorded::default(),
                                )
                            } else {
                                run_cargo(
                                    &work,
                                    &plan,
                                    &["check"],
                                    None,
                                    BuildLimits::default(),
                                    None,
                                    &mut Recorded::default(),
                                )
                            }
                            .unwrap();
                            assert!(!result.succeeded);
                            assert_eq!(result.stdout, None);
                        },
                    );
                    assert_eq!(calls.get(), 1);
                }
            });
        }
    }

    #[test]
    fn selective_build_preserves_the_full_cargo_invocation_budget_streams_and_failure_semantics() {
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt as _;
        #[cfg(windows)]
        use std::os::windows::process::ExitStatusExt as _;

        for (result, operation) in ["success", "failure", "timeout"]
            .into_iter()
            .flat_map(|result| ["check", "build"].map(|operation| (result, operation)))
        {
            let root = camino::Utf8PathBuf::from("owned-scratch");
            let mut work = Workspace::adopt(root.clone(), root.join("target"));
            work.cargo.features = vec!["--all-features".into()];
            work.cargo.profile = Some("custom-profile".into());
            work.cargo.extra = vec!["--target".into(), "explicit-target".into()];
            let plan = Plan {
                root,
                files: Vec::new(),
                mutants: Vec::new(),
                suppressed: 0,
                idle: Vec::new(),
                sharded_out: 0,
                settled_out: 0,
                digests: crate::HashMap::default(),
                skipped: Vec::new(),
                reach: crate::HashMap::default(),
                specs: crate::HashMap::default(),
            };
            let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let observed = std::rc::Rc::clone(&calls);
            let context = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let observed_context = std::rc::Rc::clone(&context);
            super::super::native_test_util::with_compiler(
                move |_work, args, budget, _events| {
                    observed.borrow_mut().push((args.to_vec(), budget));
                    if result == "timeout" {
                        return Ok(None);
                    }
                    Ok(Some(Output {
                        status: ExitStatus::from_raw(if result == "success" { 0 } else { 256 }),
                        stdout: b"original Cargo JSON".to_vec(),
                        stderr: b"original Cargo error".to_vec(),
                    }))
                },
                || {
                    super::super::super::proof_context::with_operations(
                        move |action| {
                            use super::super::super::proof_context::Operation;
                            observed_context.borrow_mut().push(match action {
                                Operation::Seal(args, stdout) => ("seal", args.to_vec(), stdout.to_owned()),
                                Operation::Invalidate => ("invalidate", Vec::new(), String::new()),
                                Operation::Identity(_) => panic!("build orchestration does not query the proof identity"),
                            });
                            None
                        },
                        || {
                            let limits = BuildLimits {
                                multiplier: Some(6.0),
                                ..BuildLimits::default()
                            };
                            let first_round = Some(Duration::from_secs(10));
                            let roots = ["original-root".into()];
                            let verb = [operation, "--all-targets", "--keep-going"];
                            let broad =
                                run_cargo(&work, &plan, &verb, Some(&roots), limits, first_round, &mut Recorded::default()).unwrap();
                            let selective =
                                run_cargo_selective(&work, &plan, &verb, Some(&roots), limits, first_round, &mut Recorded::default())
                                    .unwrap();
                            assert_eq!(selective.succeeded, broad.succeeded);
                            assert_eq!(selective.stdout, broad.stdout);
                            assert_eq!(selective.stderr, broad.stderr);
                            assert_eq!(selective.failure_context, broad.failure_context);
                        },
                    );
                },
            );
            let calls = calls.borrow();
            assert_eq!(calls.len(), 2);
            assert_eq!(
                calls[0], calls[1],
                "replay changes neither roots, verb, target, features, profile, nor budget"
            );
            assert_eq!(calls[0].1, Some(Duration::from_mins(1)));
            let context = context.borrow();
            let expected = match (operation, result) {
                ("check", "success") => vec!["seal"],
                ("check", _) => Vec::new(),
                (_, "success") => vec!["invalidate", "seal"],
                _ => vec!["invalidate"],
            };
            assert_eq!(context.iter().map(|entry| entry.0).collect::<Vec<_>>(), expected);
            if let Some((_, arguments, stdout)) = context.iter().find(|entry| entry.0 == "seal") {
                assert_eq!(arguments, &calls[1].0, "the seal uses the actual complete Cargo invocation");
                assert_eq!(stdout, "original Cargo JSON");
            }
        }
    }

    #[derive(Default)]
    struct Recorded {
        wanted: bool,
        progress: Vec<String>,
        output: Vec<String>,
        finished: usize,
    }

    impl Events for Recorded {
        #[cfg_attr(coverage_nightly, coverage(off))]
        fn phase(&mut self, _verb: &str, _detail: &str) {}
        fn build_progress(&mut self, line: &str) {
            self.progress.push(line.to_owned());
        }
        fn build_output(&mut self, line: &str) {
            self.output.push(line.to_owned());
        }
        fn wants_build_output(&self) -> bool {
            self.wanted
        }
        fn build_finished(&mut self) {
            self.finished += 1;
        }
        #[cfg_attr(coverage_nightly, coverage(off))]
        fn mutant(&mut self, _mutant: &Mutant) {}
    }

    #[test]
    fn independently_refused_readers_are_finished_failures_not_live_threads_or_empty_successes() {
        for stderr_refused in [false, true] {
            let (_sender, receiver) = mpsc::sync_channel(1);
            let failure = Some(Err(io::Error::other("selected reader creation refused")));
            let (stdout, stderr) = if stderr_refused { (None, failure) } else { (failure, None) };
            let (stdout, stderr) = finish_readers(stdout, stderr, &receiver, &mut Recorded::default(), Instant::now());
            let (refused, absent) = if stderr_refused { (stderr, stdout) } else { (stdout, stderr) };
            assert!(refused.is_none(), "a creation failure cannot authorize an empty successful pipe");
            let absent = absent.expect("an absent reader remains a complete empty stream, even at an expired deadline");
            assert_eq!(absent.text, b"");
            assert!(absent.complete);
            assert!(absent.within_limits);
        }
    }

    #[test]
    fn native_supervision_rejects_a_refused_stdout_reader_after_finishing_the_build() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = camino::Utf8Path::from_path(directory.path()).unwrap().to_owned();
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let mut command = Command::new(crate::testing::helper_binary_path().as_std_path());
        let _command = command
            .arg(crate::testing::directive("print:short"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut events = Recorded::default();
        let _refused = faults::arm(Fault::Thread);
        let failure = supervise(command, &work, Some(Duration::from_secs(30)), &mut events)
            .expect_err("a failed native stdout reader must escape the supervisor rather than panic at complete_pipes");
        assert_eq!(
            failure.message(),
            format!(
                "cargo in `{}` finished, but its output could not be read to the end, so what it built could not be read",
                encode_controls(root.as_str())
            )
        );
        assert_eq!(events.finished, 1);
    }

    #[test]
    fn native_supervision_propagates_prepare_and_postspawn_adoption_refusals_with_their_exact_completion_counts() {
        for fault in [ProcessFault::Prepare, ProcessFault::Adopt] {
            let directory = tempfile::tempdir_in(".").unwrap();
            let root = camino::Utf8Path::from_path(directory.path()).unwrap().to_owned();
            let work = Workspace::adopt(root.clone(), root.join("target"));
            let mut command = Command::new(crate::testing::helper_binary_path());
            let _ = command
                .arg(crate::testing::directive("print:never authoritative"))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut events = Recorded::default();
            let _refused = process_faults::arm(fault);
            let failure = supervise(command, &work, Some(Duration::from_secs(30)), &mut events)
                .expect_err("native containment refusal is not successful output or an unwrap precondition");
            assert!(failure.message().starts_with(&format!(
                "the cargo build in `{}` could not be contained:",
                encode_controls(root.as_str())
            )));
            assert_eq!(events.finished, usize::from(matches!(fault, ProcessFault::Adopt)));
            assert_eq!(events.progress.len(), 0);
            assert_eq!(events.output.len(), 0);
        }
    }

    #[test]
    fn narration_drains_a_bounded_queue_while_a_real_build_is_still_producing_output() {
        const CHILD: &str = "GAMMA_EXEC_BUILD_NARRATION_BACKPRESSURE";
        let tail = "x".repeat(512);
        if std::env::var_os(CHILD).is_some() {
            for index in 0..256 {
                eprintln!("   Compiling [{index}] {tail}");
            }
            return;
        }
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = camino::Utf8Path::from_path(directory.path()).unwrap().to_owned();
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let module = module_path!().split_once("::").unwrap().1;
        let mut command = Command::new(std::env::current_exe().unwrap());
        let _ = command
            .args([
                "--exact",
                &format!("{module}::narration_drains_a_bounded_queue_while_a_real_build_is_still_producing_output"),
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut events = Recorded::default();
        let output = supervise_with_limits(
            command,
            &work,
            Some(Duration::from_secs(30)),
            &mut events,
            OutputLimits {
                retained: 256 * 1024,
                line: 1024,
                backlog: 8,
            },
        )
        .unwrap()
        .expect("continuous bounded narration lets the contained writer finish without expiring its build budget");
        assert!(output.status.success());
        assert_eq!(events.finished, 1);
        assert_eq!(
            events.progress,
            (0..256).map(|index| format!("   Compiling [{index}] {tail}")).collect::<Vec<_>>()
        );
    }

    #[test]
    fn stderr_alone_can_refuse_a_complete_build_even_when_its_stdout_is_within_the_retained_cap() {
        const CHILD: &str = "GAMMA_EXEC_BUILD_STDERR_CAP";
        if std::env::var_os(CHILD).is_some() {
            eprint!("{}", "stderr overflow bytes".repeat(8));
            std::process::exit(0);
        }
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = camino::Utf8Path::from_path(directory.path()).unwrap().to_owned();
        let work = Workspace::adopt(root.clone(), root.join("target"));
        for pipe_stderr in [false, true] {
            let module = module_path!().split_once("::").unwrap().1;
            let mut command = Command::new(std::env::current_exe().unwrap());
            let _ = command
                .args([
                    "--quiet",
                    "--exact",
                    &format!("{module}::stderr_alone_can_refuse_a_complete_build_even_when_its_stdout_is_within_the_retained_cap"),
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .stdout(Stdio::piped())
                .stderr(if pipe_stderr { Stdio::piped() } else { Stdio::null() });
            let mut events = Recorded::default();
            let output = supervise_with_limits(
                command,
                &work,
                Some(Duration::from_secs(30)),
                &mut events,
                OutputLimits {
                    retained: 64,
                    line: 1024,
                    backlog: 8,
                },
            );
            if pipe_stderr {
                assert_eq!(
                    output.unwrap_err().message(),
                    output_limit_error(
                        &encode_controls(root.as_str()),
                        OutputLimits {
                            retained: 64,
                            line: 1024,
                            backlog: 8
                        }
                    )
                    .message()
                );
            } else {
                let output = output.unwrap().unwrap();
                assert!(output.status.success());
                assert!(output.stdout.len() <= 64);
                assert_eq!(output.stderr.len(), 0);
            }
            assert_eq!(events.finished, 1);
        }
    }

    #[test]
    fn supervision_propagates_a_native_collection_failure_after_releasing_the_build_and_its_completion_event() {
        const CHILD: &str = "GAMMA_EXEC_BUILD_COLLECTION_FAILURE";
        if std::env::var_os(CHILD).is_some() {
            let (_sender, receiver) = mpsc::channel::<()>();
            let _bounded = receiver.recv_timeout(Duration::from_secs(30));
            return;
        }
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = camino::Utf8Path::from_path(directory.path()).unwrap().to_owned();
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let module = module_path!().split_once("::").unwrap().1;
        let mut command = Command::new(std::env::current_exe().unwrap());
        let _ = command
            .args([
                "--exact",
                &format!("{module}::supervision_propagates_a_native_collection_failure_after_releasing_the_build_and_its_completion_event"),
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut events = Recorded::default();
        let _refused = process_faults::arm(ProcessFault::Terminate);
        let failure = supervise(command, &work, Some(Duration::ZERO), &mut events)
            .expect_err("a budget-expiry collection error must propagate through the saved supervisor outcome");
        assert_eq!(
            failure.message(),
            format!(
                "cargo in `{}` could not be terminated after its build budget expired",
                encode_controls(root.as_str())
            )
        );
        assert_eq!(events.finished, 1);
    }

    #[test]
    fn narration_counts_routes_trims_and_honors_the_output_switch() {
        let (sender, receiver) = mpsc::sync_channel(8);
        for item in [
            (Stream::Prose, " Compiling [=> ] 1/2\r\n".to_owned()),
            (Stream::Prose, "ordinary prose  \n".to_owned()),
            (
                Stream::Json,
                r#"{"reason":"compiler-message","message":{"rendered":"diagnostic  \n"}}"#.to_owned(),
            ),
            (Stream::Json, r#"{"reason":"compiler-artifact"}"#.to_owned()),
        ] {
            sender.send(item).unwrap();
        }
        let mut events = Recorded {
            wanted: true,
            ..Recorded::default()
        };

        assert_eq!(narrate(&receiver, &mut events), 4);
        assert_eq!(events.progress, [" Compiling [=> ] 1/2"]);
        assert_eq!(events.output, ["ordinary prose", "diagnostic"]);
        assert_eq!(narrate(&receiver, &mut events), 0);

        sender
            .send((
                Stream::Json,
                r#"{"reason":"compiler-message","message":{"rendered":"hidden"}}"#.to_owned(),
            ))
            .unwrap();
        events.wanted = false;
        assert_eq!(narrate(&receiver, &mut events), 1);
        assert_eq!(events.output, ["ordinary prose", "diagnostic"]);
    }

    struct Scheduled {
        chunks: VecDeque<io::Result<Vec<u8>>>,
    }

    impl Read for Scheduled {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let Some(chunk) = self.chunks.pop_front() else {
                return Ok(0);
            };
            let chunk = chunk?;
            buf[..chunk.len()].copy_from_slice(&chunk);
            Ok(chunk.len())
        }
    }

    #[test]
    fn pipe_reading_retries_interruptions_stops_on_errors_and_enforces_both_limits() {
        let (sender, receiver) = mpsc::sync_channel(8);
        let input = Scheduled {
            chunks: VecDeque::from([Err(io::Error::from(io::ErrorKind::Interrupted)), Ok(b"12345\nok\n".to_vec())]),
        };
        let pipe = read_pipe_with_limits(
            input,
            Stream::Prose,
            &sender,
            OutputLimits {
                retained: 7,
                line: 4,
                backlog: 8,
            },
        )
        .unwrap()
        .join()
        .unwrap();
        assert_eq!(pipe.text, b"12345\no");
        assert!(pipe.complete);
        assert!(!pipe.within_limits);
        assert_eq!(receiver.recv().unwrap().1, "ok");

        let failed = Scheduled {
            chunks: VecDeque::from([Ok(b"prefix".to_vec()), Err(io::Error::other("stop")), Ok(b"unreachable".to_vec())]),
        };
        let failed = read_pipe_with_limits(
            failed,
            Stream::Json,
            &sender,
            OutputLimits {
                retained: 64,
                line: 64,
                backlog: 8,
            },
        )
        .unwrap()
        .join()
        .unwrap();
        assert_eq!(failed.text, b"prefix");
        assert!(!failed.complete);
        assert!(failed.within_limits);

        let exact = read_pipe_with_limits(
            Cursor::new(b"1234\n".as_slice()),
            Stream::Prose,
            &sender,
            OutputLimits {
                retained: 5,
                line: 4,
                backlog: 8,
            },
        )
        .unwrap()
        .join()
        .unwrap();
        assert!(exact.complete);
        assert!(exact.within_limits);
    }

    #[test]
    fn absent_readers_are_complete_empty_streams_and_artifacts_are_not_diagnostics() {
        let (_sender, receiver) = mpsc::sync_channel(1);
        let mut events = Recorded::default();
        let (stdout, stderr) = finish_readers(None, None, &receiver, &mut events, Instant::now() + Duration::from_secs(1));

        for pipe in <[_; 2]>::from((stdout, stderr)) {
            let pipe = pipe.expect("an absent OS pipe is a complete empty stream");
            assert_eq!(pipe.text.len(), 0);
            assert!(pipe.complete);
            assert!(pipe.within_limits);
        }

        assert!(
            rendered_diagnostic(r#"{"reason":"compiler-artifact","message":{"rendered":"must stay hidden","level":"error"}}"#).is_none()
        );
    }

    #[test]
    fn successful_and_budgeted_supervision_each_finish_the_build_exactly_once() {
        let directory = crate::testing::workdir("invoke-finished-event-");
        let root = camino::Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 test path");
        let work = Workspace::adopt(root.clone(), root.join("target"));

        let mut success = Command::new(crate::testing::helper_binary_path().as_std_path());
        let _ = success
            .arg(crate::testing::directive("print:done"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut events = Recorded::default();
        let output = supervise(success, &work, Some(Duration::from_secs(30)), &mut events)
            .expect("supervision succeeds")
            .expect("the command finishes");
        assert!(output.status.success());
        assert_eq!(events.finished, 1);

        let mut slow = Command::new(crate::testing::helper_binary_path().as_std_path());
        let _ = slow
            .arg(crate::testing::directive("sleep:30000"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut events = Recorded::default();
        assert!(
            supervise(slow, &work, Some(Duration::from_millis(20)), &mut events)
                .unwrap()
                .is_none()
        );
        assert_eq!(events.finished, 1);
    }

    #[test]
    fn supervision_helpers_preserve_every_error_and_completion_contract() {
        assert_eq!(CARGO_PROGRESS_WHEN, "always");

        let mut events = Recorded::default();
        finish_build(&mut events);
        assert_eq!(events.finished, 1);

        let complete = || Pipe {
            text: b"complete".to_vec(),
            complete: true,
            within_limits: true,
        };
        let (stdout, stderr) = complete_pipes("root", Some(complete()), Some(complete())).expect("both complete");
        assert_eq!(stdout.text, b"complete");
        assert_eq!(stderr.text, b"complete");

        for result in [
            complete_pipes("root", None, Some(complete())),
            complete_pipes("root", Some(complete()), None),
            complete_pipes(
                "root",
                Some(Pipe {
                    complete: false,
                    ..complete()
                }),
                Some(complete()),
            ),
            complete_pipes(
                "root",
                Some(complete()),
                Some(Pipe {
                    complete: false,
                    ..complete()
                }),
            ),
        ] {
            assert!(result.unwrap_err().to_string().contains("output could not be read to the end"));
        }

        assert!(
            containment_error("root", "reason")
                .to_string()
                .contains("could not be contained: reason")
        );
        assert!(
            wait_error("root", io::Error::other("wait"))
                .to_string()
                .contains("could not wait for cargo")
        );
        assert!(
            cleanup_error("root", io::Error::other("cleanup"))
                .to_string()
                .contains("could not be terminated after it stopped being observable")
        );
        assert!(
            output_limit_error(
                "root",
                OutputLimits {
                    retained: 12,
                    line: 34,
                    backlog: 1,
                }
            )
            .to_string()
            .contains("configured 12-byte retained or 34-byte per-line")
        );

        let (sender, receiver) = mpsc::sync_channel(1);
        close_narration_channel(sender);
        receiver.recv().unwrap_err();
    }
}
