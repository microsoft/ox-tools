// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the `cargo each` command: resolve the selection,
//! apply filters, build the plan, and run it.

use std::collections::{BTreeSet, VecDeque};
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::num::NonZeroUsize;
use std::panic::{self, AssertUnwindSafe, UnwindSafe};
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitCode, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use std::{fmt, thread};

use cargo_gamma_process::{MemoryRequest, PreparedCommand, ProcessTree, prepare};
use cargo_metadata::TargetKind;
use ohno::{AppError, IntoAppError};

use crate::cli::EachArgs;
use crate::error::{InvalidTargetKindError, JobsConflictWithOnceError};
use crate::filter::Predicate;
use crate::plan::{BuildOptions, Invocation, Mode, PackagesExpansion, Plan};
use crate::select::Selection;
use crate::substitute::{uses_workspace_rust_version, validate_placeholders};
use crate::workspace::{Member, Workspace};

#[cfg(test)]
const WORKER_PANIC_TEST_PROGRAM: &str = "__cargo_each_injected_worker_panic";
#[cfg(test)]
const WORKER_SPAWN_ERROR_TEST_PROGRAM: &str = "__cargo_each_injected_worker_spawn_error";
const TERMINATION_GRACE: Duration = Duration::from_millis(250);
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(1);
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

    // Validate mode-specific tokens before resolving the lazy workspace token
    // so a malformed command reports its direct usage error first.
    validate_placeholders(&args.command, mode).into_app_err("failed to build command plan")?;
    let workspace_rust_version = if uses_workspace_rust_version(&args.command) {
        Some(
            workspace
                .workspace_rust_version()
                .into_app_err("failed to resolve workspace Rust version")?,
        )
    } else {
        None
    };

    let plan = Plan::build(
        &members,
        &args.command,
        BuildOptions {
            mode,
            chdir: args.chdir,
            packages,
            target_kinds: &target_kinds,
            target_required_features: &target_required_features,
            workspace_rust_version: workspace_rust_version.as_deref(),
        },
    )
    .into_app_err("failed to build command plan")?;

    if plan.invocations.is_empty() {
        eprintln!("cargo each: selection resolved to no work; nothing to do");
        return Ok(ExitCode::SUCCESS);
    }

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
    if jobs.get() == 1 {
        Ok(execute_sequential(plan, keep_going, timeout))
    } else {
        execute_parallel(plan, keep_going, jobs, timeout)
    }
}

