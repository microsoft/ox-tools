// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Opt-in system checks of real compiler acceptance and offline fixture buildability.

use std::process::Command;
use std::{env, fs};

use cargo_gamma_lib::testing::write_project;
use tempfile::TempDir;

#[path = "support/child_test.rs"]
mod child_test;
#[path = "support/rustflags.rs"]
mod rustflags;

fn builds_offline(directory: &TempDir) {
    let home = TempDir::new().expect("could not create an empty Cargo home");
    let mut command = Command::new(env!("CARGO"));
    command
        .current_dir(directory.path())
        .env("CARGO_HOME", home.path())
        .args(["test", "--offline", "--no-run", "--target-dir"])
        .arg(directory.path().join("target"));

    let configuration = if directory.path().join(".cargo/config").exists() {
        ".cargo/config"
    } else {
        ".cargo/config.toml"
    };
    let document: toml_edit::DocumentMut = fs::read_to_string(directory.path().join(configuration))
        .expect("write_project creates a Cargo configuration for each buildable fixture")
        .parse()
        .expect("write_project requires valid Cargo configuration");
    if let Some(flags) = document.get("build").and_then(|build| build.get("rustflags")) {
        let flags = flags
            .as_array()
            .expect("these embedded fixtures declare build.rustflags as an array");
        let mut inherited = rustflags::inherited();
        for flag in flags {
            rustflags::append(
                &mut inherited,
                flag.as_str()
                    .expect("these embedded fixtures declare each rustflags entry as a string"),
            );
        }
        command.env("CARGO_ENCODED_RUSTFLAGS", inherited);
    }

    let built = command.output().expect("Cargo must be installed to build this system test suite");
    assert!(
        built.status.success(),
        "the embedded project must build with an empty registry cache:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );
}

#[test]
#[cfg_attr(miri, ignore = "requires provisioned Cargo and rustc subprocesses")]
fn library_and_binary_samples_build_without_a_registry_cache() {
    for (path, source) in [
        ("src/lib.rs", "pub fn accepts(value: u32) -> bool { value >= 1 }\n"),
        ("src/main.rs", "fn main() { assert!(1_u32 >= 1); }\n"),
    ] {
        let directory = TempDir::new().expect("could not create a project directory");
        write_project(directory.path(), &[(path, source)]);
        builds_offline(&directory);
    }
}

#[test]
#[cfg_attr(miri, ignore = "requires provisioned Cargo and rustc subprocesses")]
fn local_package_graphs_and_custom_rustflags_still_build_offline() {
    let directory = TempDir::new().expect("could not create a project directory");
    write_project(
        directory.path(),
        &[
            ("Cargo.toml", "[workspace]\nmembers = [\"subject\", \"oracle\"]\nresolver = \"2\"\n"),
            (
                "subject/Cargo.toml",
                "[package]\nname = \"subject\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
            ),
            (
                "subject/src/lib.rs",
                "#[cfg(not(embedded_fixture))]\ncompile_error!(\"fixture rustflags were lost\");\n\
                 pub fn accepts(value: u32) -> bool { value >= 1 }\n",
            ),
            (
                "oracle/Cargo.toml",
                "[package]\nname = \"oracle\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
                 [dependencies]\nsubject = { path = \"../subject\" }\n",
            ),
            (
                "oracle/src/lib.rs",
                "#[test]\nfn the_boundary_is_pinned() { assert!(subject::accepts(1)); }\n",
            ),
            (
                ".cargo/config.toml",
                "[build]\nrustflags = [\"--cfg\", \"embedded_fixture\", \"--check-cfg\", \"cfg(embedded_fixture)\"]\n",
            ),
        ],
    );
    builds_offline(&directory);
}

#[test]
#[cfg_attr(miri, ignore = "starts a child test harness and provisioned compiler subprocesses")]
fn configured_rustflags_and_inherited_analysis_flags_both_reach_the_compiler() {
    const CHILD: &str = "GAMMA_PROJECT_RUSTFLAGS_CHILD";
    if env::var_os(CHILD).is_none() {
        let mut inherited = rustflags::inherited();
        for flag in ["--cfg", "inherited_fixture", "--check-cfg", "cfg(inherited_fixture)"] {
            rustflags::append(&mut inherited, flag);
        }
        let output = Command::new(env::current_exe().expect("the running test executable must be locatable for its child process"))
            .args([
                "--exact",
                "configured_rustflags_and_inherited_analysis_flags_both_reach_the_compiler",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("CARGO_ENCODED_RUSTFLAGS", inherited)
            .output()
            .expect("the running test executable must be runnable as a child process");
        assert!(
            output.status.success() && child_test::passed_exactly_one(&output.stdout),
            "the child harness must execute and pass exactly one compiler-flag test:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let directory = TempDir::new().expect("could not create a project directory");
    write_project(
        directory.path(),
        &[
            (
                "src/lib.rs",
                "#[cfg(not(all(embedded_fixture, inherited_fixture)))]\n\
                 compile_error!(\"both fixture and inherited flags must reach rustc\");\n",
            ),
            (
                ".cargo/config.toml",
                "[build]\nrustflags = [\"--cfg\", \"embedded_fixture\", \"--check-cfg\", \"cfg(embedded_fixture)\"]\n",
            ),
        ],
    );
    builds_offline(&directory);
}
