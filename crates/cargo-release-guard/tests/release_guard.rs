// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Offline Cargo source-resolution and release-selection regression fixtures.

#![expect(clippy::unwrap_used, reason = "Fixture setup errors must fail the test immediately")]

use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

struct Fixture {
    directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        // Fixtures must not grow beneath W_b when the guard rehearses its own release.
        let directory = tempfile::Builder::new().prefix("case-").tempdir().unwrap();
        let fixture = Self { directory };
        fixture.write(".gitignore", "target/\nartifacts/\nregistry/\nregistries/\n");
        fixture.write("Cargo.toml", "[workspace]\nresolver='2'\nmembers=['crates/*']\n");
        fs::create_dir_all(fixture.root().join("registry")).unwrap();
        let mut config = toml_edit::DocumentMut::new();
        config["source"]["crates-io"]["replace-with"] = toml_edit::value("fixture");
        config["source"]["fixture"]["directory"] = toml_edit::value(fixture.root().join("registry").to_string_lossy().as_ref());
        fixture.write(".cargo\\config.toml", &config.to_string());
        fixture.git(&["init", "--quiet"]);
        fixture.package("helper", "1.0.0", "", "pub fn released() -> u32 { 1 }\npub struct Token;");
        fixture.package(
            "consumer",
            "1.0.0",
            "[dependencies]\nhelper={path='../helper', version='1'}\n",
            "pub fn value() -> u32 { helper::released() }",
        );
        fixture.publish("helper", "1.0.0", "", "pub fn released() -> u32 { 1 }\npub struct Token;");
        fixture.commit();
        fixture
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.root().join(path.replace('\\', std::path::MAIN_SEPARATOR_STR));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn package(&self, name: &str, version: &str, extra: &str, source: &str) {
        self.write(
            &format!("crates\\{name}\\Cargo.toml"),
            &format!("[package]\nname='{name}'\nversion='{version}'\nedition='2021'\n{extra}"),
        );
        self.write(&format!("crates\\{name}\\src\\lib.rs"), source);
    }

    fn publish(&self, name: &str, version: &str, extra: &str, source: &str) {
        self.publish_to("registry", name, version, extra, source);
    }

    fn publish_to(&self, registry: &str, name: &str, version: &str, extra: &str, source: &str) {
        let manifest = format!("[package]\nname='{name}'\nversion='{version}'\nedition='2021'\n{extra}");
        let directory = self.root().join(registry).join(format!("{name}-{version}"));
        fs::create_dir_all(directory.join("src")).unwrap();
        fs::write(directory.join("Cargo.toml"), &manifest).unwrap();
        fs::write(directory.join("src").join("lib.rs"), source).unwrap();
        let hash = |bytes: &[u8]| {
            let mut text = String::with_capacity(64);
            for byte in Sha256::digest(bytes) {
                write!(text, "{byte:02x}").unwrap();
            }
            text
        };
        let checksum = json!({"files": {"Cargo.toml": hash(manifest.as_bytes()), "src/lib.rs": hash(source.as_bytes())}, "package": null});
        fs::write(directory.join(".cargo-checksum.json"), serde_json::to_vec(&checksum).unwrap()).unwrap();
    }

    fn named_registry(&self, name: &str) {
        let directory = self.root().join("registries").join(name);
        fs::create_dir_all(&directory).unwrap();
        let mut config: toml_edit::DocumentMut = fs::read_to_string(self.root().join(".cargo").join("config.toml"))
            .unwrap()
            .parse()
            .unwrap();
        let index = format!("sparse+https://{name}.example.invalid/index/");
        config["registries"][name]["index"] = toml_edit::value(&index);
        config["source"][name]["registry"] = toml_edit::value(index);
        config["source"][name]["replace-with"] = toml_edit::value(format!("{name}-vendor"));
        config["source"][&format!("{name}-vendor")]["directory"] = toml_edit::value(directory.to_string_lossy().as_ref());
        self.write(".cargo\\config.toml", &config.to_string());
    }

    fn git(&self, args: &[&str]) {
        let output = Command::new("git").current_dir(self.root()).args(args).output().unwrap();
        assert_success(&output);
    }

    fn commit(&self) {
        self.git(&["add", "."]);
        self.git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture base",
        ]);
    }

    fn commit_historical_symlink(&self, path: &str, target: &str) {
        self.write(path, target);
        let object = Command::new("git")
            .current_dir(self.root())
            .args(["hash-object", "-w", "--", path])
            .output()
            .unwrap();
        assert_success(&object);
        let object = String::from_utf8(object.stdout).unwrap();
        self.git(&["update-index", "--add", "--cacheinfo", &format!("120000,{},{path}", object.trim())]);
        self.git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "historical fixture link",
        ]);
    }

    fn cargo(&self, args: &[&str]) -> Output {
        Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .current_dir(self.root())
            .args(args)
            .arg("--offline")
            .env("CARGO_TERM_COLOR", "never")
            .env("CARGO_TARGET_DIR", self.root().join("target"))
            .output()
            .unwrap()
    }

    fn guard(&self, operation: &str, output: &str, extra: &[&str]) -> Output {
        self.guard_at(operation, &self.artifact(output), extra)
    }

    fn guard_at(&self, operation: &str, output: &Path, extra: &[&str]) -> Output {
        self.guard_command(operation, output, extra).output().unwrap()
    }

    fn guard_command(&self, operation: &str, output: &Path, extra: &[&str]) -> Command {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cargo-release-guard"));
        command
            .current_dir(self.root())
            .arg("release-guard")
            .arg(operation)
            .arg("--manifest-path")
            .arg(self.root())
            .arg("--output-dir")
            .arg(output)
            .arg("--offline");
        if !extra.contains(&"--candidate-report") {
            command.args(["--base", "HEAD"]);
        }
        command.args(extra);
        command
    }

    fn artifact(&self, name: &str) -> PathBuf {
        self.root().join("artifacts").join(name)
    }

    fn report(&self, name: &str) -> Value {
        serde_json::from_slice(&fs::read(self.artifact(name).join("report.json")).unwrap()).unwrap()
    }

    fn bump_consumer(&self, source: &str) {
        self.package(
            "consumer",
            "1.1.0",
            "[dependencies]\nhelper={path='../helper', version='1'}\n",
            source,
        );
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_failure(output: &Output, diagnostic: &str) {
    assert!(
        !output.status.success(),
        "unexpected success: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(diagnostic),
        "expected {diagnostic:?} in:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn create_symlink(target: &Path, link: &Path, directory: bool) {
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    #[cfg(windows)]
    if directory {
        std::os::windows::fs::symlink_dir(target, link).unwrap();
    } else {
        std::os::windows::fs::symlink_file(target, link).unwrap();
    }
    #[cfg(unix)]
    {
        let _ = directory;
        std::os::unix::fs::symlink(target, link).unwrap();
    }
}

#[test]
fn unbumped_helper_passes_development_but_fails_publication_until_selected() {
    let fixture = Fixture::new();
    fixture.package(
        "helper",
        "1.0.0",
        "",
        "pub fn released() -> u32 { 1 }\npub fn new_api() -> u32 { 2 }",
    );
    fixture.bump_consumer("pub fn value() -> u32 { helper::new_api() }");
    assert_success(&fixture.cargo(&["test", "--workspace"]));
    assert_failure(&fixture.guard("check", "consumer-only", &[]), "new_api");
    let report = fixture.report("consumer-only");
    assert_eq!(report["candidates"].as_array().unwrap().len(), 1);
    assert_eq!(report["candidates"][0]["name"], "consumer");
    assert_eq!(
        report["commands"].as_array().unwrap().last().unwrap()["phase"],
        "production:consumer"
    );
    assert!(
        !fixture
            .artifact("consumer-only")
            .join("workspace")
            .join("crates")
            .join("helper")
            .exists()
    );

    fixture.package(
        "helper",
        "1.1.0",
        "",
        "pub fn released() -> u32 { 1 }\npub fn new_api() -> u32 { 2 }",
    );
    assert_success(&fixture.guard("check", "with-helper", &[]));
    assert_eq!(fixture.report("with-helper")["status"], "passed");
}

#[test]
fn no_candidates_is_an_explicit_no_release_without_registry_work() {
    let fixture = Fixture::new();
    fixture.write("crates\\helper\\src\\lib.rs", "pub fn released() -> u32 { 99 }");
    assert_success(&fixture.guard("check", "none", &[]));
    let report = fixture.report("none");
    assert_eq!(report["status"], "no_release");
    assert!(report["commands"].as_array().unwrap().is_empty());
    assert!(!fixture.artifact("none").join("workspace").exists());
}

#[test]
fn override_replaces_inference_and_dirty_source_does_not_expand_selection() {
    let fixture = Fixture::new();
    fixture.package("helper", "1.1.0", "", "pub fn released() -> u32 { 2 }");
    fixture.bump_consumer("pub fn value() -> u32 { helper::released() }");
    fixture.write("selection.json", "[\"consumer\"]");
    assert_success(&fixture.guard("candidates", "explicit", &["--candidate-list", "selection.json"]));
    assert_eq!(fixture.report("explicit")["candidates"].as_array().unwrap().len(), 1);
    fixture.write("selection.json", "[]");
    assert_success(&fixture.guard("check", "explicit-empty", &["--candidate-list", "selection.json"]));
    assert_eq!(fixture.report("explicit-empty")["status"], "no_release");
}

#[test]
fn inherited_versions_and_newly_publishable_packages_are_selected() {
    let fixture = Fixture::new();
    fixture.write(
        "Cargo.toml",
        "[workspace]\nresolver='2'\nmembers=['crates/*']\n[workspace.package]\nversion='1.0.0'\n",
    );
    fixture.write(
        "crates\\helper\\Cargo.toml",
        "[package]\nname='helper'\nversion.workspace=true\nedition='2021'\n",
    );
    fixture.package("private", "0.1.0", "publish=false\n", "pub fn support() {}");
    fixture.commit();
    fixture.write(
        "Cargo.toml",
        "[workspace]\nresolver='2'\nmembers=['crates/*']\n[workspace.package]\nversion='1.1.0'\n",
    );
    fixture.package("private", "0.1.0", "", "pub fn support() {}");
    fixture.package("new", "0.1.0", "", "pub fn new() {}");
    assert_success(&fixture.guard("candidates", "inherited", &[]));
    let candidates = fixture.report("inherited")["candidates"].as_array().unwrap().clone();
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate["name"] == "helper" && candidate["reason"] == "version_advance")
    );
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate["name"] == "private" && candidate["reason"] == "newly_publishable")
    );
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate["name"] == "new" && candidate["reason"] == "new_package")
    );
}

