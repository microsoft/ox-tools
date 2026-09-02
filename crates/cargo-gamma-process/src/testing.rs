// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::fmt;
#[cfg(unix)]
use core::panic::AssertUnwindSafe;
#[cfg(unix)]
use core::time::Duration;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
#[cfg(unix)]
use std::sync::mpsc;

use camino::{Utf8Path, Utf8PathBuf};

const HELPER_SOURCE: &str = r#"
thread_local! {
    static HELD: std::cell::RefCell<Vec<Vec<u8>>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn main() {
    let mut code = 0;

    for argument in std::env::args().skip(1) {
        let Some(directive) = argument.strip_prefix("--gamma-step=") else {
            continue;
        };

        if let Some(status) = step(directive) {
            code = status;
            break;
        }
    }

    std::process::exit(code);
}

fn step(directive: &str) -> Option<i32> {
    let (name, payload) = directive.split_once(':').unwrap_or((directive, ""));

    match name {
        "sleep" => {
            let ms = payload.parse().unwrap_or(0);
            std::thread::sleep(std::time::Duration::from_millis(ms));
            None
        }
        "exit" => Some(payload.parse().unwrap_or(0)),
        "touch" => {
            let _ = std::fs::write(payload, b"");
            None
        }
        "wait" => {
            while !std::path::Path::new(payload).exists() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            None
        }
        "spawn" => {
            launch(payload, false);
            None
        }
        "flee" => {
            launch(payload, true);
            None
        }
        "eat" => {
            let mib: usize = payload.parse().unwrap_or(0);
            let mut held: Vec<Vec<u8>> = Vec::new();

            for _ in 0..mib {
                let mut block = vec![0_u8; 1024 * 1024];

                for at in (0..block.len()).step_by(4096) {
                    block[at] = 1;
                }

                held.push(block);
            }

            HELD.with(|slot| slot.borrow_mut().extend(held));
            None
        }
        _ => Some(97),
    }
}

fn launch(payload: &str, own_group: bool) {
    let executable = if cfg!(target_os = "linux") {
        std::path::PathBuf::from("/proc/self/exe")
    } else {
        std::env::current_exe().expect("the helper knows its own path")
    };
    let mut child = std::process::Command::new(executable);

    for inner in payload.split('|') {
        let _ = child.arg(format!("--gamma-step={inner}"));
    }

    // The escape a process group has no answer to: one unprivileged call and every later signal to
    // the group misses this process. A cgroup leaf and a job object are not renounceable this way.
    #[cfg(unix)]
    if own_group {
        use std::os::unix::process::CommandExt as _;

        let _ = child.process_group(0);
    }

    #[cfg(not(unix))]
    let _ = own_group;

    let _spawned = child.spawn().expect("the helper can start another copy of itself");
}
"#;

pub fn helper_binary_path() -> &'static Utf8Path {
    static BUILT: OnceLock<Utf8PathBuf> = OnceLock::new();

    BUILT.get_or_init(|| build_or_reuse_helper("gamma-process-helper-4")).as_path()
}

