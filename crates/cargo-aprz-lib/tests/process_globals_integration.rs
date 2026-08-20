// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration test for the process-global setup the CLI performs before any work happens.
//!
//! This binary deliberately holds a *single* test: it installs a process-wide logger (which can
//! only ever be done once) and redirects the platform cache directory through the environment,
//! neither of which is safe to do alongside another test in the same process.
//!
//! Linux also verifies platform cache-directory discovery. Windows known-folder discovery cannot
//! be redirected through process environment, so it uses an explicit temporary cache directory.

#![cfg(not(miri))]
#![cfg(any(target_os = "linux", target_os = "windows"))]

mod support;

use support::dump::Dump;
use support::{TestHost, dump_server, dump_url, failing_server, seed_advisory_db};

/// Exercises the defaults that every other test bypasses: the platform cache directory, an
/// enabled log level (which installs the logger and disables the progress delay), forced colors
/// and an overridden advisory database address.
#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn cli_uses_the_platform_cache_directory_and_installs_a_logger() {
    let dump = dump_server(Dump::sample(chrono::Utc::now()).to_tar_gz(), None).await;
    let services = failing_server(404).await;
    let home = tempfile::tempdir().expect("creating a temp dir");
    let cache_root = home.path().join("cache");

    // `directories::BaseDirs` reads these, so the tool's default cache directory lands
    // inside the temporary home instead of the real one.
    // SAFETY: this binary contains exactly one test, so nothing else in the process can be
    // reading the environment concurrently.
    unsafe {
        std::env::set_var("HOME", home.path());
    }
    #[cfg(target_os = "linux")]
    // SAFETY: this binary contains exactly one test, so nothing else in the process can be
    // reading the environment concurrently.
    unsafe {
        std::env::set_var("XDG_CACHE_HOME", &cache_root);
    }
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
    assert_ne!(
        log::max_level(),
        log::LevelFilter::Off,
        "--log-level error must install a logger, which raises the global maximum level"
    );
    assert!(
        app_cache.join("crates").exists(),
        "the dump must be cached under the platform cache directory"
    );
}
