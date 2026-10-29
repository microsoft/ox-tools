// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_os = "macos")]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use core::time::Duration;
use std::io;
use std::os::unix::process::ExitStatusExt as _;
use std::process::{Command, Stdio};
use std::time::Instant;

use cargo_gamma_process::{MemoryRequest, MemoryUsage, ProcessTree, prepare, support};

const MIB: usize = 1024 * 1024;
const CHILD_MODE: &str = "GAMMA_MACOS_MEMORY_TEST_CHILD";
const TEST_NAME: &str = "macos_runaway_allocator_crosses_sampled_ceiling";

fn child_command(mode: &str) -> io::Result<Command> {
    let mut command = Command::new(std::env::current_exe()?);
    let _ = command
        .args(["--exact", TEST_NAME, "--ignored", "--nocapture"])
        .env(CHILD_MODE, mode)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}

/// Opt-in host check: the runaway is a descendant, bounded at 4 GiB even if enforcement breaks.
#[test]
#[ignore = "needs macOS process-group footprint accounting: run by name with --ignored"]
fn macos_runaway_allocator_crosses_sampled_ceiling() {
    match std::env::var(CHILD_MODE).as_deref() {
        Ok("allocator") => {
            let mut held = Vec::new();
            for _ in 0..4096 {
                held.push(vec![1_u8; MIB]);
                std::hint::black_box(&held);
            }
            return;
        }
        Ok("leader") => {
            let status = child_command("allocator").unwrap().status().unwrap();
            assert_eq!(status.code(), Some(0));
            return;
        }
        Err(std::env::VarError::NotPresent) => {}
        mode => panic!("unexpected child mode: {mode:?}"),
    }

    support().unwrap();
    let ceiling = 64 * MIB as u64;
    // At 10 ms sampling, allow at most 256 MiB of overshoot on this opt-in host check.
    // This is a test bound, not a hard guarantee provided by sampled enforcement.
    let overshoot_bound = 256 * MIB as u64;
    let prepared = prepare(
        child_command("leader").unwrap(),
        MemoryRequest {
            calibrating: false,
            meter: true,
            limit: Some(ceiling),
        },
    )
    .unwrap();
    let spawned = prepared.spawn().unwrap();
    let mut tree = ProcessTree::adopt(spawned).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = tree.observe().unwrap() {
            break status;
        }
        // This is only a watchdog. Allocation is independently capped in the child.
        assert!(
            Instant::now() < deadline,
            "the capped allocator was not stopped; ProcessTree drop cleans up"
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    let usage = tree.usage();
    let peak = usage.peak.unwrap();
    // `exhausted` is the boundary's breach outcome consumed by cargo-gamma-lib as `outofmem`.
    assert_eq!(
        usage,
        MemoryUsage {
            peak: Some(peak),
            exhausted: true
        }
    );
    assert_eq!(status.signal(), Some(9));
    assert!(
        peak > ceiling && peak <= ceiling + overshoot_bound,
        "peak {peak}, ceiling {ceiling}, allowed overshoot {overshoot_bound}"
    );
    eprintln!("sampled breach: ceiling={ceiling}, peak={peak}, overshoot={}", peak - ceiling);
}