#[test]
fn snapshot_mismatch_and_nonempty_output_are_rejected_without_cleanup() {
    let fixture = Fixture::new();
    fixture.bump_consumer("pub fn value() -> u32 { helper::released() }");
    assert_success(&fixture.guard("candidates", "selection", &[]));
    let report = fixture.artifact("selection").join("candidates.json");
    assert_success(&fixture.guard("prepare", "prepared", &["--candidate-report", report.to_str().unwrap()]));
    let marker = fixture.artifact("prepared").join("keep.txt");
    fs::write(&marker, "user-owned").unwrap();
    assert_failure(
        &fixture.guard("prepare", "prepared", &["--candidate-report", report.to_str().unwrap()]),
        "not empty",
    );
    assert_eq!(fs::read_to_string(marker).unwrap(), "user-owned");
    fixture.write("crates\\consumer\\src\\lib.rs", "pub fn changed() {}");
    assert_failure(
        &fixture.guard("prepare", "stale", &["--candidate-report", report.to_str().unwrap()]),
        "snapshot mismatch",
    );
}

#[test]
fn same_version_registry_identity_cannot_be_replaced_by_local_source() {
    let fixture = Fixture::new();
    fixture.write("selection.json", "[\"helper\"]");
    fixture.write("crates\\helper\\src\\lib.rs", "pub fn unpublished() {}");
    assert_failure(
        &fixture.guard("check", "published", &["--candidate-list", "selection.json"]),
        "already published",
    );
    assert_eq!(fixture.report("published")["registry_probes"][0]["state"], "published");
}

#[test]
fn registry_loading_errors_are_not_absence() {
    let fixture = Fixture::new();
    fixture.bump_consumer("pub fn value() -> u32 { helper::released() }");
    fixture.write(
        ".cargo\\config.toml",
        "[source.crates-io]\nreplace-with='broken'\n[source.broken]\ndirectory='not-a-real-directory'\n",
    );
    assert_failure(&fixture.guard("check", "registry-error", &[]), "not evidence of absence");
    assert_eq!(fixture.report("registry-error")["registry_probes"][0]["state"], "registry_error");
}

#[test]
fn private_production_dependencies_are_rejected_but_test_support_is_retained() {
    let fixture = Fixture::new();
    fixture.package("support", "0.1.0", "publish=false\n", "pub fn support() {}");
    fixture.package(
        "consumer",
        "1.1.0",
        "[dependencies]\nsupport={path='../support'}\n",
        "pub fn value() { support::support() }",
    );
    assert_failure(
        &fixture.guard("check", "production-private", &[]),
        "unpublished production dependency",
    );
    fixture.package(
        "consumer",
        "1.1.0",
        "[dev-dependencies]\nsupport={path='../support'}\n",
        "#[test] fn test_support() { support::support() }",
    );
    assert_success(&fixture.guard("check", "test-support", &[]));
}

#[test]
fn private_support_and_dev_edges_resolve_omitted_helper_from_registry() {
    let fixture = Fixture::new();
    fixture.package(
        "helper",
        "1.0.0",
        "",
        "pub fn released() -> u32 { 1 }\npub fn new_api() -> u32 { 2 }",
    );
    fixture.package(
        "consumer_tests",
        "0.0.0",
        "publish=false\n[dependencies]\nhelper={path='../helper', version='1'}\n",
        "#[test] fn maintained_probe() { assert_eq!(helper::new_api(), 2); }",
    );
    fixture.bump_consumer("pub fn value() -> u32 { helper::released() }");
    assert_failure(&fixture.guard("check", "support-leak", &[]), "new_api");
    assert_eq!(
        fixture.report("support-leak")["commands"].as_array().unwrap().last().unwrap()["phase"],
        "member_tests"
    );

    fixture.package(
        "consumer_tests",
        "0.0.0",
        "publish=false\n",
        "#[test] fn private_test() { assert_eq!(2 + 2, 4); }",
    );
    fixture.package(
        "consumer",
        "1.1.0",
        "[dev-dependencies]\nhelper={path='../helper'}\n",
        "#[test] fn direct_dev() { helper::new_api(); }",
    );
    assert_failure(&fixture.guard("check", "dev-leak", &[]), "new_api");
    assert_eq!(fixture.report("dev-leak")["derived_requirements"].as_array().unwrap().len(), 1);
}

#[test]
fn candidate_patch_unifies_compatible_types_and_preserves_incompatible_registry_baseline() {
    let fixture = Fixture::new();
    fixture.package("helper", "1.1.0", "", "pub struct Token;\npub fn released() -> u32 { 1 }");
    fixture.bump_consumer("pub fn token() -> helper::Token { helper::Token }");
    fixture.package(
        "consumer_tests",
        "0.0.0",
        "publish=false\n[dependencies]\nconsumer={path='../consumer', version='1'}\nhelper='1'\n",
        "#[test] fn compatible() { let _: helper::Token = consumer::token(); }",
    );
    assert_success(&fixture.guard("check", "unified", &[]));
    let provenance = fixture.report("unified")["provenance"].as_array().unwrap().clone();
    assert_eq!(provenance.iter().filter(|entry| entry["name"] == "helper").count(), 1);

    fixture.package("helper", "2.0.0", "", "pub struct Token;\npub fn released() -> u32 { 1 }");
    fixture.package(
        "consumer",
        "1.1.0",
        "[dependencies]\nhelper={path='../helper', version='2'}\n",
        "pub fn token() -> helper::Token { helper::Token }",
    );
    assert_failure(&fixture.guard("check", "incompatible", &[]), "mismatched types");
    let provenance = fixture.report("incompatible")["provenance"].as_array().unwrap().clone();
    assert!(
        provenance
            .iter()
            .any(|entry| entry["name"] == "helper" && entry["version"] == "1.0.0" && entry["source"].is_string())
    );
    assert!(
        provenance
            .iter()
            .any(|entry| entry["name"] == "helper" && entry["version"] == "2.0.0" && entry["source"].is_null())
    );
}

#[test]
fn root_patches_cannot_smuggle_unbumped_sources_into_publication() {
    let fixture = Fixture::new();
    fixture.write(
        "Cargo.toml",
        "[workspace]\nresolver='2'\nmembers=['crates/*']\n[patch.crates-io]\nhelper={path='crates/helper'}\n",
    );
    fixture.package("helper", "1.0.0", "", "pub fn new_api() {}");
    fixture.package(
        "consumer",
        "1.1.0",
        "[dependencies]\nhelper='1'\n",
        "pub fn value() { helper::new_api() }",
    );
    assert_success(&fixture.cargo(&["build", "-p", "consumer"]));
    assert_failure(&fixture.guard("check", "root-patch", &[]), "new_api");
}

#[test]
fn separate_production_build_does_not_get_dev_dependency_features() {
    let fixture = Fixture::new();
    fixture.publish(
        "helper",
        "1.0.0",
        "[features]\ntest-api=[]\n",
        "#[cfg(feature=\"test-api\")] pub fn test_only() {}",
    );
    fixture.package(
        "consumer",
        "1.1.0",
        "[dependencies]\nhelper='1'\n[dev-dependencies]\nhelper={version='1', features=['test-api']}\n",
        "pub fn value() { helper::test_only() }\n#[test] fn works_with_dev_feature() { value(); }",
    );
    assert_success(&fixture.cargo(&["test", "-p", "consumer"]));
    assert_failure(&fixture.guard("check", "dev-features", &[]), "test_only");
    assert_eq!(
        fixture.report("dev-features")["commands"].as_array().unwrap().last().unwrap()["phase"],
        "production:consumer"
    );
}

#[test]
fn unsupported_config_path_overrides_fail_explicitly() {
    let fixture = Fixture::new();
    fixture.write(".cargo\\config.toml", "paths=['crates/helper']\n");
    assert_failure(&fixture.guard("check", "unsafe-config", &[]), "unsupported Cargo config `paths`");
}

