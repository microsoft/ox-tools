// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the `cargo each` command: resolve the selection,
//! apply filters, build the plan, and run it.

use std::collections::{BTreeSet, VecDeque};
use std::io::{self, Write as _};
use std::num::NonZeroUsize;
use std::panic::{self, UnwindSafe};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use cargo_gamma_process::{MemoryRequest, ProcessTree, prepare};
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

    for indexed in &outcomes {
        emit_buffered(&invocations[indexed.index], &indexed.outcome).into_app_err("failed to emit buffered command output")?;
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
    let mut tree = match spawn_tree(command) {
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

    let (program, mut command) = match command_for(invocation) {
        Ok(command) => command,
        Err(message) => return BufferedOutcome::infrastructure(message),
    };
    let _ = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut tree = match spawn_tree(command) {
        Ok(tree) => tree,
        Err(error) => {
            return BufferedOutcome::infrastructure(format!("failed to spawn `{program}`: {error}"));
        }
    };
    let capture_fault = capture_fault(invocation);

    let stdout = if capture_fault == Some(CaptureFault::MissingStdout) {
        None
    } else {
        tree.take_stdout()
    };
    let Some(stdout) = stdout else {
        let cleanup = tree.terminate();
        return BufferedOutcome::infrastructure(with_cleanup_failure("failed to capture child stdout".to_owned(), &cleanup));
    };
    let stdout_reader = match if capture_fault == Some(CaptureFault::StdoutReader) {
        Err(io::Error::other("injected stdout reader failure"))
    } else {
        spawn_output_reader(stdout, "cargo-each-stdout")
    } {
        Ok(reader) => reader,
        Err(error) => {
            let cleanup = tree.terminate();
            return BufferedOutcome::infrastructure(with_cleanup_failure(format!("failed to create stdout reader: {error}"), &cleanup));
        }
    };
    let stderr = if capture_fault == Some(CaptureFault::MissingStderr) {
        None
    } else {
        tree.take_stderr()
    };
    let Some(stderr) = stderr else {
        let cleanup = tree.terminate();
        return BufferedOutcome::from_reader_failure("failed to capture child stderr".to_owned(), stdout_reader, &cleanup);
    };
    let stderr_reader = match if capture_fault == Some(CaptureFault::StderrReader) {
        Err(io::Error::other("injected stderr reader failure"))
    } else {
        spawn_output_reader(stderr, "cargo-each-stderr")
    } {
        Ok(reader) => reader,
        Err(error) => {
            let cleanup = tree.terminate();
            return BufferedOutcome::from_reader_failure(format!("failed to create stderr reader: {error}"), stdout_reader, &cleanup);
        }
    };

    let tree_outcome = match timeout {
        Some(timeout) => wait_for_tree(&mut tree, timeout),
        None => wait_for_tree_without_timeout(&mut tree),
    };
    let stdout = finish_output_reader(stdout_reader, "stdout", tree_outcome.cleanup_proven);
    let stderr = finish_output_reader(stderr_reader, "stderr", tree_outcome.cleanup_proven);
    combine_captured_output(stdout, stderr, tree_outcome.result)
}

fn combine_captured_output(stdout: Result<Vec<u8>, String>, stderr: Result<Vec<u8>, String>, result: InvocationResult) -> BufferedOutcome {
    match (stdout, stderr) {
        (Ok(stdout), Ok(stderr)) => BufferedOutcome { stdout, stderr, result },
        (Err(error), Ok(stderr)) => BufferedOutcome {
            stdout: Vec::new(),
            stderr,
            result: InvocationResult::Infrastructure(error),
        },
        (Ok(stdout), Err(error)) => BufferedOutcome {
            stdout,
            stderr: Vec::new(),
            result: InvocationResult::Infrastructure(error),
        },
        (Err(stdout), Err(stderr)) => BufferedOutcome {
            stdout: Vec::new(),
            stderr: Vec::new(),
            result: InvocationResult::Infrastructure(format!("{stdout}; {stderr}")),
        },
    }
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

fn spawn_tree(command: Command) -> Result<ProcessTree, String> {
    let prepared =
        prepare(command, MemoryRequest::default()).map_err(|error| format!("could not prepare process-tree containment: {error}"))?;
    let spawned = prepared.spawn().map_err(|failure| failure.to_string())?;
    ProcessTree::adopt(spawned).map_err(|error| format!("could not adopt child into process-tree containment: {error}"))
}

fn wait_for_tree(tree: &mut ProcessTree, timeout: Duration) -> TreeOutcome {
    wait_for_tree_with(tree, timeout, ProcessTree::observe, ProcessTree::terminate)
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
            Ok(Some(status)) => return TreeOutcome::closed(InvocationResult::Exited(status)),
            Ok(None) => {}
            Err(error) => {
                let cleanup = terminate(control);
                return match cleanup {
                    Ok(_) => TreeOutcome::closed(InvocationResult::Infrastructure(format!(
                        "failed to observe child process tree: {error}"
                    ))),
                    Err(cleanup) => TreeOutcome::unproven(InvocationResult::Infrastructure(format!(
                        "failed to observe child process tree: {error}; cleanup also failed: {cleanup}"
                    ))),
                };
            }
        }
        let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
            return match terminate(control) {
                Ok(_) => TreeOutcome::closed(InvocationResult::TimedOut(timeout)),
                Err(error) => TreeOutcome::unproven(InvocationResult::Infrastructure(format!(
                    "invocation timed out after {}; process-tree termination failed: {error}",
                    display_duration(timeout)
                ))),
            };
        };
        thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}

