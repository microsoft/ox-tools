// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the `--color` modes of the console report.
//!
//! The shared `MockWorld` pins `--color never`, so these tests strip that pair from the world's
//! arguments and supply their own value instead.

#![cfg(not(miri))]

mod support;

use support::{MockWorld, TestHost};

/// The world's arguments with its `--color <mode>` pair removed.
fn args_without_color(world: &MockWorld) -> Vec<&str> {
    let world_args = world.args();
    let mut kept = Vec::with_capacity(world_args.len());
    let mut skip_value = false;

    for arg in world_args {
        if skip_value {
            skip_value = false;
        } else if arg == "--color" {
            skip_value = true;
        } else {
            kept.push(arg);
        }
    }

    kept
}

async fn run_with_color(color: &str) -> TestHost {
    let world = MockWorld::new().await;

    let mut args = vec!["cargo", "aprz", "crates", "serde@1.0.200", "--console"];
    args.extend(args_without_color(&world));
    args.extend_from_slice(&["--color", color]);

    let mut host = TestHost::new();
    cargo_aprz_lib::run(&mut host, args).await;
    host
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn console_report_forces_colors_on_request() {
    let host = run_with_color("always").await;

    assert!(host.exit_code.is_none(), "the command should succeed: {}", host.error_str());
    let output = host.output_str();
    assert!(output.contains("serde"), "console output should mention the crate: {output}");
    assert!(output.contains('\u{1b}'), "--color always must emit ANSI escapes: {output:?}");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn console_report_detects_colors_automatically() {
    let host = run_with_color("auto").await;

    assert!(host.exit_code.is_none(), "the command should succeed: {}", host.error_str());
    let output = host.output_str();
    assert!(output.contains("serde"), "console output should mention the crate: {output}");
    // Whether auto-detection turns colors on depends on how the harness captures stdout, so the
    // only stable expectation is that the report is still produced.
}
