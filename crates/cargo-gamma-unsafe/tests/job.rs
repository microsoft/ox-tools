// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(windows)]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::io::{BufRead as _, Write as _};
use std::os::windows::io::{AsHandle as _, AsRawHandle as _};
use std::process::{Command, Stdio};

use cargo_gamma_unsafe::job::{Job, release, start_suspended};
use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::WaitForSingleObject;

#[path = "support/job_child.rs"]
mod job_child;
#[path = "support/job_watchdog.rs"]
mod job_watchdog;

use job_child::TestChild;
use job_watchdog::{Watchdog, native_smoke_enabled};

#[test]
#[ignore = "manual native process smoke uses a wall-clock watchdog"]
fn an_unassigned_test_child_is_cleaned_up_on_assertion_unwind() {
    if !native_smoke_enabled() {
        return;
    }
    let mut command = Command::new("cmd");
    let _command = command.args(["/C", "exit", "/B", "0"]);
    start_suspended(&mut command);
    let _watchdog;
    let child = TestChild::spawn(&mut command);
    _watchdog = Watchdog::new(&child);
    let observed = child
        .as_handle()
        .try_clone_to_owned()
        .expect("retain an independent process handle");
    // SAFETY: the independently owned handle names the suspended child; zero only queries it.
    assert_eq!(unsafe { WaitForSingleObject(observed.as_raw_handle(), 0) }, WAIT_TIMEOUT);
    let unwind = std::panic::catch_unwind(move || {
        let _owned = child;
        panic!("simulated assertion failure before job assignment");
    });
    let cause = unwind.expect_err("the simulated assertion must unwind");
    assert_eq!(
        cause.downcast_ref::<&str>(),
        Some(&"simulated assertion failure before job assignment")
    );
    // SAFETY: the independent handle stays valid after TestChild kills, reaps and closes its child.
    assert_eq!(unsafe { WaitForSingleObject(observed.as_raw_handle(), 0) }, WAIT_OBJECT_0);
}

#[test]
#[ignore = "manual native job smoke uses a wall-clock watchdog"]
fn a_child_dies_when_the_last_handle_to_its_job_closes() {
    if !native_smoke_enabled() {
        return;
    }
    let job = Job::create(None).expect("a job is created");
    let mut command = Command::new(std::env::current_exe().expect("the test executable"));
    let _command = command
        .args(["--ignored", "--exact", "the_job_close_child_waits_for_parent_input", "--nocapture"])
        .env("CARGO_GAMMA_JOB_CLOSE_CHILD", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    start_suspended(&mut command);
    let _watchdog;
    let mut child = TestChild::spawn(&mut command);
    _watchdog = Watchdog::new(&child);
    assert!(job.assign(&child), "the job accepts the suspended child");
    assert!(release(child.id()), "the child is released after assignment");
    let input = child.stdin.take().expect("the parent owns the unreleased input gate");
    let output = child.stdout.take().expect("the child reports reaching its input gate");
    let ready = std::io::BufReader::new(output)
        .lines()
        .map(|line| line.expect("the child readiness line"))
        .any(|line| line == "job-close input gate reached");
    assert!(ready, "the exact helper must reach its input gate");
    assert!(child.try_wait().expect("query the child status").is_none());
    drop(job);
    let status = child.wait().expect("the child exits when its job closes");
    assert_eq!(status.code(), Some(0), "kernel job-close termination, not the helper's gate exit");
    // Holding stdin through wait prevents EOF from substituting for job-close termination.
    drop(input);
}

#[test]
#[ignore = "subprocess helper for the manual native job-close smoke"]
fn the_job_close_child_waits_for_parent_input() {
    if !native_smoke_enabled() {
        return;
    }
    if std::env::var_os("CARGO_GAMMA_JOB_CLOSE_CHILD").is_none() {
        return;
    }
    println!("job-close input gate reached");
    std::io::stdout().flush().expect("publish readiness before blocking");
    let mut input = String::new();
    let _read = std::io::stdin().read_line(&mut input);
    std::process::exit(78);
}
