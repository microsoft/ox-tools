// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the `cargo each` command: resolve the selection,
//! apply filters, build the plan, and run it.

use std::collections::BTreeSet;
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::num::NonZeroUsize;
use std::panic::{self, UnwindSafe};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, mpsc};
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
const REAPER_POLL_INTERVAL: Duration = Duration::from_millis(10);

type ReaperJob = Box<dyn FnOnce() + Send + 'static>;

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
    let reaper = ProcessReaper::start().into_app_err("failed to start cargo-each process reaper")?;
    let worker_count = effective_worker_count(jobs, plan.invocations.len());
    if worker_count.get() == 1 {
        Ok(execute_sequential(plan, keep_going, timeout, &reaper))
    } else {
        execute_parallel(plan, keep_going, worker_count, timeout, &reaper)
    }
}

fn effective_worker_count(requested: NonZeroUsize, plan_size: usize) -> NonZeroUsize {
    NonZeroUsize::new(requested.get().min(plan_size)).expect("Plan::is_empty is checked before execute, so the execution plan is nonempty")
}

fn execute_sequential(plan: &Plan, keep_going: bool, timeout: Option<Duration>, reaper: &ProcessReaper) -> ExitCode {
    execute_sequential_with(plan, keep_going, timeout, |invocation, timeout| {
        if let Some(timeout) = timeout {
            run_streamed_with_timeout(invocation, timeout, reaper)
        } else {
            run_streamed(invocation, reaper)
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

fn execute_parallel(
    plan: &Plan,
    keep_going: bool,
    worker_count: NonZeroUsize,
    timeout: Option<Duration>,
    reaper: &ProcessReaper,
) -> Result<ExitCode, AppError> {
    let invocations = plan.invocations.clone();
    let mut workers = Vec::with_capacity(worker_count.get());
    let mut outcomes = Vec::with_capacity(worker_count.get());
    let mut stop_launching = false;
    let mut any_failed = false;
    let mut first_failure = None;
    let mut next_index = 0;

    for wave in invocations.chunks(worker_count.get()) {
        for invocation in wave.iter().cloned() {
            if stop_launching {
                break;
            }
            let index = next_index;
            next_index += 1;
            match spawn_worker(index, invocation, timeout, reaper.clone()) {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    outcomes.push(IndexedOutcome {
                        index,
                        outcome: BufferedOutcome::infrastructure(format!("failed to create cargo-each worker thread: {error}")),
                    });
                    any_failed = true;
                    if failure_stops_launching(keep_going, true) {
                        stop_launching = true;
                    }
                }
            }
        }

        while let Some(outcome) = wait_for_worker(&mut workers) {
            if outcome.outcome.result.failed() {
                any_failed = true;
                if failure_stops_launching(keep_going, true) {
                    stop_launching = true;
                }
            }
            outcomes.push(outcome);
        }

        outcomes.sort_by_key(|outcome| outcome.index);
        for indexed in &mut outcomes {
            emit_buffered(&invocations[indexed.index], &mut indexed.outcome).into_app_err("failed to emit buffered command output")?;
            record_emitted_failure(
                &indexed.outcome.result,
                keep_going,
                &mut any_failed,
                &mut stop_launching,
                &mut first_failure,
            );
        }
        outcomes.clear();
        if stop_launching {
            break;
        }
    }

    if any_failed {
        if keep_going {
            Ok(ExitCode::from(1))
        } else {
            Ok(first_failure.expect("a fail-fast failure is recorded while its completed wave is emitted"))
        }
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

fn record_emitted_failure(
    result: &InvocationResult,
    keep_going: bool,
    any_failed: &mut bool,
    stop_launching: &mut bool,
    first_failure: &mut Option<ExitCode>,
) {
    if !result.failed() {
        return;
    }
    *any_failed = true;
    if failure_stops_launching(keep_going, true) {
        *stop_launching = true;
    }
    if first_failure.is_none() {
        *first_failure = Some(parallel_failure_exit_code(result));
    }
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

fn spawn_worker(index: usize, invocation: Invocation, timeout: Option<Duration>, reaper: ProcessReaper) -> io::Result<RunningWorker> {
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
        complete_worker(&sender, move || run_captured(&invocation, timeout, &reaper));
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

fn run_streamed(invocation: &Invocation, reaper: &ProcessReaper) -> InvocationResult {
    run_streamed_with(invocation, reaper, spawn_child)
}

fn run_streamed_with(
    invocation: &Invocation,
    reaper: &ProcessReaper,
    spawn: impl FnOnce(Command) -> Result<Child, String>,
) -> InvocationResult {
    let (program, command) = match command_for(invocation) {
        Ok(command) => command,
        Err(message) => return InvocationResult::Infrastructure(message),
    };
    let child = match spawn(command) {
        Ok(child) => child,
        Err(error) => {
            return InvocationResult::Infrastructure(format!("failed to spawn `{program}`: {error}"));
        }
    };
    let control = StreamedChild { child, reaper };
    wait_for_process(
        control,
        None,
        observe_streamed_child,
        terminate_streamed_child,
        "observe child process",
    )
    .result
}

struct StreamedChild<'a> {
    child: Child,
    reaper: &'a ProcessReaper,
}

fn observe_streamed_child(control: &mut StreamedChild<'_>) -> io::Result<Option<ExitStatus>> {
    control.child.try_wait()
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[mutants::skip] // Thin ownership adapter for an OS wait-error path; terminate_child_bounded has a real-process regression.
fn terminate_streamed_child(control: StreamedChild<'_>) -> io::Result<ExitStatus> {
    terminate_child_bounded(control.child, control.reaper)
}

fn run_streamed_with_timeout(invocation: &Invocation, timeout: Duration, reaper: &ProcessReaper) -> InvocationResult {
    run_streamed_with_timeout_with(invocation, timeout, reaper, spawn_group)
}

fn run_streamed_with_timeout_with(
    invocation: &Invocation,
    timeout: Duration,
    reaper: &ProcessReaper,
    spawn: impl FnOnce(Command) -> Result<GroupChild, String>,
) -> InvocationResult {
    run_streamed_group_with(invocation, Some(timeout), reaper, spawn)
}

fn run_streamed_group_with(
    invocation: &Invocation,
    timeout: Option<Duration>,
    reaper: &ProcessReaper,
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
        timeout,
        |process| process.inner().try_wait(),
        |process| terminate_group_bounded(process, reaper),
        "observe child process leader",
    )
    .result
}

fn run_captured(invocation: &Invocation, timeout: Option<Duration>, reaper: &ProcessReaper) -> BufferedOutcome {
    #[cfg(test)]
    assert!(
        invocation.argv.first().is_none_or(|program| program != WORKER_PANIC_TEST_PROGRAM),
        "injected worker panic"
    );

    run_captured_with(invocation, timeout, reaper, create_output_capture, spawn_group)
}

fn run_captured_with(
    invocation: &Invocation,
    timeout: Option<Duration>,
    reaper: &ProcessReaper,
    mut capture: impl FnMut(&'static str) -> io::Result<(Box<dyn SnapshotSource>, Stdio)>,
    spawner: impl FnOnce(Command) -> Result<GroupChild, String>,
) -> BufferedOutcome {
    let (program, mut command) = match command_for(invocation) {
        Ok(command) => command,
        Err(message) => return BufferedOutcome::infrastructure(message),
    };
    let (stdout, stdout_stdio) = match capture("stdout") {
        Ok(capture) => capture,
        Err(error) => {
            return BufferedOutcome::infrastructure(format!("failed to prepare child stdout capture: {error}"));
        }
    };
    let (stderr, stderr_stdio) = match capture("stderr") {
        Ok(capture) => capture,
        Err(error) => {
            return BufferedOutcome::infrastructure(format!("failed to prepare child stderr capture: {error}"));
        }
    };
    let _ = command.stdin(Stdio::null()).stdout(stdout_stdio).stderr(stderr_stdio);
    let process = match spawner(command) {
        Ok(process) => process,
        Err(error) => {
            return BufferedOutcome::infrastructure(format!("failed to spawn `{program}`: {error}"));
        }
    };

    let process_outcome = wait_for_process(
        process,
        timeout,
        |process| process.inner().try_wait(),
        |process| terminate_group_bounded(process, reaper),
        "observe child process leader",
    );
    combine_captured_output(
        finish_capture(stdout, "stdout"),
        finish_capture(stderr, "stderr"),
        process_outcome.result,
    )
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

fn spawn_child(mut command: Command) -> Result<Child, String> {
    command.spawn().map_err(|error| error.to_string())
}

fn create_output_capture(_stream: &'static str) -> io::Result<(Box<dyn SnapshotSource>, Stdio)> {
    let temporary = tempfile::NamedTempFile::new()?;
    let reader = temporary.reopen()?;
    let child = temporary.reopen()?;
    let path = temporary.into_temp_path();
    Ok((Box::new(TemporarySnapshot { _path: path, reader }), Stdio::from(child)))
}

fn finish_capture(mut source: Box<dyn SnapshotSource>, stream: &str) -> CapturedStream {
    let result = source
        .snapshot_len()
        .and_then(|length| source.seek(SeekFrom::Start(0)).map(|_| length));
    match result {
        Ok(length) => CapturedStream {
            output: CapturedOutput::Snapshot { source, length },
            failure: None,
        },
        Err(error) => CapturedStream {
            output: CapturedOutput::empty(),
            failure: Some(format!("failed to finalize child {stream} capture: {error}")),
        },
    }
}

fn wait_for_process<T>(
    mut control: T,
    timeout: Option<Duration>,
    mut observe: impl FnMut(&mut T) -> io::Result<Option<ExitStatus>>,
    terminate: impl FnOnce(T) -> io::Result<ExitStatus>,
    operation: &str,
) -> TreeOutcome {
    let started = Instant::now();
    let mut terminate = Some(terminate);
    loop {
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

        let pause = timeout
            .and_then(|timeout| timeout.checked_sub(started.elapsed()))
            .map_or(Duration::from_millis(10), |remaining| remaining.min(Duration::from_millis(10)));
        thread::sleep(pause);
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn terminate_group_bounded(child: GroupChild, reaper: &ProcessReaper) -> io::Result<ExitStatus> {
    terminate_group_with(
        child,
        TERMINATION_GRACE,
        GroupChild::kill,
        |child| child.inner().try_wait(),
        |child| reaper.handoff_group(child),
    )
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn terminate_child_bounded(child: Child, reaper: &ProcessReaper) -> io::Result<ExitStatus> {
    terminate_group_with(child, TERMINATION_GRACE, Child::kill, Child::try_wait, |child| {
        reaper.handoff_child(child)
    })
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
                || format!("process boundary did not exit within {} ms after termination", grace.as_millis()),
                |error| {
                    format!(
                        "{error}; process boundary did not exit within {} ms after termination",
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
                with_reaper_handoff(&format!("failed to observe process boundary after termination: {error}"), &reaper),
            ))
        }
    }
}

#[derive(Debug)]
struct ProcessReaper {
    group_sender: mpsc::Sender<GroupChild>,
    child_sender: mpsc::Sender<Child>,
}

impl Clone for ProcessReaper {
    fn clone(&self) -> Self {
        Self {
            group_sender: self.group_sender.clone(),
            child_sender: self.child_sender.clone(),
        }
    }
}

impl ProcessReaper {
    fn start() -> io::Result<Self> {
        Self::start_with(|job| {
            thread::Builder::new()
                .name("cargo-each-process-reaper".to_owned())
                .spawn(job)
                .map(drop)
        })
    }

    fn start_with(mut spawn: impl FnMut(ReaperJob) -> io::Result<()>) -> io::Result<Self> {
        let (group_sender, group_receiver) = mpsc::channel();
        spawn(Box::new(move || {
            poll_reaper(&group_receiver, GroupChild::try_wait, report_reaper_failure);
        }))?;
        let (child_sender, child_receiver) = mpsc::channel();
        spawn(Box::new(move || {
            poll_reaper(&child_receiver, Child::try_wait, report_reaper_failure);
        }))?;
        Ok(Self {
            group_sender,
            child_sender,
        })
    }

    fn handoff_group(&self, child: GroupChild) -> io::Result<()> {
        let retained = failed_handoffs();
        handoff_group_with_fallback(&self.group_sender, retained, child, || start_failed_handoff_reaper(retained))
    }

    fn handoff_child(&self, child: Child) -> io::Result<()> {
        let retained = failed_child_handoffs();
        handoff_group_with_fallback(&self.child_sender, retained, child, || start_failed_child_handoff_reaper(retained))
    }
}

fn failed_handoffs() -> &'static Mutex<Vec<GroupChild>> {
    static FAILED_HANDOFFS: OnceLock<Mutex<Vec<GroupChild>>> = OnceLock::new();
    FAILED_HANDOFFS.get_or_init(|| Mutex::new(Vec::new()))
}

fn start_failed_handoff_reaper(retained: &'static Mutex<Vec<GroupChild>>) -> io::Result<()> {
    static RUNNING: AtomicBool = AtomicBool::new(false);
    start_failed_handoff_reaper_with(retained, &RUNNING, GroupChild::try_wait, report_reaper_failure, |job| {
        thread::Builder::new()
            .name("cargo-each-fallback-reaper".to_owned())
            .spawn(job)
            .map(drop)
    })
}

fn failed_child_handoffs() -> &'static Mutex<Vec<Child>> {
    static FAILED_HANDOFFS: OnceLock<Mutex<Vec<Child>>> = OnceLock::new();
    FAILED_HANDOFFS.get_or_init(|| Mutex::new(Vec::new()))
}

fn start_failed_child_handoff_reaper(retained: &'static Mutex<Vec<Child>>) -> io::Result<()> {
    static RUNNING: AtomicBool = AtomicBool::new(false);
    start_failed_handoff_reaper_with(retained, &RUNNING, Child::try_wait, report_reaper_failure, |job| {
        thread::Builder::new()
            .name("cargo-each-fallback-child-reaper".to_owned())
            .spawn(job)
            .map(drop)
    })
}

fn start_failed_handoff_reaper_with<T: Send + 'static>(
    retained: &'static Mutex<Vec<T>>,
    running: &'static AtomicBool,
    try_wait: fn(&mut T) -> io::Result<Option<ExitStatus>>,
    report_failure: fn(&io::Error),
    spawn: impl FnOnce(ReaperJob) -> io::Result<()>,
) -> io::Result<()> {
    if running.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return Ok(());
    }
    match spawn(Box::new(move || {
        poll_failed_handoffs(retained, running, try_wait, report_failure);
    })) {
        Ok(()) => Ok(()),
        Err(error) => {
            running.store(false, Ordering::Release);
            Err(error)
        }
    }
}

fn poll_failed_handoffs<T>(
    retained: &Mutex<Vec<T>>,
    running: &AtomicBool,
    mut try_wait: impl FnMut(&mut T) -> io::Result<Option<ExitStatus>>,
    mut report_failure: impl FnMut(&io::Error),
) {
    loop {
        let mut children = retained.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        children.retain_mut(|child| retain_after_reaper_observation(try_wait(child), &mut report_failure));
        if children.is_empty() {
            running.store(false, Ordering::Release);
            return;
        }
        drop(children);
        thread::sleep(REAPER_POLL_INTERVAL);
    }
}

fn handoff_group<T>(sender: &mpsc::Sender<T>, retained: &Mutex<Vec<T>>, child: T) -> io::Result<()> {
    match sender.send(child) {
        Ok(()) => Ok(()),
        Err(error) => {
            retained.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(error.0);
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "process-group reaper channel disconnected; a persistent fallback retained the wait handle",
            ))
        }
    }
}

fn handoff_group_with_fallback<T>(
    sender: &mpsc::Sender<T>,
    retained: &Mutex<Vec<T>>,
    child: T,
    start_fallback: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let handoff = handoff_group(sender, retained, child);
    if handoff.is_err()
        && let Err(error) = start_fallback()
    {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            format!("process-group reaper channel disconnected; the fallback retained the wait handle but failed to start: {error}"),
        ));
    }
    handoff
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[mutants::skip] // Process-thread diagnostic for an OS observation failure; behavior is covered through the injected reporter seam.
fn report_reaper_failure(error: &io::Error) {
    let message = error.to_string();
    let _ = thread::Builder::new()
        .name("cargo-each-reaper-diagnostic".to_owned())
        .spawn(move || {
            let _ = writeln!(
                io::stderr().lock(),
                "cargo each: process-group reaper failed to observe a retained group: {message}"
            );
        });
}

#[mutants::skip] // Deleting the disconnected-and-empty shutdown arm hangs by definition; deterministic tests cover polling and exit.
fn poll_reaper<T>(
    receiver: &mpsc::Receiver<T>,
    mut try_wait: impl FnMut(&mut T) -> io::Result<Option<ExitStatus>>,
    mut report_failure: impl FnMut(&io::Error),
) {
    let mut retained: Vec<T> = Vec::new();
    let mut connected = true;
    loop {
        if connected {
            match receiver.recv_timeout(REAPER_POLL_INTERVAL) {
                Ok(child) => retained.push(child),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => connected = false,
            }
        }

        retained.retain_mut(|child| retain_after_reaper_observation(try_wait(child), &mut report_failure));

        match (connected, retained.is_empty()) {
            (false, true) => return,
            _ => thread::sleep(REAPER_POLL_INTERVAL),
        }
    }
}

fn retain_after_reaper_observation(observation: io::Result<Option<ExitStatus>>, report_failure: &mut impl FnMut(&io::Error)) -> bool {
    match observation {
        Ok(Some(_)) => false,
        Ok(None) => true,
        Err(error) if error.kind() == io::ErrorKind::Interrupted => true,
        Err(error) => {
            report_failure(&error);
            false
        }
    }
}

fn with_reaper_handoff(message: &str, reaper: &io::Result<()>) -> String {
    match reaper {
        Ok(()) => format!("{message}; the process wait handle was moved to the local polling reaper"),
        Err(error) => format!("{message}; failed to hand the process wait handle to the local reaper: {error}"),
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
            source_failures.push(format!("failed to read captured child stdout: {error}"));
        }
        Err(OutputEmitError::Destination(error)) => return Err(error),
    }
    stdout.flush()?;
    match outcome.stderr.emit_to(stderr) {
        Ok(()) => {}
        Err(OutputEmitError::Source(error)) => {
            source_failures.push(format!("failed to read captured child stderr: {error}"));
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

trait SnapshotSource: io::Read + io::Seek + Send + fmt::Debug {
    fn snapshot_len(&self) -> io::Result<u64>;
}

#[derive(Debug)]
struct TemporarySnapshot {
    _path: tempfile::TempPath,
    reader: std::fs::File,
}

impl io::Read for TemporarySnapshot {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

impl io::Seek for TemporarySnapshot {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.reader.seek(pos)
    }
}

impl SnapshotSource for TemporarySnapshot {
    fn snapshot_len(&self) -> io::Result<u64> {
        self.reader.metadata().map(|metadata| metadata.len())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
impl SnapshotSource for io::Cursor<Vec<u8>> {
    fn snapshot_len(&self) -> io::Result<u64> {
        u64::try_from(self.get_ref().len()).map_err(io::Error::other)
    }
}

#[derive(Debug)]
enum CapturedOutput {
    Empty,
    Snapshot { source: Box<dyn SnapshotSource>, length: u64 },
}

impl CapturedOutput {
    fn empty() -> Self {
        Self::Empty
    }

    fn emit_to(&mut self, destination: &mut dyn io::Write) -> Result<(), OutputEmitError> {
        match self {
            Self::Empty => Ok(()),
            Self::Snapshot { source, length } => {
                source.seek(SeekFrom::Start(0)).map_err(OutputEmitError::Source)?;
                let mut buffer = [0_u8; 8192];
                let mut remaining = *length;
                while remaining > 0 {
                    let limit = usize::try_from(remaining.min(buffer.len() as u64))
                        .expect("the read size is capped by the 8192-byte buffer length");
                    let read = source.read(&mut buffer[..limit]).map_err(OutputEmitError::Source)?;
                    let Some(read) = NonZeroUsize::new(read) else {
                        return Err(OutputEmitError::Source(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "captured output ended before its finalized snapshot length",
                        )));
                    };
                    destination.write_all(&buffer[..read.get()]).map_err(OutputEmitError::Destination)?;
                    remaining -= u64::try_from(read.get()).expect("a read byte count always fits in u64");
                }
                Ok(())
            }
        }
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
    use std::io::{Read as _, Seek as _};
    use std::num::NonZeroUsize;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt as _;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt as _;
    use std::process::{Command, ExitCode, ExitStatus, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, OnceLock, mpsc};
    use std::time::{Duration, Instant};
    use std::{io, thread};

    use super::{
        BufferedOutcome, CapturedOutput, CapturedStream, Invocation, InvocationResult, OutputEmitError, Plan, ProcessReaper, RunningWorker,
        SnapshotSource, StreamedChild, TemporarySnapshot, TreeOutcome, WORKER_PANIC_TEST_PROGRAM, WORKER_SPAWN_ERROR_TEST_PROGRAM,
        add_infrastructure_failure, combine_captured_output, display_duration, effective_worker_count, emit_buffered_to, execute_parallel,
        exit_byte, failed_child_handoffs, failed_handoffs, failure_stops_launching, finish_capture, handoff_group,
        handoff_group_with_fallback, observe_streamed_child, panic_description, parallel_failure_exit_code, poll_process_exit, poll_reaper,
        record_emitted_failure, retain_after_reaper_observation, run_captured, run_captured_with, run_streamed, run_streamed_with_timeout,
        run_streamed_with_timeout_with, spawn_group, spawn_worker, start_failed_handoff_reaper_with, terminate_child_bounded,
        terminate_group_bounded, terminate_group_with, wait_for_process, wait_for_worker, with_cleanup_failure, with_reaper_handoff,
    };

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

    static FALLBACK_TEST_GROUPS: OnceLock<Mutex<Vec<usize>>> = OnceLock::new();
    static FALLBACK_TEST_RUNNING: AtomicBool = AtomicBool::new(false);
    static FALLBACK_TEST_COLLECTED: AtomicUsize = AtomicUsize::new(0);
    static FALLBACK_TEST_REPORTED: AtomicUsize = AtomicUsize::new(0);

    fn observe_fallback_test_group(state: &mut usize) -> io::Result<Option<ExitStatus>> {
        match *state {
            3 => Err(io::Error::other("injected terminal fallback observation failure")),
            2 => {
                *state = 1;
                Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "injected interrupted fallback observation",
                ))
            }
            1 => {
                *state = 0;
                Ok(None)
            }
            _ => {
                FALLBACK_TEST_COLLECTED.fetch_add(1, Ordering::SeqCst);
                Ok(Some(successful_status()))
            }
        }
    }

    fn report_fallback_test_error(_error: &io::Error) {
        FALLBACK_TEST_REPORTED.fetch_add(1, Ordering::SeqCst);
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
            output: CapturedOutput::Snapshot {
                source: Box::new(io::Cursor::new(bytes.to_vec())),
                length: u64::try_from(bytes.len()).expect("test output length fits in u64"),
            },
            failure: failure.map(str::to_owned),
        }
    }

    fn test_reaper() -> ProcessReaper {
        ProcessReaper::start().expect("the test process can start its reaper")
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

    #[derive(Debug)]
    struct FaultySnapshot {
        cursor: io::Cursor<Vec<u8>>,
        reported_length: u64,
        fail_length: bool,
        fail_seek: bool,
        fail_read: bool,
    }

    impl io::Read for FaultySnapshot {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.fail_read {
                Err(io::Error::other("injected snapshot read failure"))
            } else {
                io::Read::read(&mut self.cursor, buf)
            }
        }
    }

    impl io::Seek for FaultySnapshot {
        fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
            if self.fail_seek {
                Err(io::Error::other("injected snapshot seek failure"))
            } else {
                io::Seek::seek(&mut self.cursor, pos)
            }
        }
    }

    impl SnapshotSource for FaultySnapshot {
        fn snapshot_len(&self) -> io::Result<u64> {
            if self.fail_length {
                Err(io::Error::other("injected snapshot length failure"))
            } else {
                Ok(self.reported_length)
            }
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
    fn emitted_capture_failures_update_parallel_scheduler_state() {
        let result = InvocationResult::Infrastructure("capture read failed".to_owned());
        let mut any_failed = false;
        let mut stop_launching = false;
        let mut first_failure = None;
        record_emitted_failure(
            &InvocationResult::Exited(successful_status()),
            false,
            &mut any_failed,
            &mut stop_launching,
            &mut first_failure,
        );
        assert!(!any_failed);
        assert!(!stop_launching);
        assert!(first_failure.is_none());

        record_emitted_failure(&result, false, &mut any_failed, &mut stop_launching, &mut first_failure);
        assert!(any_failed);
        assert!(stop_launching);
        assert_eq!(first_failure, Some(ExitCode::from(2)));

        let mut keep_going_failed = false;
        let mut keep_going_stop = false;
        let mut keep_going_first = None;
        record_emitted_failure(&result, true, &mut keep_going_failed, &mut keep_going_stop, &mut keep_going_first);
        assert!(keep_going_failed);
        assert!(!keep_going_stop);
        assert_eq!(keep_going_first, Some(ExitCode::from(2)));

        record_emitted_failure(
            &InvocationResult::Exited(failed_status(7)),
            true,
            &mut keep_going_failed,
            &mut keep_going_stop,
            &mut keep_going_first,
        );
        assert_eq!(keep_going_first, Some(ExitCode::from(2)), "the first plan-order failure wins");
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
    #[cfg_attr(miri, ignore = "spawns rustc subprocesses")]
    fn scheduler_surfaces_worker_launch_failures_in_both_policies() {
        let reaper = test_reaper();
        let fail_fast = Plan {
            invocations: vec![invocation(&[WORKER_SPAWN_ERROR_TEST_PROGRAM]), invocation(&["rustc", "--version"])],
        };
        let code = execute_parallel(
            &fail_fast,
            false,
            NonZeroUsize::new(2).expect("literal two is nonzero"),
            None,
            &reaper,
        )
        .expect("worker launch failure is an invocation outcome");
        assert_eq!(code, ExitCode::from(2));

        let keep_going = Plan {
            invocations: vec![invocation(&[WORKER_SPAWN_ERROR_TEST_PROGRAM]), invocation(&["rustc", "--version"])],
        };
        let code = execute_parallel(
            &keep_going,
            true,
            NonZeroUsize::new(2).expect("literal two is nonzero"),
            None,
            &reaper,
        )
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
            FakeProcess::terminate,
            "observe fake process",
        );
        assert!(matches!(outcome.result, InvocationResult::TimedOut(duration) if duration.is_zero()));

        let late_success = FakeProcess {
            observations: VecDeque::from([Ok(None), Ok(Some(successful_status()))]),
            termination: Some(Ok(successful_status())),
        };
        let outcome = wait_for_process(
            late_success,
            Some(Duration::from_millis(1)),
            FakeProcess::observe,
            FakeProcess::terminate,
            "observe fake process",
        );
        assert!(
            matches!(outcome.result, InvocationResult::TimedOut(duration) if duration == Duration::from_millis(1)),
            "completion observed after the deadline must not win the race"
        );

        let failed = FakeProcess {
            observations: VecDeque::from([Err(io::Error::other("observation failed"))]),
            termination: Some(Err(io::Error::other("cleanup failed"))),
        };
        let outcome = wait_for_process(failed, None, FakeProcess::observe, FakeProcess::terminate, "observe fake process");
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
            FakeProcess::terminate,
            "observe fake process",
        );
        assert!(result_infrastructure_message(outcome.result).contains("timeout cleanup failed"));
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
        assert!(deadline.to_string().contains("local polling reaper"));

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
        assert!(failed_observation.to_string().contains("local polling reaper"));
    }

    #[test]
    fn reaper_startup_failure_is_reported_synchronously() {
        let error = ProcessReaper::start_with(|job| {
            drop(job);
            Err(io::Error::other("injected reaper startup failure"))
        })
        .expect_err("startup failure must be returned");
        assert!(error.to_string().contains("injected reaper startup failure"));

        let mut starts = 0;
        let error = ProcessReaper::start_with(|job| {
            starts += 1;
            if starts == 1 {
                thread::Builder::new().spawn(job).map(drop)
            } else {
                drop(job);
                Err(io::Error::other("injected child-reaper startup failure"))
            }
        })
        .expect_err("child-reaper startup failure must be returned");
        assert!(error.to_string().contains("injected child-reaper startup failure"));
    }

    #[test]
    fn polling_reaper_checks_every_retained_group_and_eventually_collects_them() {
        struct FakeGroup {
            errors_remaining: usize,
            error_kind: io::ErrorKind,
            polls_remaining: usize,
            collected: Arc<AtomicUsize>,
        }

        let (sender, receiver) = mpsc::channel();
        let collected = Arc::new(AtomicUsize::new(0));
        let reports = Arc::new(AtomicUsize::new(0));
        let first = FakeGroup {
            errors_remaining: 2,
            error_kind: io::ErrorKind::Interrupted,
            polls_remaining: 20,
            collected: Arc::clone(&collected),
        };
        let second = FakeGroup {
            errors_remaining: 0,
            error_kind: io::ErrorKind::Other,
            polls_remaining: 0,
            collected: Arc::clone(&collected),
        };
        let terminal = FakeGroup {
            errors_remaining: 1,
            error_kind: io::ErrorKind::Other,
            polls_remaining: 0,
            collected: Arc::clone(&collected),
        };
        sender.send(first).expect("reaper receiver is connected");
        sender.send(second).expect("reaper receiver is connected");
        sender.send(terminal).expect("reaper receiver is connected");
        drop(sender);

        let (done_sender, done_receiver) = mpsc::channel();
        let worker = thread::spawn({
            let reports = Arc::clone(&reports);
            move || {
                poll_reaper(
                    &receiver,
                    |group| {
                        if group.errors_remaining > 0 {
                            group.errors_remaining -= 1;
                            return Err(io::Error::new(group.error_kind, "injected reaper observation failure"));
                        }
                        if group.polls_remaining == 0 {
                            group.collected.fetch_add(1, Ordering::SeqCst);
                            Ok(Some(successful_status()))
                        } else {
                            group.polls_remaining -= 1;
                            Ok(None)
                        }
                    },
                    |_| {
                        reports.fetch_add(1, Ordering::SeqCst);
                    },
                );
                let _receiver_gone = done_sender.send(());
            }
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while collected.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            collected.load(Ordering::SeqCst),
            1,
            "the ready group must be collected while another group remains pending"
        );
        done_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("the finite fake reaper must stop after the sender disconnects");
        worker.join().expect("the finite fake reaper exits");
        assert_eq!(collected.load(Ordering::SeqCst), 2);
        assert_eq!(reports.load(Ordering::SeqCst), 1, "each failing group is reported once");
    }

    #[test]
    fn reaper_observation_retention_distinguishes_transient_and_terminal_states() {
        let mut reported = Vec::new();
        assert!(!retain_after_reaper_observation(Ok(Some(successful_status())), &mut |error| {
            reported.push(error.kind());
        }));
        assert!(retain_after_reaper_observation(Ok(None), &mut |error| {
            reported.push(error.kind());
        }));
        assert!(retain_after_reaper_observation(
            Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted")),
            &mut |error| {
                reported.push(error.kind());
            },
        ));
        assert!(!retain_after_reaper_observation(Err(io::Error::other("terminal")), &mut |error| {
            reported.push(error.kind());
        },));
        assert_eq!(reported, [io::ErrorKind::Other]);
    }

    #[test]
    fn failed_reaper_handoff_recovers_ownership_before_returning() {
        let (connected_sender, connected_receiver) = mpsc::channel();
        let retained = Mutex::new(Vec::new());
        handoff_group(&connected_sender, &retained, "delivered group").expect("connected handoff succeeds");
        assert_eq!(connected_receiver.try_recv().expect("group is delivered"), "delivered group");
        assert!(retained.lock().expect("fallback ownership mutex is not poisoned").is_empty());

        let (sender, receiver) = mpsc::channel();
        drop(receiver);
        let retained = Mutex::new(Vec::new());
        let error = handoff_group(&sender, &retained, "owned group").expect_err("disconnected handoff is reported");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(
            retained.lock().expect("fallback ownership mutex is not poisoned").as_slice(),
            ["owned group"]
        );
    }

    #[test]
    fn fallback_handoff_reports_startup_failure_without_losing_ownership() {
        let (sender, receiver) = mpsc::channel();
        drop(receiver);
        let retained = Mutex::new(Vec::new());
        let error = handoff_group_with_fallback(&sender, &retained, "owned group", || {
            Err(io::Error::other("injected fallback startup failure"))
        })
        .expect_err("fallback startup failure is reported");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(error.to_string().contains("injected fallback startup failure"));
        assert_eq!(
            retained.lock().expect("fallback ownership mutex is not poisoned").as_slice(),
            ["owned group"]
        );
    }

    #[test]
    fn fallback_reaper_startup_and_polling_cover_every_state() {
        let retained = FALLBACK_TEST_GROUPS.get_or_init(|| Mutex::new(Vec::new()));
        retained.lock().expect("fallback ownership mutex is not poisoned").clear();
        FALLBACK_TEST_RUNNING.store(true, Ordering::Release);
        FALLBACK_TEST_COLLECTED.store(0, Ordering::SeqCst);
        FALLBACK_TEST_REPORTED.store(0, Ordering::SeqCst);
        start_failed_handoff_reaper_with(
            retained,
            &FALLBACK_TEST_RUNNING,
            observe_fallback_test_group,
            report_fallback_test_error,
            |_| {
                panic!("an already-running fallback must not spawn another thread");
            },
        )
        .expect("an already-running fallback accepts more work");

        FALLBACK_TEST_RUNNING.store(false, Ordering::Release);
        retained.lock().expect("fallback ownership mutex is not poisoned").push(2);
        let error = start_failed_handoff_reaper_with(
            retained,
            &FALLBACK_TEST_RUNNING,
            observe_fallback_test_group,
            report_fallback_test_error,
            |job| {
                drop(job);
                Err(io::Error::other("injected fallback thread failure"))
            },
        )
        .expect_err("fallback thread failure is reported");
        assert!(error.to_string().contains("injected fallback thread failure"));
        assert!(!FALLBACK_TEST_RUNNING.load(Ordering::Acquire));
        assert_eq!(retained.lock().expect("fallback ownership mutex is not poisoned").len(), 1);

        retained.lock().expect("fallback ownership mutex is not poisoned").push(3);
        start_failed_handoff_reaper_with(
            retained,
            &FALLBACK_TEST_RUNNING,
            observe_fallback_test_group,
            report_fallback_test_error,
            |job| thread::Builder::new().spawn(job).map(drop),
        )
        .expect("fallback polling thread starts");
        let deadline = Instant::now() + Duration::from_secs(1);
        while FALLBACK_TEST_RUNNING.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            !FALLBACK_TEST_RUNNING.load(Ordering::Acquire),
            "fallback polling thread did not finish"
        );
        assert_eq!(FALLBACK_TEST_COLLECTED.load(Ordering::SeqCst), 1);
        assert_eq!(FALLBACK_TEST_REPORTED.load(Ordering::SeqCst), 1);
        assert!(retained.lock().expect("fallback ownership mutex is not poisoned").is_empty());
    }

    #[test]
    fn failed_handoff_storage_is_process_stable() {
        assert!(std::ptr::eq(failed_handoffs(), failed_handoffs()));
        assert!(std::ptr::eq(failed_child_handoffs(), failed_child_handoffs()));
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns a process group and a fallback reaper thread")]
    fn disconnected_reaper_handoff_is_eventually_collected() {
        let (sender, receiver) = mpsc::channel();
        drop(receiver);
        let (child_sender, _child_receiver) = mpsc::channel();
        let reaper = ProcessReaper {
            group_sender: sender,
            child_sender,
        };
        let mut command = Command::new("rustc");
        let _ = command.arg("--version").stdout(Stdio::null()).stderr(Stdio::null());
        let child = spawn_group(command).expect("spawn a short-lived process group");

        let error = reaper
            .handoff_group(child)
            .expect_err("the disconnected primary reaper is reported");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !failed_handoffs()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }

        assert!(
            failed_handoffs()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "the fallback reaper must eventually collect a recovered handoff"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns a child process and a fallback reaper thread")]
    fn disconnected_child_reaper_handoff_is_eventually_collected() {
        let (group_sender, _group_receiver) = mpsc::channel();
        let (child_sender, child_receiver) = mpsc::channel();
        drop(child_receiver);
        let reaper = ProcessReaper {
            group_sender,
            child_sender,
        };
        let mut command = Command::new("rustc");
        let child = command
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn a short-lived child");

        let error = reaper.handoff_child(child).expect_err("the disconnected child reaper is reported");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !failed_child_handoffs()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            failed_child_handoffs()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "the fallback child reaper must eventually collect a recovered handoff"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns process groups")]
    fn real_group_execution_observes_completion_and_timeout() {
        let reaper = test_reaper();
        let success = run_streamed_with_timeout(&invocation(&["rustc", "--version"]), Duration::from_secs(5), &reaper);
        assert!(matches!(success, InvocationResult::Exited(status) if status.success()));

        let group = spawn_group(sleeping_test_command()).expect("spawn sleeping process group");
        let started = Instant::now();
        let error = terminate_group_bounded(group, &reaper).expect("killed process group is reaped");
        assert!(!error.success());
        assert!(started.elapsed() < Duration::from_secs(2));

        let child = sleeping_test_command().spawn().expect("spawn sleeping direct child");
        let started = Instant::now();
        let status = terminate_child_bounded(child, &reaper).expect("killed direct child is reaped");
        assert!(!status.success());
        assert!(started.elapsed() < Duration::from_secs(2));

        let mut quick = Command::new("rustc");
        let _ = quick.arg("--version").stdout(Stdio::null()).stderr(Stdio::null());
        let mut group = spawn_group(quick).expect("spawn quick process group");
        group.inner().wait().expect("quick leader exits");
        reaper.handoff_group(group).expect("completed group reaches the local reaper");
        drop(reaper);
        thread::sleep(Duration::from_millis(100));
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns and captures process groups")]
    fn captured_runner_uses_group_control_and_keeps_output() {
        let reaper = test_reaper();
        let mut untimed = run_captured(&invocation(&["rustc", "--version"]), None, &reaper);
        assert!(matches!(untimed.result, InvocationResult::Exited(status) if status.success()));
        assert!(
            String::from_utf8(output_bytes(&mut untimed.stdout))
                .expect("rustc output is UTF-8")
                .contains("rustc")
        );

        let timed = run_captured(&invocation(&["rustc", "--version"]), Some(Duration::from_secs(5)), &reaper);
        assert!(matches!(timed.result, InvocationResult::Exited(status) if status.success()));
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns subprocesses")]
    fn direct_runners_report_empty_and_unspawnable_commands() {
        let reaper = test_reaper();
        let empty = invocation(&[]);
        assert!(
            matches!(run_streamed(&empty, &reaper), InvocationResult::Infrastructure(message) if message.contains("empty argument vector"))
        );
        assert!(infrastructure_message(run_captured(&empty, None, &reaper)).contains("empty argument vector"));
        assert!(matches!(
            run_streamed_with_timeout(&empty, Duration::from_secs(1), &reaper),
            InvocationResult::Infrastructure(message) if message.contains("empty argument vector")
        ));

        let missing = invocation(&["__cargo_each_missing_program_for_unit_test__"]);
        assert!(
            matches!(run_streamed(&missing, &reaper), InvocationResult::Infrastructure(message) if message.contains("failed to spawn"))
        );
        assert!(infrastructure_message(run_captured(&missing, None, &reaper)).contains("failed to spawn"));

        let injected = run_streamed_with_timeout_with(&invocation(&["rustc", "--version"]), Duration::from_secs(1), &reaper, |_| {
            Err("injected group spawn failure".to_owned())
        });
        assert!(matches!(
            injected,
            InvocationResult::Infrastructure(message) if message.contains("injected group spawn failure")
        ));
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns a child process")]
    fn streamed_child_observation_returns_a_completed_status() {
        let reaper = test_reaper();
        let mut command = Command::new("rustc");
        let mut child = command
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn short-lived child");
        let expected = child.wait().expect("wait for short-lived child");
        let mut control = StreamedChild { child, reaper: &reaper };
        assert_eq!(
            observe_streamed_child(&mut control).expect("observe completed child"),
            Some(expected)
        );
    }

    #[test]
    fn capture_setup_failure_is_reported_before_process_spawn() {
        let reaper = test_reaper();
        let invocation = invocation(&["rustc", "--version"]);
        let spawn_calls = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&spawn_calls);
        let outcome = run_captured_with(
            &invocation,
            None,
            &reaper,
            |_| Err(io::Error::other("injected capture setup failure")),
            move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Err("spawn must not be reached".to_owned())
            },
        );
        assert!(infrastructure_message(outcome).contains("injected capture setup failure"));
        assert_eq!(spawn_calls.load(Ordering::SeqCst), 0);

        let stderr_failure = run_captured_with(
            &invocation,
            None,
            &reaper,
            |stream| {
                if stream == "stdout" {
                    Ok((Box::new(io::Cursor::new(Vec::new())), Stdio::null()))
                } else {
                    Err(io::Error::other("injected stderr capture failure"))
                }
            },
            |_| Err("spawn must not be reached".to_owned()),
        );
        assert!(infrastructure_message(stderr_failure).contains("injected stderr capture failure"));

        let spawn_failure = run_captured_with(
            &invocation,
            None,
            &reaper,
            |_| Ok((Box::new(io::Cursor::new(Vec::new())), Stdio::null())),
            |_| Err("injected captured spawn failure".to_owned()),
        );
        assert!(infrastructure_message(spawn_failure).contains("injected captured spawn failure"));
    }

    #[test]
    fn capture_finalization_and_emission_use_a_finite_snapshot() {
        let source = FaultySnapshot {
            cursor: io::Cursor::new(b"snapshot-later".to_vec()),
            reported_length: 8,
            fail_length: false,
            fail_seek: false,
            fail_read: false,
        };
        let mut captured = finish_capture(Box::new(source), "stdout");
        assert!(captured.failure.is_none());
        assert_eq!(output_bytes(&mut captured.output), b"snapshot");

        let failed_length = finish_capture(
            Box::new(FaultySnapshot {
                cursor: io::Cursor::new(Vec::new()),
                reported_length: 0,
                fail_length: true,
                fail_seek: false,
                fail_read: false,
            }),
            "stdout",
        );
        assert!(
            failed_length
                .failure
                .is_some_and(|failure| failure.contains("injected snapshot length failure"))
        );

        let failed_seek = finish_capture(
            Box::new(FaultySnapshot {
                cursor: io::Cursor::new(Vec::new()),
                reported_length: 0,
                fail_length: false,
                fail_seek: true,
                fail_read: false,
            }),
            "stderr",
        );
        assert!(
            failed_seek
                .failure
                .is_some_and(|failure| failure.contains("injected snapshot seek failure"))
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses filesystem-backed temporary files; Miri isolation forbids them")]
    fn temporary_snapshot_seek_rewinds_the_independent_reader() {
        let temporary = tempfile::NamedTempFile::new().expect("create named temporary capture");
        std::fs::write(temporary.path(), b"snapshot").expect("write temporary capture");
        let reader = temporary.reopen().expect("reopen temporary capture reader");
        let path = temporary.into_temp_path();
        let mut snapshot = TemporarySnapshot { _path: path, reader };
        let mut first = [0_u8; 1];
        snapshot.read_exact(&mut first).expect("read first snapshot byte");
        assert_eq!(first, [b's']);

        snapshot.seek(io::SeekFrom::Start(0)).expect("rewind snapshot reader");
        let mut output = Vec::new();
        snapshot.read_to_end(&mut output).expect("read rewound snapshot");
        assert_eq!(output, b"snapshot");
    }

    #[test]
    fn capture_source_failures_become_infrastructure_failures() {
        let mut outcome = BufferedOutcome {
            stdout: CapturedOutput::Snapshot {
                source: Box::new(FaultySnapshot {
                    cursor: io::Cursor::new(Vec::new()),
                    reported_length: 1,
                    fail_length: false,
                    fail_seek: false,
                    fail_read: true,
                }),
                length: 1,
            },
            stderr: CapturedOutput::empty(),
            result: InvocationResult::Exited(successful_status()),
        };
        emit_buffered_to(&invocation(&["probe"]), &mut outcome, &mut Vec::new(), &mut Vec::new()).expect("destinations remain writable");
        assert!(infrastructure_message(outcome).contains("injected snapshot read failure"));

        let mut stderr_outcome = BufferedOutcome {
            stdout: CapturedOutput::empty(),
            stderr: CapturedOutput::Snapshot {
                source: Box::new(FaultySnapshot {
                    cursor: io::Cursor::new(Vec::new()),
                    reported_length: 1,
                    fail_length: false,
                    fail_seek: false,
                    fail_read: true,
                }),
                length: 1,
            },
            result: InvocationResult::Exited(successful_status()),
        };
        emit_buffered_to(&invocation(&["probe"]), &mut stderr_outcome, &mut Vec::new(), &mut Vec::new())
            .expect("destinations remain writable");
        assert!(infrastructure_message(stderr_outcome).contains("injected snapshot read failure"));

        let mut short = CapturedOutput::Snapshot {
            source: Box::new(io::Cursor::new(Vec::new())),
            length: 1,
        };
        let error = short.emit_to(&mut Vec::new()).expect_err("short snapshots are reported");
        assert!(matches!(error, OutputEmitError::Source(error) if error.kind() == io::ErrorKind::UnexpectedEof));
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
            stdout: captured(b"stdout", None).output,
            stderr: CapturedOutput::empty(),
            result: InvocationResult::Exited(successful_status()),
        };
        let error = emit_buffered_to(&invocation(&["probe"]), &mut outcome, &mut FailingWriter, &mut Vec::new())
            .expect_err("destination failure propagates");
        assert!(error.to_string().contains("injected destination write failure"));

        let mut stderr_failure = BufferedOutcome {
            stdout: CapturedOutput::empty(),
            stderr: captured(b"stderr", None).output,
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
    fn worker_panics_and_disconnects_become_infrastructure_outcomes() {
        let reaper = test_reaper();
        let plan = Plan {
            invocations: vec![Invocation {
                label: Some("panic-probe".to_owned()),
                argv: vec![WORKER_PANIC_TEST_PROGRAM.to_owned()],
                work_dir: None,
            }],
        };
        let code = execute_parallel(&plan, false, NonZeroUsize::new(2).expect("literal two is nonzero"), None, &reaper)
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
    #[cfg_attr(miri, ignore = "spawns a rustc subprocess")]
    fn worker_spawn_errors_are_local_and_scheduler_visible() {
        let reaper = test_reaper();
        let error = spawn_worker(0, invocation(&[WORKER_SPAWN_ERROR_TEST_PROGRAM]), None, reaper.clone())
            .expect_err("the local seam rejects only its sentinel");
        assert!(error.to_string().contains("injected worker spawn failure"));

        let worker = spawn_worker(1, invocation(&["rustc", "--version"]), None, reaper).expect("ordinary program launches");
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
        assert!(with_reaper_handoff("deadline", &Ok(())).contains("local polling reaper"));
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
