// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Running cargo, and turning its two output streams into something a reader can follow.

use core::time::Duration;
use std::io::{self, Read};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use cargo_gamma_process::{MemoryRequest, ProcessTree, prepare};

use super::super::cargo_options::BuildLimits;
use super::super::events::Events;
#[cfg(test)]
use super::super::faults::{self, Fault};
use super::super::workspace::Workspace;
use super::messages::cargo_message;
use crate::Result;
use crate::discover::Plan;
use crate::error::{Error, error};
use crate::report::encode_controls;

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
    let mut command = work.cargo();

    let _command = command
        .env("CARGO_TERM_PROGRESS_WHEN", "always")
        .env("CARGO_TERM_PROGRESS_WIDTH", PROGRESS_WIDTH.to_string())
        .args(args)
        .stderr(Stdio::piped())
        .stdout(Stdio::piped());

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

    let mut subtree = match ProcessTree::adopt(spawned) {
        Ok(subtree) => subtree,
        Err(reason) => {
            events.build_finished();
            let raw_reason = reason.to_string();
            let reason = encode_controls(&raw_reason);

            return Err(error!("the cargo build in `{root}` could not be contained: {reason}"));
        }
    };

    let (sender, lines) = mpsc::sync_channel(limits.backlog);

    let stdout = subtree
        .take_stdout()
        .map(|pipe| read_pipe_with_limits(pipe, Stream::Json, &sender, limits));
    let stderr = subtree
        .take_stderr()
        .map(|pipe| read_pipe_with_limits(pipe, Stream::Prose, &sender, limits));

    // The senders the reader threads hold are the only ones that matter; this one would keep the
    // channel open forever after they finish.
    drop(sender);

    let deadline = budget.map(|budget| Instant::now() + budget);

    let outcome = loop {
        let _narrated = narrate(&lines, events);

        match subtree.observe() {
            Ok(Some(status)) => {
                break Ok(Some(status));
            }

            Ok(None) => {
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    collect(&mut subtree);

                    break Ok(None);
                }

                thread::sleep(BUILD_POLL_INTERVAL);
            }

            Err(cause) => {
                // Nothing more can be asked of this child, and dropping it would leave the whole
                // build tree running with no handle on it at all.
                collect(&mut subtree);

                break Err(error!("could not wait for cargo in `{root}`").caused_by(cause));
            }
        }
    };

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

    events.build_finished();

    let Some(status) = outcome? else {
        return Ok(None);
    };

    // Not reported as a build that produced no diagnostics, which is what an empty stream reads as
    // to everything downstream: a partial JSON stream loses artifacts, and a run that lost the
    // artifacts of a build reports the tests it could not find as ones that do not exist. Both ways
    // of losing it are refused here — a reader still blocked on a pipe something else holds open,
    // and one that stopped on a read that failed — because the difference is invisible in the bytes.
    let (Some(stdout), Some(stderr)) = (said, printed) else {
        return Err(error!(
            "cargo in `{root}` finished, but its output could not be read to the end, so what it built could not be read"
        ));
    };

    if !stdout.complete || !stderr.complete {
        return Err(error!(
            "cargo in `{root}` finished, but its output could not be read to the end, so what it built could not be read"
        ));
    }

    if !stdout.within_limits || !stderr.within_limits {
        return Err(error!(
            "cargo in `{root}` exceeded the configured {}-byte retained or {}-byte per-line build-output limit, so its truncated output could not be trusted",
            limits.retained, limits.line
        ));
    }

    Ok(Some(Output {
        status,
        stdout: stdout.text,
        stderr: stderr.text,
    }))
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
fn collect(subtree: &mut ProcessTree) {
    #[cfg(unix)]
    debug_assert!(
        !subtree.released(),
        "the subtree is signalled while it still holds its leader and its watch slot"
    );

    let _reaped = subtree.terminate();
}

