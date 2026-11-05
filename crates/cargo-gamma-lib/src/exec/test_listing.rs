// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bounded libtest enumeration for source-declared resource admission.

use core::time::Duration;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Instant;
use std::{io, thread};

use cargo_gamma_process::{MemoryRequest, ProcessTree, prepare};

#[cfg(test)]
use super::faults::{self, Fault};
use super::harness_filters::HarnessFilters;
use super::loader::configure_loader;
use super::resources::is_marker;
use super::test_binary::TestBinary;
use super::verdict::working_directory;
use super::workspace::Workspace;
use crate::Result;
use crate::error::{Error, error};

// Allow cold or loaded hosts while bounding executables that never finish listing.
const LIST_BUDGET: Duration = Duration::from_secs(30);
// Accept large short-line listings without buffering unbounded child output.
const LIST_CAP: usize = 4 * 1024 * 1024;
// Avoid spinning while waiting for an uncooperative child.
const LIST_POLL: Duration = Duration::from_millis(5);
// Give readers time to publish pipes closed by termination, without waiting on escaped children.
const LIST_DRAIN_BUDGET: Duration = Duration::from_millis(100);

/// Lists user-selected cases without resource markers, accepting an empty successful listing.
#[cfg_attr(coverage_nightly, coverage(off))]
pub(super) fn list_selected_allow_empty(work: &Workspace, binary: &TestBinary, request: MemoryRequest) -> Result<Vec<Box<str>>> {
    let command = listing_command(work, binary, true, false);
    list_in_workspace(work, command, request).map(|mut names| {
        names.retain(|name| !is_marker(name));
        names
    })
}

/// Lists ignored resource marker tests without applying user filters.
pub(super) fn list_resource_markers(work: &Workspace, binary: &TestBinary, request: MemoryRequest) -> Result<Vec<Box<str>>> {
    #[cfg(test)]
    RESOURCE_LISTING_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));

    let command = listing_command(work, binary, false, true);
    list_in_workspace(work, command, request).map(|mut names| {
        names.retain(|name| is_marker(name));
        names
    })
}

fn list_in_workspace(work: &Workspace, command: Command, request: MemoryRequest) -> Result<Vec<Box<str>>> {
    listed_with_request(command, LIST_BUDGET, request).map_err(|mut failure| {
        work.settle();
        failure.append_message(&format!(
            "\n\nThe workspace at `{}` and build artifacts at `{}` have been retained for diagnosis. \
             Retry before starting another campaign, which may replace them. Use the same configured test environment.",
            work.root, work.target
        ));
        failure
    })
}

#[cfg(test)]
thread_local! {
    static RESOURCE_LISTING_CALLS: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

#[cfg(test)]
pub(super) fn reset_resource_listing_calls() {
    RESOURCE_LISTING_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
pub(super) fn resource_listing_calls() -> usize {
    RESOURCE_LISTING_CALLS.with(core::cell::Cell::get)
}

fn listing_command(work: &Workspace, binary: &TestBinary, include_filters: bool, ignored_only: bool) -> Command {
    let mut command = Command::new(binary.path.as_std_path());
    let _ = command.args(["--list", "--format=terse"]);
    if ignored_only {
        // Only ignored tests can authenticate the resource markers emitted by the macro.
        let _ = command.arg("--ignored");
    }
    if include_filters {
        let _ = command.args(HarnessFilters::parse(work.test_arguments()).selecting());
    }
    let _ = command.current_dir(working_directory(work, binary).as_std_path());
    configure_loader(&mut command, work.launch());
    work.configure_test_environment(&mut command, binary);
    work.inherit_test_cache_home(&mut command);
    command
}

fn read_listing(pipe: &mut impl io::Read) -> ListingOutput {
    let mut text = Vec::new();
    let mut buffer = [
    // #[gamma::skip(literal.int_increment, reason = "The initialized read buffer's unused bytes are never read: only buffer[..successful_read_length] contributes to output/search. Successful Read/read_exact overwrites those bytes before use; initial byte0 versus1 is unobservable.", tag = "equivalent")]
    0_u8; 8192];
    let mut problem = None;
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                // Drain past the cap so the child cannot block on a full pipe.
                let room = LIST_CAP.saturating_sub(text.len());
                text.extend_from_slice(&buffer[..read.min(room)]);
                if read > room {
                    problem.get_or_insert_with(|| format!("output exceeded the {LIST_CAP}-byte capture limit"));
                }
            }
            Err(cause) if cause.kind() == io::ErrorKind::Interrupted => {}
            Err(cause) => {
                problem = Some(format!("could not read output: {cause}"));
                break;
            }
        }
    }
    (text, problem)
}

#[cfg(test)]
type ScriptedListing = Box<dyn FnMut(&Command, Duration, MemoryRequest) -> Option<Vec<Box<str>>>>;

