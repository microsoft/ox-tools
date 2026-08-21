// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration test for the `init` command driven through the real CLI entry point.

#![cfg(not(miri))]

mod support;

use support::TestHost;

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call GetTempPathW")]
async fn init_command_writes_a_default_configuration() {
    let dir = tempfile::tempdir().expect("creating a temp dir");
    let output = dir.path().join("aprz.toml");
    let output_arg = output.to_str().expect("temp dir paths are UTF-8");

    let mut host = TestHost::new();
    cargo_aprz_lib::run(&mut host, ["cargo", "aprz", "init", output_arg]).await;

    assert!(host.exit_code.is_none(), "init should succeed: {}", host.error_str());
    assert!(output.exists(), "the configuration file must be written");
    assert!(
        host.output_str().contains("Generated default configuration file"),
        "unexpected output: {}",
        host.output_str()
    );

    let contents = std::fs::read_to_string(&output).expect("read the generated config");
    assert!(!contents.is_empty(), "the generated configuration must not be empty");
}
