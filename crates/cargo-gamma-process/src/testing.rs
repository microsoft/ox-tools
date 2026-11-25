// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::fmt;
use core::panic::AssertUnwindSafe;
use core::time::Duration;
use std::fs;
use std::io::{self, ErrorKind, Read as _};
use std::path::Path;
use std::process::Command;
use std::sync::mpsc;

use camino::Utf8Path;

pub fn helper_binary_path() -> &'static Utf8Path {
    Utf8Path::new(cargo_gamma_process_test_helper::PATH)
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

pub fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + core::time::Duration::from_secs(6);
    while !ready() {
        assert!(std::time::Instant::now() < deadline, "{what} did not become ready");
        std::thread::sleep(core::time::Duration::from_millis(10));
    }
}

pub fn listener() -> std::net::TcpListener {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a local readiness listener");
    listener.set_nonblocking(true).expect("readiness accepts must be bounded");
    listener
}

pub fn accepted(listener: &std::net::TcpListener) -> std::net::TcpStream {
    let mut connection = None;
    wait_until("the helper's readiness connection", || {
        match listener.accept() {
            Ok((stream, _peer)) => connection = Some(stream),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("readiness accept failed: {error}"),
        }
        connection.is_some()
    });
    connection.expect("the successful readiness probe stored its connection")
}

pub fn assert_closed(mut connection: std::net::TcpStream) {
    connection
        .set_nonblocking(true)
        .expect("the readiness oracle owns a valid TCP connection");
    wait_until("the killed helper's connection closure", || closed(connection.read(&mut [0_u8])));
}

// The helper owns the readiness socket. EOF, reset, and abort all prove its release;
// pending reads do not, and unrelated observation failures must remain visible.
fn closed(outcome: io::Result<usize>) -> bool {
    match outcome {
        Ok(0) => true,
        Err(error) if matches!(error.kind(), ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted) => true,
        Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => false,
        outcome => panic!("the killed helper must release its connection, got {outcome:?}"),
    }
}

#[cfg(unix)]
pub const WATCHDOG: Duration = Duration::from_mins(1);

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
fn finish_within<T>(what: &str, budget: Duration, received: Result<std::thread::Result<T>, mpsc::RecvTimeoutError>) -> T {
    match received {
        Ok(Ok(value)) => value,
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("{what} did not finish within {budget:?}; it is hung"),
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("{what} ended without an answer"),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_directive_is_formatted_for_the_helper_binary() {
        assert_eq!(directive("exit:0"), "--gamma-step=exit:0");
    }

    #[test]
    fn the_closure_oracle_retries_pending_io_until_eof() {
        let mut reads = [Err(ErrorKind::WouldBlock.into()), Err(ErrorKind::Interrupted.into()), Ok(0)].into_iter();

        assert!(reads.by_ref().any(closed));

        assert!(reads.next().is_none(), "pending reads must not count as closure");
    }

    #[test]
    #[should_panic = "the killed helper must release its connection"]
    fn the_closure_oracle_does_not_accept_received_data_as_eof() {
        let _closed = closed(Ok(1));
    }

    #[test]
    fn the_closure_oracle_accepts_terminal_socket_errors_but_not_observation_failures() {
        for kind in [ErrorKind::ConnectionReset, ErrorKind::ConnectionAborted] {
            assert!(closed(Err(kind.into())));
        }
        for kind in [ErrorKind::WouldBlock, ErrorKind::Interrupted] {
            assert!(!closed(Err(kind.into())));
        }
    }

    #[test]
    #[should_panic = "the killed helper must release its connection"]
    fn the_closure_oracle_rejects_an_unrelated_read_failure() {
        let _closed = closed(Err(ErrorKind::PermissionDenied.into()));
    }

    #[test]
    fn an_empty_wait_path_is_rejected_instead_of_blocking() {
        let status = Command::new(helper_binary_path().as_std_path())
            .arg(directive("wait:"))
            .status()
            .expect("the helper should run");

        assert_eq!(status.code(), Some(98));
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
