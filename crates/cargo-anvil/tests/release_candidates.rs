// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]
#![expect(clippy::unwrap_used, reason = "integration fixtures panic on failure")]

use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serial_test::serial;
use tempfile::{Builder, TempDir};

const RELEASE: &str = include_str!("../templates/justfiles/anvil/release.just");
const VALIDATION: &str = include_str!("../templates/justfiles/anvil/checks/release-dependency-validation.just");
const HELPERS: &str = include_str!("../templates/justfiles/anvil/helpers.just");

fn tools_available() -> bool {
    for (tool, arg) in [
        ("git", "--version"),
        ("cargo", "--version"),
        ("just", "--version"),
        ("pwsh", "--version"),
    ] {
        if !Command::new(tool).arg(arg).output().is_ok_and(|o| o.status.success()) {
            eprintln!("skipping: required tool '{tool}' unavailable");
            return false;
        }
    }
    if !Command::new("cargo")
        .args(["each", "--version"])
        .output()
        .is_ok_and(|o| o.status.success())
    {
        eprintln!("skipping: required tool 'cargo-each' unavailable");
        return false;
    }
    true
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git").args(args).current_dir(root).output().unwrap();
    assert_success(&output);
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new(manifest: &str) -> Self {
        let dir = Builder::new().prefix("anvil release's workspace ").tempdir().unwrap();
        let root = dir.path();
        write(&root.join("release.just"), RELEASE);
        write(&root.join("validation.just"), VALIDATION);
        write(&root.join("helpers.just"), HELPERS);
        write(
            &root.join("Justfile"),
            concat!(
                "set unstable\n",
                "set allow-duplicate-recipes\n",
                "set windows-shell := [\"pwsh\", \"-NoProfile\", \"-Command\"]\n",
                "_anvil_stable_toolchain_args := \"@()\"\n",
                "import 'release.just'\n",
                "import 'validation.just'\n",
                "import 'helpers.just'\n",
                "anvil-toolchain-stable-install:\n",
                "anvil-tool-pwsh-validate-prereqs:\n",
                "anvil-tool-rustc-validate-prereqs:\n",
                "anvil-tool-cargo-each-validate-prereqs:\n",
                "anvil-tool-cargo-each-install installer:\n",
            ),
        );
        write(&root.join("Cargo.toml"), manifest);
        write(&root.join(".gitignore"), "target/\nCargo.lock\n");
        git(root, &["init", "--initial-branch=main"]);
        git(root, &["config", "user.name", "anvil test"]);
        git(root, &["config", "user.email", "anvil@example.com"]);
        git(root, &["config", "core.autocrlf", "false"]);
        git(root, &["config", "core.safecrlf", "false"]);
        Self { dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn package(&self, directory: &str, name: &str, version: &str, extra: &str) {
        write(
            &self.root().join(directory).join("Cargo.toml"),
            &format!("[package]\nname = \"{name}\"\n{version}\nedition = \"2024\"\n{extra}\n"),
        );
        write(&self.root().join(directory).join("src/lib.rs"), "pub fn fixture() {}\n");
    }

    fn commit(&self) {
        git(self.root(), &["add", "."]);
        git(self.root(), &["commit", "--quiet", "-m", "fixture"]);
        git(self.root(), &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    }

    fn run(&self, recipe: &str, vars: &[(&str, &OsStr)]) -> Output {
        self.run_args(&[recipe], vars)
    }

    fn run_args(&self, arguments: &[&str], vars: &[(&str, &OsStr)]) -> Output {
        let mut command = Command::new("just");
        command.current_dir(self.root()).args(arguments);
        for key in [
            "BASE_REF",
            "ANVIL_RELEASE",
            "ANVIL_RELEASE_INPUT_DIR",
            "ANVIL_IMPACT",
            "GITHUB_BASE_REF",
            "SYSTEM_PULLREQUEST_TARGETBRANCH",
            "GITHUB_ACTIONS",
            "TF_BUILD",
            "CARGO_TARGET_DIR",
        ] {
            command.env_remove(key);
        }
        for (key, value) in vars {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    fn candidates(&self) -> String {
        fs::read_to_string(self.root().join("target/anvil/release/candidates.packages")).unwrap()
    }
}

#[test]
#[serial]
fn release_candidates_compare_effective_versions_and_current_publication() {
    if !tools_available() {
        return;
    }
    let fixture = Fixture::new(
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n\
         [workspace.package]\nversion = \"0.1.0\"\npublish = [\"fixture\"]\n",
    );
    for name in ["alpha", "moved", "removed", "renamed", "same", "private", "disabled"] {
        let publish = if matches!(name, "private" | "disabled") {
            "publish = false"
        } else {
            "publish.workspace = true"
        };
        fixture.package(&format!("crates/{name}"), name, "version.workspace = true", publish);
    }
    fixture.commit();
    let head = git(fixture.root(), &["rev-parse", "HEAD"]);
    let worktrees = git(fixture.root(), &["worktree", "list", "--porcelain"]);
    assert_success(&fixture.run("anvil-release-candidates", &[]));
    assert_eq!(fixture.candidates(), "");

    write(
        &fixture.root().join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n\
         [workspace.package]\nversion = \"0.2.0\"\npublish = [\"fixture\"]\n",
    );
    fs::rename(fixture.root().join("crates/moved"), fixture.root().join("crates/new-location")).unwrap();
    fixture.package("crates/new-location", "moved", "version = \"0.1.0\"", "");
    fs::remove_dir_all(fixture.root().join("crates/removed")).unwrap();
    fixture.package("crates/renamed", "new-name", "version = \"0.1.0\"", "");
    fixture.package("crates/same", "same", "version = \"0.1.0\"", "");
    fixture.package("crates/disabled", "disabled", "version = \"0.1.0\"", "");
    fixture.package("crates/private", "private", "version.workspace = true", "publish = false");
    fixture.package("crates/bin", "binary", "version = \"0.1.0\"", "");
    fs::rename(
        fixture.root().join("crates/bin/src/lib.rs"),
        fixture.root().join("crates/bin/src/main.rs"),
    )
    .unwrap();
    write(&fixture.root().join("crates/bin/src/main.rs"), "fn main() {}\n");
    fixture.package("crates/macro", "macro", "version = \"0.1.0\"", "[lib]\nproc-macro = true");
    git(fixture.root(), &["add", "Cargo.toml"]);
    let before = git(fixture.root(), &["status", "--porcelain"]);
    assert_success(&fixture.run("anvil-release-candidates", &[("ANVIL_IMPACT", OsStr::new("off"))]));
    assert_eq!(
        fixture.candidates().replace("\r\n", "\n"),
        "alpha@0.2.0\nbinary@0.1.0\nmacro@0.1.0\nnew-name@0.1.0\n"
    );
    assert_eq!(git(fixture.root(), &["status", "--porcelain"]), before);
    assert_eq!(git(fixture.root(), &["rev-parse", "HEAD"]), head);
    assert_eq!(git(fixture.root(), &["worktree", "list", "--porcelain"]), worktrees);

    fixture.package("crates/alpha", "alpha", "version = \"0.1.0\"", "");
    assert_success(&fixture.run("anvil-release-candidates", &[("ANVIL_IMPACT", OsStr::new("consume"))]));
    assert!(!fixture.candidates().contains("alpha"));
}

#[test]
#[serial]
fn release_candidates_fail_closed_and_clean_baseline_worktrees() {
    if !tools_available() {
        return;
    }
    let fixture = Fixture::new("[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n");
    fixture.package("crates/alpha", "alpha", "version = \"0.1.0\"", "");
    fixture.commit();
    fixture.package("crates/alpha", "alpha", "version = \"0.2.0\"", "");
    assert_success(&fixture.run("anvil-release-candidates", &[]));
    assert_eq!(fixture.candidates().trim(), "alpha@0.2.0");
    let output = fixture.run("anvil-release-candidates", &[("BASE_REF", OsStr::new("not-a-ref"))]);
    assert!(!output.status.success());
    assert!(!fixture.root().join("target/anvil/release/candidates.packages").exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("baseline"));
    write(&fixture.root().join("crates/alpha/Cargo.toml"), "invalid manifest");
    assert!(!fixture.run("anvil-release-candidates", &[]).status.success());
    assert!(!fixture.root().join("target/anvil/release/candidates.packages").exists());

    fixture.package("crates/alpha", "alpha", "version = \"0.2.0\"", "");
    // Existing but incomplete baseline workspaces are errors, not all-new releases.
    git(fixture.root(), &["rm", "--cached", "crates/alpha/Cargo.toml"]);
    git(fixture.root(), &["commit", "--quiet", "-m", "broken baseline"]);
    let before = git(fixture.root(), &["worktree", "list", "--porcelain"]);
    assert!(
        !fixture
            .run("anvil-release-candidates", &[("BASE_REF", OsStr::new("HEAD"))])
            .status
            .success()
    );
    assert!(!fixture.root().join("target/anvil/release/candidates.packages").exists());
    assert_eq!(git(fixture.root(), &["worktree", "list", "--porcelain"]), before);
}

#[test]
#[serial]
fn release_candidates_reject_metadata_errors_and_malformed_json_without_stale_output() {
    if !tools_available() {
        return;
    }
    let fixture = Fixture::new("[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n");
    fixture.package("crates/alpha", "alpha", "version = \"0.1.0\"", "");
    fixture.commit();
    let shim = fixture.root().join("shim");
    let mut paths = vec![shim.clone()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
    let path = std::env::join_paths(paths).unwrap();
    for script in ["exit 29\n", "Write-Output '{invalid json'\n", "Write-Output '{}'\n"] {
        write(&fixture.root().join("target/anvil/release/candidates.packages"), "stale@0.0.0\n");
        write(&shim.join("cargo.ps1"), script);
        let output = fixture.run("anvil-release-candidates", &[("PATH", &path)]);
        assert!(!output.status.success(), "{output:?}");
        assert!(!fixture.root().join("target/anvil/release/candidates.packages").exists());
    }
}

#[test]
#[serial]
fn release_candidates_support_baselines_before_workspace_and_workspace_subdirectories() {
    if !tools_available() {
        return;
    }
    let fixture = Fixture::new("[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n");
    fixture.package("crates/alpha", "alpha", "version = \"0.1.0\"", "");
    // Commit the repository before introducing Cargo.
    git(
        fixture.root(),
        &["add", "Justfile", "release.just", "validation.just", "helpers.just", ".gitignore"],
    );
    git(fixture.root(), &["commit", "--quiet", "-m", "before workspace"]);
    assert_success(&fixture.run("anvil-release-candidates", &[("BASE_REF", OsStr::new("HEAD"))]));
    assert_eq!(fixture.candidates().trim(), "alpha@0.1.0");
    fixture.commit();
    let child = fixture.root().join("nested");
    fs::create_dir_all(&child).unwrap();
    for name in [
        "Justfile",
        "release.just",
        "validation.just",
        "helpers.just",
        "Cargo.toml",
        "crates",
    ] {
        fs::rename(fixture.root().join(name), child.join(name)).unwrap();
    }
    fixture.commit();
    let output = Command::new("just")
        .arg("anvil-release-candidates")
        .current_dir(&child)
        .env("BASE_REF", "HEAD")
        .env_remove("ANVIL_RELEASE")
        .output()
        .unwrap();
    assert_success(&output);
    assert_eq!(
        fs::read_to_string(child.join("target/anvil/release/candidates.packages")).unwrap(),
        ""
    );
}

#[test]
#[serial]
fn release_candidates_consume_validates_without_git_and_preserves_input() {
    if !tools_available() {
        return;
    }
    let fixture = Fixture::new("[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n");
    fixture.package("crates/alpha", "alpha", "version = \"0.1.0\"", "");
    fs::remove_dir_all(fixture.root().join(".git")).unwrap();
    let input = fixture.root().join("handoff with spaces");
    let file = input.join("candidates.packages");
    let vars = [
        ("ANVIL_RELEASE", OsStr::new("consume")),
        ("ANVIL_RELEASE_INPUT_DIR", input.as_os_str()),
        ("BASE_REF", OsStr::new("unavailable")),
        ("ANVIL_IMPACT", OsStr::new("consume")),
    ];
    assert!(!fixture.run("anvil-release-candidates", &vars).status.success());
    for content in ["--workspace\n", "alpha@9.9.9\n", "alpha@0.1.0\n", ""] {
        write(&file, content);
        let output = fixture.run("anvil-release-candidates", &vars);
        if matches!(content, "alpha@0.1.0\n" | "") {
            assert_success(&output);
        } else {
            assert!(!output.status.success());
        }
        assert_eq!(fs::read_to_string(&file).unwrap(), content);
    }
    fs::write(&file, [0xff, 0xfe]).unwrap();
    assert!(!fixture.run("anvil-release-candidates", &vars).status.success());
    assert_eq!(fs::read(&file).unwrap(), [0xff, 0xfe]);
    let output = fixture.run("anvil-release-candidates", &[("ANVIL_RELEASE", OsStr::new("off"))]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("ANVIL_RELEASE"));
}

#[test]
#[serial]
fn release_validation_groups_candidates_and_applies_local_and_ci_dirty_policy() {
    if !tools_available() {
        return;
    }
    let fixture = Fixture::new("[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n");
    for name in ["alpha", "beta"] {
        fixture.package(&format!("crates/{name}"), name, "version = \"0.1.0\"", "rust-version = \"1.95\"");
    }
    fixture.commit();
    for name in ["alpha", "beta"] {
        fixture.package(&format!("crates/{name}"), name, "version = \"0.2.0\"", "rust-version = \"1.95\"");
    }
    // Root fallback is exercised by the child Cargo command.
    write(
        &fixture.root().join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n[workspace.package]\nrust-version = \"1.95\"\n",
    );
    let output = fixture.run("anvil-release-dependency-validation", &[]);
    assert_success(&output);
    for name in ["alpha", "beta"] {
        assert!(fixture.root().join(format!("target/package/{name}-0.2.0.crate")).exists());
    }
    assert_eq!(String::from_utf8_lossy(&output.stderr).matches("Packaging alpha").count(), 1);
    assert_eq!(String::from_utf8_lossy(&output.stderr).matches("Packaging beta").count(), 1);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("--workspace"));
    let input_file = fixture.root().join("target/anvil/release/candidates.packages");
    let selected = fs::read(&input_file).unwrap();

    for mode in ["", "consume"] {
        for provider in ["GITHUB_ACTIONS", "TF_BUILD"] {
            let output = fixture.run(
                "anvil-release-dependency-validation",
                &[(provider, OsStr::new("TrUe")), ("ANVIL_RELEASE", OsStr::new(mode))],
            );
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains("uncommitted"), "{output:?}");
            assert_eq!(fs::read(&input_file).unwrap(), selected);
        }
    }

    write(&input_file, "");
    let output = fixture.run(
        "anvil-release-dependency-validation",
        &[("ANVIL_RELEASE", OsStr::new("consume")), ("GITHUB_ACTIONS", OsStr::new("true"))],
    );
    assert_success(&output);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("Packaging"));
    assert_eq!(fs::read(&input_file).unwrap(), b"");
}

#[test]
#[serial]
fn release_validation_flag_expressions_follow_existing_provider_and_toolchain_conventions() {
    if !tools_available() {
        return;
    }
    let fixture = Fixture::new("[workspace]\nresolver = \"2\"\nmembers = []\n");
    for (github, ado, expected) in [
        ("", "", "--allow-dirty"),
        ("false", "false", "--allow-dirty"),
        ("1", "yes", "--allow-dirty"),
        ("true", "", ""),
        ("TRUE", "", ""),
        ("", "True", ""),
        ("false", "tRuE", ""),
    ] {
        let output = fixture.run_args(
            &["--evaluate", "anvil_release_dirty_arg"],
            &[("GITHUB_ACTIONS", OsStr::new(github)), ("TF_BUILD", OsStr::new(ado))],
        );
        assert_success(&output);
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), expected);
    }
    for toolchain_file in ["rust-toolchain", "rust-toolchain.toml"] {
        write(&fixture.root().join(toolchain_file), "");
        let output = fixture.run_args(&["--evaluate", "anvil_release_toolchain_arg"], &[]);
        assert_success(&output);
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "");
        fs::remove_file(fixture.root().join(toolchain_file)).unwrap();
    }
    let output = fixture.run_args(
        &["--evaluate", "anvil_release_toolchain_arg"],
        &[("RUSTUP_TOOLCHAIN", OsStr::new("caller-selected"))],
    );
    assert_success(&output);
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "");
    let output = fixture.run_args(
        &["--evaluate", "anvil_release_toolchain_arg"],
        &[("RUSTUP_TOOLCHAIN", OsStr::new(""))],
    );
    assert_success(&output);
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "'+{workspace-rust-version}'");
}

#[test]
fn release_recipes_remain_standalone_and_do_not_weaken_packaged_verification() {
    for source in [
        include_str!("../templates/justfiles/anvil/tiers.just"),
        include_str!("../templates/justfiles/anvil/groups/pr-slow.just"),
        include_str!("../templates/justfiles/anvil/groups/pr-fast.just"),
        include_str!("../templates/justfiles/anvil/groups/pr-test.just"),
        include_str!("../templates/justfiles/anvil/groups/pr-msrv.just"),
        include_str!("../templates/justfiles/anvil/groups/pr-runtime-analysis.just"),
        include_str!("../templates/justfiles/anvil/groups/pr-mutants.just"),
    ] {
        assert!(!source.contains("anvil-release"));
    }
    for forbidden in ["--workspace", "--no-verify", "cargo release", "cargo publish", "ANVIL_IMPACT"] {
        assert!(!VALIDATION.contains(forbidden));
    }
    assert!(VALIDATION.contains("--once -- cargo"));
    assert!(VALIDATION.contains("package '{packages}' --all-features"));
}