/// How wide cargo is asked to draw its progress bar.
///
/// Fixed rather than taken from the terminal, because cargo is writing to a pipe and has no
/// terminal to measure. The display this is handed to truncates to the real width.
pub(super) const PROGRESS_WIDTH: usize = 100;

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

    while narrated < NARRATION_BATCH {
        let Ok((stream, line)) = lines.try_recv() else {
            break;
        };

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
    retained: 4 * 1024 * 1024,
    line: 64 * 1024,
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

    thread::Builder::new().name("cargo-gamma-build-output".to_owned()).spawn(move || {
        let mut text = Vec::with_capacity(limits.retained);
        let mut buffer = [0_u8; 8192];
        let mut line = Vec::with_capacity(limits.line);
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

                    complete = false;

                    break;
                }
            };

            let room = limits.retained.saturating_sub(text.len());
            let kept = read.min(room);

            text.extend_from_slice(&buffer[..kept]);

            if kept < read {
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
                    line_limited = false;

                    continue;
                }

                if line.len() < limits.line {
                    line.push(*byte);
                } else {
                    // A line's owned copy is part of the queue bound. Refuse a result built from
                    // output that cannot be held within that bound, while continuing to drain.
                    line_limited = true;
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

/// Waits for one pipe reader to finish with the whole of its stream, but not past `deadline`.
///
/// `None` when it did not, which happens two ways. The thread may still be running: the pipe is
/// held by something that outlived the kill — a descendant that left the process group, or one the
/// platform could not reach — and the thread is left where it is, blocked in a read on a handle it
/// owns, with no way to make it return short of closing the pipe underneath it. Or it finished on a
/// read that failed, having taken only a prefix of what was written.
///
/// Both are the same fact to the caller: what came back is not the stream. Abandoning one thread is
/// the lesser cost, and the caller reports the loss rather than passing off a truncated stream as
/// the build's output.
#[cfg(test)]
pub(super) fn drained(handle: Option<JoinHandle<Pipe>>, deadline: Instant) -> Option<Vec<u8>> {
    let Some(handle) = handle else {
        return Some(Vec::new());
    };

    while !handle.is_finished() {
        if Instant::now() >= deadline {
            return None;
        }

        thread::sleep(BUILD_POLL_INTERVAL);
    }

    // A reader that panicked has no bytes at all, and one that stopped on a failed read has only a
    // prefix; neither is the stream, and the caller's guard treats them alike.
    let Ok(pipe) = handle.join() else {
        return None;
    };

    (pipe.complete && pipe.within_limits).then_some(pipe.text)
}

/// Narrates while waiting for bounded output readers to finish.
///
/// A reader can legitimately be blocked on the synchronous channel when a compiler is noisy.
/// Draining that channel here is therefore part of joining it: waiting for the thread without
/// narrating would deadlock at a full, deliberately bounded backlog.
pub(super) fn finish_readers(
    stdout: Option<io::Result<JoinHandle<Pipe>>>,
    stderr: Option<io::Result<JoinHandle<Pipe>>>,
    lines: &Receiver<(Stream, String)>,
    events: &mut dyn Events,
    deadline: Instant,
) -> (Option<Pipe>, Option<Pipe>) {
    let finished = |reader: &Option<io::Result<JoinHandle<Pipe>>>| {
        reader
            .as_ref()
            .is_none_or(|reader| reader.as_ref().is_err() || reader.as_ref().is_ok_and(JoinHandle::is_finished))
    };

    while !finished(&stdout) || !finished(&stderr) {
        let _narrated = narrate(lines, events);

        if Instant::now() >= deadline {
            return (None, None);
        }

        thread::sleep(BUILD_POLL_INTERVAL);
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
pub(super) fn run_cargo(
    work: &Workspace,
    plan: &Plan,
    verb: &[&str],
    select: Option<&[String]>,
    limits: BuildLimits,
    first_round: Option<Duration>,
    events: &mut dyn Events,
) -> Result<Compiled> {
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

    work.cargo.extend_build_args(&mut args);

    let Some(output) = compile(work, &args, limits.budget(first_round), events)? else {
        return Ok(Compiled {
            succeeded: false,
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

    Ok(Compiled {
        succeeded: output.status.success(),
        stdout: Some(stdout),
        stderr,
    })
}
