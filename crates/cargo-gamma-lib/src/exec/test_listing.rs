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

// Allow cold or loaded hosts while bounding executables that never finish listing.
const LIST_BUDGET: Duration = Duration::from_secs(30);
// Accept large short-line listings without buffering unbounded child output.
const LIST_CAP: usize = 4 * 1024 * 1024;
// Avoid spinning while waiting for an uncooperative child.
const LIST_POLL: Duration = Duration::from_millis(5);

/// Lists user-selected cases without resource markers, accepting an empty successful listing.
#[cfg_attr(coverage_nightly, coverage(off))]
pub(super) fn list_selected_allow_empty(work: &Workspace, binary: &TestBinary, request: MemoryRequest) -> Option<Vec<Box<str>>> {
    let command = listing_command(work, binary, true, false);
    listed_with_request(command, LIST_BUDGET, request).map(|mut names| {
        names.retain(|name| !is_marker(name));
        names
    })
}

/// Lists ignored resource marker tests without applying user filters.
pub(super) fn list_resource_markers(work: &Workspace, binary: &TestBinary, request: MemoryRequest) -> Option<Vec<Box<str>>> {
    #[cfg(test)]
    RESOURCE_LISTING_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));

    let command = listing_command(work, binary, false, true);
    listed_with_request(command, LIST_BUDGET, request).map(|mut names| {
        names.retain(|name| is_marker(name));
        names
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
    let _ = command
        .current_dir(working_directory(work, binary).as_std_path())
        .stderr(Stdio::null());
    configure_loader(&mut command, work.launch());
    work.configure_test_environment(&mut command, binary);
    work.inherit_test_cache_home(&mut command);
    command
}

fn read_listing(pipe: &mut impl io::Read) -> (Vec<u8>, bool) {
    let mut text = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut whole = true;
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                // Drain past the cap so the child cannot block on a full pipe.
                let room = LIST_CAP.saturating_sub(text.len());
                text.extend_from_slice(&buffer[..read.min(room)]);
                whole &= read <= room;
            }
            Err(cause) if cause.kind() == io::ErrorKind::Interrupted => {}
            Err(_truncated) => {
                whole = false;
                break;
            }
        }
    }
    (text, whole)
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
fn listed_with_request(mut command: Command, budget: Duration, request: MemoryRequest) -> Option<Vec<Box<str>>> {
    #[cfg(test)]
    if let Some(listing) = SCRIPTED_LISTING.with_borrow_mut(|runner| runner.as_mut().map(|runner| runner(&command, budget, request))) {
        return listing;
    }
    let _ = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let prepared = prepare(command, request).ok()?;
    let spawned = prepared.spawn().ok()?;
    let mut subtree = ProcessTree::adopt(spawned).ok()?;
    let (sender, receiver) = mpsc::sync_channel(1);
    let reading = subtree.take_stdout().map(|mut pipe| {
        let sender = sender.clone();
        let failed = sender.clone();
        #[cfg(test)]
        let refused = faults::fired(Fault::Thread);
        #[cfg(not(test))]
        let refused = false;
        let spawned = if refused {
            Err(io::Error::other("the reader thread a test asked to fail"))
        } else {
            // A descendant can retain this pipe after escaping containment; never join its reader.
            thread::Builder::new().name("cargo-gamma-listing-output".to_owned()).spawn(move || {
                let (text, whole) = read_listing(&mut pipe);
                let _sent = sender.send((text, whole));
            })
        };
        if spawned.is_err() {
            let _sent = failed.send((Vec::new(), false));
        }
    });
    drop(sender);
    let deadline = Instant::now() + budget;
    let status = loop {
        match subtree.observe() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => break None,
            Ok(None) => thread::sleep(LIST_POLL.min(deadline.saturating_duration_since(Instant::now()))),
            Err(_unaskable) => break None,
        }
    };
    if status.is_none() {
        let _reaped = subtree.terminate();
    } else {
        debug_assert!(
            subtree.released(),
            "the observed listing released containment before its output was read"
        );
    }
    let (text, whole) = reading
        .and_then(|()| receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())).ok())
        .unwrap_or_default();
    if !whole || !status?.success() {
        return None;
    }
    let text = String::from_utf8(text).ok()?;
    Some(
        text.lines()
            .filter_map(|line| line.strip_suffix(": test"))
            .map(|name| name.trim().into())
            .collect(),
    )
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #[cfg(unix)]
    use std::fs;
    use std::io::Read;

    use camino::Utf8Path;
    #[cfg(unix)]
    use camino::Utf8PathBuf;
    use cargo_gamma_process::faults::{self as process_faults, Fault as ProcessFault};

    use super::*;
    use crate::exec::loader::LOADER_VAR;

    fn listed(command: Command, budget: Duration) -> Option<Vec<Box<str>>> {
        listed_with_request(command, budget, MemoryRequest::default())
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
    }

    impl Read for DataThenError {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let Some(data) = self.data.take() else {
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
            listed_with_request(command, Duration::from_secs(10), MemoryRequest::default()),
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
            let names = listed_with_request(command, Duration::from_secs(10), MemoryRequest::default());
            assert_eq!(names, (exit == 0).then(|| vec!["suite::selected".into()]));
        }
        let (_directory, work) = crate::testing::helper_workspace("listing-containment", &["print:suite::selected: test", "exit:0"]);
        let command = listing_command(&work, &crate::testing::helper(), true, false);
        let _refused = process_faults::arm(ProcessFault::Prepare);
        assert_eq!(
            listed_with_request(command, Duration::from_secs(10), MemoryRequest::default()),
            None
        );
    }

    #[test]
    fn listing_reader_retries_interrupts_and_rejects_terminal_errors() {
        let mut interrupted = InterruptedThenData {
            interrupted: false,
            data: std::io::Cursor::new(b"whole".to_vec()),
        };
        assert_eq!(read_listing(&mut interrupted), (b"whole".to_vec(), true));

        let mut truncated = DataThenError {
            data: Some(b"prefix".to_vec()),
        };
        assert_eq!(read_listing(&mut truncated), (b"prefix".to_vec(), false));

        let mut exact = std::io::Cursor::new(vec![b'x'; LIST_CAP]);
        assert!(read_listing(&mut exact).1);
        let mut excessive = std::io::Cursor::new(vec![b'x'; LIST_CAP + 1]);
        assert!(!read_listing(&mut excessive).1);
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
            list_selected_allow_empty(&work, &binary, MemoryRequest::default()),
            Some(vec!["suite::parser_works".into()]),
            "the user's filter should have reached the listing"
        );

        work.set_test_args(vec![crate::testing::directive(SCRIPT)]);

        assert_eq!(
            list_selected_allow_empty(&work, &binary, MemoryRequest::default()),
            Some(Vec::new())
        );
    }
}
