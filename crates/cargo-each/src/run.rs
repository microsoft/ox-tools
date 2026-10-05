// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the `cargo each` command: resolve the selection,
//! apply filters, build the plan, and run it.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::num::NonZeroUsize;
use std::panic::{self, UnwindSafe};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use std::{fmt, thread};

use cargo_metadata::TargetKind;
use command_group::{CommandGroup as _, GroupChild};
use ohno::{AppError, IntoAppError};
use tempfile::NamedTempFile;

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
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const WORKER_READY_POLL_INTERVAL: Duration = Duration::from_millis(1);
const CHILD_OBSERVATION: &str = "observe child process";
const CHILD_LEADER_OBSERVATION: &str = "observe child process leader";
const STDOUT_STREAM: &str = "stdout";
const STDERR_STREAM: &str = "stderr";
const SNAPSHOT_EOF_MESSAGE: &str = "captured output ended before its finalized snapshot length";
const SELECTION_READ_CONTEXT: &str = "failed to read package selection";
const EXECUTION_CONFIGURATION_CONTEXT: &str = "invalid execution configuration";
const WORKSPACE_RUST_VERSION_CONTEXT: &str = "failed to resolve workspace Rust version";
const PLAN_BUILD_CONTEXT: &str = "failed to build command plan";

pub(crate) fn run(args: &EachArgs) -> Result<ExitCode, AppError> {
    let selection = build_selection(args).into_app_err(SELECTION_READ_CONTEXT)?;
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
        return Err(JobsConflictWithOnceError::new()).into_app_err(EXECUTION_CONFIGURATION_CONTEXT);
    }

    let mut build_options = BuildOptions {
        mode,
        chdir: args.chdir,
        packages,
        target_kinds: &target_kinds,
        target_required_features: &target_required_features,
        workspace_rust_version: None,
    };
    if Plan::is_empty(&members, &args.command, build_options).into_app_err(PLAN_BUILD_CONTEXT)? {
        eprintln!("cargo each: selection resolved to no work; nothing to do");
        return Ok(ExitCode::SUCCESS);
    }

    let workspace_rust_version = if uses_workspace_rust_version(&args.command) {
        Some(workspace.workspace_rust_version().into_app_err(WORKSPACE_RUST_VERSION_CONTEXT)?)
    } else {
        None
    };
    build_options.workspace_rust_version = workspace_rust_version.as_deref();

    let plan = Plan::build(&members, &args.command, build_options)
        .expect("the preceding emptiness check validated these build options before building a nonempty command plan");

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
    NonZeroUsize::new(requested.get().min(plan_size)).expect("Plan::is_empty is checked before execute, so the execution plan is nonempty")
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
    execute_parallel_with(plan, keep_going, worker_count, timeout, spawn_worker, emit_buffered)
}