/// Builds the helper binary under `name`, or reuses one already there.
///
/// Separate from [`helper_binary_path`] so a test can drive the build path with a name of its own
/// that nothing else on the host is using, rather than racing every other test process that shares
/// the cached name `helper_binary_path` hands out.
fn build_or_reuse_helper(name: &str) -> Utf8PathBuf {
    let work =
        Utf8PathBuf::from_path_buf(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-work")).expect("the target path is UTF-8");

    fs::create_dir_all(work.as_std_path()).expect("the test work directory should be creatable");

    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let helper = work.join(format!("{name}{suffix}"));

    if helper.exists() {
        return helper;
    }

    let staging = tempfile::Builder::new()
        .prefix("gamma-process-helper-build")
        .tempdir_in(work.as_std_path())
        .expect("the staging directory should be creatable");
    let source = staging.path().join("helper.rs");
    let staged = staging.path().join(format!("helper{suffix}"));

    fs::write(&source, HELPER_SOURCE).expect("the helper source should be writable");

    let built = Command::new("rustc")
        .arg("--edition")
        .arg("2024")
        .arg("-C")
        .arg("debuginfo=0")
        .arg("-o")
        .arg(&staged)
        .arg(&source)
        .output()
        .expect("rustc should be available to the test suite");

    assert!(
        built.status.success(),
        "the test helper should compile: {}",
        String::from_utf8_lossy(&built.stderr)
    );

    let _moved = fs::rename(&staged, helper.as_std_path());

    assert!(helper.exists(), "the test helper should be in place at {helper}");

    helper
}

pub fn directive(step: impl fmt::Display) -> String {
    format!("--gamma-step={step}")
}

pub fn workdir(prefix: &str) -> tempfile::TempDir {
    let work = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-work");

    fs::create_dir_all(&work).expect("the test work directory should be creatable");
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(work)
        .expect("the temporary directory should be creatable")
}

#[cfg(unix)]
pub const WATCHDOG: Duration = Duration::from_mins(1);

#[cfg(unix)]
pub fn within<T: Send + 'static>(budget: Duration, what: &str, body: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = mpsc::channel();

    let _worker = std::thread::spawn(move || {
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(body));
        let _delivered = sender.send(outcome);
    });

    finish_within(what, budget, receiver.recv_timeout(budget))
}

/// The decision behind [`within`], taking the receive outcome rather than racing a real thread for
/// it, so a test can drive every arm — including the panic, timeout, and disconnect cases that a
/// real race can only produce by chance — with a value it constructed directly.
#[cfg(unix)]
fn finish_within<T>(what: &str, budget: Duration, received: Result<std::thread::Result<T>, mpsc::RecvTimeoutError>) -> T {
    match received {
        Ok(Ok(value)) => value,
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("{what} did not finish within {budget:?}; it is hung"),
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("{what} ended without an answer"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The helper is built the first time a name is asked for, and reused after that.
    ///
    /// Uses a name of its own — unique to this process — rather than the shared name
    /// `helper_binary_path` hands out, so this does not race every other test process on the host
    /// that may already be relying on that shared cache existing.
    #[test]
    fn a_fresh_name_is_built_and_then_reused() {
        let name = format!("gamma-process-helper-build-test-{}", std::process::id());

        let built = build_or_reuse_helper(&name);

        assert!(built.exists(), "the helper should have been built at {built}");

        let reused = build_or_reuse_helper(&name);

        assert_eq!(built, reused, "a second call should reuse rather than rebuild");

        let _removed = fs::remove_file(built.as_std_path());
    }

    #[test]
    fn a_directive_is_formatted_for_the_helper_binary() {
        assert_eq!(directive("exit:0"), "--gamma-step=exit:0");
    }

    #[test]
    fn a_workdir_is_created_under_the_shared_test_work_directory() {
        let dir = workdir("gamma-testing-workdir-check");

        assert!(dir.path().is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn finishing_within_returns_the_value_a_body_produced() {
        assert_eq!(finish_within("a body", Duration::from_secs(1), Ok(Ok(42))), 42);
    }

    #[cfg(unix)]
    #[test]
    #[should_panic = "the body panicked"]
    fn finishing_within_resumes_a_body_that_panicked() {
        let panic: Box<dyn core::any::Any + Send> = Box::new("the body panicked");

        let _ignored: () = finish_within("a body", Duration::from_secs(1), Ok(Err(panic)));
    }

    #[cfg(unix)]
    #[test]
    #[should_panic = "did not finish within"]
    fn finishing_within_reports_a_timeout() {
        let _ignored: () = finish_within("a body", Duration::from_secs(1), Err(mpsc::RecvTimeoutError::Timeout));
    }

    #[cfg(unix)]
    #[test]
    #[should_panic = "ended without an answer"]
    fn finishing_within_reports_a_disconnected_sender() {
        let _ignored: () = finish_within("a body", Duration::from_secs(1), Err(mpsc::RecvTimeoutError::Disconnected));
    }
}