#[test]
fn stale_lock_and_higher_registry_version_do_not_replace_a_compatible_candidate() {
    let fixture = Fixture::new();
    fixture.publish("helper", "1.2.0", "", "pub struct Token;\npub fn released() -> u32 { 12 }");
    fixture.package("helper", "1.1.0", "", "pub struct Token;\npub fn released() -> u32 { 11 }");
    fixture.bump_consumer("pub fn token() -> helper::Token { helper::Token }");
    fixture.package(
        "consumer_tests",
        "0.0.0",
        "publish=false\n[dependencies]\nconsumer={path='../consumer', version='1'}\nhelper='1'\n",
        "#[test] fn compatible() { let _: helper::Token = consumer::token(); assert_eq!(helper::released(), 11); }",
    );
    assert_success(&fixture.cargo(&["generate-lockfile"]));
    assert_success(&fixture.guard("check", "higher-registry", &[]));
    let provenance = fixture.report("higher-registry")["provenance"].as_array().unwrap().clone();
    assert!(
        provenance
            .iter()
            .filter(|entry| entry["name"] == "helper")
            .all(|entry| entry["source"].is_null())
    );
}

#[test]
fn equal_versions_from_path_and_registry_have_different_nominal_type_identity() {
    let fixture = Fixture::new();
    fixture.bump_consumer("pub fn token() -> helper::Token { helper::Token }");
    fixture.package(
        "consumer_tests",
        "0.0.0",
        "publish=false\n[dependencies]\nconsumer={path='../consumer', version='1'}\nhelper='1'\n",
        "#[test] fn nominal_identity() { let _: helper::Token = consumer::token(); }",
    );
    assert_failure(&fixture.cargo(&["test", "--workspace"]), "mismatched types");
    assert_success(&fixture.guard("check", "registry-identity", &[]));
}

#[test]
fn inherited_manifest_data_lints_targets_and_dependency_features_are_materialized() {
    let fixture = Fixture::new();
    fixture.write("README.md", "Workspace package data.");
    fixture.write("Cargo.toml", "[workspace]\nresolver='2'\nmembers=['crates/*']\n[workspace.package]\nversion='1.1.0'\nreadme='README.md'\n[workspace.dependencies]\nhelper={path='crates/helper', version='1', features=['extra']}\n[workspace.lints.rust]\nunsafe_code='forbid'\n[profile.test]\nopt-level=1\n");
    fixture.publish(
        "helper",
        "1.0.0",
        "[features]\nextra=[]\n",
        "#[cfg(feature=\"extra\")] pub fn released() -> u32 { 1 }",
    );
    fixture.package("helper", "1.0.0", "[features]\nextra=[]\n", "pub fn released() -> u32 { 1 }");
    fixture.write("crates\\consumer\\Cargo.toml", "[package]\nname='consumer'\nversion.workspace=true\nedition='2021'\nreadme.workspace=true\n[dependencies]\nhelper.workspace=true\n[lints]\nworkspace=true\n[[test]]\nname='custom'\npath='tests/custom.rs'\n");
    fixture.write(
        "crates\\consumer\\tests\\custom.rs",
        "#[test] fn target_retained() { assert_eq!(consumer::value(), 1); }",
    );
    assert_success(&fixture.guard("check", "inherited-materialization", &[]));
    let document: toml_edit::DocumentMut = fs::read_to_string(
        fixture
            .artifact("inherited-materialization")
            .join("workspace")
            .join("crates")
            .join("consumer")
            .join("Cargo.toml"),
    )
    .unwrap()
    .parse()
    .unwrap();
    assert_eq!(document["package"]["version"].as_str(), Some("1.1.0"));
    assert_eq!(document["lints"]["rust"]["unsafe_code"].as_str(), Some("forbid"));
}

#[test]
fn version_regressions_invalid_overrides_and_offline_unknown_indexes_fail() {
    let fixture = Fixture::new();
    fixture.package("helper", "0.9.0", "", "pub fn released() -> u32 { 1 }");
    assert_failure(&fixture.guard("candidates", "regression", &[]), "version regression");
    fixture.package("helper", "1.0.0", "", "pub fn released() -> u32 { 1 }");
    fixture.write("selection.json", "[\"unknown\"]");
    assert_failure(
        &fixture.guard("candidates", "unknown", &["--candidate-list", "selection.json"]),
        "unknown explicit",
    );
    fixture.write("selection.json", "[\"helper\",\"helper\"]");
    assert_failure(
        &fixture.guard("candidates", "duplicate", &["--candidate-list", "selection.json"]),
        "duplicate",
    );
    fixture.bump_consumer("pub fn value() -> u32 { helper::released() }");
    fixture.write(".cargo\\config.toml", "");
    assert_failure(
        &fixture.guard("check", "unknown-cache", &[]),
        "offline registry cache cannot establish absence",
    );
}

#[test]
fn build_and_target_dev_dependencies_cannot_use_omitted_source() {
    let fixture = Fixture::new();
    fixture.package("helper", "1.0.0", "", "pub fn released() -> u32 { 1 }\npub fn new_api() {}");
    fixture.package(
        "consumer",
        "1.1.0",
        "build='build.rs'\n[build-dependencies]\nhelper={path='../helper', version='1'}\n",
        "pub fn value() {}",
    );
    fixture.write("crates\\consumer\\build.rs", "fn main() { helper::new_api(); }");
    assert_failure(&fixture.guard("check", "build-leak", &[]), "new_api");
    fs::remove_file(fixture.root().join("crates").join("consumer").join("build.rs")).unwrap();
    fixture.package(
        "consumer",
        "1.1.0",
        "[target.'cfg(any(unix,windows))'.dev-dependencies]\nhelper={path='../helper'}\n",
        "#[test] fn target_dev() { helper::new_api(); }",
    );
    assert_failure(&fixture.guard("check", "target-dev-leak", &[]), "new_api");
}

#[test]
fn approved_named_registry_is_used_for_probes_patches_and_dependencies() {
    let fixture = Fixture::new();
    let mut config: toml_edit::DocumentMut = fs::read_to_string(fixture.root().join(".cargo").join("config.toml"))
        .unwrap()
        .parse()
        .unwrap();
    config["registries"]["internal"]["index"] = toml_edit::value("sparse+https://registry.example.invalid/index/");
    config["source"]["internal"]["registry"] = toml_edit::value("sparse+https://registry.example.invalid/index/");
    config["source"]["internal"]["replace-with"] = toml_edit::value("fixture");
    fixture.write(".cargo\\config.toml", &config.to_string());
    fixture.package("helper", "1.1.0", "publish=['internal']\n", "pub fn released() -> u32 { 1 }");
    fixture.package(
        "consumer",
        "1.1.0",
        "publish=['internal']\n[dependencies]\nhelper={path='../helper', version='1'}\n",
        "pub fn value() -> u32 { helper::released() }",
    );
    assert_success(&fixture.guard("check", "private-registry", &[]));
    let report = fixture.report("private-registry");
    assert!(
        report["registry_probes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|probe| probe["registry"] == "internal")
    );
}

#[test]
fn candidate_path_does_not_ignore_incompatible_declared_requirement() {
    let fixture = Fixture::new();
    fixture.package("helper", "2.0.0", "", "pub fn only_new() {}");
    fixture.bump_consumer("pub fn value() -> u32 { helper::released() }");
    assert_success(&fixture.guard("check", "honest-requirement", &[]));
    let report = fixture.report("honest-requirement");
    assert!(
        report["provenance"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == "helper" && entry["version"] == "1.0.0" && entry["source"].is_string())
    );
}

#[test]
fn versionless_production_and_private_explicit_selection_fail() {
    let fixture = Fixture::new();
    fixture.package(
        "consumer",
        "1.1.0",
        "[dependencies]\nhelper={path='../helper'}\n",
        "pub fn value() -> u32 { helper::released() }",
    );
    assert_failure(&fixture.guard("check", "versionless", &[]), "versionless production");
    fixture.package("support", "0.0.0", "publish=false\n", "pub fn support() {}");
    fixture.write("selection.json", "[\"support\"]");
    assert_failure(
        &fixture.guard("candidates", "private-explicit", &["--candidate-list", "selection.json"]),
        "private",
    );
}

#[test]
fn missing_transitive_registry_package_is_not_candidate_absence() {
    let fixture = Fixture::new();
    fixture.bump_consumer("pub fn value() -> u32 { helper::released() }");
    fixture.publish("consumer", "1.1.0", "[dependencies]\nmissing_transitive='1'\n", "pub fn value() {}");
    assert_failure(&fixture.guard("check", "missing-transitive", &[]), "not evidence of absence");
    assert_eq!(
        fixture.report("missing-transitive")["registry_probes"][0]["state"],
        "registry_error"
    );
}

#[test]
fn build_output_overrides_inherited_target_directory_without_verbatim_paths() {
    let fixture = Fixture::new();
    fixture.bump_consumer("pub fn value() -> u32 { helper::released() }");
    fixture.write(
        "crates\\consumer\\build.rs",
        r#"fn main() {
    assert_eq!(std::env::var("CARGO_TARGET_DIR").unwrap(), "target");
}"#,
    );
    let inherited = fixture.root().join("inherited-target");
    let output = fixture
        .guard_command("check", &fixture.artifact("native-path"), &[])
        .env("CARGO_TARGET_DIR", &inherited)
        .output()
        .unwrap();
    assert_success(&output);
    assert!(!inherited.exists());
    assert!(fixture.artifact("native-path").join("workspace").join("target").is_dir());
    for command in fixture.report("native-path")["commands"].as_array().unwrap() {
        let directory = Path::new(command["working_directory"].as_str().unwrap());
        assert_eq!(Path::new(command["target_directory"].as_str().unwrap()), directory.join("target"));
    }
}