fn wait_for_tree_without_timeout(tree: &mut ProcessTree) -> TreeOutcome {
    wait_for_tree_without_timeout_with(tree, ProcessTree::observe, ProcessTree::terminate)
}

fn wait_for_tree_without_timeout_with<T>(
    control: &mut T,
    mut observe: impl FnMut(&mut T) -> io::Result<Option<ExitStatus>>,
    mut terminate: impl FnMut(&mut T) -> io::Result<ExitStatus>,
) -> TreeOutcome {
    loop {
        match observe(control) {
            Ok(Some(status)) => return TreeOutcome::closed(InvocationResult::Exited(status)),
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                let cleanup = terminate(control);
                return match cleanup {
                    Ok(_) => TreeOutcome::closed(InvocationResult::Infrastructure(format!(
                        "failed to observe child process tree: {error}"
                    ))),
                    Err(cleanup) => TreeOutcome::unproven(InvocationResult::Infrastructure(format!(
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
}

fn capture_fault(invocation: &Invocation) -> Option<CaptureFault> {
    #[cfg(test)]
    {
        match invocation.label.as_deref() {
            Some("__cargo_each_missing_stdout") => Some(CaptureFault::MissingStdout),
            Some("__cargo_each_stdout_reader_failure") => Some(CaptureFault::StdoutReader),
            Some("__cargo_each_missing_stderr") => Some(CaptureFault::MissingStderr),
            Some("__cargo_each_stderr_reader_failure") => Some(CaptureFault::StderrReader),
            _ => None,
        }
    }
    #[cfg(not(test))]
    {
        let _ = invocation;
        None
    }
}

fn spawn_output_reader<R>(mut stream: R, name: &'static str) -> io::Result<OutputReader>
where
    R: io::Read + Send + 'static,
{
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let retaining = Arc::new(AtomicBool::new(true));
    let captured = Arc::clone(&bytes);
    let capture_enabled = Arc::clone(&retaining);
    let thread = thread::Builder::new().name(name.to_owned()).spawn(move || {
        let mut chunk = [0_u8; 8192];
        loop {
            if !capture_enabled.load(Ordering::Acquire) {
                return Ok(());
            }
            let read = match stream.read(&mut chunk)? {
                0 => return Ok(()),
                read => read,
            };
            let mut output = captured
                .lock()
                .map_err(|error| io::Error::other(format!("child {name} capture buffer was poisoned: {error}")))?;
            if !capture_enabled.load(Ordering::Acquire) {
                return Ok(());
            }
            output.extend_from_slice(&chunk[..read]);
        }
    })?;
    Ok(OutputReader { thread, bytes, retaining })
}

fn finish_output_reader(reader: OutputReader, stream: &str, cleanup_proven: bool) -> Result<Vec<u8>, String> {
    let OutputReader { thread, bytes, retaining } = reader;
    if cleanup_proven {
        match thread.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(format!("failed to read child {stream}: {error}")),
            Err(payload) => {
                return Err(format!(
                    "child {stream} reader thread panicked: {}",
                    panic_description(payload.as_ref())
                ));
            }
        }
    } else {
        // A surviving descendant may keep the write end open forever. Stop
        // retaining data, take the bytes already captured, and detach the
        // blocked reader rather than defeating the invocation timeout.
        retaining.store(false, Ordering::Release);
        drop(thread);
    }
    let mut captured = bytes
        .lock()
        .map_err(|error| format!("child {stream} capture buffer was poisoned: {error}"))?;
    Ok(std::mem::take(&mut *captured))
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

fn emit_buffered(invocation: &Invocation, outcome: &BufferedOutcome) -> io::Result<()> {
    emit_label(invocation);
    let mut stdout = io::stdout().lock();
    stdout.write_all(&outcome.stdout)?;
    stdout.flush()?;
    let mut stderr = io::stderr().lock();
    stderr.write_all(&outcome.stderr)?;
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
    thread: thread::JoinHandle<io::Result<()>>,
    bytes: Arc<Mutex<Vec<u8>>>,
    retaining: Arc<AtomicBool>,
}

#[derive(Debug)]
struct TreeOutcome {
    result: InvocationResult,
    cleanup_proven: bool,
}

impl TreeOutcome {
    fn closed(result: InvocationResult) -> Self {
        Self {
            result,
            cleanup_proven: true,
        }
    }

    fn unproven(result: InvocationResult) -> Self {
        Self {
            result,
            cleanup_proven: false,
        }
    }
}

#[derive(Debug)]
struct BufferedOutcome {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    result: InvocationResult,
}

impl BufferedOutcome {
    fn infrastructure(message: String) -> Self {
        Self {
            stdout: Vec::new(),
            stderr: Vec::new(),
            result: InvocationResult::Infrastructure(message),
        }
    }

    fn from_reader_failure<T>(message: String, stdout_reader: OutputReader, cleanup: &io::Result<T>) -> Self {
        let message = with_cleanup_failure(message, cleanup);
        match finish_output_reader(stdout_reader, "stdout", cleanup.is_ok()) {
            Ok(stdout) => Self {
                stdout,
                stderr: Vec::new(),
                result: InvocationResult::Infrastructure(message),
            },
            Err(reader_error) => Self {
                stdout: Vec::new(),
                stderr: Vec::new(),
                result: InvocationResult::Infrastructure(format!("{message}; {reader_error}")),
            },
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
    use std::num::NonZeroUsize;
    use std::process::{Command, ExitCode, ExitStatus, Stdio};
    use std::sync::{Arc, Condvar, Mutex, mpsc};
    use std::time::{Duration, Instant};
    use std::{io, thread};

    use super::{
        BufferedOutcome, Invocation, InvocationResult, Plan, RunningWorker, TreeOutcome, WORKER_PANIC_TEST_PROGRAM,
        WORKER_SPAWN_ERROR_TEST_PROGRAM, combine_captured_output, display_duration, execute_parallel, exit_byte, failure_stops_launching,
        finish_output_reader, panic_description, run_captured, run_streamed, run_streamed_with_timeout, spawn_output_reader, spawn_tree,
        wait_for_tree_with, wait_for_tree_without_timeout_with, wait_for_worker, with_cleanup_failure,
    };

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
        Command::new("rustc")
            .arg("--version")
            .status()
            .expect("rustc is available to the crate's test suite")
    }

    fn infrastructure_message(outcome: BufferedOutcome) -> String {
        result_infrastructure_message(outcome.result)
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
    fn unproven_cleanup_does_not_join_a_stubborn_pipe_reader() {
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
            let result = finish_output_reader(reader, "stdout", false);
            let _receiver_gone = finished_tx.send(result);
        });
        let result = finished_rx.recv_timeout(Duration::from_millis(500));

        let (lock, condition) = &*release;
        *lock.lock().expect("the test owns the release mutex without panicking") = true;
        condition.notify_all();

        let captured = result
            .expect("unproven cleanup must not wait for a descendant to close its pipe")
            .expect("capturing already-read output succeeds");
        finisher.join().expect("the bounded finisher thread does not panic");
        reader_finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("the detached reader exits after the test releases its simulated pipe");
        assert_eq!(captured, b"captured-before-timeout");
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

        let captured = finish_output_reader(reader, "stdout", false).expect("stop capture without joining");
        assert!(captured.is_empty());

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
        let success = combine_captured_output(
            Ok(b"stdout".to_vec()),
            Ok(b"stderr".to_vec()),
            InvocationResult::Infrastructure("primary".to_owned()),
        );
        assert_eq!(success.stdout, b"stdout");
        assert_eq!(success.stderr, b"stderr");
        assert_eq!(infrastructure_message(success), "primary");

        let stdout_failed = combine_captured_output(
            Err("stdout failed".to_owned()),
            Ok(b"stderr".to_vec()),
            InvocationResult::Infrastructure("primary".to_owned()),
        );
        assert!(stdout_failed.stdout.is_empty());
        assert_eq!(stdout_failed.stderr, b"stderr");
        assert_eq!(infrastructure_message(stdout_failed), "stdout failed");

        let stderr_failed = combine_captured_output(
            Ok(b"stdout".to_vec()),
            Err("stderr failed".to_owned()),
            InvocationResult::Infrastructure("primary".to_owned()),
        );
        assert_eq!(stderr_failed.stdout, b"stdout");
        assert!(stderr_failed.stderr.is_empty());
        assert_eq!(infrastructure_message(stderr_failed), "stderr failed");

        let both_failed = combine_captured_output(
            Err("stdout failed".to_owned()),
            Err("stderr failed".to_owned()),
            InvocationResult::Infrastructure("primary".to_owned()),
        );
        assert!(both_failed.stdout.is_empty());
        assert!(both_failed.stderr.is_empty());
        assert_eq!(infrastructure_message(both_failed), "stdout failed; stderr failed");
    }

    #[test]
    fn output_reader_surfaces_read_and_panic_failures() {
        let failed = spawn_output_reader(FailingReader, "failing-reader").expect("create failing reader");
        assert!(
            finish_output_reader(failed, "stdout", true)
                .expect_err("read failure must be reported")
                .contains("injected read failure")
        );

        let panicked = spawn_output_reader(PanickingReader, "panicking-reader").expect("create panicking reader");
        assert!(
            finish_output_reader(panicked, "stderr", true)
                .expect_err("reader panic must be reported")
                .contains("injected reader panic")
        );

        let eof = spawn_output_reader(EofThenPanicReader { reached_eof: false }, "eof-reader").expect("create EOF reader");
        assert_eq!(
            finish_output_reader(eof, "stdout", true).expect("EOF completes the output reader"),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn reader_setup_failure_preserves_cleanup_and_reader_errors() {
        let successful_reader =
            spawn_output_reader(io::Cursor::new(b"partial".to_vec()), "successful-reader").expect("create successful reader");
        let outcome = BufferedOutcome::from_reader_failure("stderr unavailable".to_owned(), successful_reader, &Ok::<_, io::Error>(()));
        assert_eq!(outcome.stdout, b"partial");
        assert_eq!(infrastructure_message(outcome), "stderr unavailable");

        let failing_reader = spawn_output_reader(FailingReader, "failing-reader").expect("create failing reader");
        let outcome = BufferedOutcome::from_reader_failure(
            "stderr unavailable".to_owned(),
            failing_reader,
            &Err::<(), _>(io::Error::other("cleanup failed")),
        );
        let message = infrastructure_message(outcome);
        assert!(message.contains("stderr unavailable"));
        assert!(message.contains("cleanup failed"));

        let failing_reader = spawn_output_reader(FailingReader, "failing-reader").expect("create failing reader");
        let outcome = BufferedOutcome::from_reader_failure("stderr unavailable".to_owned(), failing_reader, &Ok::<_, io::Error>(()));
        assert!(infrastructure_message(outcome).contains("injected read failure"));
    }

    #[test]
    fn tree_outcome_constructors_record_cleanup_certainty() {
        let closed = TreeOutcome::closed(InvocationResult::Infrastructure("closed".to_owned()));
        assert!(closed.cleanup_proven);
        assert_eq!(
            infrastructure_message(BufferedOutcome {
                stdout: Vec::new(),
                stderr: Vec::new(),
                result: closed.result,
            }),
            "closed"
        );

        let unproven = TreeOutcome::unproven(InvocationResult::Infrastructure("unproven".to_owned()));
        assert!(!unproven.cleanup_proven);
        assert_eq!(
            infrastructure_message(BufferedOutcome {
                stdout: Vec::new(),
                stderr: Vec::new(),
                result: unproven.result,
            }),
            "unproven"
        );
    }

    #[test]
    fn process_waiting_classifies_observation_and_cleanup_failures() {
        let mut cleaned = FakeProcess {
            observations: VecDeque::from([Err(io::Error::other("observe failed"))]),
            termination: Some(Ok(successful_status())),
        };
        let outcome = wait_for_tree_with(&mut cleaned, Duration::from_secs(1), FakeProcess::observe, FakeProcess::terminate);
        assert!(outcome.cleanup_proven);
        assert!(result_infrastructure_message(outcome.result).contains("observe failed"));

        let mut uncleaned = FakeProcess {
            observations: VecDeque::from([Err(io::Error::other("observe failed"))]),
            termination: Some(Err(io::Error::other("cleanup failed"))),
        };
        let outcome = wait_for_tree_without_timeout_with(&mut uncleaned, FakeProcess::observe, FakeProcess::terminate);
        assert!(!outcome.cleanup_proven);
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
        assert!(!outcome.cleanup_proven);
        let message = result_infrastructure_message(outcome.result);
        assert!(message.contains("timed observe failed"));
        assert!(message.contains("timed cleanup failed"));

        let mut untimed_cleaned = FakeProcess {
            observations: VecDeque::from([Err(io::Error::other("untimed observe failed"))]),
            termination: Some(Ok(successful_status())),
        };
        let outcome = wait_for_tree_without_timeout_with(&mut untimed_cleaned, FakeProcess::observe, FakeProcess::terminate);
        assert!(outcome.cleanup_proven);
        assert!(result_infrastructure_message(outcome.result).contains("untimed observe failed"));
    }

    #[test]
    fn timed_process_waiting_covers_completion_and_failed_termination() {
        let mut completed = FakeProcess {
            observations: VecDeque::from([Ok(Some(successful_status()))]),
            termination: None,
        };
        let outcome = wait_for_tree_with(&mut completed, Duration::from_secs(1), FakeProcess::observe, FakeProcess::terminate);
        assert!(outcome.cleanup_proven);
        let InvocationResult::Exited(status) = outcome.result else {
            panic!("a completed process must retain its exit status");
        };
        assert!(status.success());

        let mut uncleaned = FakeProcess {
            observations: VecDeque::from([Ok(None)]),
            termination: Some(Err(io::Error::other("termination failed"))),
        };
        let outcome = wait_for_tree_with(&mut uncleaned, Duration::ZERO, FakeProcess::observe, FakeProcess::terminate);
        assert!(!outcome.cleanup_proven);
        assert!(result_infrastructure_message(outcome.result).contains("termination failed"));
    }

    #[test]
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
    fn captured_runner_reports_stream_setup_failures() {
        for (label, expected) in [
            ("__cargo_each_missing_stdout", "failed to capture child stdout"),
            ("__cargo_each_stdout_reader_failure", "injected stdout reader failure"),
            ("__cargo_each_missing_stderr", "failed to capture child stderr"),
            ("__cargo_each_stderr_reader_failure", "injected stderr reader failure"),
        ] {
            let outcome = run_captured(&labelled_invocation(label, &["rustc", "--version"]), None);
            assert!(infrastructure_message(outcome).contains(expected), "{label}");
        }
    }
}