#[cfg(test)]
thread_local! {
    static SCRIPTED_LISTING: std::cell::RefCell<Option<ScriptedListing>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn with_listing_runner<T>(
    runner: impl FnMut(&Command, Duration, MemoryRequest) -> Option<Vec<Box<str>>> + 'static,
    body: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            SCRIPTED_LISTING.with_borrow_mut(|runner| {
                let _ = runner.take();
            });
        }
    }
    SCRIPTED_LISTING.with_borrow_mut(|slot| {
        assert!(slot.is_none(), "a thread cannot install two listing runners simultaneously");
        *slot = Some(Box::new(runner));
    });
    let _reset = Reset;
    body()
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn listed_with_request(command: Command, budget: Duration, request: MemoryRequest) -> Result<Vec<Box<str>>> {
    let quote = |value: &std::ffi::OsStr| {
        let value = value.to_string_lossy();
        #[cfg(windows)]
        let escaped = value.replace('\'', "''");
        #[cfg(not(windows))]
        let escaped = value.replace('\'', "'\"'\"'");
        format!("'{escaped}'")
    };
    let arguments = std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(quote)
        .collect::<Vec<_>>()
        .join(" ");
    #[cfg(windows)]
    let arguments = format!("& {arguments}");
    let retry = command.get_current_dir().map_or_else(
        || arguments.clone(),
        |directory| {
            let directory = quote(directory.as_os_str());
            #[cfg(windows)]
            {
                format!("Set-Location -LiteralPath {directory}; {arguments}")
            }
            #[cfg(not(windows))]
            {
                format!("cd -- {directory} && {arguments}")
            }
        },
    );
    let result = run_listing(command, budget, request);
    result.map_err(|mut failure| {
        failure.append_message(&format!("\n\nRetry command:\n{retry}"));
        if let Some(limit) = request.limit {
            failure.append_message(&format!("\nEnumeration memory limit: {limit} bytes."));
        }
        failure
    })
}