fn execute_sequential(plan: &Plan, keep_going: bool, timeout: Option<Duration>) -> ExitCode {
    let mut any_failed = false;
    for invocation in &plan.invocations {
        emit_label(invocation);
        let result = if let Some(timeout) = timeout {
            run_streamed_with_timeout(invocation, timeout)
        } else {
            run_streamed(invocation)
        };
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

fn execute_parallel(plan: &Plan, keep_going: bool, jobs: NonZeroUsize, timeout: Option<Duration>) -> Result<ExitCode, AppError> {
    let invocations = plan.invocations.clone();
    let worker_count = jobs.get().min(invocations.len()).min(cargo_gamma_process::capacity().max(1));
    let mut pending: VecDeque<(usize, Invocation)> = invocations.iter().cloned().enumerate().collect();
    let mut workers = Vec::with_capacity(worker_count);
    let mut stop_launching = false;
    let mut launch_error = None;

    for (index, invocation) in pending.drain(..worker_count) {
        match spawn_worker(index, invocation, timeout) {
            Ok(worker) => {
                workers.push(worker);
            }
            Err(error) => {
                launch_error = Some(error);
                stop_launching = true;
                break;
            }
        }
    }

    let mut outcomes = Vec::with_capacity(invocations.len());
    while let Some(outcome) = wait_for_worker(&mut workers) {
        if failure_stops_launching(keep_going, outcome.outcome.result.failed()) {
            stop_launching = true;
        }
        outcomes.push(outcome);

        if stop_launching {
            continue;
        }
        let Some((index, invocation)) = pending.pop_front() else {
            continue;
        };
        match spawn_worker(index, invocation, timeout) {
            Ok(worker) => workers.push(worker),
            Err(error) => {
                launch_error = Some(error);
                stop_launching = true;
            }
        }
    }

    if let Some(error) = launch_error {
        return Err(error).into_app_err("failed to create cargo-each worker thread");
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
    Ok(match &first_failure.outcome.result {
        InvocationResult::Exited(status) => ExitCode::from(exit_byte(status.code())),
        InvocationResult::TimedOut(_) => ExitCode::from(1),
        InvocationResult::Infrastructure(_) => ExitCode::from(2),
    })
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
    let (program, command) = match command_for(invocation) {
        Ok(command) => command,
        Err(message) => return InvocationResult::Infrastructure(message),
    };
    let mut tree = match spawn_sealed_tree(command) {
        Ok(tree) => tree,
        Err(error) => {
            return InvocationResult::Infrastructure(format!("failed to spawn `{program}`: {error}"));
        }
    };
    wait_for_tree(&mut tree, timeout).result
}

fn run_captured(invocation: &Invocation, timeout: Option<Duration>) -> BufferedOutcome {
    #[cfg(test)]
    assert!(
        invocation.argv.first().is_none_or(|program| program != WORKER_PANIC_TEST_PROGRAM),
        "injected worker panic"
    );

    run_captured_with_spawner(invocation, timeout, |command| {
        spawn_sealed_tree(command).map(CapturedProcess::Contained)
    })
}

fn run_captured_with_spawner(
    invocation: &Invocation,
    timeout: Option<Duration>,
    timed_spawner: impl FnOnce(Command) -> Result<CapturedProcess, String>,
) -> BufferedOutcome {
    let (program, mut command) = match command_for(invocation) {
        Ok(command) => command,
        Err(message) => return BufferedOutcome::infrastructure(message),
    };
    let _ = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let process = match timeout {
        Some(_) => timed_spawner(command),
        None => command
            .spawn()
            .map(|child| CapturedProcess::Ordinary(Some(child)))
            .map_err(|error| error.to_string()),
    };
    let mut process = match process {
        Ok(process) => process,
        Err(error) => {
            return BufferedOutcome::infrastructure(format!("failed to spawn `{program}`: {error}"));
        }
    };
    let drain_boundary = process.drain_boundary();
    let capture_fault = capture_fault(invocation);

    let stdout = if capture_fault == Some(CaptureFault::MissingStdout) {
        None
    } else {
        process.take_stdout()
    };
    let Some(stdout) = stdout else {
        let cleanup = process.terminate_bounded();
        return BufferedOutcome::infrastructure(with_cleanup_failure("failed to capture child stdout".to_owned(), &cleanup));
    };
    let stdout_reader = match if capture_fault == Some(CaptureFault::StdoutReader) {
        Err(io::Error::other("injected stdout reader failure"))
    } else {
        spawn_output_reader(stdout, "cargo-each-stdout")
    } {
        Ok(reader) => reader,
        Err(error) => {
            let cleanup = process.terminate_bounded();
            return BufferedOutcome::infrastructure(with_cleanup_failure(format!("failed to create stdout reader: {error}"), &cleanup));
        }
    };
    let stderr = if capture_fault == Some(CaptureFault::MissingStderr) {
        None
    } else {
        process.take_stderr()
    };
    let Some(stderr) = stderr else {
        let cleanup = process.terminate_bounded();
        return BufferedOutcome::from_reader_failure("failed to capture child stderr".to_owned(), stdout_reader, &cleanup, drain_boundary);
    };
    let stderr_reader = match if capture_fault == Some(CaptureFault::StderrReader) {
        Err(io::Error::other("injected stderr reader failure"))
    } else {
        spawn_output_reader(stderr, "cargo-each-stderr")
    } {
        Ok(reader) => reader,
        Err(error) => {
            let cleanup = process.terminate_bounded();
            return BufferedOutcome::from_reader_failure(
                format!("failed to create stderr reader: {error}"),
                stdout_reader,
                &cleanup,
                drain_boundary,
            );
        }
    };

    let process_outcome = process.wait(timeout, capture_fault);
    drop(process);
    let (stdout, stderr) = finish_output_readers(stdout_reader, stderr_reader, OUTPUT_DRAIN_GRACE, drain_boundary);
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

#[cfg(test)]
fn spawn_tree(command: Command) -> Result<ProcessTree, String> {
    let prepared = prepare_tree(command)?;
    spawn_prepared_tree(prepared)
}

fn spawn_sealed_tree(command: Command) -> Result<ProcessTree, String> {
    let prepared = prepare_tree(command)?;
    spawn_if_sealed(prepared, PreparedCommand::sealed, spawn_prepared_tree)
}

fn prepare_tree(command: Command) -> Result<PreparedCommand, String> {
    prepare(command, MemoryRequest::default()).map_err(|error| format!("could not prepare process-tree containment: {error}"))
}

fn spawn_prepared_tree(prepared: PreparedCommand) -> Result<ProcessTree, String> {
    let spawned = prepared.spawn().map_err(|failure| failure.to_string())?;
    ProcessTree::adopt(spawned).map_err(|error| format!("could not adopt child into process-tree containment: {error}"))
}

fn spawn_if_sealed<T, O>(prepared: T, sealed: impl FnOnce(&T) -> bool, spawn: impl FnOnce(T) -> Result<O, String>) -> Result<O, String> {
    if !sealed(&prepared) {
        return Err(
            "timeout requires sealed process-tree containment, but this host only provides best-effort containment; the child was not started"
                .to_owned(),
        );
    }
    spawn(prepared)
}

enum CapturedProcess {
    Ordinary(Option<Child>),
    Contained(ProcessTree),
}

impl CapturedProcess {
    fn drain_boundary(&self) -> &'static str {
        match self {
            Self::Ordinary(_) => "ordinary process tree",
            Self::Contained(_) => "contained process tree",
        }
    }

    fn take_stdout(&mut self) -> Option<ChildStdout> {
        match self {
            Self::Ordinary(child) => child.as_mut()?.stdout.take(),
            Self::Contained(tree) => tree.take_stdout(),
        }
    }

    fn take_stderr(&mut self) -> Option<ChildStderr> {
        match self {
            Self::Ordinary(child) => child.as_mut()?.stderr.take(),
            Self::Contained(tree) => tree.take_stderr(),
        }
    }

    fn terminate_bounded(&mut self) -> io::Result<ExitStatus> {
        match self {
            Self::Ordinary(child) => {
                let mut child = child
                    .take()
                    .ok_or_else(|| io::Error::other("ordinary child was already reaped or detached"))?;
                let result = terminate_ordinary_child(&mut child, TERMINATION_GRACE);
                drop(child);
                result
            }
            Self::Contained(tree) => tree.terminate_bounded(TERMINATION_GRACE),
        }
    }

    fn wait(&mut self, timeout: Option<Duration>, capture_fault: Option<CaptureFault>) -> TreeOutcome {
        match (self, timeout) {
            (Self::Ordinary(child), None) => {
                let Some(mut child) = child.take() else {
                    return TreeOutcome::new(InvocationResult::Infrastructure(
                        "ordinary child was already reaped or detached".to_owned(),
                    ));
                };
                let waited = if capture_fault == Some(CaptureFault::WaitFailure) {
                    Err(io::Error::other("injected child wait failure"))
                } else {
                    child.wait()
                };
                finish_wait_with_cleanup(&mut child, waited, |child| terminate_ordinary_child(child, TERMINATION_GRACE))
            }
            (Self::Contained(tree), Some(timeout)) => wait_for_tree(tree, timeout),
            (Self::Ordinary(_), Some(_)) | (Self::Contained(_), None) => TreeOutcome::new(InvocationResult::Infrastructure(
                "internal capture mode did not match timeout configuration".to_owned(),
            )),
        }
    }
}

fn finish_wait_with_cleanup<T>(
    control: &mut T,
    waited: io::Result<ExitStatus>,
    cleanup: impl FnOnce(&mut T) -> io::Result<ExitStatus>,
) -> TreeOutcome {
    match waited {
        Ok(status) => TreeOutcome::new(InvocationResult::Exited(status)),
        Err(error) => {
            let cleanup = cleanup(control);
            TreeOutcome::new(InvocationResult::Infrastructure(with_cleanup_failure(
                format!("failed to wait for child process: {error}"),
                &cleanup,
            )))
        }
    }
}

fn terminate_ordinary_child(child: &mut Child, grace: Duration) -> io::Result<ExitStatus> {
    terminate_ordinary_with(child, grace, Child::kill, Child::try_wait)
}

fn terminate_ordinary_with<T>(
    control: &mut T,
    grace: Duration,
    kill: impl FnOnce(&mut T) -> io::Result<()>,
    mut try_wait: impl FnMut(&mut T) -> io::Result<Option<ExitStatus>>,
) -> io::Result<ExitStatus> {
    let kill_error = kill(control).err();
    let Some(status) = poll_child_exit(control, grace, &mut try_wait)? else {
        let message = kill_error.map_or_else(
            || format!("ordinary child did not exit within {} ms after termination", grace.as_millis()),
            |error| {
                format!(
                    "{error}; ordinary child did not exit within {} ms after termination",
                    grace.as_millis()
                )
            },
        );
        return Err(io::Error::new(io::ErrorKind::WouldBlock, message));
    };
    match kill_error {
        Some(error) => Err(error),
        None => Ok(status),
    }
}

fn poll_child_exit<T>(
    control: &mut T,
    grace: Duration,
    try_wait: &mut impl FnMut(&mut T) -> io::Result<Option<ExitStatus>>,
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

fn wait_for_tree(tree: &mut ProcessTree, timeout: Duration) -> TreeOutcome {
    wait_for_tree_with(tree, timeout, ProcessTree::observe, |tree| {
        tree.terminate_bounded(TERMINATION_GRACE)
    })
}

fn wait_for_tree_with<T>(
    control: &mut T,
    timeout: Duration,
    mut observe: impl FnMut(&mut T) -> io::Result<Option<ExitStatus>>,
    mut terminate: impl FnMut(&mut T) -> io::Result<ExitStatus>,
) -> TreeOutcome {
    let started = Instant::now();
    loop {
        match observe(control) {
            Ok(Some(status)) => return TreeOutcome::new(InvocationResult::Exited(status)),
            Ok(None) => {}
            Err(error) => {
                let cleanup = terminate(control);
                return match cleanup {
                    Ok(_) => TreeOutcome::new(InvocationResult::Infrastructure(format!(
                        "failed to observe child process tree: {error}"
                    ))),
                    Err(cleanup) => TreeOutcome::new(InvocationResult::Infrastructure(format!(
                        "failed to observe child process tree: {error}; cleanup also failed: {cleanup}"
                    ))),
                };
            }
        }
        let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
            return match terminate(control) {
                Ok(_) => TreeOutcome::new(InvocationResult::TimedOut(timeout)),
                Err(error) => TreeOutcome::new(InvocationResult::Infrastructure(format!(
                    "invocation timed out after {}; process-tree termination failed: {error}",
                    display_duration(timeout)
                ))),
            };
        };
        thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}

#[cfg(test)]
fn wait_for_tree_without_timeout_with<T>(
    control: &mut T,
    mut observe: impl FnMut(&mut T) -> io::Result<Option<ExitStatus>>,
    mut terminate: impl FnMut(&mut T) -> io::Result<ExitStatus>,
) -> TreeOutcome {
    loop {
        match observe(control) {
            Ok(Some(status)) => return TreeOutcome::new(InvocationResult::Exited(status)),
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                let cleanup = terminate(control);
                return match cleanup {
                    Ok(_) => TreeOutcome::new(InvocationResult::Infrastructure(format!(
                        "failed to observe child process tree: {error}"
                    ))),
                    Err(cleanup) => TreeOutcome::new(InvocationResult::Infrastructure(format!(
                        "failed to observe child process tree: {error}; cleanup also failed: {cleanup}"
                    ))),
                };
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureFault {
    MissingStdout,
    StdoutReader,
    MissingStderr,
    StderrReader,
    WaitFailure,
}

fn capture_fault(invocation: &Invocation) -> Option<CaptureFault> {
    #[cfg(test)]
    {
        match invocation.label.as_deref() {
            Some("__cargo_each_missing_stdout") => Some(CaptureFault::MissingStdout),
            Some("__cargo_each_stdout_reader_failure") => Some(CaptureFault::StdoutReader),
            Some("__cargo_each_missing_stderr") => Some(CaptureFault::MissingStderr),
            Some("__cargo_each_stderr_reader_failure") => Some(CaptureFault::StderrReader),
            Some("__cargo_each_wait_failure") => Some(CaptureFault::WaitFailure),
            _ => None,
        }
    }
    #[cfg(not(test))]
    {
        let _ = invocation;
        None
    }
}

fn spawn_output_reader<R>(stream: R, name: &'static str) -> io::Result<OutputReader>
where
    R: io::Read + Send + 'static,
{
    spawn_output_reader_with(
        stream,
        name,
        OUTPUT_MEMORY_LIMIT,
        Box::new(|| tempfile::tempfile().map(|file| Box::new(file) as Box<dyn SpillFile>)),
    )
}

fn spawn_output_reader_with<R>(
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
    })
}

fn finish_output_reader(reader: OutputReader, stream: &str, grace: Duration, boundary: &str) -> CapturedStream {
    let OutputReader {
        thread,
        completion,
        output,
        retaining,
    } = reader;
    let failure = match completion.recv_timeout(grace) {
        Ok(ReaderCompletion::Finished(Ok(()))) => None,
        Ok(ReaderCompletion::Finished(Err(error))) => Some(format!("failed to read child {stream}: {error}")),
        Ok(ReaderCompletion::Panicked(message)) => Some(format!("child {stream} reader thread panicked: {message}")),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            retaining.store(false, Ordering::Release);
            Some(format!(
                "child {stream} remained open for more than {} ms after the {boundary} completed; partial output was retained",
                grace.as_millis()
            ))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            retaining.store(false, Ordering::Release);
            Some(format!("child {stream} reader exited without reporting completion"))
        }
    };
    drop(thread);

    match output.lock() {
        Ok(mut captured) => CapturedStream {
            output: std::mem::replace(&mut *captured, CapturedOutput::empty()),
            failure,
        },
        Err(poisoned) => {
            let mut captured = poisoned.into_inner();
            CapturedStream {
                output: std::mem::replace(&mut *captured, CapturedOutput::empty()),
                failure: Some(match failure {
                    Some(failure) => format!("{failure}; child {stream} capture buffer was poisoned"),
                    None => format!("child {stream} capture buffer was poisoned"),
                }),
            }
        }
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
        Err(error) => format!("{message}; process-tree cleanup also failed: {error}"),
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
}

#[derive(Debug)]
enum ReaderCompletion {
    Finished(io::Result<()>),
    Panicked(String),
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

    fn from_reader_failure<T>(message: String, stdout_reader: OutputReader, cleanup: &io::Result<T>, boundary: &str) -> Self {
        let message = with_cleanup_failure(message, cleanup);
        combine_captured_output(
            finish_output_reader(stdout_reader, "stdout", OUTPUT_DRAIN_GRACE, boundary),
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Condvar, Mutex, mpsc};
    use std::time::{Duration, Instant};
    use std::{io, thread};

    use super::{
        BufferedOutcome, CapturedOutput, CapturedProcess, CapturedStream, Invocation, InvocationResult, OutputEmitError, OutputReader,
        Plan, ReaderCompletion, RunningWorker, SpillFile, TreeOutcome, WORKER_PANIC_TEST_PROGRAM, WORKER_SPAWN_ERROR_TEST_PROGRAM,
        combine_captured_output, display_duration, emit_buffered, emit_buffered_to, execute_parallel, exit_byte, failure_stops_launching,
        finish_output_reader, finish_wait_with_cleanup, panic_description, run_captured, run_captured_with_spawner, run_streamed,
        run_streamed_with_timeout, spawn_if_sealed, spawn_output_reader, spawn_output_reader_with, spawn_tree, terminate_ordinary_child,
        terminate_ordinary_with, wait_for_tree_with, wait_for_tree_without_timeout_with, wait_for_worker, with_cleanup_failure,
    };

    const ORDINARY_BOUNDARY: &str = "ordinary process tree";
    const CONTAINED_BOUNDARY: &str = "contained process tree";

    struct StubbornPipe {
        read: bool,
        blocked: Option<mpsc::Sender<()>>,
        finished: mpsc::Sender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
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

    struct EofThenPanicReader {
        reached_eof: bool,
    }

    struct InterruptedThenDataReader {
        state: u8,
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

    struct FailingWriter;

    impl io::Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("injected destination write failure"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
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

    impl io::Read for InterruptedThenDataReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.state {
                0 => {
                    self.state = 1;
                    Err(io::Error::from(io::ErrorKind::Interrupted))
                }
                1 => {
                    self.state = 2;
                    let content = b"after interrupt";
                    buf[..content.len()].copy_from_slice(content);
                    Ok(content.len())
                }
                _ => Ok(0),
            }
        }
    }

    impl io::Read for EofThenPanicReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            assert!(!self.reached_eof, "the output reader must stop after the first EOF");
            self.reached_eof = true;
            Ok(0)
        }
    }

    struct LateDataPipe {
        started: Option<mpsc::Sender<()>>,
        finished: Option<mpsc::Sender<()>>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl io::Read for LateDataPipe {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if let Some(started) = self.started.take() {
                let _receiver_gone = started.send(());
            }
            let (lock, condition) = &*self.release;
            let mut released = lock.lock().expect("the test owns the release mutex without panicking");
            while !*released {
                released = condition.wait(released).expect("the test owns the release mutex without panicking");
            }
            let content = b"late data";
            buf[..content.len()].copy_from_slice(content);
            Ok(content.len())
        }
    }

    impl Drop for LateDataPipe {
        fn drop(&mut self) {
            if let Some(finished) = self.finished.take() {
                let _receiver_gone = finished.send(());
            }
        }
    }

    fn invocation(argv: &[&str]) -> Invocation {
        Invocation {
            label: None,
            argv: argv.iter().map(|value| (*value).to_owned()).collect(),
            work_dir: None,
        }
    }

    fn labelled_invocation(label: &str, argv: &[&str]) -> Invocation {
        Invocation {
            label: Some(label.to_owned()),
            ..invocation(argv)
        }
    }

    fn spawn_ordinary_capture(mut command: Command) -> Result<CapturedProcess, String> {
        command
            .spawn()
            .map(|child| CapturedProcess::Ordinary(Some(child)))
            .map_err(|error| error.to_string())
    }

    fn result_infrastructure_message(result: InvocationResult) -> String {
        let InvocationResult::Infrastructure(message) = result else {
            panic!("the test expects an infrastructure outcome");
        };
        message
    }

    struct FakeProcess {
        observations: VecDeque<io::Result<Option<ExitStatus>>>,
        termination: Option<io::Result<ExitStatus>>,
    }

    struct FakeOrdinaryChild {
        kill_error: Option<io::Error>,
        observations: VecDeque<io::Result<Option<ExitStatus>>>,
    }

    impl FakeOrdinaryChild {
        fn kill_child(&mut self) -> io::Result<()> {
            match self.kill_error.take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }

        fn try_wait_child(&mut self) -> io::Result<Option<ExitStatus>> {
            self.observations.pop_front().unwrap_or(Ok(None))
        }
    }

    impl FakeProcess {
        fn observe(&mut self) -> io::Result<Option<ExitStatus>> {
            self.observations.pop_front().unwrap_or(Ok(None))
        }

        fn terminate(&mut self) -> io::Result<ExitStatus> {
            self.termination
                .take()
                .unwrap_or_else(|| Err(io::Error::other("unexpected termination")))
        }
    }

    fn successful_status() -> ExitStatus {
        ExitStatus::from_raw(0)
    }

    fn infrastructure_message(outcome: BufferedOutcome) -> String {
        result_infrastructure_message(outcome.result)
    }

    fn captured(bytes: &[u8], failure: Option<&str>) -> CapturedStream {
        CapturedStream {
            output: CapturedOutput::Memory(bytes.to_vec()),
            failure: failure.map(str::to_owned),
        }
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

    fn poisoned_buffer() -> Arc<Mutex<CapturedOutput>> {
        let bytes = Arc::new(Mutex::new(CapturedOutput::Memory(b"poisoned bytes".to_vec())));
        let poisoned = Arc::clone(&bytes);
        let _panic = thread::spawn(move || {
            let _guard = poisoned.lock().expect("the fresh capture mutex is available");
            panic!("poison capture buffer");
        })
        .join();
        bytes
    }

    impl io::Read for StubbornPipe {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.read {
                self.read = true;
                let content = b"captured-before-timeout";
                buf[..content.len()].copy_from_slice(content);
                return Ok(content.len());
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

    #[test]
    fn signal_terminated_child_maps_to_one() {
        assert_eq!(exit_byte(None), 1);
    }

    #[test]
    fn in_range_codes_pass_through() {
        assert_eq!(exit_byte(Some(1)), 1);
        assert_eq!(exit_byte(Some(2)), 2);
        assert_eq!(exit_byte(Some(255)), 255);
    }

    #[test]
    fn wide_codes_reduce_to_low_byte() {
        assert_eq!(exit_byte(Some(259)), 3);
        assert_eq!(exit_byte(Some(257)), 1);
    }

    #[test]
    fn nonzero_code_with_zero_low_byte_maps_to_one() {
        assert_eq!(exit_byte(Some(256)), 1);
        assert_eq!(exit_byte(Some(512)), 1);
    }

    #[test]
    fn durations_have_compact_diagnostics() {
        assert_eq!(display_duration(Duration::from_millis(250)), "250ms");
        assert_eq!(display_duration(Duration::from_secs(30)), "30s");
        assert_eq!(display_duration(Duration::from_mins(2)), "2m");
    }

    #[test]
    fn failure_launch_policy_distinguishes_fail_fast_from_keep_going() {
        assert!(failure_stops_launching(false, true));
        assert!(!failure_stops_launching(false, false));
        assert!(!failure_stops_launching(true, true));
        assert!(!failure_stops_launching(true, false));
    }

    #[test]
    fn normal_completion_does_not_join_a_stubborn_descendant_pipe() {
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let (blocked_tx, blocked_rx) = mpsc::channel();
        let (reader_finished_tx, reader_finished_rx) = mpsc::channel();
        let reader = spawn_output_reader(
            StubbornPipe {
                read: false,
                blocked: Some(blocked_tx),
                finished: reader_finished_tx,
                release: Arc::clone(&release),
            },
            "stubborn-test-pipe",
        )
        .expect("the test reader thread can be created");
        blocked_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("the reader reaches the simulated descendant's open pipe");

        let (finished_tx, finished_rx) = mpsc::channel();
        let finisher = thread::spawn(move || {
            let result = finish_output_reader(reader, "stdout", Duration::from_millis(25), ORDINARY_BOUNDARY);
            let _receiver_gone = finished_tx.send(result);
        });
        let result = finished_rx.recv_timeout(Duration::from_millis(500));

        let (lock, condition) = &*release;
        *lock.lock().expect("the test owns the release mutex without panicking") = true;
        condition.notify_all();

        let mut captured = result.expect("normal completion must not wait for an escaped descendant to close its pipe");
        finisher.join().expect("the bounded finisher thread does not panic");
        reader_finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("the detached reader exits after the test releases its simulated pipe");
        assert_eq!(output_bytes(&mut captured.output), b"captured-before-timeout");
        assert!(
            captured.failure.as_deref().is_some_and(|failure| failure.contains("remained open")),
            "grace expiry must be an explicit infrastructure failure"
        );
    }

    #[test]
    fn unproven_cleanup_discards_data_that_arrives_after_capture_stops() {
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let (started_tx, started_rx) = mpsc::channel();
        let (reader_finished_tx, reader_finished_rx) = mpsc::channel();
        let reader = spawn_output_reader(
            LateDataPipe {
                started: Some(started_tx),
                finished: Some(reader_finished_tx),
                release: Arc::clone(&release),
            },
            "late-data-test-pipe",
        )
        .expect("create late-data reader");
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("the late-data reader starts its blocking read");

        let mut captured = finish_output_reader(reader, "stdout", Duration::from_millis(25), ORDINARY_BOUNDARY);
        assert!(output_bytes(&mut captured.output).is_empty());
        assert!(captured.failure.is_some());

        let (lock, condition) = &*release;
        *lock.lock().expect("the test owns the release mutex without panicking") = true;
        condition.notify_all();
        reader_finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("the detached reader discards late data and exits");
    }

    #[test]
    fn worker_panic_becomes_an_outcome_without_deadlocking_the_scheduler() {
        let plan = Plan {
            invocations: vec![Invocation {
                label: Some("panic-probe".to_owned()),
                argv: vec![WORKER_PANIC_TEST_PROGRAM.to_owned()],
                work_dir: None,
            }],
        };
        let (finished_tx, finished_rx) = mpsc::channel();
        let scheduler = thread::spawn(move || {
            let result = execute_parallel(&plan, false, NonZeroUsize::new(2).expect("literal two is nonzero"), None);
            let exit_two = result.is_ok_and(|code| code == ExitCode::from(2));
            let _receiver_gone = finished_tx.send(exit_two);
        });

        assert!(
            finished_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("the panic-safe worker reports before the scheduler deadline"),
            "a worker panic must become an infrastructure exit"
        );
        scheduler.join().expect("the scheduler handles the worker panic without panicking");
    }

    #[test]
    fn disconnected_worker_channel_becomes_an_infrastructure_outcome() {
        let (sender, receiver) = mpsc::channel::<BufferedOutcome>();
        drop(sender);
        let thread = thread::spawn(|| {});
        let outcome = wait_for_worker(&mut vec![RunningWorker {
            index: 4,
            receiver,
            thread,
        }])
        .expect("the disconnected worker is observable");
        assert_eq!(outcome.index, 4);
        let InvocationResult::Infrastructure(message) = outcome.outcome.result else {
            panic!("a disconnected worker must produce an infrastructure outcome");
        };
        assert!(message.contains("without reporting"));
    }

    #[test]
    fn worker_panic_descriptions_preserve_string_payloads() {
        let borrowed: Box<dyn std::any::Any + Send> = Box::new("borrowed panic");
        let owned: Box<dyn std::any::Any + Send> = Box::new("owned panic".to_owned());
        let other: Box<dyn std::any::Any + Send> = Box::new(7_u8);
        assert_eq!(panic_description(borrowed.as_ref()), "borrowed panic");
        assert_eq!(panic_description(owned.as_ref()), "owned panic");
        assert_eq!(panic_description(other.as_ref()), "non-string panic payload");
    }

    #[test]
    fn cleanup_failure_context_preserves_both_outcomes() {
        assert_eq!(with_cleanup_failure("primary".to_owned(), &Ok::<_, io::Error>(())), "primary");
        assert_eq!(
            with_cleanup_failure("primary".to_owned(), &Err::<(), _>(io::Error::other("cleanup"))),
            "primary; process-tree cleanup also failed: cleanup"
        );
    }

    #[test]
    fn ordinary_termination_polls_until_successful_reap() {
        let mut child = FakeOrdinaryChild {
            kill_error: None,
            observations: VecDeque::from([Ok(None), Ok(Some(successful_status()))]),
        };

        let cleanup = terminate_ordinary_with(
            &mut child,
            Duration::from_secs(1),
            FakeOrdinaryChild::kill_child,
            FakeOrdinaryChild::try_wait_child,
        );

        assert!(cleanup.as_ref().is_ok_and(ExitStatus::success));
        assert!(child.observations.is_empty(), "termination returned after only one immediate poll");
        assert_eq!(
            with_cleanup_failure("capture setup failed".to_owned(), &cleanup),
            "capture setup failed"
        );
    }

    #[test]
    fn ordinary_child_sleep_probe() {
        if std::env::var_os("CARGO_EACH_ORDINARY_CHILD_PROBE").is_some() {
            thread::sleep(Duration::from_secs(30));
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns and terminates a child process")]
    fn ordinary_termination_kills_polls_and_reaps_a_real_child() {
        let mut child = Command::new(std::env::current_exe().expect("the test binary knows its path"))
            .args(["--exact", "run::tests::ordinary_child_sleep_probe", "--nocapture"])
            .env("CARGO_EACH_ORDINARY_CHILD_PROBE", "1")
            .spawn()
            .expect("spawn ordinary child probe");

        let cleanup = terminate_ordinary_child(&mut child, Duration::from_secs(1));

        let status = cleanup.expect("a successfully killed child must be reaped within the grace");
        assert!(!status.success(), "a killed child must not report successful completion");
        assert!(
            child.try_wait().expect("the reaped child remains observable").is_some(),
            "bounded termination returned without reaping the child"
        );
    }

    #[test]
    fn ordinary_termination_reports_deadline_kill_and_observation_failures() {
        let mut deadline = FakeOrdinaryChild {
            kill_error: None,
            observations: VecDeque::from([Ok(None)]),
        };
        let error = terminate_ordinary_with(
            &mut deadline,
            Duration::ZERO,
            FakeOrdinaryChild::kill_child,
            FakeOrdinaryChild::try_wait_child,
        )
        .expect_err("an unreaped child reaches the deadline");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);

        let mut failed_kill_deadline = FakeOrdinaryChild {
            kill_error: Some(io::Error::new(io::ErrorKind::PermissionDenied, "kill failed")),
            observations: VecDeque::from([Ok(None)]),
        };
        let error = terminate_ordinary_with(
            &mut failed_kill_deadline,
            Duration::ZERO,
            FakeOrdinaryChild::kill_child,
            FakeOrdinaryChild::try_wait_child,
        )
        .expect_err("a failed kill with an unreaped child reaches the deadline");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("kill failed"));

        let mut failed_kill = FakeOrdinaryChild {
            kill_error: Some(io::Error::new(io::ErrorKind::PermissionDenied, "kill failed")),
            observations: VecDeque::from([Ok(Some(successful_status()))]),
        };
        let error = terminate_ordinary_with(
            &mut failed_kill,
            Duration::from_secs(1),
            FakeOrdinaryChild::kill_child,
            FakeOrdinaryChild::try_wait_child,
        )
        .expect_err("reaping must not hide a failed kill");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

        let mut failed_observation = FakeOrdinaryChild {
            kill_error: None,
            observations: VecDeque::from([Err(io::Error::other("observation failed"))]),
        };
        let error = terminate_ordinary_with(
            &mut failed_observation,
            Duration::from_secs(1),
            FakeOrdinaryChild::kill_child,
            FakeOrdinaryChild::try_wait_child,
        )
        .expect_err("try_wait failure must be preserved");
        assert!(error.to_string().contains("observation failed"));
    }

    #[test]
    fn wait_error_runs_bounded_cleanup_and_preserves_both_errors() {
        let mut cleaned = false;
        let outcome = finish_wait_with_cleanup(&mut cleaned, Err(io::Error::other("wait failed")), |cleaned| {
            *cleaned = true;
            Ok(successful_status())
        });
        assert!(cleaned, "wait failure did not invoke bounded cleanup");
        let message = result_infrastructure_message(outcome.result);
        assert!(message.contains("wait failed"));
        assert!(!message.contains("cleanup also failed"));

        let mut attempted = false;
        let outcome = finish_wait_with_cleanup(&mut attempted, Err(io::Error::other("wait failed")), |attempted| {
            *attempted = true;
            Err(io::Error::other("cleanup failed"))
        });
        assert!(attempted, "failed cleanup was not attempted");
        let message = result_infrastructure_message(outcome.result);
        assert!(message.contains("wait failed"));
        assert!(message.contains("cleanup also failed: cleanup failed"));
    }

    #[test]
    fn unsealed_timeout_is_refused_before_spawn() {
        struct FakePrepared {
            sealed: bool,
        }

        let spawned = Arc::new(AtomicBool::new(false));
        let spawn_observed = Arc::clone(&spawned);
        let error = spawn_if_sealed(
            FakePrepared { sealed: false },
            |prepared| prepared.sealed,
            move |_prepared| {
                spawn_observed.store(true, Ordering::SeqCst);
                Ok::<_, String>(())
            },
        )
        .expect_err("an unsealed timeout launch must be refused");

        assert!(
            !spawned.load(Ordering::SeqCst),
            "the child spawn path ran despite unsealed containment"
        );
        assert!(error.contains("timeout requires sealed process-tree containment"));
        assert!(error.contains("child was not started"));

        let spawned = Arc::new(AtomicBool::new(false));
        let spawn_observed = Arc::clone(&spawned);
        spawn_if_sealed(
            FakePrepared { sealed: true },
            |prepared| prepared.sealed,
            move |_prepared| {
                spawn_observed.store(true, Ordering::SeqCst);
                Ok::<_, String>(())
            },
        )
        .expect("sealed containment permits the spawn");
        assert!(spawned.load(Ordering::SeqCst));
    }

    #[test]
    fn panicked_worker_without_a_report_becomes_an_infrastructure_outcome() {
        let (sender, receiver) = mpsc::channel::<BufferedOutcome>();
        drop(sender);
        let thread = thread::spawn(|| panic!("panic before reporting"));
        let outcome = wait_for_worker(&mut vec![RunningWorker {
            index: 5,
            receiver,
            thread,
        }])
        .expect("the panicked worker is observable");
        let InvocationResult::Infrastructure(message) = outcome.outcome.result else {
            panic!("a panicked worker must produce an infrastructure outcome");
        };
        assert!(message.contains("panic before reporting"));
    }

    #[test]
    fn panic_after_a_worker_report_overrides_the_report() {
        let (sender, receiver) = mpsc::channel();
        let thread = thread::spawn(move || {
            sender
                .send(BufferedOutcome::infrastructure("premature report".to_owned()))
                .expect("the scheduler receiver remains alive");
            panic!("panic after reporting");
        });
        let outcome = wait_for_worker(&mut vec![RunningWorker {
            index: 6,
            receiver,
            thread,
        }])
        .expect("the reported worker is observable");
        let InvocationResult::Infrastructure(message) = outcome.outcome.result else {
            panic!("a worker panic must override its premature report");
        };
        assert!(message.contains("panic after reporting"));
        assert!(wait_for_worker(&mut Vec::new()).is_none());
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns child processes through the scheduler")]
    fn worker_spawn_failures_are_reported_during_initial_and_replacement_launches() {
        let initial = Plan {
            invocations: vec![invocation(&[WORKER_SPAWN_ERROR_TEST_PROGRAM])],
        };
        let error = execute_parallel(&initial, false, NonZeroUsize::new(2).expect("literal two is nonzero"), None)
            .expect_err("an initial worker spawn failure must abort scheduling");
        assert!(error.to_string().contains("injected worker spawn failure"));

        let replacement = Plan {
            invocations: vec![invocation(&["rustc", "--version"]), invocation(&[WORKER_SPAWN_ERROR_TEST_PROGRAM])],
        };
        let error = execute_parallel(&replacement, false, NonZeroUsize::new(1).expect("literal one is nonzero"), None)
            .expect_err("a replacement worker spawn failure must abort scheduling");
        assert!(error.to_string().contains("injected worker spawn failure"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns child processes")]
    fn direct_runners_report_empty_and_unspawnable_commands() {
        let empty = invocation(&[]);
        assert!(result_infrastructure_message(run_streamed(&empty)).contains("empty argument vector"));
        assert!(result_infrastructure_message(run_streamed_with_timeout(&empty, Duration::from_secs(1))).contains("empty argument vector"));
        assert!(infrastructure_message(run_captured(&empty, None)).contains("empty argument vector"));

        let missing = invocation(&["cargo-each-no-such-program-for-unit-test"]);
        assert!(result_infrastructure_message(run_streamed(&missing)).contains("failed to spawn"));
        assert!(result_infrastructure_message(run_streamed_with_timeout(&missing, Duration::from_secs(1))).contains("failed to spawn"));
        assert!(infrastructure_message(run_captured(&missing, None)).contains("failed to spawn"));
    }

    #[test]
    fn captured_output_combines_every_reader_result_shape() {
        let mut success = combine_captured_output(
            captured(b"stdout", None),
            captured(b"stderr", None),
            InvocationResult::Infrastructure("primary".to_owned()),
        );
        assert_eq!(output_bytes(&mut success.stdout), b"stdout");
        assert_eq!(output_bytes(&mut success.stderr), b"stderr");
        assert_eq!(infrastructure_message(success), "primary");

        let mut stdout_failed = combine_captured_output(
            captured(b"partial stdout", Some("stdout failed")),
            captured(b"stderr", None),
            InvocationResult::Infrastructure("primary".to_owned()),
        );
        assert_eq!(output_bytes(&mut stdout_failed.stdout), b"partial stdout");
        assert_eq!(output_bytes(&mut stdout_failed.stderr), b"stderr");
        assert_eq!(infrastructure_message(stdout_failed), "primary; stdout failed");

        let mut stderr_failed = combine_captured_output(
            captured(b"stdout", None),
            captured(b"partial stderr", Some("stderr failed")),
            InvocationResult::Infrastructure("primary".to_owned()),
        );
        assert_eq!(output_bytes(&mut stderr_failed.stdout), b"stdout");
        assert_eq!(output_bytes(&mut stderr_failed.stderr), b"partial stderr");
        assert_eq!(infrastructure_message(stderr_failed), "primary; stderr failed");

        let mut both_failed = combine_captured_output(
            captured(b"partial stdout", Some("stdout failed")),
            captured(b"partial stderr", Some("stderr failed")),
            InvocationResult::Infrastructure("primary".to_owned()),
        );
        assert_eq!(output_bytes(&mut both_failed.stdout), b"partial stdout");
        assert_eq!(output_bytes(&mut both_failed.stderr), b"partial stderr");
        assert_eq!(infrastructure_message(both_failed), "primary; stdout failed; stderr failed");

        let timed_out = combine_captured_output(
            captured(b"partial stdout", Some("stdout still open")),
            captured(b"", None),
            InvocationResult::TimedOut(Duration::from_secs(2)),
        );
        assert_eq!(
            infrastructure_message(timed_out),
            "invocation timed out after 2s; stdout still open"
        );

        let exited = combine_captured_output(
            captured(b"partial stdout", Some("stdout still open")),
            captured(b"", None),
            InvocationResult::Exited(successful_status()),
        );
        assert_eq!(infrastructure_message(exited), "stdout still open");
    }

    #[test]
    fn output_reader_surfaces_read_and_panic_failures() {
        let failed = spawn_output_reader(FailingReader, "failing-reader").expect("create failing reader");
        let failed = finish_output_reader(failed, "stdout", Duration::from_secs(1), CONTAINED_BOUNDARY);
        assert!(
            failed
                .failure
                .as_deref()
                .is_some_and(|failure| failure.contains("injected read failure"))
        );

        let panicked = spawn_output_reader(PanickingReader, "panicking-reader").expect("create panicking reader");
        let panicked = finish_output_reader(panicked, "stderr", Duration::from_secs(1), CONTAINED_BOUNDARY);
        assert!(
            panicked
                .failure
                .as_deref()
                .is_some_and(|failure| failure.contains("injected reader panic"))
        );

        let eof = spawn_output_reader(EofThenPanicReader { reached_eof: false }, "eof-reader").expect("create EOF reader");
        let mut eof = finish_output_reader(eof, "stdout", Duration::from_secs(1), CONTAINED_BOUNDARY);
        assert!(output_bytes(&mut eof.output).is_empty());
        assert!(eof.failure.is_none());

        let interrupted =
            spawn_output_reader(InterruptedThenDataReader { state: 0 }, "interrupted-reader").expect("create interrupted reader");
        let mut interrupted = finish_output_reader(interrupted, "stdout", Duration::from_secs(1), CONTAINED_BOUNDARY);
        assert_eq!(output_bytes(&mut interrupted.output), b"after interrupt");
        assert!(interrupted.failure.is_none());

        let (completion_sender, completion) = mpsc::channel();
        let sealed_timeout = OutputReader {
            thread: thread::spawn(|| {}),
            completion,
            output: Arc::new(Mutex::new(CapturedOutput::empty())),
            retaining: Arc::new(AtomicBool::new(true)),
        };
        let sealed_timeout = finish_output_reader(sealed_timeout, "stdout", Duration::ZERO, CONTAINED_BOUNDARY);
        drop(completion_sender);
        assert!(
            sealed_timeout
                .failure
                .as_deref()
                .is_some_and(|failure| failure.contains("contained process tree"))
        );

        let (completion_sender, completion) = mpsc::channel::<ReaderCompletion>();
        drop(completion_sender);
        let disconnected = OutputReader {
            thread: thread::spawn(|| {}),
            completion,
            output: Arc::new(Mutex::new(CapturedOutput::empty())),
            retaining: Arc::new(AtomicBool::new(true)),
        };
        let disconnected = finish_output_reader(disconnected, "stderr", Duration::from_secs(1), ORDINARY_BOUNDARY);
        assert!(
            disconnected
                .failure
                .as_deref()
                .is_some_and(|failure| failure.contains("without reporting completion"))
        );

        for completion_result in [
            ReaderCompletion::Finished(Ok(())),
            ReaderCompletion::Finished(Err(io::Error::other("read failed"))),
        ] {
            let (completion_sender, completion) = mpsc::channel();
            completion_sender
                .send(completion_result)
                .expect("the synthetic reader completion receiver is alive");
            let poisoned = OutputReader {
                thread: thread::spawn(|| {}),
                completion,
                output: poisoned_buffer(),
                retaining: Arc::new(AtomicBool::new(true)),
            };
            let mut poisoned = finish_output_reader(poisoned, "stdout", Duration::from_secs(1), ORDINARY_BOUNDARY);
            assert_eq!(output_bytes(&mut poisoned.output), b"poisoned bytes");
            assert!(
                poisoned
                    .failure
                    .as_deref()
                    .is_some_and(|failure| failure.contains("capture buffer was poisoned"))
            );
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "creates a named system-temporary spill file")]
    fn output_spills_after_threshold_and_cleans_up_with_its_outcome() {
        let named = tempfile::NamedTempFile::new().expect("create named spill file");
        let path = named.path().to_path_buf();
        let mut named = Some(named);
        let reader = spawn_output_reader_with(
            ChunkedReader {
                chunks: VecDeque::from([b"abcd".to_vec(), b"efgh".to_vec()]),
            },
            "spill-threshold-reader",
            4,
            Box::new(move || Ok(Box::new(named.take().expect("the capture creates at most one spill file")) as Box<dyn SpillFile>)),
        )
        .expect("create threshold reader");
        let mut captured = finish_output_reader(reader, "stdout", Duration::from_secs(1), ORDINARY_BOUNDARY);

        assert!(captured.output.is_spilled());
        assert!(path.exists(), "the spill must live while its outcome owns it");
        assert_eq!(output_bytes(&mut captured.output), b"abcdefgh");

        drop(captured);
        assert!(!path.exists(), "dropping the outcome must remove its spill file");
    }

    #[test]
    fn spill_create_and_write_failures_preserve_memory_and_become_infrastructure_failures() {
        for (factory, expected) in [
            (
                Box::new(|| Err(io::Error::other("injected spill create failure"))) as super::SpillFactory,
                "injected spill create failure",
            ),
            (
                Box::new(|| {
                    Ok(Box::new(FaultySpill {
                        cursor: io::Cursor::new(Vec::new()),
                        fail_write: true,
                        fail_read: false,
                    }) as Box<dyn SpillFile>)
                }) as super::SpillFactory,
                "injected spill write failure",
            ),
        ] {
            let reader = spawn_output_reader_with(
                ChunkedReader {
                    chunks: VecDeque::from([b"abc".to_vec(), b"def".to_vec()]),
                },
                "failing-spill-reader",
                4,
                factory,
            )
            .expect("create failing spill reader");
            let mut captured_stream = finish_output_reader(reader, "stdout", Duration::from_secs(1), ORDINARY_BOUNDARY);
            assert_eq!(output_bytes(&mut captured_stream.output), b"abc");
            let outcome = combine_captured_output(captured_stream, captured(b"", None), InvocationResult::Exited(successful_status()));

            assert!(infrastructure_message(outcome).contains(expected));
        }
    }

    #[test]
    fn spill_read_failure_becomes_an_infrastructure_failure_during_emission() {
        let reader = spawn_output_reader_with(
            io::Cursor::new(b"abcdefgh".to_vec()),
            "read-failing-spill",
            4,
            Box::new(|| {
                Ok(Box::new(FaultySpill {
                    cursor: io::Cursor::new(Vec::new()),
                    fail_write: false,
                    fail_read: true,
                }) as Box<dyn SpillFile>)
            }),
        )
        .expect("create read-failing spill reader");
        let captured_stream = finish_output_reader(reader, "stdout", Duration::from_secs(1), ORDINARY_BOUNDARY);
        assert!(captured_stream.output.is_spilled());
        let mut outcome = combine_captured_output(captured_stream, captured(b"", None), InvocationResult::Exited(successful_status()));

        emit_buffered(&invocation(&["probe"]), &mut outcome).expect("destination output remains writable");

        assert!(infrastructure_message(outcome).contains("injected spill read failure"));
    }

    #[test]
    fn stderr_spill_read_and_destination_write_failures_are_reported() {
        let read_failing_spill = || {
            CapturedOutput::Spill(Box::new(FaultySpill {
                cursor: io::Cursor::new(Vec::new()),
                fail_write: false,
                fail_read: true,
            }))
        };
        let mut outcome = BufferedOutcome {
            stdout: CapturedOutput::empty(),
            stderr: read_failing_spill(),
            result: InvocationResult::Exited(successful_status()),
        };
        emit_buffered_to(&invocation(&["probe"]), &mut outcome, &mut Vec::new(), &mut Vec::new()).expect("destinations remain writable");
        assert!(infrastructure_message(outcome).contains("failed to read spilled child stderr"));

        let mut stdout_failure = BufferedOutcome {
            stdout: CapturedOutput::Memory(b"stdout".to_vec()),
            stderr: CapturedOutput::empty(),
            result: InvocationResult::Exited(successful_status()),
        };
        let error = emit_buffered_to(&invocation(&["probe"]), &mut stdout_failure, &mut FailingWriter, &mut Vec::new())
            .expect_err("stdout destination failure must propagate");
        assert!(error.to_string().contains("injected destination write failure"));

        let mut stderr_failure = BufferedOutcome {
            stdout: CapturedOutput::empty(),
            stderr: CapturedOutput::Memory(b"stderr".to_vec()),
            result: InvocationResult::Exited(successful_status()),
        };
        let error = emit_buffered_to(&invocation(&["probe"]), &mut stderr_failure, &mut Vec::new(), &mut FailingWriter)
            .expect_err("stderr destination failure must propagate");
        assert!(error.to_string().contains("injected destination write failure"));
    }

    #[test]
    fn reader_setup_failure_preserves_cleanup_and_reader_errors() {
        let successful_reader =
            spawn_output_reader(io::Cursor::new(b"partial".to_vec()), "successful-reader").expect("create successful reader");
        let mut outcome = BufferedOutcome::from_reader_failure(
            "stderr unavailable".to_owned(),
            successful_reader,
            &Ok::<_, io::Error>(()),
            ORDINARY_BOUNDARY,
        );
        assert_eq!(output_bytes(&mut outcome.stdout), b"partial");
        assert_eq!(infrastructure_message(outcome), "stderr unavailable");

        let failing_reader = spawn_output_reader(FailingReader, "failing-reader").expect("create failing reader");
        let outcome = BufferedOutcome::from_reader_failure(
            "stderr unavailable".to_owned(),
            failing_reader,
            &Err::<(), _>(io::Error::other("cleanup failed")),
            ORDINARY_BOUNDARY,
        );
        let message = infrastructure_message(outcome);
        assert!(message.contains("stderr unavailable"));
        assert!(message.contains("cleanup failed"));

        let failing_reader = spawn_output_reader(FailingReader, "failing-reader").expect("create failing reader");
        let outcome = BufferedOutcome::from_reader_failure(
            "stderr unavailable".to_owned(),
            failing_reader,
            &Ok::<_, io::Error>(()),
            ORDINARY_BOUNDARY,
        );
        assert!(infrastructure_message(outcome).contains("injected read failure"));
    }

    #[test]
    fn tree_outcome_retains_the_invocation_result() {
        let outcome = TreeOutcome::new(InvocationResult::Infrastructure("outcome".to_owned()));
        assert_eq!(
            infrastructure_message(BufferedOutcome {
                stdout: CapturedOutput::empty(),
                stderr: CapturedOutput::empty(),
                result: outcome.result,
            }),
            "outcome"
        );
    }

    #[test]
    fn process_waiting_classifies_observation_and_cleanup_failures() {
        let mut cleaned = FakeProcess {
            observations: VecDeque::from([Err(io::Error::other("observe failed"))]),
            termination: Some(Ok(successful_status())),
        };
        let outcome = wait_for_tree_with(&mut cleaned, Duration::from_secs(1), FakeProcess::observe, FakeProcess::terminate);
        assert!(result_infrastructure_message(outcome.result).contains("observe failed"));

        let mut uncleaned = FakeProcess {
            observations: VecDeque::from([Err(io::Error::other("observe failed"))]),
            termination: Some(Err(io::Error::other("cleanup failed"))),
        };
        let outcome = wait_for_tree_without_timeout_with(&mut uncleaned, FakeProcess::observe, FakeProcess::terminate);
        let message = result_infrastructure_message(outcome.result);
        assert!(message.contains("observe failed"));
        assert!(message.contains("cleanup failed"));

        let mut timed_uncleaned = FakeProcess {
            observations: VecDeque::from([Err(io::Error::other("timed observe failed"))]),
            termination: Some(Err(io::Error::other("timed cleanup failed"))),
        };
        let outcome = wait_for_tree_with(
            &mut timed_uncleaned,
            Duration::from_secs(1),
            FakeProcess::observe,
            FakeProcess::terminate,
        );
        let message = result_infrastructure_message(outcome.result);
        assert!(message.contains("timed observe failed"));
        assert!(message.contains("timed cleanup failed"));

        let mut untimed_cleaned = FakeProcess {
            observations: VecDeque::from([Err(io::Error::other("untimed observe failed"))]),
            termination: Some(Ok(successful_status())),
        };
        let outcome = wait_for_tree_without_timeout_with(&mut untimed_cleaned, FakeProcess::observe, FakeProcess::terminate);
        assert!(result_infrastructure_message(outcome.result).contains("untimed observe failed"));

        let mut completed = FakeProcess {
            observations: VecDeque::from([Ok(None), Ok(Some(successful_status()))]),
            termination: None,
        };
        let outcome = wait_for_tree_without_timeout_with(&mut completed, FakeProcess::observe, FakeProcess::terminate);
        let InvocationResult::Exited(status) = outcome.result else {
            panic!("untimed waiting must return the completed status");
        };
        assert!(status.success());
    }

    #[test]
    fn timed_process_waiting_covers_completion_and_failed_termination() {
        let mut completed = FakeProcess {
            observations: VecDeque::from([Ok(Some(successful_status()))]),
            termination: None,
        };
        let outcome = wait_for_tree_with(&mut completed, Duration::from_secs(1), FakeProcess::observe, FakeProcess::terminate);
        let InvocationResult::Exited(status) = outcome.result else {
            panic!("a completed process must retain its exit status");
        };
        assert!(status.success());

        let mut uncleaned = FakeProcess {
            observations: VecDeque::from([Ok(None)]),
            termination: Some(Err(io::Error::other("termination failed"))),
        };
        let outcome = wait_for_tree_with(&mut uncleaned, Duration::ZERO, FakeProcess::observe, FakeProcess::terminate);
        assert!(result_infrastructure_message(outcome.result).contains("termination failed"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns a contained child process")]
    fn process_tree_control_delegates_real_exit_observation() {
        let mut command = Command::new("rustc");
        let _ = command.arg("--version").stdout(Stdio::null()).stderr(Stdio::null());
        let mut tree = spawn_tree(command).expect("spawn contained rustc probe");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match tree.observe().expect("observe contained rustc probe") {
                Some(status) => {
                    assert!(status.success());
                    break;
                }
                None if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                None => {
                    let _cleanup = tree.terminate();
                    panic!("process-control delegation did not observe the exited probe before the deadline");
                }
            }
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns ordinary and contained child processes")]
    fn captured_runner_reports_stream_setup_failures() {
        for (label, expected) in [
            ("__cargo_each_missing_stdout", "failed to capture child stdout"),
            ("__cargo_each_stdout_reader_failure", "injected stdout reader failure"),
            ("__cargo_each_missing_stderr", "failed to capture child stderr"),
            ("__cargo_each_stderr_reader_failure", "injected stderr reader failure"),
            ("__cargo_each_wait_failure", "injected child wait failure"),
        ] {
            let outcome = run_captured(&labelled_invocation(label, &["rustc", "--version"]), None);
            assert!(infrastructure_message(outcome).contains(expected), "{label}");
        }

        for (label, expected) in [
            ("__cargo_each_missing_stdout", "failed to capture child stdout"),
            ("__cargo_each_stdout_reader_failure", "injected stdout reader failure"),
            ("__cargo_each_missing_stderr", "failed to capture child stderr"),
            ("__cargo_each_stderr_reader_failure", "injected stderr reader failure"),
        ] {
            let outcome = run_captured_with_spawner(
                &labelled_invocation(label, &["rustc", "--version"]),
                Some(Duration::from_secs(1)),
                spawn_ordinary_capture,
            );
            assert!(infrastructure_message(outcome).contains(expected), "contained {label}");
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns ordinary and contained child processes")]
    fn captured_process_rejects_mismatched_timeout_modes() {
        let mut ordinary_command = Command::new("rustc");
        let _ = ordinary_command.arg("--version").stdout(Stdio::null()).stderr(Stdio::null());
        let mut ordinary = CapturedProcess::Ordinary(Some(ordinary_command.spawn().expect("spawn ordinary rustc")));
        assert_eq!(ordinary.drain_boundary(), "ordinary process tree");
        assert!(
            result_infrastructure_message(ordinary.wait(Some(Duration::from_secs(1)), None).result)
                .contains("did not match timeout configuration")
        );
        let first_wait = ordinary.wait(None, None);
        let InvocationResult::Exited(status) = first_wait.result else {
            panic!("the ordinary child must be reaped by the matching wait mode");
        };
        assert!(status.success());
        assert!(result_infrastructure_message(ordinary.wait(None, None).result).contains("already reaped or detached"));

        let mut contained_command = Command::new("rustc");
        let _ = contained_command.arg("--version").stdout(Stdio::null()).stderr(Stdio::null());
        let mut contained = CapturedProcess::Contained(spawn_tree(contained_command).expect("spawn contained rustc"));
        assert_eq!(contained.drain_boundary(), "contained process tree");
        assert!(result_infrastructure_message(contained.wait(None, None).result).contains("did not match timeout configuration"));
        let _first = contained.terminate_bounded();
        assert!(
            contained.terminate_bounded().is_err(),
            "a fabricated successful second termination must not be accepted"
        );
    }
}
