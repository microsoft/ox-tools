// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration test for the process-global setup the CLI performs before any work happens.
//!
//! This binary deliberately holds a *single* test. The test relaunches itself with its environment
//! configured before the child test harness starts, then installs the process-wide logger there.
//!
//! Linux also verifies platform cache-directory discovery. Windows known-folder discovery cannot
//! be redirected through process environment, so it uses an explicit temporary cache directory.

#![cfg(not(miri))]
#![cfg(any(target_os = "linux", target_os = "windows"))]

mod support;

use support::dump::Dump;
use support::{TestHost, dump_server, dump_url, failing_server, seed_advisory_db};

const CHILD: &str = "CARGO_APRZ_PROCESS_GLOBALS_CHILD";
const CACHE_ROOT: &str = "CARGO_APRZ_PROCESS_GLOBALS_CACHE_ROOT";

/// Exercises the defaults that every other test bypasses: the platform cache directory, an
/// enabled log level (which installs the logger and disables the progress delay), forced colors
/// and an overridden advisory database address.
#[test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
fn cli_uses_the_platform_cache_directory_and_installs_a_logger() {
    if std::env::var_os(CHILD).is_none() {
        let home = tempfile::tempdir().expect("creating a temp dir");
        let cache_root = home.path().join("cache");
        let status = std::process::Command::new(std::env::current_exe().expect("current test executable"))
            .args([
                "--exact",
                "cli_uses_the_platform_cache_directory_and_installs_a_logger",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env(CACHE_ROOT, &cache_root)
            .env("HOME", home.path())
            .env("RUST_LOG", "trace")
            .env("XDG_CACHE_HOME", &cache_root)
            .status()
            .expect("launching isolated process-global test");

        assert!(status.success(), "isolated process-global test failed with {status}");
        return;
    }

    let runtime = tokio::runtime::Runtime::new().expect("creating test runtime");
    runtime.block_on(cli_uses_process_globals());
}

async fn cli_uses_process_globals() {
    let dump = dump_server(Dump::sample(chrono::Utc::now()).to_tar_gz(), None).await;
    let services = failing_server(404).await;
    let cache_root = std::path::PathBuf::from(std::env::var_os(CACHE_ROOT).expect("parent-provided cache root"));
    let app_cache = cache_root.join("cargo-aprz");
    seed_advisory_db(&app_cache);

    let service_uri = services.uri();
    let dump_uri = dump_url(&dump);
    let args = vec![
        "cargo",
        "aprz",
        "crates",
        "serde@1.0.200",
        "--console",
        "--log-level",
        "error",
        "--color",
        "always",
        "--dump-url",
        &dump_uri,
        "--docs-url",
        &service_uri,
        "--coverage-url",
        &service_uri,
        "--github-url",
        &service_uri,
        "--codeberg-url",
        &service_uri,
        "--advisory-url",
        &service_uri,
    ];
    #[cfg(target_os = "windows")]
    let cache_arg = app_cache.to_string_lossy().into_owned();
    #[cfg(target_os = "windows")]
    let args = {
        let mut args = args;
        args.extend(["--cache-dir", &cache_arg]);
        args
    };

    let mut host = TestHost::new();
    cargo_aprz_lib::run(&mut host, args).await;

    assert!(host.exit_code.is_none(), "the command should succeed: {}", host.error_str());
    assert!(host.output_str().contains("serde"), "console output should mention the crate");
    assert_eq!(
        log::max_level(),
        log::LevelFilter::Trace,
        "the logger must honor the RUST_LOG override rather than the --log-level fallback"
    );
    assert!(
        app_cache.join("crates").exists(),
        "the dump must be cached under the platform cache directory"
    );
}