fn listing_error(reason: &str, stdout: &[u8], stderr: &[u8]) -> Error {
    error!(
        "test enumeration failed: {reason}\n\n{}",
        super::process_output::failure_output("Test", &String::from_utf8_lossy(stdout), &String::from_utf8_lossy(stderr))
    )
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn run_listing(mut command: Command, budget: Duration, request: MemoryRequest) -> Result<Vec<Box<str>>> {
    #[cfg(test)]
    if let Some(listing) = SCRIPTED_LISTING.with_borrow_mut(|runner| runner.as_mut().map(|runner| runner(&command, budget, request))) {
        return listing.ok_or_else(|| listing_error("the scripted listing failed", &[], &[]));
    }
    let _ = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let prepared =
        prepare(command, request).map_err(|cause| listing_error(&format!("could not prepare process containment: {cause}"), &[], &[]))?;
    let spawned = prepared.spawn().map_err(|failure| {
        let (cause, _) = failure.into_parts();
        listing_error(&format!("could not start the test executable: {cause}"), &[], &[])
    })?;
    let mut subtree =
        ProcessTree::adopt(spawned).map_err(|cause| listing_error(&format!("could not adopt the test process: {cause}"), &[], &[]))?;
    let (stdout, stderr) = listing_readers(subtree.take_stdout(), subtree.take_stderr());
    finish_listing(&mut subtree, &stdout, &stderr, budget, Instant::now, thread::sleep)
}

fn listing_readers(
    stdout: Option<impl io::Read + Send + 'static>,
    stderr: Option<impl io::Read + Send + 'static>,
) -> (mpsc::Receiver<ListingOutput>, mpsc::Receiver<ListingOutput>) {
    (listing_reader(stdout, "stdout"), listing_reader(stderr, "stderr"))
}

fn listing_reader(pipe: Option<impl io::Read + Send + 'static>, stream: &'static str) -> mpsc::Receiver<ListingOutput> {
    let (sender, receiver) = listing_output_channel();
    if let Some(mut pipe) = pipe {
        let sender = sender.clone();
        let failed = sender.clone();
        #[cfg(test)]
        let refused = faults::fired(Fault::Thread);
        #[cfg(not(test))]
        let refused = false;
        let spawned = if refused {
            Err(io::Error::other("the reader thread a test asked to fail"))
        } else {
            thread::Builder::new().name(format!("cargo-gamma-listing-{stream}")).spawn(move || {
                let output = read_listing(&mut pipe);
                let _sent = sender.send(output);
            })
        };
        if let Err(cause) = spawned {
            failed
                .try_send((Vec::new(), Some(format!("could not start the {stream} reader: {cause}"))))
                .expect("a refused reader has not published into the single reserved listing result slot");
        }
    } else {
        sender
            .try_send((Vec::new(), Some(format!("the {stream} pipe was unavailable"))))
            .expect("an unavailable pipe has no reader that could publish a result");
    }
    drop(sender);
    receiver
}

type ListingOutput = (Vec<u8>, Option<String>);

fn listing_output_channel() -> (mpsc::SyncSender<ListingOutput>, mpsc::Receiver<ListingOutput>) {
    mpsc::sync_channel(1)
}

trait ListingChild {
    fn observe_exit(&mut self) -> io::Result<Option<std::process::ExitStatus>>;
    fn terminate_tree(&mut self) -> io::Result<()>;
    fn leader_released(&self) -> bool;
}

impl ListingChild for ProcessTree {
    fn observe_exit(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        Self::observe(self)
    }

    // #[gamma::skip(fn_value.ok, reason = "This thin adapter invokes native ProcessTree termination. Scripted ListingChild tests cover termination requests and failures, but cannot exercise this actual adapter without spawning a real child.", tag = "native-boundary")]
    fn terminate_tree(&mut self) -> io::Result<()> {
        Self::terminate(self).map(|_status| ())
    }

    // #[gamma::skip(fn_value.bool_true, reason = "This thin ProcessTree adapter reads native containment/reaping state. An owned fake bypasses this exact native adapter; deterministic tests cannot launch a child or mutate native process state.", tag = "native-boundary")]
    fn leader_released(&self) -> bool {
        Self::released(self)
    }
}

/// Supervises exit and cleanup with injected polling time; output reception uses a native channel timeout.
fn finish_listing(
    subtree: &mut impl ListingChild,
    stdout: &mpsc::Receiver<ListingOutput>,
    stderr: &mpsc::Receiver<ListingOutput>,
    budget: Duration,
    clock: impl FnMut() -> Instant,
    sleep: impl FnMut(Duration),
) -> Result<Vec<Box<str>>> {
    finish_listing_with(subtree, stdout, stderr, budget, clock, sleep, |receiver, remaining| {
        receiver.recv_timeout(remaining)
    })
}

fn finish_listing_with(
    subtree: &mut impl ListingChild,
    stdout: &mpsc::Receiver<ListingOutput>,
    stderr: &mpsc::Receiver<ListingOutput>,
    budget: Duration,
    mut clock: impl FnMut() -> Instant,
    mut sleep: impl FnMut(Duration),
    mut receive_output: impl FnMut(&mpsc::Receiver<ListingOutput>, Duration) -> std::result::Result<ListingOutput, mpsc::RecvTimeoutError>,
) -> Result<Vec<Box<str>>> {
    let deadline = clock() + budget;
    let mut problems = Vec::new();
    let status = loop {
        match subtree.observe_exit() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if clock() >= deadline => {
                problems.push(format!("the test process exceeded its {budget:.0?} enumeration timeout"));
                // #[gamma::skip(loop.break_value_default, tag = "equivalent", reason = "the inferred Option<ExitStatus> default is already None")]
                break None;
            }
            Ok(None) => sleep(LIST_POLL.min(deadline.saturating_duration_since(clock()))),
            Err(cause) => {
                problems.push(format!("could not observe the test process: {cause}"));
                // #[gamma::skip(loop.break_value_default, tag = "equivalent", reason = "the inferred Option<ExitStatus> default is already None")]
                break None;
            }
        }
    };
    if status.is_none() {
        if let Err(cause) = subtree.terminate_tree() {
            problems.push(format!("could not terminate the test process: {cause}"));
        }
    } else {
        debug_assert!(
            subtree.leader_released(),
            "the observed listing released containment before its output was read"
        );
    }
    let deadline = if status.is_none() { clock() + LIST_DRAIN_BUDGET } else { deadline };
    let mut receive = |receiver: &mpsc::Receiver<ListingOutput>, stream: &str| match receive_output(
        receiver,
        deadline.saturating_duration_since(clock()),
    ) {
        Ok((text, problem)) => {
            if let Some(problem) = problem {
                problems.push(format!("{stream}: {problem}"));
            }
            text
        }
        Err(cause) => {
            problems.push(format!("{stream}: output collection did not complete: {cause}"));
            Vec::new()
        }
    };
    let text = receive(stdout, "stdout");
    let stderr = receive(stderr, "stderr");
    if let Some(status) = status
        && !status.success()
    {
        let detail = format!("the test process exited with {status}");
        #[cfg(windows)]
        let detail = if let Some(code) = status.code() {
            let code = u32::from_ne_bytes(code.to_ne_bytes());
            format!("{detail} (0x{code:08X})")
        } else {
            detail
        };
        problems.insert(0, detail);
    }
    if let Err(cause) = std::str::from_utf8(&text) {
        problems.push(format!("stdout is not valid UTF-8: {cause}"));
    }
    if !problems.is_empty() {
        return Err(listing_error(&problems.join("; "), &text, &stderr));
    }
    let text = std::str::from_utf8(&text).expect("stdout UTF-8 was validated above");
    Ok(text
        .lines()
        .filter_map(|line| line.strip_suffix(": test"))
        .map(|name| name.trim().into())
        .collect())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #[cfg(unix)]
    use std::fs;
    use std::io::Read;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt as _;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt as _;

    use camino::Utf8Path;
    #[cfg(unix)]
    use camino::Utf8PathBuf;
    use cargo_gamma_process::faults::{self as process_faults, Fault as ProcessFault};

    use super::*;
    use crate::exec::loader::LOADER_VAR;

    #[test]
    fn failed_listings_keep_exit_status_stderr_and_partial_stdout() {
        #[cfg(windows)]
        let status = std::process::ExitStatus::from_raw(0xC000_0135);
        #[cfg(unix)]
        let status = std::process::ExitStatus::from_raw(7 << 8);
        let mut process = MemoryListingProcess {
            observations: std::collections::VecDeque::from([Ok(Some(status))]),
            terminated: 0,
        };
        let (out, stdout) = listing_output_channel();
        let (err, stderr) = listing_output_channel();
        out.send((b"partial::case: test\n".to_vec(), None)).unwrap();
        err.send((b"required library could not be loaded\n".to_vec(), None)).unwrap();
        let now = Instant::now();
        let failure = finish_listing(&mut process, &stdout, &stderr, LIST_BUDGET, || now, |_| panic!("already exited"))
            .unwrap_err()
            .to_string();
        assert!(failure.contains(&status.to_string()), "{failure}");
        #[cfg(windows)]
        assert!(failure.contains("0xC0000135"), "{failure}");
        assert!(failure.contains("Test Stderr\nrequired library could not be loaded"), "{failure}");
        assert!(failure.contains("Test Stdout\npartial::case: test"), "{failure}");
        assert_eq!(process.terminated, 0);
    }

    #[test]
    fn listing_capture_failures_are_distinguished_and_never_accepted_as_empty_success() {
        for (bytes, problem, publish_stderr, expected) in [
            (vec![0xff], None, true, "not valid UTF-8"),
            (
                b"partial".to_vec(),
                Some("capture limit exceeded".to_owned()),
                true,
                "stdout: capture limit exceeded",
            ),
            (
                Vec::new(),
                Some("owned read failure".to_owned()),
                true,
                "stdout: owned read failure",
            ),
            (Vec::new(), None, false, "stderr: output collection did not complete"),
        ] {
            let mut process = MemoryListingProcess {
                observations: std::collections::VecDeque::from([Ok(Some(recovered_listing_status()))]),
                terminated: 0,
            };
            let (out, stdout) = listing_output_channel();
            let (err, stderr) = listing_output_channel();
            out.send((bytes, problem)).unwrap();
            if publish_stderr {
                err.send((Vec::new(), None)).unwrap();
            }
            drop(err);
            let now = Instant::now();
            let failure = finish_listing(&mut process, &stdout, &stderr, LIST_BUDGET, || now, |_| panic!("already exited"))
                .unwrap_err()
                .to_string();
            assert!(failure.contains(expected), "{failure}");
        }
    }

    #[test]
    fn listing_retry_commands_preserve_arguments_without_printing_environment_secrets() {
        for limit in [None, Some(4096)] {
            let failure = with_listing_runner(
                move |_, _, request| {
                    assert_eq!(request.limit, limit);
                    None
                },
                || {
                    let mut command = Command::new("test bin's.exe");
                    command.current_dir("package dir").args(["--list", "--format=terse", "--ignored"]);
                    command.env("PRIVATE_TOKEN", "do-not-print");
                    listed_with_request(
                        command,
                        LIST_BUDGET,
                        MemoryRequest {
                            limit,
                            ..MemoryRequest::default()
                        },
                    )
                    .unwrap_err()
                },
            )
            .to_string();
            #[cfg(windows)]
            let retry = "Set-Location -LiteralPath 'package dir'; & 'test bin''s.exe' '--list' '--format=terse' '--ignored'";
            #[cfg(unix)]
            let retry = "cd -- 'package dir' && 'test bin'\"'\"'s.exe' '--list' '--format=terse' '--ignored'";
            let memory = limit.map_or_else(String::new, |limit| format!("\nEnumeration memory limit: {limit} bytes."));
            assert_eq!(
                failure,
                format!(
                    "test enumeration failed: the scripted listing failed\n\nTest Stderr\n(no output)\n\nRetry command:\n{retry}{memory}"
                )
            );
        }
    }

    #[test]
    fn unavailable_listing_pipes_publish_the_exact_stream_failure_without_starting_a_reader() {
        let (stdout, stderr) = listing_readers(None::<io::Cursor<Vec<u8>>>, None::<io::Cursor<Vec<u8>>>);
        for (stream, receiver) in [("stdout", stdout), ("stderr", stderr)] {
            assert_eq!(
                receiver.try_recv().unwrap(),
                (Vec::new(), Some(format!("the {stream} pipe was unavailable")))
            );
            assert_eq!(receiver.try_recv(), Err(mpsc::TryRecvError::Disconnected));
        }
    }

    #[test]
    fn simultaneous_capture_failures_keep_both_stream_labels_and_the_separator() {
        let mut process = MemoryListingProcess {
            observations: std::collections::VecDeque::from([Ok(Some(recovered_listing_status()))]),
            terminated: 0,
        };
        let (out, stdout) = listing_output_channel();
        let (err, stderr) = listing_output_channel();
        out.send((b"partial: test\n".to_vec(), Some("stdout refused".into()))).unwrap();
        err.send((b"partial error\n".to_vec(), Some("stderr refused".into()))).unwrap();
        let now = Instant::now();
        let failure = finish_listing(&mut process, &stdout, &stderr, LIST_BUDGET, || now, |_| panic!("already exited")).unwrap_err();
        assert_eq!(
            failure.message(),
            "test enumeration failed: stdout: stdout refused; stderr: stderr refused\n\nTest Stderr\npartial error\n\nTest Stdout\npartial: test"
        );
        assert_eq!(process.terminated, 0);
        assert!(process.observations.is_empty());
    }

    #[test]
    fn listing_reader_reports_thread_creation_failure_without_a_process() {
        let _refused = faults::arm(Fault::Thread);
        let output = listing_reader(Some(io::Cursor::new(Vec::new())), "stderr")
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert_eq!(output.0, Vec::<u8>::new());
        assert_eq!(
            output.1.unwrap(),
            "could not start the stderr reader: the reader thread a test asked to fail"
        );
    }

    #[test]
    fn containment_preparation_failures_keep_the_action_and_retry_command() {
        let _refused = process_faults::arm(ProcessFault::Prepare);
        let failure = listed_with_request(Command::new("not-launched"), LIST_BUDGET, MemoryRequest::default())
            .unwrap_err()
            .to_string();
        assert!(failure.contains("could not prepare process containment"), "{failure}");
        assert!(failure.contains("Test Stderr\n(no output)"), "{failure}");
        assert!(failure.contains("Retry command:\n"), "{failure}");
        assert!(failure.contains("'not-launched'"), "{failure}");
    }

    #[test]
    fn oct25_listing_native_cleanup_adapter_terminates_after_a_failed_exit_observation() {
        #[derive(Default)]
        struct Child {
            observations: usize,
            terminations: usize,
        }
        impl ListingChild for Child {
            fn observe_exit(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
                self.observations += 1;
                assert_eq!(self.observations, 1);
                Err(io::Error::other("owned observation refusal"))
            }

            fn terminate_tree(&mut self) -> io::Result<()> {
                self.terminations += 1;
                assert_eq!(self.terminations, 1);
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "owned termination refusal"))
            }

            fn leader_released(&self) -> bool {
                panic!("an unavailable exit observation must stop before release inspection")
            }
        }
        let mut child = Child::default();
        let (sender, receiver) = mpsc::sync_channel(1);
        drop(sender);
        let (_, stderr) = listing_output_channel();
        let anchor = Instant::now();
        let failure = finish_listing(
            &mut child,
            &receiver,
            &stderr,
            Duration::from_secs(1),
            || anchor,
            |_| panic!("a failed observation must not wait"),
        )
        .unwrap_err()
        .to_string();
        assert!(failure.contains("owned observation refusal"), "{failure}");
        assert!(failure.contains("owned termination refusal"), "{failure}");
        assert_eq!(child.observations, 1);
        assert_eq!(child.terminations, 1);
    }

    #[test]
    fn oct25_listing_output_channel_buffers_exactly_one_reader_result_before_supervision_begins() {
        let (sender, receiver) = listing_output_channel();
        sender
            .try_send((b"reader creation failed".to_vec(), Some("reader refused".into())))
            .unwrap();
        assert_eq!(
            sender.try_send((b"second result".to_vec(), None)),
            Err(mpsc::TrySendError::Full((b"second result".to_vec(), None)))
        );
        assert_eq!(
            receiver.try_recv().unwrap(),
            (b"reader creation failed".to_vec(), Some("reader refused".into()))
        );
        sender.try_send((b"complete output".to_vec(), None)).unwrap();
        assert_eq!(receiver.try_recv().unwrap(), (b"complete output".to_vec(), None));
    }

    struct MemoryListingProcess {
        observations: std::collections::VecDeque<io::Result<Option<std::process::ExitStatus>>>,
        terminated: usize,
    }

    impl ListingChild for MemoryListingProcess {
        fn observe_exit(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
            self.observations
                .pop_front()
                .expect("the scripted process supplies exactly the observations the supervisor may make")
        }

        fn terminate_tree(&mut self) -> io::Result<()> {
            self.terminated += 1;
            Ok(())
        }

        fn leader_released(&self) -> bool {
            true
        }
    }

    fn recovered_listing_status() -> std::process::ExitStatus {
        std::process::ExitStatus::from_raw(0)
    }

    #[test]
    fn recovered_listing_supervision_reports_the_exact_deadline_and_observation_errors() {
        for observe_error in [false, true] {
            let mut process = MemoryListingProcess {
                observations: std::collections::VecDeque::from([
                    if observe_error {
                        Err(io::Error::other("in-memory observation refusal"))
                    } else {
                        Ok(None)
                    },
                    Ok(Some(recovered_listing_status())),
                ]),
                terminated: 0,
            };
            let now = Instant::now();
            let mut clocks =
                std::collections::VecDeque::from([now, now + LIST_BUDGET, now + LIST_BUDGET, now + LIST_BUDGET, now + LIST_BUDGET]);
            let (sender, receiver) = mpsc::sync_channel(1);
            sender.send((b"not-a-real-process: test\n".to_vec(), None)).unwrap();
            let (_, stderr) = listing_output_channel();
            let failure = finish_listing(
                &mut process,
                &receiver,
                &stderr,
                LIST_BUDGET,
                || clocks.pop_front().expect("the deadline test permits no extra clock query"),
                |_| panic!("an elapsed deadline or refused observation must never sleep"),
            )
            .unwrap_err()
            .to_string();
            assert!(
                failure.contains(if observe_error {
                    "in-memory observation refusal"
                } else {
                    "30s enumeration timeout"
                }),
                "{failure}"
            );
            assert!(failure.contains("Test Stdout\nnot-a-real-process: test"), "{failure}");
            assert_eq!(process.terminated, 1);
            assert_eq!(process.observations.len(), 1);
        }
    }

    #[test]
    fn terminated_listings_get_a_fresh_bounded_drain_deadline_after_the_execution_deadline() {
        let mut process = MemoryListingProcess {
            observations: std::collections::VecDeque::from([Ok(None)]),
            terminated: 0,
        };
        let now = Instant::now();
        let mut clocks = std::collections::VecDeque::from([
            now,
            now + Duration::from_secs(1),
            now + Duration::from_secs(10),
            now + Duration::from_secs(10),
            now + Duration::from_millis(10_010),
        ]);
        let (_out, stdout) = listing_output_channel();
        let (_err, stderr) = listing_output_channel();
        let mut received = 0;
        let failure = finish_listing_with(
            &mut process,
            &stdout,
            &stderr,
            Duration::from_secs(1),
            || clocks.pop_front().expect("every clock query is scripted"),
            |_| panic!("the execution deadline already elapsed"),
            |receiver, remaining| {
                received += 1;
                match received {
                    1 => {
                        assert!(std::ptr::eq(receiver, &raw const stdout));
                        assert_eq!(remaining, Duration::from_millis(100));
                        Ok((b"partial: test\n".to_vec(), None))
                    }
                    2 => {
                        assert!(std::ptr::eq(receiver, &raw const stderr));
                        assert_eq!(remaining, Duration::from_millis(90));
                        Ok((Vec::new(), None))
                    }
                    _ => panic!("each stream must be collected exactly once"),
                }
            },
        )
        .unwrap_err();
        assert_eq!(
            failure.message(),
            "test enumeration failed: the test process exceeded its 1s enumeration timeout\n\nTest Stderr\n(no output)\n\nTest Stdout\npartial: test"
        );
        assert_eq!(received, 2);
        assert!(clocks.is_empty());
        assert_eq!(process.terminated, 1);
        assert!(process.observations.is_empty());
    }

    #[test]
    fn recovered_listing_supervision_caps_poll_intervals_and_does_not_terminate_a_completed_process() {
        let mut process = MemoryListingProcess {
            observations: std::collections::VecDeque::from([Ok(None), Ok(None), Ok(Some(recovered_listing_status()))]),
            terminated: 0,
        };
        let now = Instant::now();
        let remaining = LIST_POLL.checked_div(2).unwrap();
        let near_deadline = now + LIST_BUDGET.checked_sub(remaining).unwrap();
        let mut clocks = std::collections::VecDeque::from([now, now, now, near_deadline, near_deadline, near_deadline, near_deadline]);
        let mut sleeps = Vec::new();
        let (sender, receiver) = mpsc::sync_channel(1);
        sender
            .send((b"first: test\nignored: benchmark\n second: test\n".to_vec(), None))
            .unwrap();
        let (stderr_sender, stderr) = listing_output_channel();
        stderr_sender.send((Vec::new(), None)).unwrap();
        let listing = finish_listing(
            &mut process,
            &receiver,
            &stderr,
            LIST_BUDGET,
            || {
                clocks
                    .pop_front()
                    .expect("the bounded listing script supplies every expected clock query")
            },
            |duration| sleeps.push(duration),
        )
        .unwrap();
        assert_eq!(listing, [Box::<str>::from("first"), Box::<str>::from("second")]);
        assert_eq!(sleeps, [LIST_POLL, remaining]);
        assert_eq!(process.terminated, 0);
        assert!(process.observations.is_empty());
        assert!(clocks.is_empty());
    }

    #[test]
    fn recovered_selected_and_resource_listings_keep_disjoint_names_and_flags() {
        let root = camino::Utf8PathBuf::from("synthetic-listing-root");
        let mut work = Workspace::adopt(root.clone(), root.join("target"));
        work.set_test_args(vec!["suite::selected".into()]);
        let binary = crate::testing::test_binary("never-launched");
        let marker = "__cargo_gamma_resource_696f_binary";
        for resource in [false, true] {
            with_listing_runner(
                move |command, budget, _request| {
                    let args: Vec<_> = command.get_args().map(|arg| arg.to_str().unwrap()).collect();
                    assert_eq!(budget, LIST_BUDGET);
                    assert_eq!(
                        args,
                        if resource {
                            vec!["--list", "--format=terse", "--ignored"]
                        } else {
                            vec!["--list", "--format=terse", "suite::selected"]
                        }
                    );
                    Some(vec!["suite::selected".into(), marker.into(), "suite::another".into()])
                },
                || {
                    let actual = if resource {
                        list_resource_markers(&work, &binary, MemoryRequest::default())
                    } else {
                        list_selected_allow_empty(&work, &binary, MemoryRequest::default())
                    };
                    assert_eq!(
                        actual.unwrap(),
                        if resource {
                            vec![marker.into()]
                        } else {
                            vec!["suite::selected".into(), "suite::another".into()]
                        }
                    );
                },
            );
        }
    }

    #[test]
    fn oct25_listing_reader_stops_at_the_first_noninterrupt_error() {
        struct FiniteError {
            reads: usize,
        }
        impl Read for FiniteError {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.reads += 1;
                match self.reads {
                    1 => {
                        buf[..6].copy_from_slice(b"prefix");
                        Ok(6)
                    }
                    2 => Err(io::ErrorKind::BrokenPipe.into()),
                    3 => {
                        buf[..4].copy_from_slice(b"tail");
                        Ok(4)
                    }
                    _ => Ok(0),
                }
            }
        }
        let mut reader = FiniteError { reads: 0 };
        let (text, problem) = read_listing(&mut reader);
        assert_eq!(text, b"prefix");
        assert!(problem.unwrap().contains("could not read output"));
        assert_eq!(reader.reads, 2);
    }

    fn listed(command: Command, budget: Duration) -> Option<Vec<Box<str>>> {
        listed_with_request(command, budget, MemoryRequest::default()).ok()
    }

    struct InterruptedThenData {
        interrupted: bool,
        data: io::Cursor<Vec<u8>>,
    }

    impl Read for InterruptedThenData {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(io::ErrorKind::Interrupted.into());
            }
            self.data.read(buf)
        }
    }

    struct DataThenError {
        data: Option<Vec<u8>>,
        errored: bool,
    }

    impl Read for DataThenError {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            assert!(!self.errored, "a terminal listing error must not be retried");
            let Some(data) = self.data.take() else {
                self.errored = true;
                return Err(io::ErrorKind::BrokenPipe.into());
            };
            buf[..data.len()].copy_from_slice(&data);
            Ok(data.len())
        }
    }

    #[test]
    fn listing_commands_apply_the_selected_loader_and_package_environment_without_launching() {
        use super::super::loader::{LOADER_VAR, Launch};

        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8Path::from_path(directory.path()).unwrap().to_owned();
        let mut work = Workspace::adopt(root.clone(), root.join("target"));
        let binary = crate::testing::test_binary("not-executed");
        work.set_launch(Launch {
            loader: Some("selected-loader".into()),
            stack: "7654321".into(),
        });
        work.set_test_environment(&binary.package_id, "PACKAGE_VALUE", "selected-package");
        let command = listing_command(&work, &binary, true, false);
        let variables = command.get_envs().collect::<std::collections::BTreeMap<_, _>>();
        for (name, expected) in [(LOADER_VAR, "selected-loader"), ("PACKAGE_VALUE", "selected-package")] {
            assert_eq!(
                variables.get(std::ffi::OsStr::new(name)).copied().flatten(),
                Some(std::ffi::OsStr::new(expected))
            );
        }
        assert!(!variables.contains_key(std::ffi::OsStr::new("RUST_MIN_STACK")));
        assert_eq!(command.get_current_dir(), Some(work.root.as_std_path()));
    }

    #[test]
    #[cfg_attr(miri, ignore = "requires native subprocess execution")]
    fn malformed_utf8_listing_is_not_an_empty_success() {
        use std::io::Write as _;

        const CHILD: &str = "GAMMA_EXEC_INVALID_UTF8_LISTING_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let mut stdout = io::stdout().lock();
            stdout.write_all(&[0xff]).unwrap();
            stdout.flush().unwrap();
            std::process::exit(0);
        }
        let mut command = Command::new(std::env::current_exe().unwrap());
        let _ = command
            .args([
                "--exact",
                "exec::test_listing::tests::malformed_utf8_listing_is_not_an_empty_success",
                "--nocapture",
            ])
            .env(CHILD, "1");
        assert_eq!(
            listed_with_request(command, Duration::from_secs(10), MemoryRequest::default()).ok(),
            None
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "requires native subprocess execution and containment")]
    fn failed_listing_processes_and_uncontainable_launches_never_publish_empty_listings() {
        for exit in [0, 1] {
            let script = if exit == 0 { "exit:0" } else { "exit:1" };
            let (_directory, work) = crate::testing::helper_workspace("listing-terminal", &["print:suite::selected: test", script]);
            let binary = crate::testing::helper();
            let command = listing_command(&work, &binary, true, false);
            let names = listed_with_request(command, Duration::from_secs(10), MemoryRequest::default()).ok();
            assert_eq!(names, (exit == 0).then(|| vec!["suite::selected".into()]));
        }
        let (_directory, work) = crate::testing::helper_workspace("listing-containment", &["print:suite::selected: test", "exit:0"]);
        let command = listing_command(&work, &crate::testing::helper(), true, false);
        let _refused = process_faults::arm(ProcessFault::Prepare);
        assert_eq!(
            listed_with_request(command, Duration::from_secs(10), MemoryRequest::default()).ok(),
            None
        );
    }

    #[test]
    fn listing_reader_retries_interrupts_and_rejects_terminal_errors() {
        let mut interrupted = InterruptedThenData {
            interrupted: false,
            data: std::io::Cursor::new(b"whole".to_vec()),
        };
        assert_eq!(read_listing(&mut interrupted), (b"whole".to_vec(), None));

        let mut truncated = DataThenError {
            errored: false,
            data: Some(b"prefix".to_vec()),
        };
        let (text, problem) = read_listing(&mut truncated);
        assert_eq!(text, b"prefix");
        assert!(problem.unwrap().contains("could not read output"));

        let mut exact = std::io::Cursor::new(vec![b'x'; LIST_CAP]);
        assert!(read_listing(&mut exact).1.is_none());
        let mut excessive = std::io::Cursor::new(vec![b'x'; LIST_CAP + 1]);
        assert!(read_listing(&mut excessive).1.unwrap().contains("capture limit"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "requires native subprocess execution")]
    fn listing_output_beyond_the_cap_is_not_authoritative() {
        let (_directory, work) =
            crate::testing::helper_workspace("listing-list-cap", &["flood:4194305", "print:meaningful::killer: test", "exit:0"]);
        let command = listing_command(&work, &crate::testing::helper(), true, false);

        assert!(listed(command, Duration::from_secs(30)).is_none());
    }

    #[test]
    #[cfg_attr(miri, ignore = "requires native subprocess execution")]
    fn listing_output_exactly_at_the_cap_is_still_a_whole_stream() {
        let (_directory, work) = crate::testing::helper_workspace("listing-list-cap-exact", &["flood:4194304", "exit:0"]);
        let command = listing_command(&work, &crate::testing::helper(), true, false);

        assert_eq!(
            listed(command, Duration::from_secs(30)),
            Some(Vec::new()),
            "the byte cap is inclusive; only the first byte beyond it makes a listing partial"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "requires native subprocess execution")]
    fn listing_reader_thread_failure_degrades_to_no_listing() {
        let (_directory, work) = crate::testing::helper_workspace("listing-list-thread", &["print:a::test: test", "exit:0"]);
        let command = listing_command(&work, &crate::testing::helper(), true, false);
        let _refused = faults::arm(Fault::Thread);

        assert!(listed(command, Duration::from_secs(30)).is_none());
    }

    #[test]
    #[cfg_attr(miri, ignore = "requires native subprocess execution and containment")]
    fn a_listing_child_that_cannot_be_adopted_degrades_to_no_listing() {
        let (_directory, work) = crate::testing::helper_workspace("listing-list-adopt", &["print:a::test: test", "exit:0"]);
        let command = listing_command(&work, &crate::testing::helper(), true, false);
        let _refused = process_faults::arm(ProcessFault::Adopt);

        assert!(listed(command, Duration::from_secs(30)).is_none());
    }

    #[test]
    fn listing_a_binary_uses_the_runs_loader_path() {
        let (_scratch, work) = crate::testing::helper_workspace("list-loader", &["exit:0"]);
        let mut binary = crate::testing::helper();
        binary.manifest_dir = work.root().join("crate");
        let command = listing_command(&work, &binary, true, false);
        let arguments: Vec<_> = command.get_args().collect();
        let configured = command
            .get_envs()
            .find(|(name, _value)| *name == std::ffi::OsStr::new(LOADER_VAR))
            .and_then(|(_name, value)| value);
        let cache_home = command
            .get_envs()
            .find(|(name, _value)| *name == std::ffi::OsStr::new(crate::testing::CACHE_HOME_VAR))
            .and_then(|(_name, value)| value);
        let expected_cache_home = crate::testing::cache_home(work.root());

        assert_eq!(
            arguments.get(..2),
            Some([std::ffi::OsStr::new("--list"), std::ffi::OsStr::new("--format=terse")].as_slice())
        );
        assert_eq!(command.get_current_dir(), Some(binary.manifest_dir.as_std_path()));
        assert_eq!(
            configured,
            work.launch().loader.as_deref(),
            "the listing launch omitted the libraries used to build its executable"
        );
        assert_eq!(
            cache_home,
            expected_cache_home.as_deref().map(|path| path.as_std_path().as_os_str()),
            "the listing launch omitted the isolated cache home inherited by test processes"
        );
    }

    #[test]
    fn resource_marker_listing_requests_only_ignored_tests() {
        let (_scratch, work) = crate::testing::helper_workspace("list-resource-markers", &["exit:0"]);
        let command = listing_command(&work, &crate::testing::helper(), false, true);
        let arguments: Vec<_> = command.get_args().collect();

        assert_eq!(
            arguments,
            [
                std::ffi::OsStr::new("--list"),
                std::ffi::OsStr::new("--format=terse"),
                std::ffi::OsStr::new("--ignored"),
            ]
        );
    }

    #[test]
    #[cfg(unix)]
    #[cfg_attr(miri, ignore = "requires native subprocess execution")]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_binary_that_never_answers_a_listing_is_abandoned_when_its_budget_runs_out() {
        let mut command = std::process::Command::new("/bin/sh");

        let _ = command.args(["-c", "sleep 300"]);

        let started = Instant::now();
        let names = listed(command, Duration::from_millis(200));

        assert!(names.is_none(), "a binary that never listed was believed: {names:?}");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the listing waited far past its budget: {:?}",
            started.elapsed()
        );
    }

    #[test]
    #[cfg(unix)]
    #[cfg_attr(miri, ignore = "requires native subprocess execution and containment")]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn an_escaped_descendant_holding_listing_stdout_does_not_outlive_the_listing_budget() {
        if !std::process::Command::new("setsid")
            .arg("true")
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("skipping escaped-descendant fixture because `setsid` is unavailable");
            return;
        }

        let directory = crate::testing::workdir("listing-escaped-listing-");
        let marker = Utf8PathBuf::from_path_buf(directory.path().join("escaped.pid")).expect("UTF-8 path");
        let mut command = std::process::Command::new("/bin/sh");
        let script = format!("setsid sh -c 'echo $$ > \"{marker}\"; sleep 5' & while [ ! -s \"{marker}\" ]; do sleep 0.01; done");

        let _ = command.args(["-c", &script]);

        let started = Instant::now();
        let names = listed(command, Duration::from_millis(100));

        for _attempt in 0..20 {
            if marker.as_std_path().exists() {
                break;
            }

            thread::sleep(Duration::from_millis(10));
        }

        let pid = fs::read_to_string(marker.as_std_path()).expect("the escaped descendant started");
        let _stopped = std::process::Command::new("kill").args(["-TERM", pid.trim()]).status();

        assert!(names.is_none(), "an incomplete listing was accepted: {names:?}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the listing waited for an escaped pipe holder: {:?}",
            started.elapsed()
        );
    }

    #[test]
    #[cfg(unix)]
    #[cfg_attr(miri, ignore = "requires native subprocess execution")]
    fn a_binary_that_answers_the_listing_is_read_as_the_tests_it_named() {
        let mut command = std::process::Command::new("/bin/sh");

        let _ = command.args(["-c", "printf 'suite::first: test\\nsuite::second: test\\n'"]);

        let names = listed(command, Duration::from_secs(30)).expect("the listing was answered");

        assert_eq!(names, vec!["suite::first".into(), "suite::second".into()]);
    }

    #[test]
    #[cfg(unix)]
    #[cfg_attr(miri, ignore = "requires native subprocess execution")]
    fn a_binary_that_fails_while_listing_is_not_read_as_the_tests_it_managed_to_print() {
        let mut command = std::process::Command::new("/bin/sh");

        let _ = command.args(["-c", "printf 'suite::first: test\\n'; exit 1"]);

        assert!(listed(command, Duration::from_secs(30)).is_none());
    }

    #[test]
    #[cfg_attr(miri, ignore = "requires native subprocess execution")]
    fn a_listing_is_narrowed_by_the_filters_the_user_gave() {
        const SCRIPT: &str = "when-arg:parser|print:suite::parser_works: test";

        let (_scratch, mut work) = crate::testing::helper_workspace("listing-list-filtered", &[SCRIPT]);
        let binary = crate::testing::helper();

        work.set_test_args(vec![crate::testing::directive(SCRIPT), "parser".to_owned()]);

        assert_eq!(
            list_selected_allow_empty(&work, &binary, MemoryRequest::default()).ok(),
            Some(vec!["suite::parser_works".into()]),
            "the user's filter should have reached the listing"
        );

        work.set_test_args(vec![crate::testing::directive(SCRIPT)]);

        assert_eq!(
            list_selected_allow_empty(&work, &binary, MemoryRequest::default()).ok(),
            Some(Vec::new())
        );
    }
}
