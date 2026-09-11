// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the `cargo each` command: resolve the selection,
//! apply filters, build the plan, and run it.

use std::collections::BTreeSet;
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
    let invocations = Arc::new(plan.invocations.clone());
    let worker_count = jobs.get().min(invocations.len()).min(cargo_gamma_process::capacity().max(1));
    let mut workers = Vec::with_capacity(worker_count);
    let mut next_index = 0;
    let mut stop_launching = false;
    let mut launch_error = None;

    while next_index < worker_count {
        match spawn_worker(next_index, Arc::clone(&invocations), timeout) {
            Ok(worker) => {
                workers.push(worker);
                next_index += 1;
            }
            Err(error) => {
                launch_error = Some(error);
                break;
            }
        }
    }

    let mut outcomes = Vec::with_capacity(invocations.len());
    while !workers.is_empty() {
        let outcome = wait_for_worker(&mut workers);
        if !keep_going && outcome.outcome.result.failed() {
            stop_launching = true;
        }
        outcomes.push(outcome);

        if !stop_launching && launch_error.is_none() && next_index < invocations.len() {
            match spawn_worker(next_index, Arc::clone(&invocations), timeout) {
                Ok(worker) => {
                    workers.push(worker);
                    next_index += 1;
                }
                Err(error) => launch_error = Some(error),
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

fn spawn_worker(index: usize, invocations: Arc<Vec<Invocation>>, timeout: Option<Duration>) -> io::Result<RunningWorker> {
    let (sender, receiver) = mpsc::channel();
    let thread = thread::Builder::new().name(format!("cargo-each-worker-{index}")).spawn(move || {
        complete_worker(&sender, move || {
            let Some(invocation) = invocations.get(index) else {
                return BufferedOutcome::infrastructure(format!(
                    "internal scheduler error: invocation index {index} is outside the command plan"
                ));
            };
            run_captured(invocation, timeout)
        });
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

fn wait_for_worker(workers: &mut Vec<RunningWorker>) -> IndexedOutcome {
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
        return IndexedOutcome { index, outcome };
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

    let Some(stdout) = tree.take_stdout() else {
        let cleanup = tree.terminate();
        return BufferedOutcome::infrastructure(with_cleanup_failure("failed to capture child stdout".to_owned(), &cleanup));
    };
    let stdout_reader = match spawn_output_reader(stdout, "cargo-each-stdout") {
        Ok(reader) => reader,
        Err(error) => {
            let cleanup = tree.terminate();
            return BufferedOutcome::infrastructure(with_cleanup_failure(format!("failed to create stdout reader: {error}"), &cleanup));
        }
    };
    let Some(stderr) = tree.take_stderr() else {
        let cleanup = tree.terminate();
        return BufferedOutcome::from_reader_failure("failed to capture child stderr".to_owned(), stdout_reader, &cleanup);
    };
    let stderr_reader = match spawn_output_reader(stderr, "cargo-each-stderr") {
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
    let result = tree_outcome.result;
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
    let started = Instant::now();
    loop {
        match tree.observe() {
            Ok(Some(status)) => return TreeOutcome::closed(InvocationResult::Exited(status)),
            Ok(None) => {}
            Err(error) => {
                let cleanup = tree.terminate();
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
        let elapsed = started.elapsed();
        if elapsed >= timeout {
            return match tree.terminate() {
                Ok(_) => TreeOutcome::closed(InvocationResult::TimedOut(timeout)),
                Err(error) => TreeOutcome::unproven(InvocationResult::Infrastructure(format!(
                    "invocation timed out after {}; process-tree termination failed: {error}",
                    display_duration(timeout)
                ))),
            };
        }
        let remaining = timeout.saturating_sub(elapsed);
        thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}

fn wait_for_tree_without_timeout(tree: &mut ProcessTree) -> TreeOutcome {
    loop {
        match tree.observe() {
            Ok(Some(status)) => return TreeOutcome::closed(InvocationResult::Exited(status)),
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                let cleanup = tree.terminate();
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
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                return Ok(());
            }
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

fn with_cleanup_failure(message: String, cleanup: &io::Result<ExitStatus>) -> String {
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

    fn from_reader_failure(message: String, stdout_reader: OutputReader, cleanup: &io::Result<ExitStatus>) -> Self {
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
    use std::num::NonZeroUsize;
    use std::process::ExitCode;
    use std::sync::{Arc, Condvar, Mutex, mpsc};
    use std::time::Duration;
    use std::{io, thread};

    use super::{
        BufferedOutcome, Invocation, InvocationResult, Plan, RunningWorker, WORKER_PANIC_TEST_PROGRAM, display_duration, execute_parallel,
        exit_byte, finish_output_reader, spawn_output_reader, wait_for_worker,
    };

    struct StubbornPipe {
        read: bool,
        blocked: Option<mpsc::Sender<()>>,
        finished: mpsc::Sender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
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
        }]);
        assert_eq!(outcome.index, 4);
        let InvocationResult::Infrastructure(message) = outcome.outcome.result else {
            panic!("a disconnected worker must produce an infrastructure outcome");
        };
        assert!(message.contains("without reporting"));
    }
}
