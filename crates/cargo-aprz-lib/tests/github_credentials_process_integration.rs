// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! End-to-end coverage for GitHub CLI credential discovery.
//!
//! This binary deliberately contains one test because it temporarily changes
//! process-global environment variables used by executable discovery.

#![cfg(not(miri))]

mod support;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::Utc;
use serde_json::json;
use support::dump::Dump;
use support::{TestHost, dump_server, dump_url, failing_server, seed_advisory_db};
use url::Url;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DISCOVERED_TOKEN: &str = "integration-discovered-secret";

fn compile_fake_gh(bin: &Path) -> PathBuf {
    const SOURCE: &str = r#"
use std::env;
use std::fs;

fn main() {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let log = format!(
        "inherited-token={}\nargs={}",
        env::var_os("GITHUB_TOKEN").is_some(),
        args.join("|")
    );
    fs::write(env::var_os("FAKE_GH_LOG").expect("log path"), log)
        .expect("write fake gh log");
    println!("integration-discovered-secret");
}
"#;

    std::fs::create_dir_all(bin).expect("creating fake executable directory");
    let source = bin.join("fake-gh.rs");
    std::fs::write(&source, SOURCE).expect("writing fake gh source");
    let executable = bin.join(if cfg!(windows) { "gh.exe" } else { "gh" });
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
    let output = Command::new(rustc)
        .args(["--edition=2024", "-o"])
        .arg(&executable)
        .arg(source)
        .output()
        .expect("rustc is available while running Rust integration tests");
    assert!(
        output.status.success(),
        "compiling fake gh failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    executable
}

#[test]
fn run_discovers_an_enterprise_token_through_the_production_process_path() {
    let temp = tempfile::tempdir().expect("creating test directory");
    let bin = temp.path().join("bin");
    compile_fake_gh(&bin);
    let gh_log = temp.path().join("gh.log");
    let mut command = Command::new(std::env::current_exe().expect("the integration test knows its executable"));
    command
        .args([
            "--ignored",
            "--exact",
            "helper_run_discovers_an_enterprise_token_through_the_production_process_path",
            "--nocapture",
        ])
        .env("PATH", bin)
        .env("GITHUB_TOKEN", " \t ")
        .env("FAKE_GH_LOG", &gh_log);
    if cfg!(windows) {
        command.env("PATHEXT", ".COM;.EXE;.BAT;.CMD");
    }

    let output = command.output().expect("start isolated credential scenario");
    assert!(
        output.status.success(),
        "isolated credential scenario failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("production credential path passed"),
        "isolated scenario did not report completion"
    );
    let gh_call = std::fs::read_to_string(gh_log).expect("the production path invokes fake gh");
    assert!(
        gh_call.contains("inherited-token=false"),
        "gh inherited the rejected blank token:\n{gh_call}"
    );
    assert!(
        !gh_call.contains(DISCOVERED_TOKEN),
        "the token appeared in gh arguments or test diagnostics"
    );
}

#[test]
#[ignore = "subprocess fixture for run_discovers_an_enterprise_token_through_the_production_process_path"]
fn helper_run_discovers_an_enterprise_token_through_the_production_process_path() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("create isolated runtime")
        .block_on(run_credential_scenario());
    println!("production credential path passed");
}

async fn run_credential_scenario() {
    let dump = dump_server(Dump::sample(Utc::now()).to_tar_gz(), Some(1)).await;
    let github = MockServer::start().await;
    let services = failing_server(404).await;
    let temp = tempfile::tempdir().expect("creating test directory");
    let cache = temp.path().join("cache");
    seed_advisory_db(&cache);

    for request_path in [
        "/repos/fake-org/schemeless-repo-crate",
        "/repos/fake-org/schemeless-repo-crate/issues",
    ] {
        let body = if request_path.ends_with("/issues") {
            json!([])
        } else {
            json!({
                "stargazers_count": 1,
                "forks_count": 2,
                "subscribers_count": 3,
            })
        };
        Mock::given(method("GET"))
            .and(path(request_path))
            .and(header("authorization", format!("token {DISCOVERED_TOKEN}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&github)
            .await;
    }

    let github_url = github.uri();
    let dump_url = dump_url(&dump);
    let service_url = services.uri();
    let cache = cache.to_string_lossy().into_owned();
    let args = [
        "cargo",
        "aprz",
        "crates",
        "schemeless-repo-crate@0.1.0",
        "--console",
        "--color",
        "never",
        "--cache-dir",
        &cache,
        "--dump-url",
        &dump_url,
        "--docs-url",
        &service_url,
        "--coverage-url",
        &service_url,
        "--github-url",
        &github_url,
        "--github-token-from-gh",
        "--codeberg-url",
        &service_url,
        "--advisory-url",
        &service_url,
    ];
    let mut host = TestHost::new();

    cargo_aprz_lib::run(&mut host, args).await;

    assert!(host.exit_code.is_none(), "the command should succeed: {}", host.error_str());
    let expected_hostname = Url::parse(&github_url)
        .expect("wiremock URI is valid")
        .host_str()
        .expect("wiremock URI has a host")
        .to_owned();
    let gh_call = std::fs::read_to_string(std::env::var_os("FAKE_GH_LOG").expect("fake gh log configured"))
        .expect("the production path invokes fake gh");
    assert!(
        gh_call.contains(&format!("args=auth|token|--hostname|{expected_hostname}")),
        "gh did not receive the Enterprise hostname:\n{gh_call}"
    );
    let diagnostics = format!("{}{}", host.output_str(), host.error_str());
    assert!(!diagnostics.contains(DISCOVERED_TOKEN), "diagnostics exposed the discovered token");
}
