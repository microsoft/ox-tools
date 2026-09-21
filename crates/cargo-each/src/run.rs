// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the `cargo each` command: resolve the selection,
//! apply filters, build the plan, and run it.

use std::collections::{BTreeSet, VecDeque};
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::num::NonZeroUsize;
use std::panic::{self, AssertUnwindSafe, UnwindSafe};
use std::process::{ChildStderr, ChildStdout, Command, ExitCode, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError, mpsc};
use std::time::{Duration, Instant};
use std::{fmt, thread};

use cargo_metadata::TargetKind;
use command_group::{CommandGroup as _, GroupChild};
use ohno::{AppError, IntoAppError};

use crate::cli::EachArgs;
use crate::error::{InvalidTargetKindError, JobsConflictWithOnceError};
use crate::filter::Predicate;
use crate::plan::{BuildOptions, Invocation, Mode, PackagesExpansion, Plan};
use crate::select::Selection;
use crate::substitute::uses_workspace_rust_version;
use crate::workspace::{Member, Workspace};

#[cfg(test)]
const WORKER_PANIC_TEST_PROGRAM: &str = "__cargo_each_injected_worker_panic";
#[cfg(test)]
const WORKER_SPAWN_ERROR_TEST_PROGRAM: &str = "__cargo_each_injected_worker_spawn_error";
const TERMINATION_GRACE: Duration = Duration::from_millis(250);
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(1);
const OUTPUT_READER_CANCEL_GRACE: Duration = Duration::from_millis(100);
const OUTPUT_READER_POLL: Duration = Duration::from_millis(5);
const OUTPUT_MEMORY_LIMIT: usize = 1_048_576;

pub(crate) fn run(args: &EachArgs) -> Result<ExitCode, AppError> {
    let selection = build_selection(args).into_app_err("failed to read package selection")?;
    let workspace = Workspace::load(args.manifest_path.as_deref()).into_app_err("failed to load workspace")?;

    let mut members = selection.resolve(&workspace).into_app_err("failed to resolve package selection")?;
    apply_filters(&mut members, args)?;

    // The `{packages}` pass-through only applies when the resolved set is the
    // untouched whole workspace: no per-package narrowing and no filters.
    let packages = if selection.is_whole_workspace() && args.filters.is_empty() && args.exclude_filters.is_empty() {
        PackagesExpansion::Workspace
    } else {
        PackagesExpansion::Explicit
    };

    let target_kinds = parse_target_kinds(&args.each_targets)?;
    let target_required_features = args.target_required_feature.iter().cloned().collect();
    let mode = if args.once {
        Mode::Once
    } else if target_kinds.is_empty() {
        Mode::PerPackage
    } else {
        Mode::PerTarget
    };
    if mode == Mode::Once && args.jobs.get() != 1 {
        return Err(JobsConflictWithOnceError::new()).into_app_err("invalid execution configuration");
    }

    let mut build_options = BuildOptions {
        mode,
        chdir: args.chdir,
        packages,
        target_kinds: &target_kinds,
        target_required_features: &target_required_features,
        workspace_rust_version: None,
    };
    if Plan::is_empty(&members, &args.command, build_options).into_app_err("failed to build command plan")? {
        eprintln!("cargo each: selection resolved to no work; nothing to do");
        return Ok(ExitCode::SUCCESS);
    }

    let workspace_rust_version = if uses_workspace_rust_version(&args.command) {
        Some(
            workspace
                .workspace_rust_version()
                .into_app_err("failed to resolve workspace Rust version")?,
        )
    } else {
        None
    };
    build_options.workspace_rust_version = workspace_rust_version.as_deref();

    let plan = Plan::build(&members, &args.command, build_options).into_app_err("failed to build command plan")?;

    if args.dry_run {
        for inv in &plan.invocations {
            match &inv.work_dir {
                Some(dir) => println!("(cd {}) {}", dir.display(), shell_join(&inv.argv)),
                None => println!("{}", shell_join(&inv.argv)),
            }
        }

        return Ok(ExitCode::SUCCESS);
    }

    execute(&plan, args.keep_going, args.jobs, args.timeout)
}

/// Assemble a [`Selection`] from direct and file-backed package specs.
fn build_selection(args: &EachArgs) -> Result<Selection, crate::error::EachError> {
    Selection::from_sources(&args.packages, &args.package_files, args.workspace, &args.exclude, args.none)
}

/// Narrow `members` by package keep and drop expressions. Repeated `--filter`
/// expressions are AND-combined; repeated `--exclude-filter` expressions are
/// OR-combined, and exclusion wins.
fn apply_filters(members: &mut Vec<&Member>, args: &EachArgs) -> Result<(), AppError> {
    let keep = parse_predicates(&args.filters)?;
    let drop = parse_predicates(&args.exclude_filters)?;
    members.retain(|m| keep.iter().all(|p| p.matches(m)) && !drop.iter().any(|p| p.matches(m)));
    Ok(())
}

fn parse_predicates(specs: &[String]) -> Result<Vec<Predicate>, AppError> {
    specs
        .iter()
        .map(|s| Predicate::parse(s).into_app_err("invalid filter expression"))
        .collect()
}

fn parse_target_kinds(kinds: &[String]) -> Result<BTreeSet<TargetKind>, AppError> {
    kinds
        .iter()
        .map(|kind| {
            if let Some(kind) = crate::workspace::parse_target_kind(kind) {
                Ok(kind)
            } else {
                Err(InvalidTargetKindError::new(kind.clone()).into())
            }
        })
        .collect::<Result<_, crate::error::EachError>>()
        .into_app_err("invalid per-target configuration")
}

fn execute(plan: &Plan, keep_going: bool, jobs: NonZeroUsize, timeout: Option<Duration>) -> Result<ExitCode, AppError> {
    let worker_count = effective_worker_count(jobs, plan.invocations.len());
    if worker_count.get() == 1 {
        Ok(execute_sequential(plan, keep_going, timeout))
    } else {
        execute_parallel(plan, keep_going, worker_count, timeout)
    }
}

fn effective_worker_count(requested: NonZeroUsize, plan_size: usize) -> NonZeroUsize {
    NonZeroUsize::new(requested.get().min(plan_size)).expect("execution receives a nonempty plan")
}

fn execute_sequential(plan: &Plan, keep_going: bool, timeout: Option<Duration>) -> ExitCode {
    execute_sequential_with(plan, keep_going, timeout, |invocation, timeout| {
        if let Some(timeout) = timeout {
            run_streamed_with_timeout(invocation, timeout)
        } else {
            run_streamed(invocation)
        }
    })
}

fn execute_sequential_with(
    plan: &Plan,
    keep_going: bool,
    timeout: Option<Duration>,
    mut run_invocation: impl FnMut(&Invocation, Option<Duration>) -> InvocationResult,
) -> ExitCode {
    let mut any_failed = false;
    for invocation in &plan.invocations {
        emit_label(invocation);
        let result = run_invocation(invocation, timeout);
        match result {
            InvocationResult::Exited(status) if status.success() => {}
            InvocationResult::Exited(status) => {
                if !keep_going {
                    return ExitCode::from(exit_byte(status.code()));
                }
                any_failed = true;
            }
            InvocationResult::TimedOut(duration) => {
                eprintln!("cargo each: invocation timed out after {}", display_duration(duration));
                if !keep_going {
                    return ExitCode::from(1);
                }
                any_failed = true;
            }
            InvocationResult::Infrastructure(message) => {
                eprintln!("cargo each: {message}");
                if !keep_going {
                    return ExitCode::from(2);
                }
                any_failed = true;
            }
        }
    }
    if any_failed { ExitCode::from(1) } else { ExitCode::SUCCESS }
}

fn execute_parallel(plan: &Plan, keep_going: bool, worker_count: NonZeroUsize, timeout: Option<Duration>) -> Result<ExitCode, AppError> {
    let invocations = plan.invocations.clone();
    let mut pending: VecDeque<(usize, Invocation)> = invocations.iter().cloned().enumerate().collect();
    let mut workers = Vec::with_capacity(worker_count.get());
    let mut outcomes = Vec::with_capacity(invocations.len());
    let mut stop_launching = false;

    loop {
        while !stop_launching && workers.len() < worker_count.get() {
            let Some((index, invocation)) = pending.pop_front() else {
                break;
            };
            match spawn_worker(index, invocation, timeout) {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    outcomes.push(IndexedOutcome {
                        index,
                        outcome: BufferedOutcome::infrastructure(format!("failed to create cargo-each worker thread: {error}")),
                    });
                    if failure_stops_launching(keep_going, true) {
                        stop_launching = true;
                    }
                }
            }
        }

        let Some(outcome) = wait_for_worker(&mut workers) else {
            break;
        };
        if failure_stops_launching(keep_going, outcome.outcome.result.failed()) {
            stop_launching = true;
        }
        outcomes.push(outcome);
    }

    outcomes.sort_by_key(|outcome| outcome.index);

    for indexed in &mut outcomes {
        emit_buffered(&invocations[indexed.index], &mut indexed.outcome).into_app_err("failed to emit buffered command output")?;
    }

    let Some(first_failure) = outcomes.iter().find(|outcome| outcome.outcome.result.failed()) else {
        return Ok(ExitCode::SUCCESS);
    };
    if keep_going {
        return Ok(ExitCode::from(1));
    }
    Ok(parallel_failure_exit_code(&first_failure.outcome.result))
}

fn parallel_failure_exit_code(result: &InvocationResult) -> ExitCode {
    match result {
        InvocationResult::Exited(status) => ExitCode::from(exit_byte(status.code())),
        InvocationResult::TimedOut(_) => ExitCode::from(1),
        InvocationResult::Infrastructure(_) => ExitCode::from(2),
    }
}

fn failure_stops_launching(keep_going: bool, failed: bool) -> bool {
    matches!((keep_going, failed), (false, true))
}