#[test]
fn private_test_failures_are_not_silently_skipped_and_feature_modes_run_explicitly() {
    let fixture = Fixture::new();
    fixture.package(
        "consumer",
        "1.1.0",
        "[features]\ndefault=['normal']\nnormal=[]\n",
        "#[test] fn mode() { assert!(cfg!(feature=\"normal\")); }",
    );
    assert_failure(
        &fixture.guard(
            "check",
            "feature-matrix",
            &["--feature-mode", "default", "--feature-mode", "no-default"],
        ),
        "member_tests failed",
    );
    let report = fixture.report("feature-matrix");
    assert!(report["commands"].as_array().unwrap().iter().any(|command| {
        command["arguments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|argument| argument == "--no-default-features")
    }));
}

#[test]
fn external_output_preserves_config_relative_registry_paths_without_copying_credentials() {
    let fixture = Fixture::new();
    let output = tempfile::Builder::new()
        .prefix("case-output-")
        .tempdir_in(fixture.root().parent().unwrap())
        .unwrap();
    fixture.write(
        ".cargo\\config.toml",
        "[source.crates-io]\nreplace-with='fixture'\n[source.fixture]\ndirectory='registry'\n",
    );
    fixture.bump_consumer("pub fn value() -> u32 { helper::released() }");
    assert_success(&fixture.guard_at("check", output.path(), &[]));
    assert!(!output.path().join("workspace").join(".cargo").exists());
    assert!(!output.path().join("base").join(".cargo").exists());
}

#[test]
fn private_test_support_can_have_versionless_facade_edges_with_explicit_derivation() {
    let fixture = Fixture::new();
    fixture.bump_consumer("pub fn value() -> u32 { helper::released() }");
    fixture.package(
        "consumer_tests",
        "0.0.0",
        "publish=false\n[dependencies]\nconsumer={path='../consumer'}\nhelper={path='../helper'}\n",
        "#[test] fn support_edges() { assert_eq!(consumer::value(), helper::released()); }",
    );
    assert_success(&fixture.guard("check", "private-versionless", &[]));
    assert_eq!(
        fixture.report("private-versionless")["derived_requirements"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn target_paths_cannot_reach_omitted_packages_and_reports_reject_config_drift() {
    let fixture = Fixture::new();
    fixture.package("consumer", "1.1.0", "[lib]\npath='../helper/src/lib.rs'\n", "pub fn value() {}");
    assert_failure(&fixture.guard("check", "escaping-target", &[]), "escapes retained source");
    fixture.bump_consumer("pub fn value() -> u32 { helper::released() }");
    assert_success(&fixture.guard("candidates", "before-config-change", &[]));
    let report = fixture.artifact("before-config-change").join("candidates.json");
    let config_path = fixture.root().join(".cargo").join("config.toml");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str("\n[net]\nretry=4\n");
    fs::write(config_path, config).unwrap();
    assert_failure(
        &fixture.guard("prepare", "config-changed", &["--candidate-report", report.to_str().unwrap()]),
        "configuration snapshot mismatch",
    );
}

#[test]
fn relative_manifest_and_fresh_nested_output_work_from_workspace_directory() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.root().join("target")).unwrap();
    let output_directory = Path::new("target").join("anvil").join("release").join("selection-relative");
    let output = Command::new(assert_cmd::cargo::cargo_bin!("cargo-release-guard"))
        .current_dir(fixture.root())
        .args(["candidates", "--base", "HEAD", "--manifest-path", "Cargo.toml", "--output-dir"])
        .arg(&output_directory)
        .output()
        .unwrap();
    assert_success(&output);
    let report: Value = serde_json::from_slice(&fs::read(fixture.root().join(output_directory).join("report.json")).unwrap()).unwrap();
    assert_eq!(report["status"], "no_release");
    fixture.bump_consumer("pub fn value() -> u32 { helper::released() }");
    let output_directory = Path::new("target").join("anvil").join("release").join("check-relative");
    let output = Command::new(assert_cmd::cargo::cargo_bin!("cargo-release-guard"))
        .current_dir(fixture.root())
        .args(["check", "--base", "HEAD", "--manifest-path", ".", "--offline", "--output-dir"])
        .arg(&output_directory)
        .output()
        .unwrap();
    assert_success(&output);
    let report: Value = serde_json::from_slice(&fs::read(fixture.root().join(output_directory).join("report.json")).unwrap()).unwrap();
    assert_eq!(report["status"], "passed");
}

#[test]
fn filesystem_failures_identify_the_operation_and_path() {
    let fixture = Fixture::new();
    fixture.write("blocked-output", "preserve this file");
    let blocked = Path::new("blocked-output").join("nested");
    let output = Command::new(assert_cmd::cargo::cargo_bin!("cargo-release-guard"))
        .current_dir(fixture.root())
        .args(["candidates", "--base", "HEAD", "--manifest-path", "Cargo.toml", "--output-dir"])
        .arg(&blocked)
        .output()
        .unwrap();
    assert_failure(&output, "cannot create output directory");
    assert!(String::from_utf8_lossy(&output.stderr).contains(blocked.to_str().unwrap()));
    assert_eq!(
        fs::read_to_string(fixture.root().join("blocked-output")).unwrap(),
        "preserve this file"
    );

    let missing = Path::new("missing-package").join("Cargo.toml");
    let output = Command::new(assert_cmd::cargo::cargo_bin!("cargo-release-guard"))
        .current_dir(fixture.root())
        .args(["candidates", "--base", "HEAD", "--manifest-path"])
        .arg(&missing)
        .arg("--output-dir")
        .arg(Path::new("target").join("missing-manifest"))
        .output()
        .unwrap();
    assert_failure(&output, "cannot inspect filesystem path");
    assert!(String::from_utf8_lossy(&output.stderr).contains(missing.to_str().unwrap()));
}

#[test]
fn review_same_name_cross_registry_absence_does_not_authorize_a_published_candidate() {
    let fixture = Fixture::new();
    fixture.named_registry("registry_a");
    fixture.named_registry("registry_b");
    fixture.package("same_name", "1.0.0", "publish=['registry_a']\n", "pub fn current_source() {}");
    fixture.publish_to(
        &format!("registries{}registry_a", std::path::MAIN_SEPARATOR),
        "same_name",
        "1.0.0",
        "[dependencies]\nshadow={package='same_name', version='=1.0.0', registry='registry_b'}\n",
        "pub fn published_source() {}",
    );
    assert_failure(&fixture.guard("check", "same-name-cross-registry", &[]), "not evidence of absence");
    assert_eq!(
        fixture.report("same-name-cross-registry")["registry_probes"][0]["state"],
        "registry_error"
    );
    assert!(!fixture.artifact("same-name-cross-registry").join("workspace").exists());

    fixture.publish_to(
        &format!("registries{}registry_b", std::path::MAIN_SEPARATOR),
        "same_name",
        "0.9.0",
        "",
        "pub fn older_version() {}",
    );
    assert_failure(
        &fixture.guard("check", "same-name-cross-registry-version", &[]),
        "not evidence of absence",
    );
    let report = fixture.report("same-name-cross-registry-version");
    assert_eq!(report["registry_probes"][0]["state"], "registry_error");
    assert!(
        report["commands"][0]["diagnostic"]
            .as_str()
            .unwrap()
            .contains("failed to select a version for the requirement `same_name = \"=1.0.0\"`")
    );
    assert!(!fixture.artifact("same-name-cross-registry-version").join("workspace").exists());
}

#[test]
fn review_independent_implicit_registry_baseline_is_not_redirected_to_internal_candidate() {
    let fixture = Fixture::new();
    fixture.named_registry("internal");
    fixture.package("helper", "1.1.0", "publish=['internal']\n", "pub struct Token;");
    fixture.bump_consumer("pub fn token() -> helper::Token { helper::Token }");
    fixture.package(
        "consumer_tests",
        "0.0.0",
        "publish=false\n[dependencies]\nconsumer={path='../consumer', version='1'}\nhelper='1'\n",
        "#[test] fn independent_origin() { let _: helper::Token = consumer::token(); }",
    );
    assert_failure(&fixture.guard("check", "independent-registry", &[]), "mismatched types");
    let report = fixture.report("independent-registry");
    assert!(
        report["provenance"]
            .as_array()
            .unwrap()
            .iter()
            .any(|package| { package["name"] == "helper" && package["version"] == "1.0.0" && package["source"].is_string() })
    );
    let manifest: toml_edit::DocumentMut = fs::read_to_string(
        fixture
            .artifact("independent-registry")
            .join("workspace")
            .join("crates")
            .join("consumer_tests")
            .join("Cargo.toml"),
    )
    .unwrap()
    .parse()
    .unwrap();
    assert!(manifest["dependencies"]["helper"].get("registry").is_none());
}

#[test]
fn review_examples_are_compiled_but_arbitrary_example_and_benchmark_mains_are_not_run() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.root().join("target")).unwrap();
    fixture.package("consumer", "1.1.0",
        "[features]\noptional=[]\n[[test]]\nname='optional_probe'\nrequired-features=['optional']\n[[example]]\nname='example_main'\nharness=false\ntest=true\n[[bench]]\nname='bench_main'\nharness=false\ntest=true\n[[bin]]\nname='production_main'\ntest=false\nharness=false\n",
        "/// ```\n/// assert_eq!(consumer::value(), 1);\n/// ```\npub fn value() -> u32 { 1 }\n#[test] fn ordinary_test() { assert_eq!(value(), 1); }");
    fixture.write(
        "crates\\consumer\\tests\\optional_probe.rs",
        "compile_error!(\"optional test compilation negative control\");",
    );
    for (directory, name) in [
        ("examples", "example_main"),
        ("benches", "bench_main"),
        ("src\\bin", "production_main"),
    ] {
        let marker = fixture.root().join("target").join(format!("{name}-executed"));
        fixture.write(
            &format!("crates\\consumer\\{directory}\\{name}.rs"),
            &format!(
                "fn main() {{ std::fs::write({:?}, \"executed\").unwrap(); panic!(\"arbitrary target main was run\"); }}",
                marker.to_str().unwrap()
            ),
        );
    }
    let source_manifest = fixture.root().join("crates").join("consumer").join("Cargo.toml");
    let original_manifest = fs::read(&source_manifest).unwrap();
    assert_success(&fixture.guard("check", "compile-examples-only", &[]));
    assert_eq!(fs::read(&source_manifest).unwrap(), original_manifest);
    let isolated: toml_edit::DocumentMut = fs::read_to_string(
        fixture
            .artifact("compile-examples-only")
            .join("workspace")
            .join("crates")
            .join("consumer")
            .join("Cargo.toml"),
    )
    .unwrap()
    .parse()
    .unwrap();
    assert_eq!(
        isolated["example"].as_array_of_tables().unwrap().iter().next().unwrap()["test"].as_bool(),
        Some(false)
    );
    assert_eq!(
        isolated["bench"].as_array_of_tables().unwrap().iter().next().unwrap()["test"].as_bool(),
        Some(false)
    );
    for name in ["example_main", "bench_main", "production_main"] {
        assert!(!fixture.root().join("target").join(format!("{name}-executed")).exists());
    }
    let report = fixture.report("compile-examples-only");
    assert!(
        report["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command["phase"] == "example_build")
    );
    assert_eq!(
        report["commands"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|command| command["phase"] == "doc_tests")
            .count(),
        1
    );
    assert!(
        report["commands"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|command| command["phase"] == "member_tests")
            .all(|command| {
                command["arguments"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|argument| argument == "--tests")
            })
    );
    assert!(
        report["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| { command["phase"] == "doc_tests" && command["output"].as_str().unwrap().contains("1 passed") })
    );
    assert_failure(
        &fixture.guard("check", "enabled-optional-test", &["--feature-mode", "all"]),
        "optional test compilation negative control",
    );
    fixture.write(
        "crates\\consumer\\examples\\example_main.rs",
        "compile_error!(\"example compilation negative control\"); fn main() {}",
    );
    assert_failure(
        &fixture.guard("check", "invalid-example", &[]),
        "example compilation negative control",
    );
}

#[test]
fn review_inherited_build_directory_environment_cannot_escape_artifact_isolation() {
    let fixture = Fixture::new();
    fixture.bump_consumer("pub fn value() -> u32 { helper::released() }");
    let escaped = fixture.root().join("escaped-build-output");
    let output = Command::new(assert_cmd::cargo::cargo_bin!("cargo-release-guard"))
        .current_dir(fixture.root())
        .args([
            "check",
            "--base",
            "HEAD",
            "--manifest-path",
            "Cargo.toml",
            "--offline",
            "--output-dir",
        ])
        .arg(fixture.artifact("build-dir-env"))
        .env("CARGO_BUILD_BUILD_DIR", &escaped)
        .output()
        .unwrap();
    assert_failure(&output, "CARGO_BUILD_BUILD_DIR");
    assert!(!escaped.exists());
    assert!(fixture.report("build-dir-env")["commands"].as_array().unwrap().is_empty());
}

#[test]
fn review_inherited_defaults_preserve_cargo_semantics_and_features_remain_additive() {
    for (root_default, member_default, expected) in [
        ("", false, true),
        (", default-features=true", false, true),
        (", default-features=false", true, true),
        (", default-features=false", false, false),
    ] {
        let fixture = Fixture::new();
        fixture.publish("helper", "1.0.0",
            "[features]\ndefault=['defaults']\ndefaults=[]\nroot=[]\nlocal=[]\n",
            "pub const DEFAULTS: bool = cfg!(feature=\"defaults\");\n#[cfg(feature=\"root\")] pub fn root_api() -> u32 { 1 }\n#[cfg(feature=\"local\")] pub fn local_api() -> u32 { 2 }");
        fixture.write("Cargo.toml", &format!("[workspace]\nresolver='2'\nmembers=['crates/*']\n[workspace.dependencies]\nhelper={{version='1', features=['root']{root_default}}}\n"));
        fixture.package("consumer", "1.1.0",
            &format!("[dependencies]\nhelper={{workspace=true, default-features={member_default}, features=['local']}}\n"),
            &format!("pub fn value() -> u32 {{ helper::root_api() + helper::local_api() }}\n#[test] fn inherited_defaults() {{ assert_eq!(value(), 3); assert_eq!(helper::DEFAULTS, {expected}); }}"));
        assert_success(&fixture.cargo(&["test", "--package", "consumer"]));
        assert_success(&fixture.guard("check", "inherited-defaults", &[]));
    }
}

#[test]
fn nextest_handles_zero_test_members_without_running_arbitrary_targets() {
    let fixture = Fixture::new();
    fixture.package("helper", "1.1.0", "", "pub fn no_test_target_cases() {}");
    fixture.package(
        "consumer",
        "1.1.0",
        "[[example]]\nname='example_main'\nharness=false\ntest=true\n[[bench]]\nname='bench_main'\nharness=false\ntest=true\n",
        "#[test] fn ordinary_probe() { assert_eq!(2 + 2, 4); }",
    );
    fixture.write(
        "crates\\consumer\\examples\\example_main.rs",
        "fn main() { panic!(\"example main must not run\"); }",
    );
    fixture.write(
        "crates\\consumer\\benches\\bench_main.rs",
        "fn main() { panic!(\"benchmark main must not run\"); }",
    );
    assert_success(&fixture.guard("check", "nextest-targets", &["--test-runner", "nextest"]));
    let report = fixture.report("nextest-targets");
    assert!(report["commands"].as_array().unwrap().iter().any(|command| {
        command["phase"] == "member_tests"
            && command["arguments"]
                .as_array()
                .unwrap()
                .iter()
                .any(|argument| argument == "--no-tests=pass")
    }));
}

#[test]
fn direct_absence_is_recognized_through_an_authoritative_local_registry_index() {
    let fixture = Fixture::new();
    let registry = fixture.root().join("registries").join("local");
    fs::create_dir_all(registry.join("index").join("co").join("ns")).unwrap();
    let mut config = toml_edit::DocumentMut::new();
    config["source"]["crates-io"]["replace-with"] = toml_edit::value("local");
    config["source"]["local"]["local-registry"] = toml_edit::value(registry.to_string_lossy().as_ref());
    fixture.write(".cargo\\config.toml", &config.to_string());
    fixture.package("consumer", "1.1.0", "", "pub fn candidate() {}");
    assert_success(&fixture.guard("check", "local-registry-missing-name", &[]));
    assert_eq!(
        fixture.report("local-registry-missing-name")["registry_probes"][0]["state"],
        "absent"
    );

    let previous = json!({
        "name": "consumer", "vers": "1.0.0", "deps": [], "cksum": "0".repeat(64),
        "features": {}, "yanked": false
    });
    fs::write(
        registry.join("index").join("co").join("ns").join("consumer"),
        format!("{previous}\n"),
    )
    .unwrap();
    assert_success(&fixture.guard("check", "local-registry-missing-version", &[]));
    assert_eq!(
        fixture.report("local-registry-missing-version")["registry_probes"][0]["state"],
        "absent"
    );
}

#[test]
fn review_followup_opted_in_harnessed_example_tests_are_executed() {
    let fixture = Fixture::new();
    fixture.package(
        "consumer",
        "1.1.0",
        "[[example]]\nname='ordinary_example'\ntest=true\n",
        "#[test] fn library_probe() { assert_eq!(2 + 2, 4); }",
    );
    fixture.write(
        "crates\\consumer\\examples\\ordinary_example.rs",
        "fn main() {}\n#[test] fn must_run() { panic!(\"EXAMPLE_TEST_MUST_RUN\"); }",
    );
    let source_manifest = fixture.root().join("crates").join("consumer").join("Cargo.toml");
    let original_manifest = fs::read(&source_manifest).unwrap();
    assert_failure(&fixture.cargo(&["test", "--workspace"]), "test failed");
    for runner in ["cargo", "nextest"] {
        assert_failure(
            &fixture.guard("check", &format!("harnessed-example-{runner}"), &["--test-runner", runner]),
            "EXAMPLE_TEST_MUST_RUN",
        );
        assert_eq!(fs::read(&source_manifest).unwrap(), original_manifest);
        let isolated: toml_edit::DocumentMut = fs::read_to_string(
            fixture
                .artifact(&format!("harnessed-example-{runner}"))
                .join("workspace")
                .join("crates")
                .join("consumer")
                .join("Cargo.toml"),
        )
        .unwrap()
        .parse()
        .unwrap();
        assert_eq!(
            isolated["example"].as_array_of_tables().unwrap().iter().next().unwrap()["test"].as_bool(),
            Some(true)
        );
    }
}

#[test]
fn review_followup_member_tests_preserve_workspace_unified_features() {
    let fixture = Fixture::new();
    fixture.package("helper", "1.1.0", "[features]\nextra=[]\n",
        "pub fn released() -> u32 { 1 }\n#[cfg(all(test, feature=\"extra\"))] #[test] fn must_run() { panic!(\"WORKSPACE_FEATURE_TEST_MUST_RUN\"); }");
    fixture.package(
        "support",
        "0.0.0",
        "publish=false\n[dependencies]\nhelper={path='../helper', version='1', features=['extra']}\n",
        "#[test] fn support_probe() { assert_eq!(helper::released(), 1); }",
    );
    assert_failure(&fixture.cargo(&["test", "--workspace"]), "test failed");
    for runner in ["cargo", "nextest"] {
        assert_failure(
            &fixture.guard("check", &format!("workspace-features-{runner}"), &["--test-runner", runner]),
            "WORKSPACE_FEATURE_TEST_MUST_RUN",
        );
    }

    fixture.package(
        "helper",
        "1.1.0",
        "[features]\nextra=[]\n[[test]]\nname='feature_target'\nrequired-features=['extra']\n",
        "pub fn released() -> u32 { 1 }",
    );
    fixture.write(
        "crates\\helper\\tests\\feature_target.rs",
        "#[test] fn must_run() { panic!(\"WORKSPACE_REQUIRED_FEATURE_TARGET_MUST_RUN\"); }",
    );
    for runner in ["cargo", "nextest"] {
        assert_failure(
            &fixture.guard("check", &format!("workspace-feature-target-{runner}"), &["--test-runner", runner]),
            "WORKSPACE_REQUIRED_FEATURE_TARGET_MUST_RUN",
        );
    }
    fs::remove_file(fixture.root().join("crates").join("helper").join("tests").join("feature_target.rs")).unwrap();
    fixture.package(
        "helper",
        "1.1.0",
        "[features]\nextra=[]\n",
        "/// ```\n/// assert!(!cfg!(feature=\"extra\"), \"WORKSPACE_DOCTEST_FEATURE_MUST_RUN\");\n/// ```\npub fn released() -> u32 { 1 }",
    );
    for runner in ["cargo", "nextest"] {
        assert_failure(
            &fixture.guard("check", &format!("workspace-doctest-feature-{runner}"), &["--test-runner", runner]),
            "WORKSPACE_DOCTEST_FEATURE_MUST_RUN",
        );
    }
}

#[test]
fn inline_target_tables_receive_execution_policy_and_source_path_isolation() {
    let fixture = Fixture::new();
    fixture.write(
        "crates\\consumer\\Cargo.toml",
        "example=[{name='inline_example',test=true,harness=false}]\n[package]\nname='consumer'\nversion='1.1.0'\nedition='2021'\n",
    );
    fixture.write(
        "crates\\consumer\\src\\lib.rs",
        "#[test] fn ordinary_probe() { assert_eq!(1 + 1, 2); }",
    );
    fixture.write(
        "crates\\consumer\\examples\\inline_example.rs",
        "fn main() { panic!(\"INLINE_EXAMPLE_MAIN_MUST_NOT_RUN\"); }",
    );
    assert_failure(&fixture.cargo(&["test", "--workspace"]), "INLINE_EXAMPLE_MAIN_MUST_NOT_RUN");
    let original = fs::read(fixture.root().join("crates").join("consumer").join("Cargo.toml")).unwrap();
    for runner in ["cargo", "nextest"] {
        assert_success(&fixture.guard("check", &format!("inline-example-{runner}"), &["--test-runner", runner]));
        assert_eq!(
            fs::read(fixture.root().join("crates").join("consumer").join("Cargo.toml")).unwrap(),
            original
        );
    }
}

#[test]
fn inline_target_paths_cannot_reach_omitted_source() {
    let fixture = Fixture::new();
    fixture.write("crates\\consumer\\src\\lib.rs", "pub fn candidate() {}");
    fixture.write("crates\\helper\\outside_target.rs", "fn main() {}");
    let omitted = fixture.root().join("crates").join("helper").join("outside_target.rs");
    let path = toml_edit::Value::from(omitted.to_string_lossy().as_ref()).to_string();
    for kind in ["bin", "test"] {
        fixture.write(
            "crates\\consumer\\Cargo.toml",
            &format!("{kind}=[{{name='escaped_target',path={path}}}]\n[package]\nname='consumer'\nversion='1.1.0'\nedition='2021'\n"),
        );
        assert_failure(
            &fixture.guard("check", &format!("inline-escaped-{kind}"), &[]),
            "escapes retained source",
        );
    }
}

#[test]
fn command_errors_identify_missing_inputs_and_invalid_workspace_selection() {
    let fixture = Fixture::new();
    assert_failure(&fixture.guard("prepare", "missing-report", &[]), "prepare requires");
    let output = Command::new(assert_cmd::cargo::cargo_bin!("cargo-release-guard"))
        .current_dir(fixture.root())
        .args(["candidates", "--output-dir"])
        .arg(fixture.artifact("missing-base"))
        .output()
        .unwrap();
    assert_failure(&output, "requires --base");
    let output = Command::new(assert_cmd::cargo::cargo_bin!("cargo-release-guard"))
        .current_dir(fixture.root())
        .args(["candidates", "--base", "not-a-ref", "--output-dir"])
        .arg(fixture.artifact("invalid-base"))
        .output()
        .unwrap();
    assert_failure(&output, "Git failed");
    let output = fixture
        .guard_command("candidates", &fixture.artifact("member-manifest"), &[])
        .args(["--manifest-path", "crates/consumer/Cargo.toml"])
        .output()
        .unwrap();
    assert_failure(&output, "cannot be used multiple times");
    let output = Command::new(assert_cmd::cargo::cargo_bin!("cargo-release-guard"))
        .current_dir(fixture.root())
        .args([
            "candidates",
            "--base",
            "HEAD",
            "--manifest-path",
            "crates/consumer/Cargo.toml",
            "--output-dir",
        ])
        .arg(fixture.artifact("member-root"))
        .output()
        .unwrap();
    assert_failure(&output, "must identify the workspace root");
    fixture.write("Cargo.toml", "[workspace\nmalformed");
    assert_failure(&fixture.guard("candidates", "bad-manifest", &[]), "cannot discover Cargo workspace");
}

#[test]
fn stale_and_malformed_candidate_reports_fail_closed() {
    let fixture = Fixture::new();
    fixture.bump_consumer("pub fn value() -> u32 { helper::released() }");
    assert_success(&fixture.guard("candidates", "report-base", &[]));
    let original: Value = serde_json::from_slice(&fs::read(fixture.artifact("report-base").join("candidates.json")).unwrap()).unwrap();
    for (case, diagnostic) in [
        ("schema", "schema_version"),
        ("root", "different source workspace"),
        ("exclusions", "invalid artifact exclusion"),
        ("duplicate", "duplicate candidate"),
        ("missing", "candidate no longer exists"),
        ("version", "candidate version mismatch"),
    ] {
        let mut report = original.clone();
        match case {
            "schema" => report["schema_version"] = json!(999),
            "root" => report["source_root"] = json!(fixture.root().join("elsewhere")),
            "exclusions" => report["artifact_roots"] = json!([fixture.root()]),
            "duplicate" => {
                let candidate = report["candidates"][0].clone();
                report["candidates"].as_array_mut().unwrap().push(candidate);
            }
            "missing" => report["candidates"][0]["name"] = json!("not_a_package"),
            "version" => report["candidates"][0]["version"] = json!("9.0.0"),
            _ => unreachable!(),
        }
        let path = fixture.artifact("report-base").join(format!("{case}.json"));
        fs::write(&path, serde_json::to_vec(&report).unwrap()).unwrap();
        assert_failure(
            &fixture.guard("prepare", case, &["--candidate-report", path.to_str().unwrap()]),
            diagnostic,
        );
    }
}

#[test]
fn publication_destination_changes_and_permissions_are_explicit() {
    let fixture = Fixture::new();
    fixture.package("helper", "1.0.0", "publish=['first']\n", "pub fn released() -> u32 { 1 }");
    fixture.commit();
    fixture.package("helper", "1.0.0", "publish=['first','second']\n", "pub fn released() -> u32 { 1 }");
    assert_failure(&fixture.guard("candidates", "ambiguous", &[]), "multiple registries");
    fixture.write("selection.json", "[\"helper\"]");
    assert_failure(
        &fixture.guard(
            "candidates",
            "forbidden",
            &["--registry", "forbidden", "--candidate-list", "selection.json"],
        ),
        "does not allow",
    );
    assert_success(&fixture.guard("candidates", "new-destination", &["--registry", "second"]));
    assert_eq!(
        fixture.report("new-destination")["candidates"][0]["reason"],
        "new_registry_destination"
    );
    assert_success(&fixture.guard("candidates", "existing-destination", &["--registry", "first"]));
    assert_eq!(fixture.report("existing-destination")["status"], "no_release");
    fixture.package("helper", "1.0.0", "publish=['second']\n", "pub fn released() -> u32 { 1 }");
    assert_success(&fixture.guard("candidates", "single-destination", &[]));
    assert_eq!(fixture.report("single-destination")["candidates"][0]["registry"], "second");
}

#[test]
fn cargo_configuration_sources_environment_and_precedence_are_checked() {
    let fixture = Fixture::new();
    fixture.bump_consumer("pub fn candidate() {}");
    for variable in ["CARGO_SOURCE_CUSTOM_REPLACE_WITH", "CARGO_PATCH_CRATES_IO", "CARGO_PATHS"] {
        let output = fixture
            .guard_command("candidates", &fixture.artifact(variable), &[])
            .env(variable, "unsupported")
            .output()
            .unwrap();
        assert_failure(&output, "unsupported source override environment variable");
    }
    fixture.write(
        ".cargo\\config.toml",
        "[registry]\ndefault='first'\n[net]\noffline=true\n[env]\nLOCAL_MODE='enabled'\n",
    );
    let output = fixture
        .guard_command("candidates", &fixture.artifact("environment-registry"), &[])
        .env("CARGO_REGISTRY_DEFAULT", "second")
        .env("CARGO_REGISTRIES_SECOND_INDEX", "sparse+https://second.example.invalid/index/")
        .env("CARGO_NET_OFFLINE", "true")
        .env_remove("CARGO_HOME")
        .output()
        .unwrap();
    assert_success(&output);
    assert_eq!(fixture.report("environment-registry")["candidates"][0]["registry"], "second");
    for (name, config, diagnostic) in [
        ("include", "[unstable]\nconfig-include=true\n", "config inclusion"),
        ("relative-env", "[env]\nROOT={value='data',relative=true}\n", "relative=true"),
        ("malformed-config", "[registry\n", "cannot parse Cargo configuration"),
        ("build-dir", "[build]\nbuild-dir='elsewhere'\n", "build.build-dir"),
    ] {
        fixture.write(".cargo\\config.toml", config);
        assert_failure(&fixture.guard("candidates", name, &[]), diagnostic);
    }
    fixture.write(".cargo\\config.toml", "");
    fixture.write("artifacts\\extra\\.cargo\\config.toml", "[net]\nretry=3\n");
    assert_failure(&fixture.guard("candidates", "extra\\run", &[]), "additional Cargo config");
}

#[test]
fn named_registry_environment_index_and_missing_index_are_not_ignored() {
    let fixture = Fixture::new();
    fixture.package("consumer", "1.1.0", "publish=['internal']\n", "pub fn candidate() {}");
    fixture.named_registry("internal");
    let output = fixture
        .guard_command("check", &fixture.artifact("env-index"), &[])
        .env("CARGO_REGISTRIES_INTERNAL_INDEX", "sparse+https://internal.example.invalid/index/")
        .output()
        .unwrap();
    assert_success(&output);
    fixture.write(
        ".cargo\\config.toml",
        "[source.internal]\nreplace-with='loop'\n[source.loop]\nreplace-with='internal'\n",
    );
    assert_failure(&fixture.guard("check", "cycle", &[]), "offline registry cache");
}

#[test]
fn target_matrix_is_forwarded_and_binary_only_workspaces_need_no_doctests() {
    let fixture = Fixture::new();
    fixture.package("consumer", "1.1.0", "[features]\nextra=[]\n", "pub fn candidate() {}");
    let output = Command::new("rustc").arg("-vV").output().unwrap();
    assert_success(&output);
    let version = String::from_utf8(output.stdout).unwrap();
    let host = version.lines().find_map(|line| line.strip_prefix("host: ")).unwrap();
    assert_success(&fixture.guard("check", "target-host", &["--target", host, "--features", "consumer/extra"]));
    fs::remove_file(fixture.root().join("crates").join("consumer").join("src").join("lib.rs")).unwrap();
    fixture.write(
        "crates\\consumer\\src\\main.rs",
        "fn main() {}\n#[test] fn executable_test() { assert_eq!(1 + 1, 2); }",
    );
    assert_success(&fixture.guard("check", "binary-only", &[]));
    assert!(
        fixture.report("binary-only")["commands"]
            .as_array()
            .unwrap()
            .iter()
            .all(|command| command["phase"] != "doc_tests")
    );
}

#[test]
fn root_package_profiles_explicit_library_paths_and_external_dependencies_are_preserved() {
    let fixture = Fixture::new();
    fixture.package("helper", "1.1.0", "", "pub fn released() -> u32 { 1 }");
    fixture.package(
        "consumer",
        "1.1.0",
        "[lib]\npath='src/lib.rs'\n[target.'cfg(any(unix,windows))'.dependencies]\nexternal='1'\n",
        "pub fn value() -> u32 { external::released() }",
    );
    fixture.publish("external", "1.0.0", "", "pub fn released() -> u32 { 1 }");
    fixture.write("Cargo.toml", "[workspace]\nresolver='2'\nmembers=['crates/*']\n[package]\nname='root_candidate'\nversion='0.1.0'\nedition='2021'\n[profile.test]\nopt-level=1\n");
    fixture.write("src\\lib.rs", "#[test] fn root_test() { assert_eq!(2 + 2, 4); }");
    assert_success(&fixture.guard("check", "root-package", &[]));
    let manifest: toml_edit::DocumentMut = fs::read_to_string(fixture.artifact("root-package").join("workspace").join("Cargo.toml"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(manifest["profile"]["test"]["opt-level"].as_integer(), Some(1));
    fixture.write("Cargo.toml", "[workspace]\nresolver='1'\nmembers=['crates/*']\n");
    assert_failure(&fixture.guard("check", "resolver-one", &[]), "requires workspace resolver 2 or 3");
}

#[test]
fn cargo_home_is_optional_and_tracked_build_outputs_do_not_change_candidates() {
    let fixture = Fixture::new();
    fixture.git(&["add", "--force", ".gitignore"]);
    fixture.write("target\\tracked-output", "generated build output");
    fixture.git(&["add", "--force", "target/tracked-output"]);
    fs::remove_file(fixture.root().join("crates").join("helper").join("src").join("lib.rs")).unwrap();
    fixture.write(
        "crates\\helper\\Cargo.toml",
        "[package]\nname='helper'\nversion='1.0.0'\nedition='2021'\n[lib]\npath='replacement.rs'\n",
    );
    fixture.write("crates\\helper\\replacement.rs", "pub fn released() -> u32 { 1 }");
    let output = fixture
        .guard_command("candidates", &fixture.artifact("no-home"), &[])
        .env_remove("CARGO_HOME")
        .env_remove("HOME")
        .env_remove("USERPROFILE")
        .output()
        .unwrap();
    assert_success(&output);
    assert_eq!(fixture.report("no-home")["status"], "no_release");
}

#[test]
fn new_nested_workspace_compares_against_a_base_without_any_workspace_manifest() {
    let fixture = Fixture::new();
    fixture.write("nested\\Cargo.toml", "[workspace]\nresolver='2'\nmembers=['package']\n");
    fixture.write(
        "nested\\package\\Cargo.toml",
        "[package]\nname='new_nested'\nversion='0.1.0'\nedition='2021'\n",
    );
    fixture.write("nested\\package\\src\\lib.rs", "pub fn new_nested() {}");
    let output = Command::new(assert_cmd::cargo::cargo_bin!("cargo-release-guard"))
        .current_dir(fixture.root())
        .args([
            "candidates",
            "--base",
            "HEAD",
            "--manifest-path",
            "nested/Cargo.toml",
            "--output-dir",
        ])
        .arg(fixture.artifact("nested"))
        .output()
        .unwrap();
    assert_success(&output);
    assert_eq!(fixture.report("nested")["candidates"][0]["reason"], "new_package");
}

#[test]
fn credential_bearing_registry_urls_are_not_copied_to_artifacts() {
    let fixture = Fixture::new();
    fixture.package("consumer", "1.1.0", "", "pub fn value() -> u32 { 1 }");
    fixture.named_registry("internal");
    let configuration = fs::read_to_string(fixture.root().join(".cargo").join("config.toml"))
        .unwrap()
        .replace(
            "sparse+https://internal.example.invalid/index/",
            "sparse+https://fixture-placeholder@internal.example.invalid/index/",
        );
    fixture.write(".cargo\\config.toml", &configuration);
    let output = fixture.guard("check", "credential-index", &["--registry", "internal"]);
    assert_failure(&output, "registry index URLs containing user information");
    let manifest = fs::read_to_string(fixture.artifact("credential-index").join("workspace").join("Cargo.toml")).unwrap();
    assert!(!manifest.contains("fixture-placeholder"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("fixture-placeholder"));
    assert!(!fixture.report("credential-index").to_string().contains("fixture-placeholder"));
}

#[test]
fn inline_target_dependency_tables_resolve_omitted_packages_from_the_registry() {
    let fixture = Fixture::new();
    fixture.write("crates\\consumer\\Cargo.toml", "target={ 'cfg(any(unix,windows))'={ dependencies={helper={path='../helper', version='1'}} } }\n[package]\nname='consumer'\nversion='1.1.0'\nedition='2021'\n");
    fixture.write("crates\\consumer\\src\\lib.rs", "pub fn value() -> u32 { helper::released() }");
    assert_success(&fixture.cargo(&["check", "--workspace"]));
    assert_success(&fixture.guard("check", "inline-target-dependencies", &[]));
    let materialized = fs::read_to_string(
        fixture
            .artifact("inline-target-dependencies")
            .join("workspace")
            .join("crates")
            .join("consumer")
            .join("Cargo.toml"),
    )
    .unwrap();
    assert!(!materialized.contains("../helper"));
    assert_eq!(fixture.report("inline-target-dependencies")["status"], "passed");
}

#[test]
fn historical_fixture_links_do_not_block_a_no_release_check_or_get_followed() {
    let fixture = Fixture::new();
    let path = "crates/helper/tests/fixtures/artifacts/traversal/cycle/alias_to_new";
    fixture.commit_historical_symlink(path, "../../../../../../../../outside-source");
    let configuration = fs::read_to_string(fixture.root().join(".cargo").join("config.toml")).unwrap();
    fixture.commit_historical_symlink(".cargo/config.toml", "../../unapproved-config");
    fixture.write(".cargo\\config.toml", &configuration);
    assert_success(&fixture.guard("check", "historical-fixture-link", &[]));
    let report = fixture.report("historical-fixture-link");
    assert_eq!(report["status"], "no_release");
    assert_eq!(report["candidates"], json!([]));
    assert_eq!(report["commands"], json!([]));
    assert!(report["diagnostics"].as_array().unwrap().iter().any(|diagnostic| {
        let diagnostic = diagnostic.as_str().unwrap();
        diagnostic.contains("alias_to_new") && diagnostic.contains("not followed")
    }));
    let inert = fixture.artifact("historical-fixture-link").join("base").join(path);
    assert!(inert.is_dir());
    assert!(!fs::symlink_metadata(&inert).unwrap().file_type().is_symlink());
    assert!(
        fs::read_to_string(inert.join("Cargo.toml"))
            .unwrap()
            .contains("historical symbolic link")
    );
    assert!(
        !fixture
            .artifact("historical-fixture-link")
            .join("base")
            .join(".cargo")
            .join("config.toml")
            .exists()
    );
    assert_eq!(
        fs::read_to_string(fixture.root().join(path)).unwrap(),
        "../../../../../../../../outside-source"
    );
    fixture.bump_consumer("pub fn value() -> u32 { helper::released() }");
    assert_success(&fixture.guard("check", "historical-fixture-release", &[]));
    assert_eq!(
        fixture.report("historical-fixture-release")["candidates"].as_array().unwrap().len(),
        1
    );
    assert!(
        !fixture
            .artifact("historical-fixture-release")
            .join("workspace")
            .join("crates")
            .join("helper")
            .exists()
    );
    assert_eq!(
        fs::read_to_string(fixture.root().join(path)).unwrap(),
        "../../../../../../../../outside-source"
    );
}

#[test]
fn historical_manifest_links_fail_with_the_original_entry_path_even_when_export_ignored() {
    for path in ["Cargo.toml", "crates/helper/Cargo.toml"] {
        let fixture = Fixture::new();
        let manifest = fs::read_to_string(fixture.root().join(path)).unwrap();
        fixture.write(".gitattributes", &format!("{path} export-ignore\n"));
        fixture.commit();
        fixture.commit_historical_symlink(path, "real-manifest.toml");
        fixture.write(path, &manifest);
        let output = fixture.guard("candidates", "historical-manifest-link", &[]);
        assert_failure(&output, "historical");
        assert_failure(&output, "symbolic link");
        assert_failure(&output, "Cargo.toml");
        assert!(
            fixture.report("historical-manifest-link")["diagnostics"]
                .to_string()
                .contains("Cargo.toml")
        );
        assert_eq!(fs::read_to_string(fixture.root().join(path)).unwrap(), manifest);
    }
}

#[test]
fn historical_member_directory_links_cannot_disappear_from_workspace_globs() {
    let fixture = Fixture::new();
    let helper = fixture.root().join("crates").join("helper");
    let saved = fixture.root().join("saved-helper");
    fs::rename(&helper, &saved).unwrap();
    fixture.git(&[
        "update-index",
        "--force-remove",
        "crates/helper/Cargo.toml",
        "crates/helper/src/lib.rs",
    ]);
    fixture.commit_historical_symlink("crates/helper", "../../saved-helper");
    fs::remove_file(&helper).unwrap();
    fs::rename(&saved, &helper).unwrap();
    let output = fixture.guard("candidates", "historical-member-link", &[]);
    assert_failure(&output, "historical");
    assert_failure(&output, "symbolic link");
    assert_failure(&output, "helper");
}

#[test]
fn historical_workspace_root_links_cannot_be_misclassified_as_a_new_workspace() {
    let fixture = Fixture::new();
    fixture.commit_historical_symlink("nested", "../other-workspace");
    fs::remove_file(fixture.root().join("nested")).unwrap();
    fixture.write("nested\\workspace\\Cargo.toml", "[workspace]\nresolver='2'\nmembers=['package']\n");
    fixture.write(
        "nested\\workspace\\package\\Cargo.toml",
        "[package]\nname='new_nested'\nversion='0.1.0'\nedition='2021'\n",
    );
    fixture.write("nested\\workspace\\package\\src\\lib.rs", "pub fn new_nested() {}");
    let output = Command::new(assert_cmd::cargo::cargo_bin!("cargo-release-guard"))
        .current_dir(fixture.root())
        .args([
            "candidates",
            "--base",
            "HEAD",
            "--manifest-path",
            "nested/workspace/Cargo.toml",
            "--output-dir",
        ])
        .arg(fixture.artifact("historical-root-link"))
        .output()
        .unwrap();
    assert_failure(&output, "historical");
    assert_failure(&output, "symbolic link");
    assert_failure(&output, "nested");
}

#[test]
fn real_leaf_fixture_symlinks_allow_no_release_and_omitted_package_checks() {
    let fixture = Fixture::new();
    let path = "crates/helper/tests/fixtures/artifacts/traversal/cycle/alias_to_new";
    fixture.commit_historical_symlink(path, ".");
    let link = fixture.root().join(path);
    fs::remove_file(&link).unwrap();
    create_symlink(Path::new("."), &link, true);
    assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    assert_success(&fixture.guard("check", "real-link-no-release", &[]));
    assert_eq!(fixture.report("real-link-no-release")["status"], "no_release");
    fixture.bump_consumer("pub fn value() -> u32 { helper::released() }");
    assert_success(&fixture.guard("check", "real-link-omitted", &[]));
    let report = fixture.report("real-link-omitted");
    assert_eq!(report["candidates"].as_array().unwrap().len(), 1);
    assert_eq!(report["candidates"][0]["name"], "consumer");
    assert!(
        !fixture
            .artifact("real-link-omitted")
            .join("workspace")
            .join("crates")
            .join("helper")
            .exists()
    );
    assert_eq!(fs::read_link(&link).unwrap(), Path::new("."));
}

#[test]
fn real_leaf_symlink_snapshots_distinguish_link_kind_and_target_without_reading_targets() {
    let fixture = Fixture::new();
    let link = fixture.root().join("crates").join("helper").join("fixtures").join("alias");
    create_symlink(Path::new("missing-one"), &link, false);
    assert_success(&fixture.guard("candidates", "link-identity", &[]));
    let original = fixture.artifact("link-identity").join("candidates.json");
    fs::remove_file(&link).unwrap();
    fs::write(&link, "missing-one").unwrap();
    assert_failure(
        &fixture.guard("prepare", "changed-kind", &["--candidate-report", original.to_str().unwrap()]),
        "source snapshot mismatch",
    );
    fs::remove_file(&link).unwrap();
    create_symlink(Path::new("missing-two"), &link, false);
    assert_failure(
        &fixture.guard("prepare", "changed-target", &["--candidate-report", original.to_str().unwrap()]),
        "source snapshot mismatch",
    );

    fs::remove_file(&link).unwrap();
    fixture.write("target\\pointed-to", "before");
    let target = fixture.root().join("target").join("pointed-to");
    create_symlink(&target, &link, false);
    assert_success(&fixture.guard("candidates", "target-not-read", &[]));
    fixture.write("target\\pointed-to", "after");
    let original = fixture.artifact("target-not-read").join("candidates.json");
    assert_success(&fixture.guard(
        "prepare",
        "target-content-irrelevant",
        &["--candidate-report", original.to_str().unwrap()],
    ));
    assert_eq!(fixture.report("target-content-irrelevant")["status"], "no_release");
}

#[test]
fn real_leaf_symlinks_in_retained_source_are_not_materialized() {
    let fixture = Fixture::new();
    fixture.bump_consumer("pub fn value() -> u32 { helper::released() }");
    let link = fixture.root().join("crates").join("consumer").join("fixtures").join("alias");
    create_symlink(Path::new("missing-target"), &link, false);
    assert_success(&fixture.guard("candidates", "retained-link-selection", &[]));
    let output = fixture.guard("check", "retained-link-rejected", &[]);
    assert_failure(&output, "symlink or filesystem shortcut");
    assert_failure(&output, "alias");
    assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
}

#[test]
fn real_manifest_and_parent_directory_links_remain_fail_closed() {
    for member in [false, true] {
        let fixture = Fixture::new();
        let directory = if member {
            fixture.root().join("crates").join("helper")
        } else {
            fixture.root().to_owned()
        };
        let manifest = directory.join("Cargo.toml");
        fs::rename(&manifest, directory.join("Cargo.real.toml")).unwrap();
        create_symlink(Path::new("Cargo.real.toml"), &manifest, false);
        let output = fixture.guard("candidates", "manifest-link-rejected", &[]);
        assert_failure(&output, "symlink or filesystem shortcut");
        assert_failure(&output, "Cargo.toml");
    }
    let fixture = Fixture::new();
    let helper = fixture.root().join("crates").join("helper");
    fs::rename(&helper, fixture.root().join("saved-helper")).unwrap();
    create_symlink(&Path::new("..").join("saved-helper"), &helper, true);
    let output = fixture.guard("candidates", "directory-link-rejected", &[]);
    assert_failure(&output, "symlink or filesystem shortcut");
    assert_failure(&output, "helper");
}