fn execute_parallel_with(
    plan: &Plan,
    keep_going: bool,
    worker_count: NonZeroUsize,
    timeout: Option<Duration>,
    mut spawn: impl FnMut(usize, Invocation, Option<Duration>) -> io::Result<RunningWorker>,
    mut emit: impl FnMut(&Invocation, &mut BufferedOutcome) -> io::Result<()>,
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
            let index = next_index;
            next_index += 1;
            match spawn(index, invocation, timeout) {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    outcomes.push(IndexedOutcome {
                        index,
                        outcome: BufferedOutcome::infrastructure(format!("failed to create cargo-each worker thread: {error}")),
                    });
                    if !keep_going {
                        break;
                    }
                }
            }
        }

        while let Some(outcome) = wait_for_worker(&mut workers) {
            outcomes.push(outcome);
        }

        outcomes.sort_by_key(|outcome| outcome.index);
        for indexed in &mut outcomes {
            emit(&invocations[indexed.index], &mut indexed.outcome).into_app_err("failed to emit buffered command output")?;
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

fn spawn_worker(index: usize, invocation: Invocation, timeout: Option<Duration>) -> io::Result<RunningWorker> {
    spawn_worker_with(index, invocation, timeout, |name, job| thread::Builder::new().name(name).spawn(job))
}

fn spawn_worker_with(
    index: usize,
    invocation: Invocation,
    timeout: Option<Duration>,
    spawn: impl FnOnce(String, Box<dyn FnOnce() + Send>) -> io::Result<thread::JoinHandle<()>>,
) -> io::Result<RunningWorker> {
    #[cfg(test)]
    if invocation
        .argv
        .first()
        .is_some_and(|program| program == WORKER_SPAWN_ERROR_TEST_PROGRAM)
    {
        return Err(io::Error::other("injected worker spawn failure"));
    }

    let (sender, receiver) = mpsc::channel();
    let job = Box::new(move || {
        complete_worker(&sender, move || run_captured(&invocation, timeout));
    });
    let thread = spawn(format!("cargo-each-worker-{index}"), job)?;
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
    wait_for_worker_with(workers, thread::sleep)
}

fn wait_for_worker_with(workers: &mut Vec<RunningWorker>, mut sleep: impl FnMut(Duration)) -> Option<IndexedOutcome> {
    // #[gamma::skip(cond.always_false, reason = "an empty worker set has no receiver to become ready, so entering the polling loop would never terminate")]
    if workers.is_empty() {
        return None;
    }
    loop {
        let ready = workers
            .iter()
            .enumerate()
            .find_map(|(position, worker)| match worker.receiver.try_recv() {
                Ok(outcome) => Some((position, Some(outcome))),
                // #[gamma::skip(option.some_to_none, tag = "timeout", reason = "a disconnected worker must be selected so the polling loop can remove and join it")]
                Err(mpsc::TryRecvError::Disconnected) => Some((position, None)),
                Err(mpsc::TryRecvError::Empty) => None,
            });
        let Some((position, reported)) = ready else {
            sleep(WORKER_READY_POLL_INTERVAL);
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
    run_streamed_with(invocation, spawn_child)
}

fn run_streamed_with(invocation: &Invocation, spawn: impl FnOnce(Command) -> Result<Child, String>) -> InvocationResult {
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
    wait_for_process(child, None, Child::try_wait, terminate_child, CHILD_OBSERVATION).result
}

fn run_streamed_with_timeout(invocation: &Invocation, timeout: Duration) -> InvocationResult {
    run_streamed_with_timeout_with(invocation, timeout, spawn_group)
}

fn run_streamed_with_timeout_with(
    invocation: &Invocation,
    timeout: Duration,
    spawn: impl FnOnce(Command) -> Result<GroupChild, String>,
) -> InvocationResult {
    run_streamed_group_with(invocation, Some(timeout), spawn)
}

fn run_streamed_group_with(
    invocation: &Invocation,
    timeout: Option<Duration>,
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
        terminate_group,
        CHILD_LEADER_OBSERVATION,
    )
    .result
}

fn run_captured(invocation: &Invocation, timeout: Option<Duration>) -> BufferedOutcome {
    #[cfg(test)]
    assert!(
        invocation.argv.first().is_none_or(|program| program != WORKER_PANIC_TEST_PROGRAM),
        "injected worker panic"
    );

    run_captured_with(invocation, timeout, create_output_capture, spawn_group)
}

fn run_captured_with(
    invocation: &Invocation,
    timeout: Option<Duration>,
    mut capture: impl FnMut(&'static str) -> io::Result<(Box<dyn SnapshotSource>, Stdio)>,
    spawner: impl FnOnce(Command) -> Result<GroupChild, String>,
) -> BufferedOutcome {
    let (program, mut command) = match command_for(invocation) {
        Ok(command) => command,
        Err(message) => return BufferedOutcome::infrastructure(message),
    };
    let (stdout, stdout_stdio) = match capture(STDOUT_STREAM) {
        Ok(capture) => capture,
        Err(error) => {
            return BufferedOutcome::infrastructure(format!("failed to prepare child stdout capture: {error}"));
        }
    };
    let (stderr, stderr_stdio) = match capture(STDERR_STREAM) {
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
        terminate_group,
        CHILD_LEADER_OBSERVATION,
    );
    combine_captured_output(
        finish_capture(stdout, STDOUT_STREAM),
        finish_capture(stderr, STDERR_STREAM),
        process_outcome.result,
    )
}

fn combine_captured_output(stdout: CapturedStream, stderr: CapturedStream, result: InvocationResult) -> BufferedOutcome {
    let failure = [stdout.failure.as_deref(), stderr.failure.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("; ");
    let mut result = result;
    add_infrastructure_failure(&mut result, failure);
    BufferedOutcome {
        stdout: stdout.output,
        stderr: stderr.output,
        result,
    }
}

fn add_infrastructure_failure(result: &mut InvocationResult, failure: String) {
    if failure.is_empty() {
        return;
    }
    let message = match result {
        InvocationResult::Infrastructure(primary) => format!("{primary}; {failure}"),
        InvocationResult::TimedOut(duration) => {
            format!("invocation timed out after {}; {failure}", display_duration(*duration))
        }
        InvocationResult::Exited(_) => failure,
    };
    *result = InvocationResult::Infrastructure(message);
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
    create_output_capture_with(NamedTempFile::new, NamedTempFile::reopen)
}

fn create_output_capture_with(
    create: impl FnOnce() -> io::Result<NamedTempFile>,
    mut reopen: impl FnMut(&NamedTempFile) -> io::Result<File>,
) -> io::Result<(Box<dyn SnapshotSource>, Stdio)> {
    let temporary = create()?;
    let reader = reopen(&temporary)?;
    let child = reopen(&temporary)?;
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

// #[gamma::skip(parameter.default_shadow, tag = "timeout", reason = "the production wrapper must forward the real process control and callbacks")]
fn wait_for_process<T>(
    control: T,
    timeout: Option<Duration>,
    observe: impl FnMut(&mut T) -> io::Result<Option<ExitStatus>>,
    terminate: impl FnOnce(T) -> io::Result<()>,
    operation: &str,
) -> TreeOutcome {
    let started = Instant::now();
    wait_for_process_with(control, timeout, observe, terminate, operation, || started.elapsed(), thread::sleep)
}

// #[gamma::skip(parameter.default_shadow, tag = "timeout", reason = "replacing process-control callbacks with defaults prevents the polling loop from observing progress")]
fn wait_for_process_with<T>(
    mut control: T,
    timeout: Option<Duration>,
    mut observe: impl FnMut(&mut T) -> io::Result<Option<ExitStatus>>,
    terminate: impl FnOnce(T) -> io::Result<()>,
    operation: &str,
    mut elapsed: impl FnMut() -> Duration,
    mut sleep: impl FnMut(Duration),
) -> TreeOutcome {
    let mut terminate = Some(terminate);
    loop {
        if let Some(timeout) = timeout
            && timeout.checked_sub(elapsed()).is_none()
        {
            return match terminate.take().expect("termination is consumed only on a returning branch")(control) {
                Ok(()) => TreeOutcome::new(InvocationResult::TimedOut(timeout)),
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
            .and_then(|timeout| timeout.checked_sub(elapsed()))
            .map_or(PROCESS_POLL_INTERVAL, |remaining| remaining.min(PROCESS_POLL_INTERVAL));
        sleep(pause);
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn terminate_group(mut child: GroupChild) -> io::Result<()> {
    child.kill()
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn terminate_child(mut child: Child) -> io::Result<()> {
    child.kill()
}

fn with_cleanup_failure<T>(message: String, cleanup: &io::Result<T>) -> String {
    match cleanup {
        Ok(_) => message,
        Err(error) => format!("{message}; process-group cleanup also failed: {error}"),
    }
}

fn emit_label(invocation: &Invocation) {
    let mut stderr = io::stderr().lock();
    let _ = emit_label_to(invocation, &mut stderr);
}

fn emit_label_to(invocation: &Invocation, stderr: &mut dyn io::Write) -> io::Result<()> {
    if let Some(label) = &invocation.label {
        writeln!(stderr, "cargo each: {label}")?;
    }
    Ok(())
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
    emit_label_to(invocation, stderr)?;
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
        add_infrastructure_failure(&mut outcome.result, source_failures.join("; "));
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
    reader: File,
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
                let mut buffer = [u8::default(); 8192];
                let mut remaining = *length;
                while remaining > 0 {
                    let limit = usize::try_from(remaining.min(buffer.len() as u64))
                        .expect("the read size is capped by the 8192-byte buffer length");
                    let read = source.read(&mut buffer[..limit]).map_err(OutputEmitError::Source)?;
                    let Some(read) = NonZeroUsize::new(read) else {
                        return Err(OutputEmitError::Source(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            SNAPSHOT_EOF_MESSAGE,
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
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::io::{Read as _, Seek as _};
    use std::num::NonZeroUsize;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt as _;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt as _;
    use std::process::{Command, ExitCode, ExitStatus, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};
    use std::{env, fs, io, thread};

    use clap::Parser as _;
    use tempfile::NamedTempFile;

    use super::{
        BufferedOutcome, CHILD_LEADER_OBSERVATION, CHILD_OBSERVATION, CapturedOutput, CapturedStream, EXECUTION_CONFIGURATION_CONTEXT,
        Invocation, InvocationResult, OutputEmitError, PLAN_BUILD_CONTEXT, PROCESS_POLL_INTERVAL, Plan, RunningWorker,
        SELECTION_READ_CONTEXT, SNAPSHOT_EOF_MESSAGE, STDERR_STREAM, STDOUT_STREAM, SnapshotSource, TemporarySnapshot, TreeOutcome,
        WORKER_PANIC_TEST_PROGRAM, WORKER_READY_POLL_INTERVAL, WORKER_SPAWN_ERROR_TEST_PROGRAM, WORKSPACE_RUST_VERSION_CONTEXT,
        add_infrastructure_failure, apply_filters, combine_captured_output, create_output_capture_with, display_duration,
        effective_worker_count, emit_buffered_to, emit_label_to, execute_parallel, execute_parallel_with, exit_byte,
        failure_stops_launching, finish_capture, panic_description, parallel_failure_exit_code, parse_predicates, parse_target_kinds,
        record_emitted_failure, run_captured, run_captured_with, run_streamed, run_streamed_with_timeout, run_streamed_with_timeout_with,
        shell_join, spawn_group, spawn_worker, spawn_worker_with, terminate_child, terminate_group, wait_for_process,
        wait_for_process_with, wait_for_worker, wait_for_worker_with, with_cleanup_failure,
    };
    use crate::cli::CargoCli;

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
            output: CapturedOutput::Snapshot {
                source: Box::new(io::Cursor::new(bytes.to_vec())),
                length: u64::try_from(bytes.len()).expect("test output length fits in u64"),
            },
            failure: failure.map(str::to_owned),
        }
    }

    fn ready_worker(index: usize, outcome: BufferedOutcome) -> RunningWorker {
        let (sender, receiver) = mpsc::channel();
        let thread = thread::spawn(move || {
            sender.send(outcome).expect("the scheduler keeps the receiver alive");
        });
        RunningWorker { index, receiver, thread }
    }

    fn buffered(result: InvocationResult) -> BufferedOutcome {
        BufferedOutcome {
            stdout: CapturedOutput::empty(),
            stderr: CapturedOutput::empty(),
            result,
        }
    }

    struct FakeProcess {
        observations: VecDeque<io::Result<Option<ExitStatus>>>,
        termination: Option<io::Result<()>>,
    }

    impl FakeProcess {
        fn observe(&mut self) -> io::Result<Option<ExitStatus>> {
            self.observations.pop_front().unwrap_or(Ok(None))
        }

        fn terminate(mut self) -> io::Result<()> {
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

    struct FailingFlushWriter;

    impl io::Write for FailingFlushWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("injected destination flush failure"))
        }
    }

    #[derive(Debug)]
    /// Snapshot source requiring the first read to hit an exact full-buffer boundary.
    struct ExactReadSize {
        bytes: Vec<u8>,
        position: usize,
        first_read_size: usize,
    }

    impl io::Read for ExactReadSize {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.position == 0 && buf.len() != self.first_read_size {
                return Err(io::Error::other(format!("expected first read size {}", self.first_read_size)));
            }
            let remaining = &self.bytes[self.position..];
            let count = remaining.len().min(buf.len());
            buf[..count].copy_from_slice(&remaining[..count]);
            self.position += count;
            Ok(count)
        }
    }

    impl io::Seek for ExactReadSize {
        fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
            let io::SeekFrom::Start(position) = pos else {
                return Err(io::Error::other("the test source only supports absolute seeks"));
            };
            self.position = usize::try_from(position).map_err(io::Error::other)?;
            Ok(position)
        }
    }

    impl SnapshotSource for ExactReadSize {
        fn snapshot_len(&self) -> io::Result<u64> {
            u64::try_from(self.bytes.len()).map_err(io::Error::other)
        }
    }

    fn sleeping_test_command() -> Command {
        let mut command = Command::new(env::current_exe().expect("the test binary knows its path"));
        let _ = command
            .args(["--exact", "run::tests::child_sleep_probe", "--nocapture"])
            .env("CARGO_EACH_CHILD_SLEEP_MS", "30000")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    struct ReleaseMarker(std::path::PathBuf);

    impl Drop for ReleaseMarker {
        fn drop(&mut self) {
            let _ = fs::write(&self.0, b"release");
        }
    }

    #[test]
    fn child_sleep_probe() {
        if let Some(duration) = env::var_os("CARGO_EACH_CHILD_SLEEP_MS") {
            if let Some(marker) = env::var_os("CARGO_EACH_CHILD_STARTED_MARKER") {
                fs::write(marker, b"started").expect("the parent passes a writable start marker path");
            }
            let millis = duration.to_string_lossy().parse().expect("the parent passes milliseconds");
            if let Some(marker) = std::env::var_os("CARGO_EACH_CHILD_RELEASE_MARKER") {
                let deadline = Instant::now() + Duration::from_millis(millis);
                while !std::path::Path::new(&marker).exists() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
            } else {
                thread::sleep(Duration::from_millis(millis));
            }
        }
        if let Some(marker) = env::var_os("CARGO_EACH_CHILD_MARKER") {
            fs::write(marker, b"completed").expect("the parent passes a writable marker path");
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
        assert_eq!(WORKER_READY_POLL_INTERVAL, Duration::from_millis(1));
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
    fn parallel_scheduler_launches_emits_and_stops_by_policy() {
        let plan = Plan {
            invocations: (0..5).map(|index| invocation(&[&index.to_string()])).collect(),
        };
        let workers = NonZeroUsize::new(2).expect("literal two is nonzero");

        let mut launched = Vec::new();
        let mut emitted = Vec::new();
        let code = execute_parallel_with(
            &plan,
            false,
            workers,
            None,
            |index, _, _| {
                launched.push(index);
                let outcome = match index {
                    0 => BufferedOutcome::infrastructure("first failure".to_owned()),
                    1 => buffered(InvocationResult::Exited(successful_status())),
                    _ => panic!("fail-fast must not launch later waves"),
                };
                Ok(ready_worker(index, outcome))
            },
            |invocation, _| {
                emitted.push(invocation.argv[0].parse::<usize>().expect("numeric test invocation"));
                Ok(())
            },
        )
        .expect("emission succeeds");
        assert_eq!(code, ExitCode::from(2));
        assert_eq!(launched, [0, 1], "only the initial wave may launch before its failure is observed");
        assert_eq!(emitted, [0, 1], "each completed initial-wave outcome is emitted once");

        launched.clear();
        emitted.clear();
        let code = execute_parallel_with(
            &plan,
            true,
            workers,
            None,
            |index, _, _| {
                launched.push(index);
                let outcome = if index == 0 {
                    BufferedOutcome::infrastructure("first failure".to_owned())
                } else {
                    buffered(InvocationResult::Exited(successful_status()))
                };
                Ok(ready_worker(index, outcome))
            },
            |invocation, _| {
                emitted.push(invocation.argv[0].parse::<usize>().expect("numeric test invocation"));
                Ok(())
            },
        )
        .expect("emission succeeds");
        assert_eq!(code, ExitCode::from(1));
        assert_eq!(launched, [0, 1, 2, 3, 4]);
        assert_eq!(emitted, [0, 1, 2, 3, 4], "completed outcomes must be cleared between waves");
    }

    #[test]
    fn parallel_scheduler_stops_the_current_wave_after_a_spawn_failure() {
        let plan = Plan {
            invocations: (0..4).map(|index| invocation(&[&index.to_string()])).collect(),
        };
        let workers = NonZeroUsize::new(3).expect("literal three is nonzero");
        let mut launched = Vec::new();
        let mut emitted = Vec::new();
        let code = execute_parallel_with(
            &plan,
            false,
            workers,
            None,
            |index, _, _| {
                launched.push(index);
                assert_eq!(index, 0, "fail-fast spawn failure must stop the current wave immediately");
                Err(io::Error::other("injected spawn failure"))
            },
            |invocation, _| {
                emitted.push(invocation.argv[0].parse::<usize>().expect("numeric test invocation"));
                Ok(())
            },
        )
        .expect("spawn failures are emitted as invocation outcomes");

        assert_eq!(code, ExitCode::from(2));
        assert_eq!(launched, [0], "fail-fast must not launch the rest of the current wave");
        assert_eq!(emitted, [0], "the failed spawn is emitted exactly once");

        launched.clear();
        emitted.clear();
        let code = execute_parallel_with(
            &plan,
            true,
            workers,
            None,
            |index, _, _| {
                launched.push(index);
                if index == 0 {
                    Err(io::Error::other("injected spawn failure"))
                } else {
                    Ok(ready_worker(index, buffered(InvocationResult::Exited(successful_status()))))
                }
            },
            |invocation, _| {
                emitted.push(invocation.argv[0].parse::<usize>().expect("numeric test invocation"));
                Ok(())
            },
        )
        .expect("keep-going retains spawn failures");

        assert_eq!(code, ExitCode::from(1));
        assert_eq!(launched, [0, 1, 2, 3]);
        assert_eq!(emitted, [0, 1, 2, 3]);
    }

    #[test]
    fn parallel_scheduler_stops_after_a_worker_or_emission_failure() {
        let plan = Plan {
            invocations: (0..3).map(|index| invocation(&[&index.to_string()])).collect(),
        };
        let workers = NonZeroUsize::new(2).expect("literal two is nonzero");
        let mut launched = Vec::new();
        let code = execute_parallel_with(
            &plan,
            false,
            workers,
            None,
            |index, _, _| {
                launched.push(index);
                let result = if index == 0 {
                    InvocationResult::Exited(failed_status(7))
                } else {
                    InvocationResult::Exited(successful_status())
                };
                Ok(ready_worker(index, buffered(result)))
            },
            |_, _| Ok(()),
        )
        .expect("emission succeeds");
        assert_eq!(code, ExitCode::from(7));
        assert_eq!(launched, [0, 1]);

        let error = execute_parallel_with(
            &Plan {
                invocations: vec![invocation(&["only"])],
            },
            false,
            NonZeroUsize::MIN,
            None,
            |index, _, _| Ok(ready_worker(index, buffered(InvocationResult::Exited(successful_status())))),
            |_, _| Err(io::Error::other("injected emission failure")),
        )
        .expect_err("emission I/O errors must propagate rather than panic");
        let rendered = error.to_string();
        assert!(rendered.starts_with("injected emission failure\n"), "{rendered}");
        assert!(rendered.contains("> failed to emit buffered command output"), "{rendered}");
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
            FakeProcess::terminate,
            "observe fake process",
        );
        assert!(matches!(outcome.result, InvocationResult::Exited(status) if status.success()));

        let timed_out = FakeProcess {
            observations: VecDeque::from([Ok(None)]),
            termination: Some(Ok(())),
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
            termination: Some(Ok(())),
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
    fn process_polling_uses_the_bounded_ten_millisecond_cadence() {
        assert_eq!(PROCESS_POLL_INTERVAL, Duration::from_millis(10));
        let elapsed = Cell::new(Duration::ZERO);
        let pauses = RefCell::new(Vec::new());
        let process = FakeProcess {
            observations: VecDeque::from([Ok(None), Ok(None), Ok(Some(successful_status()))]),
            termination: None,
        };
        let outcome = wait_for_process_with(
            process,
            Some(Duration::from_millis(25)),
            FakeProcess::observe,
            FakeProcess::terminate,
            "observe fake process",
            || elapsed.get(),
            |pause| {
                pauses.borrow_mut().push(pause);
                elapsed.set(elapsed.get() + pause.max(Duration::from_nanos(1)));
            },
        );
        assert!(matches!(outcome.result, InvocationResult::Exited(status) if status.success()));
        assert_eq!(*pauses.borrow(), [Duration::from_millis(10), Duration::from_millis(10)]);

        let elapsed = Cell::new(Duration::ZERO);
        let pauses = RefCell::new(Vec::new());
        let process = FakeProcess {
            observations: VecDeque::from([Ok(None), Ok(Some(successful_status()))]),
            termination: None,
        };
        let outcome = wait_for_process_with(
            process,
            None,
            FakeProcess::observe,
            FakeProcess::terminate,
            "observe fake process",
            || elapsed.get(),
            |pause| pauses.borrow_mut().push(pause),
        );
        assert!(matches!(outcome.result, InvocationResult::Exited(status) if status.success()));
        assert_eq!(*pauses.borrow(), [Duration::from_millis(10)]);
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns process groups")]
    fn real_group_execution_observes_completion_and_timeout() {
        let success = run_streamed_with_timeout(&invocation(&["rustc", "--version"]), Duration::from_secs(5));
        assert!(matches!(success, InvocationResult::Exited(status) if status.success()));

        let group = spawn_group(sleeping_test_command()).expect("spawn sleeping process group");
        let started = Instant::now();
        terminate_group(group).expect("process-group termination succeeds");
        assert!(started.elapsed() < Duration::from_secs(2));

        let temporary = tempfile::tempdir().expect("create marker directory");
        let started_marker = temporary.path().join("direct-child-started");
        let release_marker = temporary.path().join("direct-child-release");
        let completion_marker = temporary.path().join("direct-child-completed");
        let release = ReleaseMarker(release_marker.clone());
        let child = sleeping_test_command()
            .env("CARGO_EACH_CHILD_SLEEP_MS", "5000")
            .env("CARGO_EACH_CHILD_STARTED_MARKER", &started_marker)
            .env("CARGO_EACH_CHILD_RELEASE_MARKER", &release_marker)
            .env("CARGO_EACH_CHILD_MARKER", &completion_marker)
            .spawn()
            .expect("spawn sleeping direct child");
        let start_deadline = Instant::now() + Duration::from_secs(5);
        while !started_marker.exists() && Instant::now() < start_deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(started_marker.exists(), "the direct child must start before termination");
        let started = Instant::now();
        terminate_child(child).expect("direct-child termination succeeds");
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(release);
        let completion_deadline = Instant::now() + Duration::from_secs(2);
        while !completion_marker.exists() && Instant::now() < completion_deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !completion_marker.exists(),
            "the direct child must not reach its completion marker after termination"
        );
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
    #[cfg_attr(miri, ignore = "spawns subprocesses")]
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
    fn capture_setup_failure_is_reported_before_process_spawn() {
        let invocation = invocation(&["rustc", "--version"]);
        let spawn_calls = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&spawn_calls);
        let outcome = run_captured_with(
            &invocation,
            None,
            |_| Err(io::Error::other("injected capture setup failure")),
            move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Err("spawn must not be reached".to_owned())
            },
        );
        assert_eq!(
            infrastructure_message(outcome),
            "failed to prepare child stdout capture: injected capture setup failure"
        );
        assert_eq!(spawn_calls.load(Ordering::SeqCst), 0);

        let stderr_failure = run_captured_with(
            &invocation,
            None,
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
            |_| Ok((Box::new(io::Cursor::new(Vec::new())), Stdio::null())),
            |_| Err("injected captured spawn failure".to_owned()),
        );
        assert!(infrastructure_message(spawn_failure).contains("injected captured spawn failure"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses filesystem-backed temporary files; Miri isolation forbids them")]
    fn output_capture_creation_propagates_each_filesystem_failure() {
        let create_error = create_output_capture_with(
            || Err(io::Error::other("injected create failure")),
            |_| panic!("reopen must not follow create failure"),
        )
        .expect_err("create failure propagates");
        assert_eq!(create_error.to_string(), "injected create failure");

        let temporary = NamedTempFile::new().expect("create test temporary file");
        let path = temporary.path().to_owned();
        drop(temporary);
        let mut calls = 0;
        let first_reopen = create_output_capture_with(
            || NamedTempFile::new_in(path.parent().expect("temporary path has a parent")),
            |_| {
                calls += 1;
                Err(io::Error::other("injected first reopen failure"))
            },
        )
        .expect_err("first reopen failure propagates");
        assert_eq!(first_reopen.to_string(), "injected first reopen failure");
        assert_eq!(calls, 1);

        let mut calls = 0;
        let second_reopen = create_output_capture_with(NamedTempFile::new, |temporary| {
            calls += 1;
            if calls == 2 {
                Err(io::Error::other("injected second reopen failure"))
            } else {
                temporary.reopen()
            }
        })
        .expect_err("second reopen failure propagates");
        assert_eq!(second_reopen.to_string(), "injected second reopen failure");
        assert_eq!(calls, 2);
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
        let CapturedOutput::Snapshot { source, .. } = &mut captured.output else {
            panic!("successful finalization retains the snapshot");
        };
        assert_eq!(source.stream_position().expect("snapshot position"), 0);
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
        let temporary = NamedTempFile::new().expect("create named temporary capture");
        fs::write(temporary.path(), b"snapshot").expect("write temporary capture");
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
        assert_eq!(
            infrastructure_message(outcome),
            "failed to read captured child stdout: injected snapshot read failure"
        );

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
        assert_eq!(
            infrastructure_message(stderr_outcome),
            "failed to read captured child stderr: injected snapshot read failure"
        );

        let failing_source = || CapturedOutput::Snapshot {
            source: Box::new(FaultySnapshot {
                cursor: io::Cursor::new(Vec::new()),
                reported_length: 1,
                fail_length: false,
                fail_seek: false,
                fail_read: true,
            }),
            length: 1,
        };
        let mut both = BufferedOutcome {
            stdout: failing_source(),
            stderr: failing_source(),
            result: InvocationResult::Infrastructure("process failed".to_owned()),
        };
        emit_buffered_to(&invocation(&["probe"]), &mut both, &mut Vec::new(), &mut Vec::new())
            .expect("source failures become an outcome rather than an emission error");
        assert_eq!(
            infrastructure_message(both),
            "process failed; failed to read captured child stdout: injected snapshot read failure; failed to read captured child stderr: injected snapshot read failure"
        );

        let mut short = CapturedOutput::Snapshot {
            source: Box::new(io::Cursor::new(Vec::new())),
            length: 1,
        };
        let error = short.emit_to(&mut Vec::new()).expect_err("short snapshots are reported");
        assert!(matches!(error, OutputEmitError::Source(error)
                if error.kind() == io::ErrorKind::UnexpectedEof && error.to_string() == SNAPSHOT_EOF_MESSAGE));

        let mut failed_seek = CapturedOutput::Snapshot {
            source: Box::new(FaultySnapshot {
                cursor: io::Cursor::new(Vec::new()),
                reported_length: 0,
                fail_length: false,
                fail_seek: true,
                fail_read: false,
            }),
            length: 0,
        };
        let error = failed_seek.emit_to(&mut Vec::new()).expect_err("snapshot seek failures propagate");
        assert!(matches!(error, OutputEmitError::Source(error) if error.to_string() == "injected snapshot seek failure"));

        let bytes = vec![b'x'; 8192];
        let mut exact_buffer = CapturedOutput::Snapshot {
            source: Box::new(ExactReadSize {
                bytes: bytes.clone(),
                position: 0,
                first_read_size: 8192,
            }),
            length: 8192,
        };
        let mut destination = Vec::new();
        exact_buffer
            .emit_to(&mut destination)
            .expect("the first read uses the complete buffer");
        assert_eq!(destination, bytes);
    }

    #[test]
    fn timed_out_capture_source_failure_preserves_timeout_context() {
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
            result: InvocationResult::TimedOut(Duration::from_millis(10)),
        };
        emit_buffered_to(&invocation(&["probe"]), &mut outcome, &mut Vec::new(), &mut Vec::new())
            .expect("source failures become an outcome rather than an emission error");
        assert_eq!(
            infrastructure_message(outcome),
            "invocation timed out after 10ms; failed to read captured child stdout: injected snapshot read failure"
        );
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

        let mut flush_failure = BufferedOutcome {
            stdout: CapturedOutput::empty(),
            stderr: CapturedOutput::empty(),
            result: InvocationResult::Exited(successful_status()),
        };
        let error = emit_buffered_to(
            &invocation(&["probe"]),
            &mut flush_failure,
            &mut FailingFlushWriter,
            &mut Vec::new(),
        )
        .expect_err("stdout flush failure propagates");
        assert_eq!(error.to_string(), "injected destination flush failure");

        let mut success = BufferedOutcome {
            stdout: CapturedOutput::empty(),
            stderr: CapturedOutput::empty(),
            result: InvocationResult::Exited(successful_status()),
        };
        emit_buffered_to(&invocation(&["probe"]), &mut success, &mut Vec::new(), &mut Vec::new())
            .expect("empty successful output emits cleanly");
        assert!(matches!(success.result, InvocationResult::Exited(status) if status.success()));

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

        for result in [
            InvocationResult::TimedOut(Duration::from_millis(10)),
            InvocationResult::Infrastructure("infrastructure".to_owned()),
        ] {
            let mut diagnostic = buffered(result);
            let error = emit_buffered_to(&invocation(&["probe"]), &mut diagnostic, &mut Vec::new(), &mut FailingWriter)
                .expect_err("diagnostic write failures propagate");
            assert_eq!(error.to_string(), "injected destination write failure");
        }

        let mut labeled = buffered(InvocationResult::Exited(successful_status()));
        let mut stderr = Vec::new();
        emit_buffered_to(
            &Invocation {
                label: Some("alpha".to_owned()),
                argv: vec!["probe".to_owned()],
                work_dir: None,
            },
            &mut labeled,
            &mut Vec::new(),
            &mut stderr,
        )
        .expect("label emission succeeds");
        assert_eq!(stderr, b"cargo each: alpha\n");

        let labeled = Invocation {
            label: Some("alpha".to_owned()),
            argv: vec!["probe".to_owned()],
            work_dir: None,
        };
        let error = emit_label_to(&labeled, &mut FailingWriter).expect_err("label write failures propagate");
        assert_eq!(error.to_string(), "injected destination write failure");

        let mut labeled_outcome = buffered(InvocationResult::Exited(successful_status()));
        let error = emit_buffered_to(&labeled, &mut labeled_outcome, &mut Vec::new(), &mut FailingWriter)
            .expect_err("buffered emission propagates label write failures");
        assert_eq!(error.to_string(), "injected destination write failure");
    }

    #[test]
    fn infrastructure_failure_merging_preserves_primary_context() {
        let mut success = InvocationResult::Exited(successful_status());
        add_infrastructure_failure(&mut success, String::new());
        assert!(matches!(success, InvocationResult::Exited(status) if status.success()));
        let mut timeout = InvocationResult::TimedOut(Duration::from_millis(10));
        add_infrastructure_failure(&mut timeout, "drain failed".to_owned());
        assert_eq!(
            result_infrastructure_message(timeout),
            "invocation timed out after 10ms; drain failed"
        );
        let mut infrastructure = InvocationResult::Infrastructure("wait failed".to_owned());
        add_infrastructure_failure(&mut infrastructure, "drain failed".to_owned());
        assert_eq!(result_infrastructure_message(infrastructure), "wait failed; drain failed");
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
    fn waiting_for_a_worker_yields_until_an_outcome_is_ready() {
        let (sender, receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let thread = thread::spawn(move || {
            release_receiver.recv().expect("the test releases the worker");
            sender
                .send(buffered(InvocationResult::Exited(successful_status())))
                .expect("the scheduler keeps the outcome receiver alive");
        });
        let mut workers = vec![RunningWorker {
            index: 9,
            receiver,
            thread,
        }];
        let pauses = Cell::new(0);
        let mut release_sender = Some(release_sender);
        let outcome = wait_for_worker_with(&mut workers, |pause| {
            assert_eq!(pause, WORKER_READY_POLL_INTERVAL);
            pauses.set(pauses.get() + 1);
            if let Some(sender) = release_sender.take() {
                sender.send(()).expect("release the worker once");
            }
            thread::yield_now();
        })
        .expect("the released worker reports an outcome");

        assert_eq!(outcome.index, 9);
        assert!(pauses.get() >= 1);
        assert!(workers.is_empty());
        assert!(!outcome.outcome.result.failed());
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns a rustc subprocess")]
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
    fn worker_thread_creation_failure_is_returned_without_running_the_job() {
        let error = spawn_worker_with(7, invocation(&["rustc", "--version"]), None, |name, _job| {
            assert_eq!(name, "cargo-each-worker-7");
            Err(io::Error::other("injected thread creation failure"))
        })
        .expect_err("thread creation failure propagates");

        assert_eq!(error.to_string(), "injected thread creation failure");
    }

    #[test]
    fn helper_diagnostics_preserve_cleanup_context() {
        assert_eq!(with_cleanup_failure("primary".to_owned(), &Ok::<_, io::Error>(())), "primary");
        assert_eq!(
            with_cleanup_failure("primary".to_owned(), &Err::<(), _>(io::Error::other("cleanup"))),
            "primary; process-group cleanup also failed: cleanup"
        );
        assert_eq!(panic_description(&"borrowed panic"), "borrowed panic");
        assert_eq!(panic_description(&"owned panic".to_owned()), "owned panic");
        assert_eq!(panic_description(&7_u8), "non-string panic payload");
        assert_eq!(CHILD_OBSERVATION, "observe child process");
        assert_eq!(CHILD_LEADER_OBSERVATION, "observe child process leader");
        assert_eq!(STDOUT_STREAM, "stdout");
        assert_eq!(STDERR_STREAM, "stderr");
        assert_eq!(SELECTION_READ_CONTEXT, "failed to read package selection");
        assert_eq!(EXECUTION_CONFIGURATION_CONTEXT, "invalid execution configuration");
        assert_eq!(WORKSPACE_RUST_VERSION_CONTEXT, "failed to resolve workspace Rust version");
        assert_eq!(PLAN_BUILD_CONTEXT, "failed to build command plan");
    }

    #[test]
    fn tree_outcome_retains_the_invocation_result() {
        let outcome = TreeOutcome::new(InvocationResult::Infrastructure("outcome".to_owned()));
        assert_eq!(result_infrastructure_message(outcome.result), "outcome");
    }

    #[test]
    fn shell_join_only_quotes_arguments_containing_whitespace() {
        assert_eq!(
            shell_join(&[
                "cargo".to_owned(),
                "plain".to_owned(),
                "two words".to_owned(),
                "tab\tseparated".to_owned(),
            ]),
            "cargo plain \"two words\" \"tab\tseparated\""
        );
        assert_eq!(shell_join(&[]), "");
    }

    #[test]
    fn predicate_parse_error_has_run_context_and_exact_cause() {
        let error = parse_predicates(&["feature:".to_owned()]).expect_err("invalid predicate");
        let rendered = error.to_string();
        assert!(rendered.starts_with("invalid filter expression"), "{rendered}");
        assert!(
            rendered.contains("invalid filter expression `feature:`: empty feature name"),
            "{rendered}"
        );
    }

    #[test]
    fn invalid_exclude_filter_propagates_through_filter_application() {
        let CargoCli::Each(args) = CargoCli::try_parse_from(["cargo", "each", "--exclude-filter", "feature:", "--", "echo"])
            .expect("the parser leaves expression validation to execution");
        let mut members = Vec::new();

        let error = apply_filters(&mut members, &args).expect_err("invalid exclusion expression");

        assert!(error.to_string().contains("empty feature name"), "{error}");
    }

    #[test]
    fn target_kind_error_has_run_context_and_exact_cause() {
        let error = parse_target_kinds(&["future-kind".to_owned()]).expect_err("invalid target kind");
        let rendered = error.to_string();
        assert!(rendered.contains("> invalid per-target configuration"), "{rendered}");
        assert!(
            rendered.contains(
                "invalid target kind `future-kind`; expected one of: lib, rlib, dylib, cdylib, staticlib, proc-macro, bin, example, test, bench, custom-build"
            ),
            "{rendered}"
        );
    }
}
