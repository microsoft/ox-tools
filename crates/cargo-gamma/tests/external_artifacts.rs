// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![allow(clippy::unwrap_used, reason = "Integration-test fixture failures are reported by panicking")]

//! Real executable campaigns with independently located Cargo artifacts.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use cargo_gamma_lib::testing::private_system_tempdir;

/// Build-time Git metadata and a marker that counts actual build-script executions.
const BUILD_SCRIPT: &str = r#"
use std::{env, fs, io::Write, path::PathBuf, process::Command};

fn main() {
    let mut metadata = String::new();
    for arguments in [
        vec!["rev-parse", "HEAD"],
        vec!["symbolic-ref", "--short", "HEAD"],
        vec!["describe", "--tags", "--always"],
    ] {
        let output = Command::new("git").args(arguments).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        metadata.push_str(std::str::from_utf8(&output.stdout).unwrap().trim());
        metadata.push('\n');
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    fs::write(out.join("git-metadata"), metadata).unwrap();
    let mut log = fs::OpenOptions::new().create(true).append(true)
        .open(env::var_os("GAMMA_ARTIFACT_BUILD_LOG").unwrap()).unwrap();
    writeln!(log, "{}", out.display()).unwrap();
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=GAMMA_ARTIFACT_BUILD_LOG");
}
"#;

/// Owns the paths and child-only environment for one artifact-placement comparison.
struct Fixture {
    home: PathBuf,
    source: PathBuf,
    external: PathBuf,
}

impl Fixture {
    fn command(&self, program: impl AsRef<Path>) -> Command {
        let mut command = Command::new(program.as_ref());
        command
            .current_dir(&self.source)
            .env("XDG_CACHE_HOME", self.home.join("scratch"))
            .env("GAMMA_ARTIFACT_BUILD_LOG", self.home.join("builds"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.home.join("gitconfig"))
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_AUTHOR_NAME", "Gamma fixture")
            .env("GIT_AUTHOR_EMAIL", "gamma@example.invalid")
            .env("GIT_COMMITTER_NAME", "Gamma fixture")
            .env("GIT_COMMITTER_EMAIL", "gamma@example.invalid")
            .env("GIT_AUTHOR_DATE", "2001-01-01T00:00:00+00:00")
            .env("GIT_COMMITTER_DATE", "2001-01-01T00:00:00+00:00");
        for name in [
            "CARGO_TARGET_DIR",
            "CARGO_BUILD_TARGET_DIR",
            "CARGO_GAMMA_TEST_CACHE_HOME",
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
        ] {
            command.env_remove(name);
        }
        command
    }

    fn git(&self, args: &[&str]) -> String {
        checked(self.command("git").args(args)).trim().to_owned()
    }

    fn gamma(&self, verb: &str, target: Option<&Path>) -> Command {
        let mut command = self.command(env!("CARGO_BIN_EXE_cargo-gamma"));
        command.args([verb, "--dir"]).arg(&self.source);
        if let Some(target) = target {
            command.env("CARGO_TARGET_DIR", target);
        }
        command
    }

    fn run(&self, target: Option<&Path>) {
        let output = checked(self.gamma("run", target).args([
            "--lib",
            "--mutators",
            "relational.gt_to_eq,fn_value.some_default",
            "--show-unviable",
            "--whole-test-binaries",
            "--jobs",
            "1",
            // Do not let a watchdog decide mutation outcomes under a loaded test runner.
            "--minimum-test-timeout",
            "3600",
        ]));
        assert!(output.contains("1 killed"), "{output}");
        assert!(output.contains("[fn_value.some_default]"), "{output}");
    }

    fn metadata(&self) -> String {
        [
            self.git(&["rev-parse", "HEAD"]),
            self.git(&["symbolic-ref", "--short", "HEAD"]),
            self.git(&["describe", "--tags", "--always"]),
        ]
        .join("\n")
            + "\n"
    }

    fn builds(&self) -> Vec<PathBuf> {
        fs::read_to_string(self.home.join("builds"))
            .unwrap()
            .lines()
            .map(PathBuf::from)
            .collect()
    }
}

#[track_caller]
fn checked(command: &mut Command) -> String {
    let output = command.output().expect("fixture command starts");
    let text = output_text(&output);
    assert!(output.status.success(), "{command:?}\n{text}");
    text
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn only_directory(parent: &Path) -> PathBuf {
    let directories: Vec<_> = fs::read_dir(parent)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect();
    assert_eq!(directories.len(), 1, "{directories:?}");
    directories.into_iter().next().unwrap()
}

#[expect(
    clippy::unnecessary_debug_formatting,
    reason = "Debug produces escaped Rust string literals for fixture paths"
)]
fn create_fixture(home: &Path, linked: bool) -> Fixture {
    let home = home.to_path_buf();
    let ordinary = home.join("repository with spaces");
    let mut fixture = Fixture {
        home: home.clone(),
        source: ordinary.clone(),
        external: home.join("external target"),
    };
    fs::create_dir_all(fixture.source.join("src")).unwrap();
    fs::create_dir(fixture.source.join(".cargo")).unwrap();
    fs::write(fixture.home.join("gitconfig"), "").unwrap();
    fs::write(
        fixture.source.join("Cargo.toml"),
        "[package]\nname = \"subject\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n",
    )
    .unwrap();
    fs::write(
        fixture.source.join(".cargo/config.toml"),
        format!(
            "[build]\ntarget-dir = {:?}\n",
            fixture.external.to_str().unwrap().replace('\\', "/")
        ),
    )
    .unwrap();
    fs::write(fixture.source.join(".gitignore"), "/target\n/gamma-hints.yaml\n").unwrap();
    fs::write(fixture.source.join("build.rs"), BUILD_SCRIPT).unwrap();
    let expected = fixture.home.join("expected-metadata");
    let launches = fixture.home.join("test-launches");
    let library = format!(
        r#"
pub fn greater(left: u32, right: u32) -> bool {{ left > right }}
pub fn lookup() -> Option<&'static dyn core::fmt::Debug> {{ None }}
#[cfg(test)]
mod tests {{
    use std::io::Write;
    #[test]
    fn metadata_and_ordering() {{
        let mut log = std::fs::OpenOptions::new().create(true).append(true).open({launches:?}).unwrap();
        writeln!(log, "{{}}", std::env::var("GAMMA_ACTIVE").unwrap_or_default()).unwrap();
        assert_eq!(include_str!(concat!(env!("OUT_DIR"), "/git-metadata")), std::fs::read_to_string({expected:?}).unwrap());
        assert!(!super::greater(1, 2));
        assert!(!super::greater(2, 2));
        assert!(super::greater(3, 2));
        assert!(super::lookup().is_none());
    }}
}}
"#
    );
    fs::write(fixture.source.join("src/lib.rs"), &library).unwrap();
    checked(fixture.command("cargo").args(["generate-lockfile", "--offline"]));
    fixture.git(&["init", "--quiet", "-b", "main"]);
    fixture.git(&["add", "."]);
    fixture.git(&["-c", "commit.gpgsign=false", "commit", "--quiet", "-m", "fixture"]);
    fixture.git(&["tag", "fixture-v1"]);
    if linked {
        let selected = fixture.home.join("linked checkout");
        checked(
            fixture
                .command("git")
                .args(["worktree", "add", "--quiet", "-b", "artifact-selected"])
                .arg(&selected),
        );
        fixture.source = selected;
        fs::write(fixture.source.join("selected"), "linked revision").unwrap();
        fixture.git(&["add", "selected"]);
        fixture.git(&["-c", "commit.gpgsign=false", "commit", "--quiet", "-m", "linked"]);
        fixture.git(&["tag", "linked-v2"]);
        let git_dir = fs::canonicalize(fixture.git(&["rev-parse", "--absolute-git-dir"])).unwrap();
        let relative = Path::new("..").join(git_dir.strip_prefix(fs::canonicalize(&fixture.home).unwrap()).unwrap());
        let mut pointer = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(fixture.source.join(".git"))
            .unwrap();
        writeln!(pointer, "gitdir: {}", relative.display()).unwrap();
        assert_eq!(fixture.git(&["symbolic-ref", "--short", "HEAD"]), "artifact-selected");
        assert_ne!(
            fixture.git(&["rev-parse", "HEAD"]),
            checked(fixture.command("git").arg("-C").arg(&ordinary).args(["rev-parse", "HEAD"])).trim()
        );
    }
    fs::write(&expected, fixture.metadata()).unwrap();
    fixture
}

fn artifact_placement(linked: bool) {
    let directory = private_system_tempdir("gamma-artifacts-");
    let fixture = create_fixture(directory.path(), linked);
    let expected = fixture.home.join("expected-metadata");
    let launches = fixture.home.join("test-launches");
    let tracked: Vec<_> = fixture
        .git(&["ls-files", "-z"])
        .split('\0')
        .filter(|name| !name.is_empty())
        .map(|name| {
            let path = fixture.source.join(name);
            let bytes = fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    let refs = fixture.git(&["show-ref"]);
    let staged = fixture.git(&["ls-files", "--stage"]);
    let local = fixture.source.join("target");
    fixture.run(Some(&local));
    let local_campaign = only_directory(&local.join("cargo-gamma/cache"));
    let scratch = only_directory(&fixture.home.join("scratch/cargo-gamma"));
    let pointer = fs::read(scratch.join("workspace/.git")).unwrap();
    let local_builds = fixture.builds();
    assert!(!local_builds.is_empty());
    assert!(local_builds.iter().all(|path| path.starts_with(local_campaign.join("target"))));
    assert!(scratch.join("workspace/src/lib.rs").is_file());
    assert_ne!(
        fs::read(scratch.join("workspace/src/lib.rs")).unwrap(),
        fs::read(fixture.source.join("src/lib.rs")).unwrap()
    );
    let lock = File::options().read(true).write(true).open(scratch.join("lock")).unwrap();
    lock.try_lock().unwrap();
    let blocked = fixture
        .gamma("run", Some(&fixture.external))
        .args(["--lib", "--mutators", "relational.gt_to_eq"])
        .output()
        .unwrap();
    assert!(!blocked.status.success(), "{}", output_text(&blocked));
    assert!(output_text(&blocked).contains("already using"), "{}", output_text(&blocked));
    drop(lock);

    let report = fixture.source.join("target/cargo-gamma/gamma-report.json");
    fs::remove_file(&report).unwrap();
    fixture.run(Some(&fixture.external));
    let campaign = only_directory(&fixture.external.join("cargo-gamma/cache"));
    let builds = fixture.builds();
    let external_builds = &builds[local_builds.len()..];
    assert!(!external_builds.is_empty());
    for path in external_builds {
        assert!(path.starts_with(campaign.join("target")), "{}", path.display());
        assert_eq!(fs::read(path.join("git-metadata")).unwrap(), fs::read(&expected).unwrap());
    }
    assert_eq!(fs::read(scratch.join("workspace/.git")).unwrap(), pointer);
    assert_eq!(
        fs::canonicalize(fs::read_to_string(scratch.join("campaign-location")).unwrap()).unwrap(),
        fs::canonicalize(&campaign).unwrap()
    );
    assert!(campaign.join("last-gamma-run.json").is_file());
    assert!(report.is_file(), "an external campaign publishes reports in the checkout");
    let executions = fs::read_to_string(&launches).unwrap();
    assert!(executions.lines().any(|line| !line.is_empty()), "{executions}");

    // No target environment override: Cargo configuration selects the same external artifacts.
    let local_record = fs::read(local_campaign.join("last-gamma-run.json")).unwrap();
    fixture.run(None);
    assert_eq!(fs::read(local_campaign.join("last-gamma-run.json")).unwrap(), local_record);
    assert_eq!(
        fixture.builds(),
        builds,
        "unchanged build scripts should reuse their Cargo artifacts"
    );
    let fresh = fs::read_to_string(&launches).unwrap();
    assert!(fresh.len() > executions.len(), "a warm campaign must still execute tests");
    assert!(fresh[executions.len()..].lines().any(|line| !line.is_empty()), "{fresh}");

    // Only the locator can find the completed external record with the local target selected.
    fs::remove_file(local_campaign.join("last-gamma-run.json")).unwrap();
    checked(fixture.gamma("hints", Some(&local)).arg("--replace"));
    assert!(
        fs::read_to_string(fixture.source.join("gamma-hints.yaml"))
            .unwrap()
            .contains("killers:")
    );
    let suppression = checked(
        fixture
            .gamma("suppress", Some(&local))
            .args(["--eligible", "unviable", "--dry-run-suppress"]),
    );
    assert!(suppression.contains("gamma::skip"), "{suppression}");
    assert_eq!(fixture.git(&["show-ref"]), refs);
    assert_eq!(fixture.git(&["ls-files", "--stage"]), staged);
    for (path, bytes) in tracked {
        assert_eq!(fs::read(&path).unwrap(), bytes, "source changed: {}", path.display());
    }

    let mut refusal = fixture.gamma("run", None);
    let refusal = refusal.arg("--cache-dir").arg(fixture.home.join("all-in-one")).output().unwrap();
    assert!(!refusal.status.success());
    let refusal = output_text(&refusal);
    assert!(
        refusal.contains("CARGO_TARGET_DIR") && refusal.contains("build.target-dir"),
        "{refusal}"
    );

    verify_cleanup(&fixture, &campaign, &scratch, &local_campaign);
}

fn verify_cleanup(fixture: &Fixture, campaign: &Path, scratch: &Path, local_campaign: &Path) {
    let unrelated = fixture.external.join("unrelated-artifact");
    fs::write(&unrelated, "keep").unwrap();
    let other_workspace = campaign.parent().unwrap().join("other-workspace");
    fs::create_dir(&other_workspace).unwrap();
    fs::write(other_workspace.join("keep"), "keep").unwrap();
    checked(&mut fixture.gamma("clean", None));
    assert!(!campaign.join("target").exists());
    assert!(!campaign.join("last-gamma-run.json").exists());
    assert!(campaign.join(".cargo-gamma-owner").is_file());
    assert!(scratch.join("lock").is_file());
    assert!(!scratch.join("workspace").exists());
    assert!(local_campaign.join("target").exists());
    assert_eq!(fs::read_to_string(unrelated).unwrap(), "keep");
    assert_eq!(fs::read_to_string(other_workspace.join("keep")).unwrap(), "keep");
    assert!(fixture.source.join("target/cargo-gamma/gamma-report.json").is_file());
    assert!(fixture.source.join("gamma-hints.yaml").is_file());
}

#[test]
fn external_artifacts_preserve_ordinary_checkout_handling() {
    artifact_placement(false);
}

#[test]
fn external_artifacts_preserve_linked_worktree_handling() {
    artifact_placement(true);
}