fn spawn_worker(index: usize, invocation: Invocation, timeout: Option<Duration>) -> io::Result<RunningWorker> {
    #[cfg(test)]
    if invocation
        .argv
        .first()
        .is_some_and(|program| program == WORKER_SPAWN_ERROR_TEST_PROGRAM)
    {
        return Err(io::Error::other("injected worker spawn failure"));
    }

    let (sender, receiver) = mpsc::channel();
    let thread = thread::Builder::new().name(format!("cargo-each-worker-{index}")).spawn(move || {
        complete_worker(&sender, move || run_captured(&invocation, timeout));
    })?;
    Ok(RunningWorker { index, receiver, thread })
}

fn complete_worker(sender: &mpsc::Sender<BufferedOutcome>, work: impl FnOnce() -> BufferedOutcome + UnwindSafe) {
    let outcome = match panic::catch_unwind(work) {
        Ok(outcome) => outcome,
        Err(payload) => BufferedOutcome::infrastructure(format!(
            "worker panicked while running invocation: {}",
            panic_description(payload.as_ref())
        )),
    };
    let _receiver_gone = sender.send(outcome);
}

fn wait_for_worker(workers: &mut Vec<RunningWorker>) -> Option<IndexedOutcome> {
    if workers.is_empty() {
        return None;
    }
    loop {
        let ready = workers
            .iter()
            .enumerate()
            .find_map(|(position, worker)| match worker.receiver.try_recv() {
                Ok(outcome) => Some((position, Some(outcome))),
                Err(mpsc::TryRecvError::Disconnected) => Some((position, None)),
                Err(mpsc::TryRecvError::Empty) => None,
            });
        let Some((position, reported)) = ready else {
            thread::sleep(Duration::from_millis(1));
            continue;
        };

        let RunningWorker { index, thread, .. } = workers.swap_remove(position);
        let outcome = match (reported, thread.join()) {
            (Some(outcome), Ok(())) => outcome,
            (Some(_) | None, Err(payload)) => BufferedOutcome::infrastructure(format!(
                "worker panicked while running invocation: {}",
                panic_description(payload.as_ref())
            )),
            (None, Ok(())) => BufferedOutcome::infrastructure("worker exited without reporting an invocation outcome".to_owned()),
        };
        return Some(IndexedOutcome { index, outcome });
    }
}

fn panic_description(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&str>() {
        message
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message
    } else {
        "non-string panic payload"
    }
}

fn run_streamed(invocation: &Invocation) -> InvocationResult {
    let (program, mut command) = match command_for(invocation) {
        Ok(command) => command,
        Err(message) => return InvocationResult::Infrastructure(message),
    };
    match command.status() {
        Ok(status) => InvocationResult::Exited(status),
        Err(error) => InvocationResult::Infrastructure(format!("failed to spawn `{program}`: {error}")),
    }
}

fn run_streamed_with_timeout(invocation: &Invocation, timeout: Duration) -> InvocationResult {
    run_streamed_with_timeout_with(invocation, timeout, spawn_group)
}

fn run_streamed_with_timeout_with(
    invocation: &Invocation,
    timeout: Duration,
    spawn: impl FnOnce(Command) -> Result<GroupChild, String>,
) -> InvocationResult {
    let (program, command) = match command_for(invocation) {
        Ok(command) => command,
        Err(message) => return InvocationResult::Infrastructure(message),
    };
    let tree = match spawn(command) {
        Ok(tree) => tree,
        Err(error) => {
            return InvocationResult::Infrastructure(format!("failed to spawn `{program}`: {error}"));
        }
    };
    wait_for_process(
        tree,
        Some(timeout),
        GroupChild::try_wait,
        || None,
        terminate_group_bounded,
        "observe child process group",
    )
    .result
}

fn run_captured(invocation: &Invocation, timeout: Option<Duration>) -> BufferedOutcome {
    #[cfg(test)]
    assert!(
        invocation.argv.first().is_none_or(|program| program != WORKER_PANIC_TEST_PROGRAM),
        "injected worker panic"
    );

    run_captured_with(
        invocation,
        timeout,
        spawn_group,
        spawn_child_stdout_reader,
        spawn_child_stderr_reader,
    )
}

