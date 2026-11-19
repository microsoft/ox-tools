// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]

use std::process::{Command, Stdio};
use std::thread;

use cargo_gamma_process::{MemoryRequest, OutputError, ProcessTree, output, output_bounded, prepare};
use cargo_gamma_process_test_helper::PATH;

#[test]
fn terminating_a_native_tree_reaps_its_running_leader() {
    let mut command = Command::new(PATH);
    command
        .arg("--gamma-step=sleep:60000")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let prepared = prepare(command, MemoryRequest::default()).unwrap();
    let mut tree = ProcessTree::adopt(prepared.spawn().unwrap()).unwrap();
    let status = tree.terminate().unwrap().expect("the sleeping child is reaped by its owning tree");
    assert!(!status.success());
}

#[test]
fn default_capture_limits_accept_exactly_the_boundary_and_reject_one_extra_byte() {
    const STDOUT_LIMIT: usize = 64 * 1024 * 1024;
    const STDERR_LIMIT: usize = 64 * 1024;
    for wait in [false, true] {
        for stderr in [false, true] {
            let limit = if stderr { STDERR_LIMIT } else { STDOUT_LIMIT };
            for extra in [0, 1] {
                let length = limit + extra;
                let mut command = Command::new(PATH);
                let (stdout, stderr) = if stderr { (0, length) } else { (length, 0) };
                command.arg(format!("--gamma-step=raw-output:{stdout}:{stderr}"));
                let response = if wait {
                    command.stdout(Stdio::piped()).stderr(Stdio::piped());
                    let prepared = prepare(command, MemoryRequest::default()).unwrap();
                    ProcessTree::adopt(prepared.spawn().unwrap())
                        .unwrap()
                        .wait_with_output()
                        .map_err(OutputError::from)
                } else {
                    output(command, MemoryRequest::default())
                };
                if extra == 0 {
                    let captured = response.unwrap_or_else(|cause| panic!("exact capture boundary failed: {cause}"));
                    assert!(captured.status.success());
                    assert_eq!((captured.stdout.len(), captured.stderr.len()), (stdout, stderr));
                } else {
                    let cause = match response {
                        Err(cause) => cause,
                        Ok(captured) => panic!(
                            "capture overflow unexpectedly succeeded: stdout {} bytes, stderr {} bytes",
                            captured.stdout.len(),
                            captured.stderr.len()
                        ),
                    };
                    let captured = cause.captured_output().expect("overflow preserves the bounded child result");
                    assert!(captured.status.success());
                    assert_eq!(
                        (captured.stdout.len(), captured.stderr.len()),
                        (stdout.min(STDOUT_LIMIT), stderr.min(STDERR_LIMIT))
                    );
                }
            }
        }
    }
}

#[test]
fn bounded_capture_keeps_a_stdout_prefix_and_reports_stderr_only_overflow() {
    for (stdout, stderr, truncated) in [(3, 5, false), (3, 6, true), (6, 5, true)] {
        let mut command = Command::new(PATH);
        command
            .arg(format!("--gamma-step=raw-output:{stdout}:{stderr}"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let prepared = prepare(command, MemoryRequest::default()).unwrap();
        let response = ProcessTree::adopt(prepared.spawn().unwrap())
            .unwrap()
            .wait_with_output_bounded(3, 5)
            .map_err(OutputError::from);
        assert_eq!(response.is_err(), truncated);
        if let Err(cause) = &response {
            let diagnostic = cause.to_string();
            assert!(diagnostic.contains("child output exceeded its capture limit"), "{diagnostic}");
            assert!(
                diagnostic.contains(if stderr > 5 { "stderr tail:\nbcdef" } else { "stderr tail:\nabcde" }),
                "{diagnostic}"
            );
        }
        let captured = response.as_ref().unwrap_or_else(|cause| cause.captured_output().unwrap());
        assert!(captured.status.success());
        assert_eq!(captured.stdout, b"abc");
        assert_eq!(captured.stderr, if stderr > 5 { b"bcdef" } else { b"abcde" });
    }
}

#[test]
fn maximum_capture_limits_do_not_overflow_or_discard_small_outputs() {
    let mut command = Command::new(PATH);
    command
        .arg("--gamma-step=raw-output:2:2")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let prepared = prepare(command, MemoryRequest::default()).unwrap();
    let captured = ProcessTree::adopt(prepared.spawn().unwrap())
        .unwrap()
        .wait_with_output_bounded(usize::MAX, usize::MAX)
        .unwrap();
    assert!(captured.status.success());
    assert_eq!(captured.stdout, b"ab");
    assert_eq!(captured.stderr, b"ab");
}

#[test]
fn simultaneous_captures_bound_both_streams_and_preserve_status_and_diagnostic_tails() {
    let workers = (0..4)
        .map(|_| {
            thread::spawn(|| {
                let mut command = Command::new(PATH);
                command.args(["--gamma-step=flood:10000", "--gamma-step=exit:7"]);
                let error = output_bounded(command, MemoryRequest::default(), 128, 64).expect_err("oversized output is never complete");
                let captured = error
                    .captured_output()
                    .expect("the original child result survives capture overflow");
                assert_eq!(captured.status.code(), Some(7));
                assert_eq!(captured.stdout.len(), 128);
                assert_eq!(captured.stderr.len(), 64);
                assert!(captured.stderr.ends_with(b"diagnostic-tail"));
            })
        })
        .collect::<Vec<_>>();
    for worker in workers {
        worker.join().expect("each independently bounded capture completes");
    }
}

#[test]
fn oversized_successful_stdout_is_not_returned_as_parseable_json() {
    let mut command = Command::new(PATH);
    command.arg("--gamma-step=flood:1000");
    let error = output_bounded(command, MemoryRequest::default(), 8, 2048).expect_err("stdout was truncated");
    let captured = error.captured_output().expect("bounded child state");
    assert!(captured.status.success());
    assert_eq!(captured.stdout.len(), 8);
}