fn run_captured_with(
    invocation: &Invocation,
    timeout: Option<Duration>,
    spawner: impl FnOnce(Command) -> Result<GroupChild, String>,
    stdout_spawner: impl FnOnce(ChildStdout, &'static str) -> io::Result<OutputReader>,
    stderr_spawner: impl FnOnce(ChildStderr, &'static str) -> io::Result<OutputReader>,
) -> BufferedOutcome {
    let (program, mut command) = match command_for(invocation) {
        Ok(command) => command,
        Err(message) => return BufferedOutcome::infrastructure(message),
    };
    let _ = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut process = match spawner(command) {
        Ok(process) => process,
        Err(error) => {
            return BufferedOutcome::infrastructure(format!("failed to spawn `{program}`: {error}"));
        }
    };
    let stdout = process.inner().stdout.take();
    let Some(stdout) = stdout else {
        let cleanup = terminate_group_bounded(process);
        return BufferedOutcome::infrastructure(with_cleanup_failure("failed to capture child stdout".to_owned(), &cleanup));
    };
    let mut stdout_reader = match stdout_spawner(stdout, "cargo-each-stdout") {
        Ok(reader) => reader,
        Err(error) => {
            let cleanup = terminate_group_bounded(process);
            return BufferedOutcome::infrastructure(with_cleanup_failure(format!("failed to create stdout reader: {error}"), &cleanup));
        }
    };
    let stderr = process.inner().stderr.take();
    let Some(stderr) = stderr else {
        let cleanup = terminate_group_bounded(process);
        return BufferedOutcome::from_reader_failure("failed to capture child stderr".to_owned(), stdout_reader, &cleanup);
    };
    let mut stderr_reader = match stderr_spawner(stderr, "cargo-each-stderr") {
        Ok(reader) => reader,
        Err(error) => {
            let cleanup = terminate_group_bounded(process);
            return BufferedOutcome::from_reader_failure(format!("failed to create stderr reader: {error}"), stdout_reader, &cleanup);
        }
    };

    let observe_group = timeout.is_some();
    let process_outcome = wait_for_process(
        process,
        timeout,
        move |process| {
            if observe_group {
                process.try_wait()
            } else {
                process.inner().try_wait()
            }
        },
        || {
            let failure = [stdout_reader.take_failure("stdout"), stderr_reader.take_failure("stderr")]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("; ");
            (!failure.is_empty()).then_some(failure)
        },
        terminate_group_bounded,
        if observe_group {
            "observe child process group"
        } else {
            "observe child process leader"
        },
    );
    let boundary = if observe_group { "process group" } else { "process leader" };
    let (stdout, stderr) = finish_output_readers(stdout_reader, stderr_reader, OUTPUT_DRAIN_GRACE, boundary);
    combine_captured_output(stdout, stderr, process_outcome.result)
}

fn combine_captured_output(stdout: CapturedStream, stderr: CapturedStream, result: InvocationResult) -> BufferedOutcome {
    let failure = [stdout.failure.as_deref(), stderr.failure.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("; ");
    let result = add_infrastructure_failure(result, failure);
    BufferedOutcome {
        stdout: stdout.output,
        stderr: stderr.output,
        result,
    }
}

fn add_infrastructure_failure(result: InvocationResult, failure: String) -> InvocationResult {
    if failure.is_empty() {
        return result;
    }
    InvocationResult::Infrastructure(match result {
        InvocationResult::Infrastructure(primary) => format!("{primary}; {failure}"),
        InvocationResult::TimedOut(duration) => {
            format!("invocation timed out after {}; {failure}", display_duration(duration))
        }
        InvocationResult::Exited(_) => failure,
    })
}

fn command_for(invocation: &Invocation) -> Result<(&str, Command), String> {
    let Some((program, arguments)) = invocation.argv.split_first() else {
        return Err("internal command-plan error: invocation has an empty argument vector".to_owned());
    };
    let mut command = Command::new(program);
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command.args(arguments);
    if let Some(directory) = &invocation.work_dir {
        command.current_dir(directory);
    }
    Ok((program, command))
}

fn spawn_group(mut command: Command) -> Result<GroupChild, String> {
    command.group_spawn().map_err(|error| error.to_string())
}

fn wait_for_process<T>(
    mut control: T,
    timeout: Option<Duration>,
    mut observe: impl FnMut(&mut T) -> io::Result<Option<ExitStatus>>,
    mut reader_failure: impl FnMut() -> Option<String>,
    terminate: impl FnOnce(T) -> io::Result<ExitStatus>,
    operation: &str,
) -> TreeOutcome {
    let started = Instant::now();
    let mut terminate = Some(terminate);
    loop {
        if let Some(reader_failure) = reader_failure() {
            let cleanup = terminate.take().expect("termination is consumed only on a returning branch")(control);
            return TreeOutcome::new(InvocationResult::Infrastructure(with_cleanup_failure(reader_failure, &cleanup)));
        }

        match observe(&mut control) {
            Ok(Some(status)) => return TreeOutcome::new(InvocationResult::Exited(status)),
            Ok(None) => {}
            Err(error) => {
                let cleanup = terminate.take().expect("termination is consumed only on a returning branch")(control);
                return TreeOutcome::new(InvocationResult::Infrastructure(with_cleanup_failure(
                    format!("failed to {operation}: {error}"),
                    &cleanup,
                )));
            }
        }

        if let Some(timeout) = timeout
            && timeout.checked_sub(started.elapsed()).is_none()
        {
            return match terminate.take().expect("termination is consumed only on a returning branch")(control) {
                Ok(_) => TreeOutcome::new(InvocationResult::TimedOut(timeout)),
                Err(error) => TreeOutcome::new(InvocationResult::Infrastructure(format!(
                    "invocation timed out after {}; process-group termination failed: {error}",
                    display_duration(timeout)
                ))),
            };
        }

        let pause = timeout
            .and_then(|timeout| timeout.checked_sub(started.elapsed()))
            .map_or(Duration::from_millis(10), |remaining| remaining.min(Duration::from_millis(10)));
        thread::sleep(pause);
    }
}

fn terminate_group_bounded(child: GroupChild) -> io::Result<ExitStatus> {
    terminate_group_with(
        child,
        TERMINATION_GRACE,
        GroupChild::kill,
        GroupChild::try_wait,
        detach_group_reaper,
    )
}

fn terminate_group_with<T>(
    mut child: T,
    grace: Duration,
    kill: impl FnOnce(&mut T) -> io::Result<()>,
    mut try_wait: impl FnMut(&mut T) -> io::Result<Option<ExitStatus>>,
    detach: impl FnOnce(T) -> io::Result<()>,
) -> io::Result<ExitStatus> {
    let kill_error = kill(&mut child).err();
    let observed = poll_process_exit(&mut child, grace, &mut try_wait);
    match observed {
        Ok(Some(status)) => match kill_error {
            Some(error) => Err(error),
            None => Ok(status),
        },
        Ok(None) => {
            let reaper = detach(child);
            let message = kill_error.map_or_else(
                || format!("process group did not exit within {} ms after termination", grace.as_millis()),
                |error| {
                    format!(
                        "{error}; process group did not exit within {} ms after termination",
                        grace.as_millis()
                    )
                },
            );
            Err(io::Error::new(io::ErrorKind::WouldBlock, with_reaper_handoff(&message, &reaper)))
        }
        Err(error) => {
            let reaper = detach(child);
            Err(io::Error::new(
                error.kind(),
                with_reaper_handoff(&format!("failed to observe process group after termination: {error}"), &reaper),
            ))
        }
    }
}

fn detach_group_reaper(mut child: GroupChild) -> io::Result<()> {
    thread::Builder::new()
        .name("cargo-each-process-reaper".to_owned())
        .spawn(move || {
            let _ignored = child.wait();
        })
        .map(drop)
}

fn with_reaper_handoff(message: &str, reaper: &io::Result<()>) -> String {
    match reaper {
        Ok(()) => format!("{message}; the process group was moved to a detached reaper thread"),
        Err(error) => format!("{message}; failed to start the detached process-group reaper: {error}"),
    }
}

fn poll_process_exit<T>(
    control: &mut T,
    grace: Duration,
    mut try_wait: impl FnMut(&mut T) -> io::Result<Option<ExitStatus>>,
) -> io::Result<Option<ExitStatus>> {
    let started = Instant::now();
    loop {
        if let Some(status) = try_wait(control)? {
            return Ok(Some(status));
        }
        let Some(remaining) = grace.checked_sub(started.elapsed()) else {
            return Ok(None);
        };
        thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}

fn spawn_child_stdout_reader(stream: ChildStdout, name: &'static str) -> io::Result<OutputReader> {
    spawn_output_reader_inner(
        stream,
        name,
        OUTPUT_MEMORY_LIMIT,
        Box::new(|| tempfile::tempfile().map(|file| Box::new(file) as Box<dyn SpillFile>)),
    )
}

fn spawn_child_stderr_reader(stream: ChildStderr, name: &'static str) -> io::Result<OutputReader> {
    spawn_output_reader_inner(
        stream,
        name,
        OUTPUT_MEMORY_LIMIT,
        Box::new(|| tempfile::tempfile().map(|file| Box::new(file) as Box<dyn SpillFile>)),
    )
}

#[cfg(test)]
fn spawn_output_reader<R>(stream: R, name: &'static str) -> io::Result<OutputReader>
where
    R: io::Read + Send + 'static,
{
    spawn_output_reader_inner(
        stream,
        name,
        OUTPUT_MEMORY_LIMIT,
        Box::new(|| tempfile::tempfile().map(|file| Box::new(file) as Box<dyn SpillFile>)),
    )
}

#[cfg(test)]
fn spawn_output_reader_with<R>(stream: R, name: &'static str, memory_limit: usize, spill_factory: SpillFactory) -> io::Result<OutputReader>
where
    R: io::Read + Send + 'static,
{
    spawn_output_reader_inner(stream, name, memory_limit, spill_factory)
}

fn spawn_output_reader_inner<R>(
    mut stream: R,
    name: &'static str,
    memory_limit: usize,
    mut spill_factory: SpillFactory,
) -> io::Result<OutputReader>
where
    R: io::Read + Send + 'static,
{
    let output = Arc::new(Mutex::new(CapturedOutput::empty()));
    let retaining = Arc::new(AtomicBool::new(true));
    let captured = Arc::clone(&output);
    let capture_enabled = Arc::clone(&retaining);
    let (completion_sender, completion) = mpsc::channel();
    let thread = thread::Builder::new().name(name.to_owned()).spawn(move || {
        let result = panic::catch_unwind(AssertUnwindSafe(|| -> io::Result<()> {
            let mut chunk = [0_u8; 8192];
            loop {
                if !capture_enabled.load(Ordering::Acquire) {
                    return Ok(());
                }
                let read = loop {
                    match stream.read(&mut chunk) {
                        Ok(0) => return Ok(()),
                        Ok(read) => break read,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            if !capture_enabled.load(Ordering::Acquire) {
                                return Ok(());
                            }
                            thread::sleep(OUTPUT_READER_POLL);
                        }
                        Err(error) => return Err(error),
                    }
                };
                let mut output = captured
                    .lock()
                    .map_err(|error| io::Error::other(format!("child {name} capture buffer was poisoned: {error}")))?;
                if !capture_enabled.load(Ordering::Acquire) {
                    continue;
                }
                output.append(&chunk[..read], memory_limit, &mut spill_factory)?;
            }
        }));
        drop(stream);
        let completion = match result {
            Ok(result) => ReaderCompletion::Finished(result),
            Err(payload) => ReaderCompletion::Panicked(panic_description(payload.as_ref()).to_owned()),
        };
        let _receiver_gone = completion_sender.send(completion);
    })?;
    Ok(OutputReader {
        thread,
        completion,
        output,
        retaining,
        reported: None,
        failure_claimed: false,
    })
}

fn finish_output_reader(mut reader: OutputReader, stream: &str, grace: Duration, boundary: &str) -> CapturedStream {
    let mut thread_finished = false;
    let completion = reader
        .reported
        .take()
        .unwrap_or_else(|| match reader.completion.recv_timeout(grace) {
            Ok(completion) => completion,
            Err(mpsc::RecvTimeoutError::Disconnected) => ReaderCompletion::Disconnected,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                reader.retaining.store(false, Ordering::Release);
                ReaderCompletion::DrainTimedOut
            }
        });
    let mut failure = if reader.failure_claimed {
        None
    } else {
        reader_failure(&completion, stream, grace, boundary)
    };

    if matches!(completion, ReaderCompletion::DrainTimedOut) {
        match reader.completion.recv_timeout(OUTPUT_READER_CANCEL_GRACE) {
            Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                thread_finished = true;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let cancellation = format!(
                    "child {stream} reader did not stop within {} ms after capture was cancelled",
                    OUTPUT_READER_CANCEL_GRACE.as_millis()
                );
                failure = Some(match failure {
                    Some(failure) => format!("{failure}; {cancellation}"),
                    None => cancellation,
                });
            }
        }
    } else {
        thread_finished = true;
    }

    if thread_finished {
        if let Err(payload) = reader.thread.join() {
            let panic = format!(
                "child {stream} reader thread panicked after reporting completion: {}",
                panic_description(payload.as_ref())
            );
            failure = Some(match failure {
                Some(failure) => format!("{failure}; {panic}"),
                None => panic,
            });
        }
    } else {
        drop(reader.thread);
    }

    let output = take_reader_output(&reader.output, stream, thread_finished, &mut failure);
    CapturedStream { output, failure }
}

fn take_reader_output(output: &Mutex<CapturedOutput>, stream: &str, thread_finished: bool, failure: &mut Option<String>) -> CapturedOutput {
    let mut captured = if thread_finished {
        match output.lock() {
            Ok(captured) => captured,
            Err(poisoned) => {
                append_failure(failure, format!("child {stream} capture buffer was poisoned"));
                poisoned.into_inner()
            }
        }
    } else {
        match output.try_lock() {
            Ok(captured) => captured,
            Err(TryLockError::Poisoned(poisoned)) => {
                append_failure(failure, format!("child {stream} capture buffer was poisoned"));
                poisoned.into_inner()
            }
            Err(TryLockError::WouldBlock) => {
                append_failure(
                    failure,
                    format!(
                        "child {stream} capture buffer remained locked after reader cancellation; \
                         partial output could not be recovered without exceeding the drain bound"
                    ),
                );
                return CapturedOutput::empty();
            }
        }
    };
    std::mem::replace(&mut *captured, CapturedOutput::empty())
}

fn append_failure(failure: &mut Option<String>, additional: String) {
    *failure = Some(match failure.take() {
        Some(failure) => format!("{failure}; {additional}"),
        None => additional,
    });
}

fn reader_failure(completion: &ReaderCompletion, stream: &str, grace: Duration, boundary: &str) -> Option<String> {
    match completion {
        ReaderCompletion::Finished(Ok(())) => None,
        ReaderCompletion::Finished(Err(error)) => Some(format!("failed to read child {stream}: {error}")),
        ReaderCompletion::Panicked(message) => Some(format!("child {stream} reader thread panicked: {message}")),
        ReaderCompletion::Disconnected => Some(format!("child {stream} reader exited without reporting completion")),
        ReaderCompletion::DrainTimedOut => Some(format!(
            "child {stream} remained open for more than {} ms after the {boundary} completed; partial output was retained",
            grace.as_millis()
        )),
    }
}

fn finish_output_readers(stdout: OutputReader, stderr: OutputReader, grace: Duration, boundary: &str) -> (CapturedStream, CapturedStream) {
    let deadline = Instant::now().checked_add(grace);
    let stdout = finish_output_reader(stdout, "stdout", grace, boundary);
    let remaining = deadline
        .and_then(|deadline| deadline.checked_duration_since(Instant::now()))
        .unwrap_or(Duration::ZERO);
    let stderr = finish_output_reader(stderr, "stderr", remaining, boundary);
    (stdout, stderr)
}

fn with_cleanup_failure<T>(message: String, cleanup: &io::Result<T>) -> String {
    match cleanup {
        Ok(_) => message,
        Err(error) => format!("{message}; process-group cleanup also failed: {error}"),
    }
}

fn emit_label(invocation: &Invocation) {
    if let Some(label) = &invocation.label {
        eprintln!("cargo each: {label}");
    }
}

fn emit_buffered(invocation: &Invocation, outcome: &mut BufferedOutcome) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    emit_buffered_to(invocation, outcome, &mut stdout, &mut stderr)
}

fn emit_buffered_to(
    invocation: &Invocation,
    outcome: &mut BufferedOutcome,
    stdout: &mut dyn io::Write,
    stderr: &mut dyn io::Write,
) -> io::Result<()> {
    emit_label(invocation);
    let mut source_failures = Vec::new();
    match outcome.stdout.emit_to(stdout) {
        Ok(()) => {}
        Err(OutputEmitError::Source(error)) => {
            source_failures.push(format!("failed to read spilled child stdout: {error}"));
        }
        Err(OutputEmitError::Destination(error)) => return Err(error),
    }
    stdout.flush()?;
    match outcome.stderr.emit_to(stderr) {
        Ok(()) => {}
        Err(OutputEmitError::Source(error)) => {
            source_failures.push(format!("failed to read spilled child stderr: {error}"));
        }
        Err(OutputEmitError::Destination(error)) => return Err(error),
    }
    if !source_failures.is_empty() {
        let original = std::mem::replace(
            &mut outcome.result,
            InvocationResult::Infrastructure("output emission failed".to_owned()),
        );
        outcome.result = add_infrastructure_failure(original, source_failures.join("; "));
    }
    match &outcome.result {
        InvocationResult::TimedOut(duration) => {
            writeln!(stderr, "cargo each: invocation timed out after {}", display_duration(*duration))?;
        }
        InvocationResult::Infrastructure(message) => {
            writeln!(stderr, "cargo each: {message}")?;
        }
        InvocationResult::Exited(_) => {}
    }
    stderr.flush()
}

fn display_duration(duration: Duration) -> String {
    if duration.subsec_nanos() == 0 && duration.as_secs().is_multiple_of(60) {
        format!("{}m", duration.as_secs() / 60)
    } else if duration.subsec_nanos() == 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

#[derive(Debug)]
struct IndexedOutcome {
    index: usize,
    outcome: BufferedOutcome,
}

#[derive(Debug)]
struct RunningWorker {
    index: usize,
    receiver: mpsc::Receiver<BufferedOutcome>,
    thread: thread::JoinHandle<()>,
}

#[derive(Debug)]
struct OutputReader {
    thread: thread::JoinHandle<()>,
    completion: mpsc::Receiver<ReaderCompletion>,
    output: Arc<Mutex<CapturedOutput>>,
    retaining: Arc<AtomicBool>,
    reported: Option<ReaderCompletion>,
    failure_claimed: bool,
}

#[derive(Debug)]
enum ReaderCompletion {
    Finished(io::Result<()>),
    Panicked(String),
    Disconnected,
    DrainTimedOut,
}

impl OutputReader {
    fn take_failure(&mut self, stream: &str) -> Option<String> {
        if self.reported.is_none() {
            self.reported = match self.completion.try_recv() {
                Ok(completion) => Some(completion),
                Err(mpsc::TryRecvError::Disconnected) => Some(ReaderCompletion::Disconnected),
                Err(mpsc::TryRecvError::Empty) => None,
            };
        }
        if self.failure_claimed {
            return None;
        }
        let failure = self
            .reported
            .as_ref()
            .and_then(|completion| reader_failure(completion, stream, OUTPUT_DRAIN_GRACE, "process"));
        if failure.is_some() {
            self.failure_claimed = true;
        }
        failure
    }
}

trait SpillFile: io::Read + io::Write + io::Seek + Send + fmt::Debug {}

impl<T> SpillFile for T where T: io::Read + io::Write + io::Seek + Send + fmt::Debug {}

type SpillFactory = Box<dyn FnMut() -> io::Result<Box<dyn SpillFile>> + Send>;

#[derive(Debug)]
enum CapturedOutput {
    Memory(Vec<u8>),
    Spill(Box<dyn SpillFile>),
}

impl CapturedOutput {
    fn empty() -> Self {
        Self::Memory(Vec::new())
    }

    fn append(&mut self, bytes: &[u8], memory_limit: usize, spill_factory: &mut SpillFactory) -> io::Result<()> {
        match self {
            Self::Memory(memory) if memory.len().saturating_add(bytes.len()) <= memory_limit => {
                memory.extend_from_slice(bytes);
                Ok(())
            }
            Self::Memory(memory) => {
                let mut spill = spill_factory()?;
                spill.write_all(memory)?;
                spill.write_all(bytes)?;
                *self = Self::Spill(spill);
                Ok(())
            }
            Self::Spill(spill) => spill.write_all(bytes),
        }
    }

    fn emit_to(&mut self, destination: &mut dyn io::Write) -> Result<(), OutputEmitError> {
        match self {
            Self::Memory(bytes) => destination.write_all(bytes).map_err(OutputEmitError::Destination),
            Self::Spill(spill) => {
                spill.seek(SeekFrom::Start(0)).map_err(OutputEmitError::Source)?;
                let mut buffer = [0_u8; 8192];
                loop {
                    let read = spill.read(&mut buffer).map_err(OutputEmitError::Source)?;
                    if read == 0 {
                        return Ok(());
                    }
                    destination.write_all(&buffer[..read]).map_err(OutputEmitError::Destination)?;
                }
            }
        }
    }

    #[cfg(test)]
    fn is_spilled(&self) -> bool {
        matches!(self, Self::Spill(_))
    }
}

#[derive(Debug)]
enum OutputEmitError {
    Source(io::Error),
    Destination(io::Error),
}

#[derive(Debug)]
struct CapturedStream {
    output: CapturedOutput,
    failure: Option<String>,
}

#[derive(Debug)]
struct TreeOutcome {
    result: InvocationResult,
}

impl TreeOutcome {
    fn new(result: InvocationResult) -> Self {
        Self { result }
    }
}

#[derive(Debug)]
struct BufferedOutcome {
    stdout: CapturedOutput,
    stderr: CapturedOutput,
    result: InvocationResult,
}

impl BufferedOutcome {
    fn infrastructure(message: String) -> Self {
        Self {
            stdout: CapturedOutput::empty(),
            stderr: CapturedOutput::empty(),
            result: InvocationResult::Infrastructure(message),
        }
    }

    fn from_reader_failure<T>(message: String, stdout_reader: OutputReader, cleanup: &io::Result<T>) -> Self {
        let message = with_cleanup_failure(message, cleanup);
        combine_captured_output(
            finish_output_reader(stdout_reader, "stdout", OUTPUT_DRAIN_GRACE, "process group"),
            CapturedStream {
                output: CapturedOutput::empty(),
                failure: None,
            },
            InvocationResult::Infrastructure(message),
        )
    }
}

#[derive(Debug)]
enum InvocationResult {
    Exited(ExitStatus),
    TimedOut(Duration),
    Infrastructure(String),
}

impl InvocationResult {
    fn failed(&self) -> bool {
        match self {
            Self::Exited(status) => !status.success(),
            Self::TimedOut(_) | Self::Infrastructure(_) => true,
        }
    }
}

/// Render an argv for display (`--dry-run`). Best-effort quoting for
/// readability only — nothing consumes this as input.
fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.contains(char::is_whitespace) {
                format!("\"{a}\"")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Reduce a raw process exit code to the `u8` that [`ExitCode`] can carry.
fn exit_byte(raw: Option<i32>) -> u8 {
    let Some(raw) = raw else { return 1 };
    let byte = u8::try_from(raw & 0xFF).expect("`raw & 0xFF` masks to the low byte, always within 0..=255");
    if byte == 0 { 1 } else { byte }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::collections::VecDeque;
    use std::num::NonZeroUsize;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt as _;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt as _;
    use std::process::{Command, ExitCode, ExitStatus, Stdio};
    use std::sync::{Arc, Condvar, Mutex, mpsc};
    use std::time::{Duration, Instant};
    use std::{io, thread};

    use super::{
        BufferedOutcome, CapturedOutput, CapturedStream, Invocation, InvocationResult, OutputEmitError, OutputReader, Plan,
        ReaderCompletion, RunningWorker, SpillFile, TreeOutcome, WORKER_PANIC_TEST_PROGRAM, WORKER_SPAWN_ERROR_TEST_PROGRAM,
        add_infrastructure_failure, combine_captured_output, detach_group_reaper, display_duration, effective_worker_count,
        emit_buffered_to, execute_parallel, exit_byte, failure_stops_launching, finish_output_reader, panic_description,
        parallel_failure_exit_code, poll_process_exit, run_captured, run_captured_with, run_streamed, run_streamed_with_timeout,
        run_streamed_with_timeout_with, spawn_child_stderr_reader, spawn_child_stdout_reader, spawn_group, spawn_output_reader,
        spawn_output_reader_with, spawn_worker, take_reader_output, terminate_group_bounded, terminate_group_with, wait_for_process,
        wait_for_worker, with_cleanup_failure, with_reaper_handoff,
    };

    const LEADER_BOUNDARY: &str = "process leader";
    const GROUP_BOUNDARY: &str = "process group";

    fn invocation(argv: &[&str]) -> Invocation {
        Invocation {
            label: None,
            argv: argv.iter().map(|value| (*value).to_owned()).collect(),
            work_dir: None,
        }
    }

    fn successful_status() -> ExitStatus {
        ExitStatus::from_raw(0)
    }

    #[cfg(unix)]
    fn failed_status(code: i32) -> ExitStatus {
        ExitStatus::from_raw(code << 8)
    }

    #[cfg(windows)]
    fn failed_status(code: i32) -> ExitStatus {
        ExitStatus::from_raw(u32::try_from(code).expect("test exit code is nonnegative"))
    }

    fn result_infrastructure_message(result: InvocationResult) -> String {
        let InvocationResult::Infrastructure(message) = result else {
            panic!("the test expects an infrastructure outcome");
        };
        message
    }

    fn infrastructure_message(outcome: BufferedOutcome) -> String {
        result_infrastructure_message(outcome.result)
    }

    fn output_bytes(output: &mut CapturedOutput) -> Vec<u8> {
        let mut bytes = Vec::new();
        match output.emit_to(&mut bytes) {
            Ok(()) => bytes,
            Err(OutputEmitError::Source(error) | OutputEmitError::Destination(error)) => {
                panic!("captured test output cannot be read: {error}")
            }
        }
    }

    fn captured(bytes: &[u8], failure: Option<&str>) -> CapturedStream {
        CapturedStream {
            output: CapturedOutput::Memory(bytes.to_vec()),
            failure: failure.map(str::to_owned),
        }
    }

    struct FakeProcess {
        observations: VecDeque<io::Result<Option<ExitStatus>>>,
        termination: Option<io::Result<ExitStatus>>,
    }

    impl FakeProcess {
        fn observe(&mut self) -> io::Result<Option<ExitStatus>> {
            self.observations.pop_front().unwrap_or(Ok(None))
        }

        fn terminate(mut self) -> io::Result<ExitStatus> {
            self.termination
                .take()
                .unwrap_or_else(|| Err(io::Error::other("unexpected termination")))
        }
    }

    struct PendingReader {
        dropped: Option<mpsc::Sender<()>>,
    }

    impl io::Read for PendingReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        }
    }

    impl Drop for PendingReader {
        fn drop(&mut self) {
            if let Some(dropped) = self.dropped.take() {
                let _receiver_gone = dropped.send(());
            }
        }
    }

    struct BlockingReader {
        first_read: bool,
        blocked: Option<mpsc::Sender<()>>,
        finished: mpsc::Sender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl io::Read for BlockingReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.first_read {
                self.first_read = true;
                let bytes = b"captured-before-detach";
                buf[..bytes.len()].copy_from_slice(bytes);
                return Ok(bytes.len());
            }
            if let Some(blocked) = self.blocked.take() {
                let _receiver_gone = blocked.send(());
            }
            let (lock, condition) = &*self.release;
            let mut released = lock.lock().expect("the test owns the release mutex without panicking");
            while !*released {
                released = condition.wait(released).expect("the test owns the release mutex without panicking");
            }
            let _receiver_gone = self.finished.send(());
            Ok(0)
        }
    }

    struct DropSignalReader {
        bytes: Option<&'static [u8]>,
        dropped: Option<mpsc::Sender<()>>,
    }

    impl io::Read for DropSignalReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let Some(bytes) = self.bytes.take() else {
                return Ok(0);
            };
            buf[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        }
    }

    impl Drop for DropSignalReader {
        fn drop(&mut self) {
            if let Some(dropped) = self.dropped.take() {
                let _receiver_gone = dropped.send(());
            }
        }
    }

    struct LateDataReader {
        started: Option<mpsc::Sender<()>>,
        finished: Option<mpsc::Sender<()>>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl io::Read for LateDataReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if let Some(started) = self.started.take() {
                let _receiver_gone = started.send(());
            }
            let (lock, condition) = &*self.release;
            let mut released = lock.lock().expect("the test owns the release mutex without panicking");
            while !*released {
                released = condition.wait(released).expect("the test owns the release mutex without panicking");
            }
            let bytes = b"late data";
            buf[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        }
    }

    impl Drop for LateDataReader {
        fn drop(&mut self) {
            if let Some(finished) = self.finished.take() {
                let _receiver_gone = finished.send(());
            }
        }
    }

    struct InterruptedThenData {
        interrupted: bool,
        emitted: bool,
    }

    impl io::Read for InterruptedThenData {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            if self.emitted {
                return Ok(0);
            }
            self.emitted = true;
            let bytes = b"after interrupt";
            buf[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        }
    }

    struct FailingReader;

    impl io::Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("injected read failure"))
        }
    }

    struct PanickingReader;

    impl io::Read for PanickingReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            panic!("injected reader panic");
        }
    }

    struct ChunkedReader {
        chunks: VecDeque<Vec<u8>>,
    }

    impl io::Read for ChunkedReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let Some(chunk) = self.chunks.pop_front() else {
                return Ok(0);
            };
            buf[..chunk.len()].copy_from_slice(&chunk);
            Ok(chunk.len())
        }
    }

    #[derive(Debug)]
    struct FaultySpill {
        cursor: io::Cursor<Vec<u8>>,
        fail_write: bool,
        fail_read: bool,
    }

    impl io::Write for FaultySpill {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.fail_write {
                Err(io::Error::other("injected spill write failure"))
            } else {
                io::Write::write(&mut self.cursor, buf)
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            io::Write::flush(&mut self.cursor)
        }
    }

    impl io::Read for FaultySpill {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.fail_read {
                Err(io::Error::other("injected spill read failure"))
            } else {
                io::Read::read(&mut self.cursor, buf)
            }
        }
    }

    impl io::Seek for FaultySpill {
        fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
            io::Seek::seek(&mut self.cursor, pos)
        }
    }

    struct FailingWriter;

    impl io::Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("injected destination write failure"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn sleeping_test_command() -> Command {
        let mut command = Command::new(std::env::current_exe().expect("the test binary knows its path"));
        let _ = command
            .args(["--exact", "run::tests::child_sleep_probe", "--nocapture"])
            .env("CARGO_EACH_CHILD_SLEEP_MS", "30000")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    #[test]
    fn child_sleep_probe() {
        if let Some(duration) = std::env::var_os("CARGO_EACH_CHILD_SLEEP_MS") {
            let millis = duration.to_string_lossy().parse().expect("the parent passes milliseconds");
            thread::sleep(Duration::from_millis(millis));
        }
    }

    #[test]
    fn exit_codes_and_durations_follow_the_cli_contract() {
        assert_eq!(exit_byte(None), 1);
        assert_eq!(exit_byte(Some(7)), 7);
        assert_eq!(exit_byte(Some(259)), 3);
        assert_eq!(exit_byte(Some(256)), 1);
        assert_eq!(display_duration(Duration::from_millis(250)), "250ms");
        assert_eq!(display_duration(Duration::from_secs(30)), "30s");
        assert_eq!(display_duration(Duration::from_mins(2)), "2m");
    }

    #[test]
    fn worker_count_and_launch_policy_are_plan_bounded() {
        let four = NonZeroUsize::new(4).expect("literal four is nonzero");
        assert_eq!(effective_worker_count(four, 1), NonZeroUsize::MIN);
        assert_eq!(effective_worker_count(four, 8), four);
        assert!(failure_stops_launching(false, true));
        assert!(!failure_stops_launching(false, false));
        assert!(!failure_stops_launching(true, true));
    }

    #[test]
    fn sequential_and_parallel_failures_preserve_their_taxonomy() {
        let plan = Plan {
            invocations: vec![invocation(&["first"]), invocation(&["second"])],
        };
        let mut results = VecDeque::from([
            InvocationResult::TimedOut(Duration::from_millis(50)),
            InvocationResult::Exited(successful_status()),
        ]);
        let mut calls = 0;
        let fail_fast = super::execute_sequential_with(&plan, false, Some(Duration::from_millis(50)), |_, timeout| {
            assert_eq!(timeout, Some(Duration::from_millis(50)));
            calls += 1;
            results.pop_front().expect("one result per launched invocation")
        });
        assert_eq!(fail_fast, ExitCode::from(1));
        assert_eq!(calls, 1);
        assert_eq!(
            parallel_failure_exit_code(&InvocationResult::Exited(failed_status(7))),
            ExitCode::from(7)
        );
        assert_eq!(
            parallel_failure_exit_code(&InvocationResult::Infrastructure("capture failed".to_owned())),
            ExitCode::from(2)
        );
        assert_eq!(
            parallel_failure_exit_code(&InvocationResult::TimedOut(Duration::from_secs(1))),
            ExitCode::from(1)
        );
    }

    #[test]
    fn scheduler_surfaces_worker_launch_failures_in_both_policies() {
        let fail_fast = Plan {
            invocations: vec![invocation(&[WORKER_SPAWN_ERROR_TEST_PROGRAM]), invocation(&["rustc", "--version"])],
        };
        let code = execute_parallel(&fail_fast, false, NonZeroUsize::new(2).expect("literal two is nonzero"), None)
            .expect("worker launch failure is an invocation outcome");
        assert_eq!(code, ExitCode::from(2));

        let keep_going = Plan {
            invocations: vec![invocation(&[WORKER_SPAWN_ERROR_TEST_PROGRAM]), invocation(&["rustc", "--version"])],
        };
        let code = execute_parallel(&keep_going, true, NonZeroUsize::new(2).expect("literal two is nonzero"), None)
            .expect("keep-going retains worker launch failures");
        assert_eq!(code, ExitCode::from(1));
    }

    #[test]
    fn process_waiting_handles_completion_timeout_and_cleanup_failures() {
        let completed = FakeProcess {
            observations: VecDeque::from([Ok(None), Ok(Some(successful_status()))]),
            termination: None,
        };
        let outcome = wait_for_process(
            completed,
            None,
            FakeProcess::observe,
            || None,
            FakeProcess::terminate,
            "observe fake process",
        );
        assert!(matches!(outcome.result, InvocationResult::Exited(status) if status.success()));

        let timed_out = FakeProcess {
            observations: VecDeque::from([Ok(None)]),
            termination: Some(Ok(successful_status())),
        };
        let outcome = wait_for_process(
            timed_out,
            Some(Duration::ZERO),
            FakeProcess::observe,
            || None,
            FakeProcess::terminate,
            "observe fake process",
        );
        assert!(matches!(outcome.result, InvocationResult::TimedOut(duration) if duration.is_zero()));

        let failed = FakeProcess {
            observations: VecDeque::from([Err(io::Error::other("observation failed"))]),
            termination: Some(Err(io::Error::other("cleanup failed"))),
        };
        let outcome = wait_for_process(
            failed,
            None,
            FakeProcess::observe,
            || None,
            FakeProcess::terminate,
            "observe fake process",
        );
        let message = result_infrastructure_message(outcome.result);
        assert!(message.contains("observation failed"));
        assert!(message.contains("cleanup failed"));

        let failed_timeout_cleanup = FakeProcess {
            observations: VecDeque::from([Ok(None)]),
            termination: Some(Err(io::Error::other("timeout cleanup failed"))),
        };
        let outcome = wait_for_process(
            failed_timeout_cleanup,
            Some(Duration::ZERO),
            FakeProcess::observe,
            || None,
            FakeProcess::terminate,
            "observe fake process",
        );
        assert!(result_infrastructure_message(outcome.result).contains("timeout cleanup failed"));
    }

    #[test]
    fn reader_failure_terminates_the_process_through_the_local_wait_seam() {
        let process = FakeProcess {
            observations: VecDeque::from([Ok(None)]),
            termination: Some(Ok(successful_status())),
        };
        let outcome = wait_for_process(
            process,
            None,
            FakeProcess::observe,
            || Some("failed to read child stdout".to_owned()),
            FakeProcess::terminate,
            "observe fake process",
        );
        assert!(result_infrastructure_message(outcome.result).contains("failed to read child stdout"));
    }

    #[test]
    fn bounded_polling_stops_on_exit_error_or_deadline() {
        let mut exited = VecDeque::from([Ok(None), Ok(Some(successful_status()))]);
        let status = poll_process_exit(&mut exited, Duration::from_secs(1), |observations| {
            observations.pop_front().expect("the fake has enough observations")
        })
        .expect("polling succeeds")
        .expect("the fake exits");
        assert!(status.success());

        let mut failed = VecDeque::from([Err(io::Error::other("poll failed"))]);
        let error = poll_process_exit(&mut failed, Duration::from_secs(1), |observations| {
            observations.pop_front().expect("the fake has one observation")
        })
        .expect_err("polling failure propagates");
        assert!(error.to_string().contains("poll failed"));

        let mut running = ();
        assert!(
            poll_process_exit(&mut running, Duration::ZERO, |()| Ok(None))
                .expect("deadline is not an I/O failure")
                .is_none()
        );
    }

    #[test]
    fn bounded_group_termination_reports_every_local_failure_shape() {
        let status = successful_status();
        let kill_error = terminate_group_with(
            (),
            Duration::from_secs(1),
            |()| Err(io::Error::new(io::ErrorKind::PermissionDenied, "kill failed")),
            move |()| Ok(Some(status)),
            |()| Ok(()),
        )
        .expect_err("a kill error is not hidden by later completion");
        assert_eq!(kill_error.kind(), io::ErrorKind::PermissionDenied);

        let deadline = terminate_group_with((), Duration::ZERO, |()| Ok(()), |()| Ok(None), |()| Ok(()))
            .expect_err("an unreaped group reaches the deadline");
        assert_eq!(deadline.kind(), io::ErrorKind::WouldBlock);
        assert!(deadline.to_string().contains("detached reaper thread"));

        let failed_handoff = terminate_group_with(
            (),
            Duration::ZERO,
            |()| Err(io::Error::other("kill failed")),
            |()| Ok(None),
            |()| Err(io::Error::other("reaper failed")),
        )
        .expect_err("kill and handoff failures are both reported");
        assert!(failed_handoff.to_string().contains("kill failed"));
        assert!(failed_handoff.to_string().contains("reaper failed"));

        let failed_observation = terminate_group_with(
            (),
            Duration::from_secs(1),
            |()| Ok(()),
            |()| Err(io::Error::other("observation failed")),
            |()| Ok(()),
        )
        .expect_err("post-kill observation failure is reported");
        assert!(failed_observation.to_string().contains("observation failed"));
        assert!(failed_observation.to_string().contains("detached reaper thread"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns process groups")]
    fn real_group_execution_observes_completion_and_timeout() {
        let success = run_streamed_with_timeout(&invocation(&["rustc", "--version"]), Duration::from_secs(5));
        assert!(matches!(success, InvocationResult::Exited(status) if status.success()));

        let group = spawn_group(sleeping_test_command()).expect("spawn sleeping process group");
        let started = Instant::now();
        let error = terminate_group_bounded(group).expect("killed process group is reaped");
        assert!(!error.success());
        assert!(started.elapsed() < Duration::from_secs(2));

        let mut quick = Command::new("rustc");
        let _ = quick.arg("--version").stdout(Stdio::null()).stderr(Stdio::null());
        let group = spawn_group(quick).expect("spawn quick process group");
        detach_group_reaper(group).expect("detach the local process-group reaper");
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns and captures process groups")]
    fn captured_runner_uses_group_control_and_keeps_output() {
        let mut untimed = run_captured(&invocation(&["rustc", "--version"]), None);
        assert!(matches!(untimed.result, InvocationResult::Exited(status) if status.success()));
        assert!(
            String::from_utf8(output_bytes(&mut untimed.stdout))
                .expect("rustc output is UTF-8")
                .contains("rustc")
        );

        let timed = run_captured(&invocation(&["rustc", "--version"]), Some(Duration::from_secs(5)));
        assert!(matches!(timed.result, InvocationResult::Exited(status) if status.success()));
    }

    #[test]
    fn direct_runners_report_empty_and_unspawnable_commands() {
        let empty = invocation(&[]);
        assert!(matches!(run_streamed(&empty), InvocationResult::Infrastructure(message) if message.contains("empty argument vector")));
        assert!(infrastructure_message(run_captured(&empty, None)).contains("empty argument vector"));
        assert!(matches!(
            run_streamed_with_timeout(&empty, Duration::from_secs(1)),
            InvocationResult::Infrastructure(message) if message.contains("empty argument vector")
        ));

        let missing = invocation(&["__cargo_each_missing_program_for_unit_test__"]);
        assert!(matches!(run_streamed(&missing), InvocationResult::Infrastructure(message) if message.contains("failed to spawn")));
        assert!(infrastructure_message(run_captured(&missing, None)).contains("failed to spawn"));

        let injected = run_streamed_with_timeout_with(&invocation(&["rustc", "--version"]), Duration::from_secs(1), |_| {
            Err("injected group spawn failure".to_owned())
        });
        assert!(matches!(
            injected,
            InvocationResult::Infrastructure(message) if message.contains("injected group spawn failure")
        ));
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns process groups with injected capture seams")]
    fn captured_setup_failures_use_local_reader_and_process_seams() {
        let invocation = invocation(&["rustc", "--version"]);

        let missing_stdout = run_captured_with(
            &invocation,
            None,
            |command| {
                let mut group = spawn_group(command)?;
                drop(group.inner().stdout.take());
                Ok(group)
            },
            spawn_child_stdout_reader,
            spawn_child_stderr_reader,
        );
        assert!(infrastructure_message(missing_stdout).contains("failed to capture child stdout"));

        let stdout_reader = run_captured_with(
            &invocation,
            None,
            spawn_group,
            |_stream, _name| Err(io::Error::other("injected stdout reader failure")),
            spawn_child_stderr_reader,
        );
        assert!(infrastructure_message(stdout_reader).contains("injected stdout reader failure"));

        let missing_stderr = run_captured_with(
            &invocation,
            None,
            |command| {
                let mut group = spawn_group(command)?;
                drop(group.inner().stderr.take());
                Ok(group)
            },
            spawn_child_stdout_reader,
            spawn_child_stderr_reader,
        );
        assert!(infrastructure_message(missing_stderr).contains("failed to capture child stderr"));

        let stderr_reader = run_captured_with(&invocation, None, spawn_group, spawn_child_stdout_reader, |_stream, _name| {
            Err(io::Error::other("injected stderr reader failure"))
        });
        assert!(infrastructure_message(stderr_reader).contains("injected stderr reader failure"));
    }

    #[test]
    fn output_reader_retries_interrupts_and_reports_failures() {
        let interrupted = spawn_output_reader(
            InterruptedThenData {
                interrupted: false,
                emitted: false,
            },
            "interrupted-reader",
        )
        .expect("spawn interrupted reader");
        let mut interrupted = finish_output_reader(interrupted, "stdout", Duration::from_secs(1), GROUP_BOUNDARY);
        assert_eq!(output_bytes(&mut interrupted.output), b"after interrupt");
        assert!(interrupted.failure.is_none());

        let failed = spawn_output_reader(FailingReader, "failing-reader").expect("spawn failing reader");
        let failed = finish_output_reader(failed, "stdout", Duration::from_secs(1), GROUP_BOUNDARY);
        assert!(failed.failure.is_some_and(|failure| failure.contains("injected read failure")));

        let panicked = spawn_output_reader(PanickingReader, "panicking-reader").expect("spawn panicking reader");
        let panicked = finish_output_reader(panicked, "stderr", Duration::from_secs(1), GROUP_BOUNDARY);
        assert!(panicked.failure.is_some_and(|failure| failure.contains("injected reader panic")));
    }

    #[test]
    fn reader_finish_reports_post_completion_panics_and_failed_cancellation() {
        let (sender, completion) = mpsc::channel();
        let thread = thread::spawn(move || {
            sender.send(ReaderCompletion::Finished(Ok(()))).expect("the receiver remains alive");
            panic!("panic after completion");
        });
        let reader = OutputReader {
            thread,
            completion,
            output: Arc::new(Mutex::new(CapturedOutput::empty())),
            retaining: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            reported: None,
            failure_claimed: false,
        };
        let failure = finish_output_reader(reader, "stdout", Duration::from_secs(1), LEADER_BOUNDARY)
            .failure
            .expect("the join panic is reported");
        assert!(failure.contains("panic after completion"));

        let (failed_sender, failed_completion) = mpsc::channel();
        let failed_thread = thread::spawn(move || {
            failed_sender
                .send(ReaderCompletion::Finished(Err(io::Error::other("read failed"))))
                .expect("the receiver remains alive");
            panic!("panic after failed completion");
        });
        let failed_reader = OutputReader {
            thread: failed_thread,
            completion: failed_completion,
            output: Arc::new(Mutex::new(CapturedOutput::empty())),
            retaining: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            reported: None,
            failure_claimed: false,
        };
        let failure = finish_output_reader(failed_reader, "stdout", Duration::from_secs(1), LEADER_BOUNDARY)
            .failure
            .expect("read and join failures are reported");
        assert!(failure.contains("read failed"));
        assert!(failure.contains("panic after failed completion"));

        let (sender, completion) = mpsc::channel::<ReaderCompletion>();
        let reader = OutputReader {
            thread: thread::spawn(|| {}),
            completion,
            output: Arc::new(Mutex::new(CapturedOutput::empty())),
            retaining: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            reported: None,
            failure_claimed: true,
        };
        let failure = finish_output_reader(reader, "stderr", Duration::ZERO, LEADER_BOUNDARY)
            .failure
            .expect("failed cancellation is reported");
        assert!(failure.contains("reader did not stop"));
        drop(sender);
    }

    #[test]
    fn disconnected_reader_completion_is_reported_by_the_finisher() {
        let (sender, completion) = mpsc::channel::<ReaderCompletion>();
        drop(sender);
        let reader = OutputReader {
            thread: thread::spawn(|| {}),
            completion,
            output: Arc::new(Mutex::new(CapturedOutput::empty())),
            retaining: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            reported: None,
            failure_claimed: false,
        };
        let failure = finish_output_reader(reader, "stdout", Duration::from_secs(1), LEADER_BOUNDARY)
            .failure
            .expect("disconnection is reported");
        assert!(failure.contains("without reporting completion"));
    }

    #[test]
    fn cancellable_reader_stops_within_the_join_grace() {
        let (dropped_tx, dropped_rx) = mpsc::channel();
        let reader = spawn_output_reader(PendingReader { dropped: Some(dropped_tx) }, "pending-reader").expect("spawn pending reader");
        let captured = finish_output_reader(reader, "stdout", Duration::from_millis(25), LEADER_BOUNDARY);
        dropped_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("the polling reader observes capture cancellation");
        let failure = captured.failure.expect("an open pipe is an infrastructure failure");
        assert!(failure.contains("remained open"));
        assert!(!failure.contains("did not stop"));
    }

    #[test]
    fn blocking_reader_is_detached_and_partial_output_is_recovered_without_blocking() {
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let (blocked_tx, blocked_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let reader = spawn_output_reader(
            BlockingReader {
                first_read: false,
                blocked: Some(blocked_tx),
                finished: finished_tx,
                release: Arc::clone(&release),
            },
            "blocking-reader",
        )
        .expect("spawn blocking reader");
        blocked_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("the reader reaches its blocking read");

        let started = Instant::now();
        let mut captured = finish_output_reader(reader, "stdout", Duration::from_millis(25), LEADER_BOUNDARY);
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(output_bytes(&mut captured.output), b"captured-before-detach");
        let failure = captured.failure.expect("detachment is an infrastructure failure");
        assert!(failure.contains("remained open"));
        assert!(failure.contains("did not stop"));

        let (lock, condition) = &*release;
        *lock.lock().expect("the test owns the release mutex without panicking") = true;
        condition.notify_all();
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("the detached reader exits after the pipe closes");
    }

    #[test]
    fn detached_reader_never_blocks_on_a_locked_capture_buffer() {
        let (factory_started, factory_is_started) = mpsc::sync_channel(0);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let factory_release = Arc::clone(&release);
        let (dropped_tx, dropped_rx) = mpsc::channel();
        let reader = spawn_output_reader_with(
            DropSignalReader {
                bytes: Some(b"stalled append"),
                dropped: Some(dropped_tx),
            },
            "locked-buffer-reader",
            0,
            Box::new(move || {
                factory_started.send(()).expect("the finisher waits for spill creation");
                let (lock, condition) = &*factory_release;
                let mut released = lock.lock().expect("the test owns the release mutex without panicking");
                while !*released {
                    released = condition.wait(released).expect("the test owns the release mutex without panicking");
                }
                Ok(Box::new(io::Cursor::new(Vec::new())) as Box<dyn SpillFile>)
            }),
        )
        .expect("spawn locked-buffer reader");
        factory_is_started
            .recv_timeout(Duration::from_secs(1))
            .expect("the reader holds the capture mutex");

        let started = Instant::now();
        let mut captured = finish_output_reader(reader, "stdout", Duration::ZERO, LEADER_BOUNDARY);
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(output_bytes(&mut captured.output).is_empty());
        let failure = captured.failure.expect("unavailable partial bytes are explicit");
        assert!(failure.contains("capture buffer remained locked"));

        let (lock, condition) = &*release;
        *lock.lock().expect("the test owns the release mutex without panicking") = true;
        condition.notify_all();
        dropped_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("the detached reader exits after spill creation resumes");
    }

    #[test]
    fn data_arriving_after_capture_cancellation_is_discarded() {
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let reader = spawn_output_reader(
            LateDataReader {
                started: Some(started_tx),
                finished: Some(finished_tx),
                release: Arc::clone(&release),
            },
            "late-data-reader",
        )
        .expect("spawn late-data reader");
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("the reader blocks before producing data");

        let mut captured = finish_output_reader(reader, "stdout", Duration::ZERO, LEADER_BOUNDARY);
        assert!(output_bytes(&mut captured.output).is_empty());
        assert!(captured.failure.is_some());

        let (lock, condition) = &*release;
        *lock.lock().expect("the test owns the release mutex without panicking") = true;
        condition.notify_all();
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("the detached reader discards late data and exits");
    }

    #[test]
    fn output_spills_after_the_memory_limit_and_cleans_up_by_raii() {
        let named = tempfile::NamedTempFile::new().expect("create named spill file");
        let path = named.path().to_path_buf();
        let mut named = Some(named);
        let reader = spawn_output_reader_with(
            ChunkedReader {
                chunks: VecDeque::from([b"abcd".to_vec(), b"efgh".to_vec(), b"ijkl".to_vec()]),
            },
            "spill-reader",
            4,
            Box::new(move || Ok(Box::new(named.take().expect("capture creates one spill")) as Box<dyn SpillFile>)),
        )
        .expect("spawn spill reader");
        let mut captured = finish_output_reader(reader, "stdout", Duration::from_secs(1), LEADER_BOUNDARY);
        assert!(captured.output.is_spilled());
        assert_eq!(output_bytes(&mut captured.output), b"abcdefghijkl");
        assert!(path.exists());
        drop(captured);
        assert!(!path.exists());
    }

    #[test]
    fn output_at_the_memory_limit_does_not_create_a_spill() {
        let mut output = CapturedOutput::empty();
        let mut spill_factory: super::SpillFactory = Box::new(|| panic!("output at the limit must stay in memory"));

        output.append(b"abcd", 4, &mut spill_factory).expect("in-memory append succeeds");

        assert!(!output.is_spilled());
        assert_eq!(output_bytes(&mut output), b"abcd");
    }

    #[test]
    fn spill_failures_become_infrastructure_failures() {
        let reader = spawn_output_reader_with(
            ChunkedReader {
                chunks: VecDeque::from([b"abc".to_vec(), b"def".to_vec()]),
            },
            "write-failing-spill",
            4,
            Box::new(|| {
                Ok(Box::new(FaultySpill {
                    cursor: io::Cursor::new(Vec::new()),
                    fail_write: true,
                    fail_read: false,
                }) as Box<dyn SpillFile>)
            }),
        )
        .expect("spawn write-failing reader");
        let captured_stream = finish_output_reader(reader, "stdout", Duration::from_secs(1), LEADER_BOUNDARY);
        let outcome = combine_captured_output(captured_stream, captured(b"", None), InvocationResult::Exited(successful_status()));
        assert!(infrastructure_message(outcome).contains("injected spill write failure"));

        let mut outcome = BufferedOutcome {
            stdout: CapturedOutput::Spill(Box::new(FaultySpill {
                cursor: io::Cursor::new(Vec::new()),
                fail_write: false,
                fail_read: true,
            })),
            stderr: CapturedOutput::empty(),
            result: InvocationResult::Exited(successful_status()),
        };
        emit_buffered_to(&invocation(&["probe"]), &mut outcome, &mut Vec::new(), &mut Vec::new()).expect("destinations remain writable");
        assert!(infrastructure_message(outcome).contains("injected spill read failure"));

        let mut stderr_outcome = BufferedOutcome {
            stdout: CapturedOutput::empty(),
            stderr: CapturedOutput::Spill(Box::new(FaultySpill {
                cursor: io::Cursor::new(Vec::new()),
                fail_write: false,
                fail_read: true,
            })),
            result: InvocationResult::Exited(successful_status()),
        };
        emit_buffered_to(&invocation(&["probe"]), &mut stderr_outcome, &mut Vec::new(), &mut Vec::new())
            .expect("destinations remain writable");
        assert!(infrastructure_message(stderr_outcome).contains("injected spill read failure"));
    }

    #[test]
    fn capture_and_emission_preserve_all_failure_context() {
        for (stdout, stderr, expected) in [
            (captured(b"out", Some("stdout failed")), captured(b"err", None), "stdout failed"),
            (captured(b"out", None), captured(b"err", Some("stderr failed")), "stderr failed"),
            (
                captured(b"out", Some("stdout failed")),
                captured(b"err", Some("stderr failed")),
                "stdout failed; stderr failed",
            ),
        ] {
            let outcome = combine_captured_output(stdout, stderr, InvocationResult::Exited(successful_status()));
            assert_eq!(infrastructure_message(outcome), expected);
        }

        let mut outcome = BufferedOutcome {
            stdout: CapturedOutput::Memory(b"stdout".to_vec()),
            stderr: CapturedOutput::empty(),
            result: InvocationResult::Exited(successful_status()),
        };
        let error = emit_buffered_to(&invocation(&["probe"]), &mut outcome, &mut FailingWriter, &mut Vec::new())
            .expect_err("destination failure propagates");
        assert!(error.to_string().contains("injected destination write failure"));

        let mut stderr_failure = BufferedOutcome {
            stdout: CapturedOutput::empty(),
            stderr: CapturedOutput::Memory(b"stderr".to_vec()),
            result: InvocationResult::Exited(successful_status()),
        };
        let error = emit_buffered_to(&invocation(&["probe"]), &mut stderr_failure, &mut Vec::new(), &mut FailingWriter)
            .expect_err("stderr destination failure propagates");
        assert!(error.to_string().contains("injected destination write failure"));

        for result in [
            InvocationResult::TimedOut(Duration::from_millis(10)),
            InvocationResult::Infrastructure("infrastructure".to_owned()),
        ] {
            let mut diagnostic = BufferedOutcome {
                stdout: CapturedOutput::empty(),
                stderr: CapturedOutput::empty(),
                result,
            };
            let mut stderr = Vec::new();
            emit_buffered_to(&invocation(&["probe"]), &mut diagnostic, &mut Vec::new(), &mut stderr).expect("memory output succeeds");
            assert!(!stderr.is_empty());
        }
    }

    #[test]
    fn infrastructure_failure_merging_preserves_primary_context() {
        assert!(matches!(
            add_infrastructure_failure(InvocationResult::Exited(successful_status()), String::new()),
            InvocationResult::Exited(status) if status.success()
        ));
        assert_eq!(
            result_infrastructure_message(add_infrastructure_failure(
                InvocationResult::TimedOut(Duration::from_millis(10)),
                "drain failed".to_owned(),
            )),
            "invocation timed out after 10ms; drain failed"
        );
        assert_eq!(
            result_infrastructure_message(add_infrastructure_failure(
                InvocationResult::Infrastructure("wait failed".to_owned()),
                "drain failed".to_owned(),
            )),
            "wait failed; drain failed"
        );
    }

    #[test]
    fn reader_setup_failure_keeps_partial_output_and_cleanup_context() {
        let reader = spawn_output_reader(io::Cursor::new(b"partial".to_vec()), "partial-reader").expect("spawn partial reader");
        let mut outcome = BufferedOutcome::from_reader_failure(
            "stderr unavailable".to_owned(),
            reader,
            &Err::<(), _>(io::Error::other("cleanup failed")),
        );
        assert_eq!(output_bytes(&mut outcome.stdout), b"partial");
        let message = infrastructure_message(outcome);
        assert!(message.contains("stderr unavailable"));
        assert!(message.contains("cleanup failed"));
    }

    #[test]
    fn reader_state_reports_disconnection_and_never_repeats_a_failure() {
        let (sender, completion) = mpsc::channel::<ReaderCompletion>();
        drop(sender);
        let mut reader = OutputReader {
            thread: thread::spawn(|| {}),
            completion,
            output: Arc::new(Mutex::new(CapturedOutput::empty())),
            retaining: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            reported: None,
            failure_claimed: false,
        };
        assert!(
            reader
                .take_failure("stdout")
                .is_some_and(|failure| failure.contains("without reporting completion"))
        );
        assert!(reader.take_failure("stdout").is_none());
        assert!(
            finish_output_reader(reader, "stdout", Duration::from_secs(1), LEADER_BOUNDARY)
                .failure
                .is_none()
        );
    }

    #[test]
    fn detached_output_recovery_uses_only_nonblocking_mutex_acquisition() {
        let output = Arc::new(Mutex::new(CapturedOutput::Memory(b"partial".to_vec())));
        let held = output.lock().expect("the fresh capture mutex is available");
        let mut failure = None;
        let mut captured = take_reader_output(&output, "stdout", false, &mut failure);
        assert!(output_bytes(&mut captured).is_empty());
        assert!(failure.is_some_and(|message| message.contains("partial output could not be recovered")));
        drop(held);
    }

    #[test]
    fn poisoned_capture_buffers_are_recovered_in_joined_and_detached_modes() {
        fn poisoned_output() -> Arc<Mutex<CapturedOutput>> {
            let output = Arc::new(Mutex::new(CapturedOutput::Memory(b"poisoned".to_vec())));
            let poisoned = Arc::clone(&output);
            let _panic = thread::spawn(move || {
                let _guard = poisoned.lock().expect("the fresh mutex is available");
                panic!("poison output");
            })
            .join();
            output
        }

        for thread_finished in [true, false] {
            let output = poisoned_output();
            let mut failure = None;
            let mut captured = take_reader_output(&output, "stdout", thread_finished, &mut failure);
            assert_eq!(output_bytes(&mut captured), b"poisoned");
            assert!(failure.is_some_and(|message| message.contains("capture buffer was poisoned")));
        }
    }

    #[test]
    fn worker_panics_and_disconnects_become_infrastructure_outcomes() {
        let plan = Plan {
            invocations: vec![Invocation {
                label: Some("panic-probe".to_owned()),
                argv: vec![WORKER_PANIC_TEST_PROGRAM.to_owned()],
                work_dir: None,
            }],
        };
        let code = execute_parallel(&plan, false, NonZeroUsize::new(2).expect("literal two is nonzero"), None)
            .expect("worker panic is represented as an outcome");
        assert_eq!(code, ExitCode::from(2));

        let (sender, receiver) = mpsc::channel::<BufferedOutcome>();
        drop(sender);
        let outcome = wait_for_worker(&mut vec![RunningWorker {
            index: 4,
            receiver,
            thread: thread::spawn(|| {}),
        }])
        .expect("disconnection is observable");
        assert_eq!(outcome.index, 4);
        assert!(result_infrastructure_message(outcome.outcome.result).contains("without reporting"));

        let (sender, receiver) = mpsc::channel();
        let outcome = wait_for_worker(&mut vec![RunningWorker {
            index: 5,
            receiver,
            thread: thread::spawn(move || {
                sender
                    .send(BufferedOutcome::infrastructure("premature".to_owned()))
                    .expect("the receiver remains alive");
                panic!("panic after report");
            }),
        }])
        .expect("reported panic is observable");
        assert!(result_infrastructure_message(outcome.outcome.result).contains("panic after report"));

        let (sender, receiver) = mpsc::channel::<BufferedOutcome>();
        drop(sender);
        let outcome = wait_for_worker(&mut vec![RunningWorker {
            index: 6,
            receiver,
            thread: thread::spawn(|| panic!("panic before report")),
        }])
        .expect("unreported panic is observable");
        assert!(result_infrastructure_message(outcome.outcome.result).contains("panic before report"));
    }

    #[test]
    fn worker_spawn_errors_are_local_and_scheduler_visible() {
        let error =
            spawn_worker(0, invocation(&[WORKER_SPAWN_ERROR_TEST_PROGRAM]), None).expect_err("the local seam rejects only its sentinel");
        assert!(error.to_string().contains("injected worker spawn failure"));

        let worker = spawn_worker(1, invocation(&["rustc", "--version"]), None).expect("ordinary program launches");
        let outcome = wait_for_worker(&mut vec![worker]).expect("ordinary worker reports");
        assert_eq!(outcome.index, 1);
        assert!(!outcome.outcome.result.failed());
    }

    #[test]
    fn helper_diagnostics_preserve_cleanup_and_reaper_context() {
        assert_eq!(with_cleanup_failure("primary".to_owned(), &Ok::<_, io::Error>(())), "primary");
        assert_eq!(
            with_cleanup_failure("primary".to_owned(), &Err::<(), _>(io::Error::other("cleanup"))),
            "primary; process-group cleanup also failed: cleanup"
        );
        assert!(with_reaper_handoff("deadline", &Ok(())).contains("detached reaper"));
        assert!(with_reaper_handoff("deadline", &Err(io::Error::other("thread unavailable"))).contains("thread unavailable"));
        assert_eq!(panic_description(&"borrowed panic"), "borrowed panic");
        assert_eq!(panic_description(&"owned panic".to_owned()), "owned panic");
        assert_eq!(panic_description(&7_u8), "non-string panic payload");
    }

    #[test]
    fn tree_outcome_retains_the_invocation_result() {
        let outcome = TreeOutcome::new(InvocationResult::Infrastructure("outcome".to_owned()));
        assert_eq!(result_infrastructure_message(outcome.result), "outcome");
    }
}
