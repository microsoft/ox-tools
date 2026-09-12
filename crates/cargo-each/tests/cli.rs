// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! End-to-end tests for the `cargo-each` binary, driven through a temporary
//! fixture workspace so selection / filtering / execution are exercised
//! against real `cargo metadata`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// Write a fixture workspace with these members:
/// - `alpha` (lib with a `loom` feature and matching required-feature test),
/// - `beta` (bin only, with a path dev-dependency on `alpha` so `dep:alpha`
///   selects it),
/// - `gamma` (private lib carrying `[package.metadata.role] = "script-only"`),
/// - `delta` (lib, versioned `1.2.3-beta.1+build` to exercise prerelease /
///   build-metadata version matching end-to-end),
/// - `epsilon` (both a lib and a bin target, to exercise `--filter`
///   intersection).
fn fixture() -> (TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"alpha\", \"beta\", \"gamma\", \"delta\", \"epsilon\"]\n",
    )
    .expect("write workspace root");

    write_lib(
        root,
        "alpha",
        "0.1.0",
        "\n[features]\nloom = []\n\n[[test]]\nname = \"loom-test\"\npath = \"tests/loom.rs\"\nrequired-features = [\"loom\"]\n",
    );
    fs::create_dir_all(root.join("alpha/tests")).expect("mkdir alpha tests");
    fs::write(root.join("alpha/tests/loom.rs"), "#[test]\nfn loom_test() {}\n").expect("write loom test");
    write_bin(root, "beta", "\n[dev-dependencies]\nalpha = { path = \"../alpha\" }\n");
    fs::create_dir_all(root.join("beta/examples")).expect("mkdir beta examples");
    fs::write(root.join("beta/examples/demo.rs"), "fn main() {}\n").expect("write beta example");
    write_lib(
        root,
        "gamma",
        "0.1.0",
        "publish = false\n\n[package.metadata]\nrole = \"script-only\"\n",
    );
    write_lib(root, "delta", "1.2.3-beta.1+build", "");
    write_lib_and_bin(root, "epsilon");

    let manifest = root.join("Cargo.toml");
    (tmp, manifest)
}

/// Write a two-member workspace whose `default-members` is just `alpha`, to
/// exercise implicit default-member selection end-to-end.
fn default_members_fixture() -> (TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"alpha\", \"beta\"]\ndefault-members = [\"alpha\"]\n",
    )
    .expect("write workspace root");
    write_lib(root, "alpha", "0.1.0", "");
    write_bin(root, "beta", "");
    let manifest = root.join("Cargo.toml");
    (tmp, manifest)
}

fn write_lib(root: &Path, name: &str, version: &str, extra: &str) {
    let dir = root.join(name);
    fs::create_dir_all(dir.join("src")).expect("mkdir src");
    fs::write(
        dir.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2021\"\n{extra}"),
    )
    .expect("write member Cargo.toml");
    fs::write(dir.join("src/lib.rs"), "// fixture\n").expect("write lib.rs");
}

fn write_bin(root: &Path, name: &str, extra: &str) {
    let dir = root.join(name);
    fs::create_dir_all(dir.join("src")).expect("mkdir src");
    fs::write(
        dir.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{extra}"),
    )
    .expect("write member Cargo.toml");
    fs::write(dir.join("src/main.rs"), "fn main() {}\n").expect("write main.rs");
}

fn write_lib_and_bin(root: &Path, name: &str) {
    let dir = root.join(name);
    fs::create_dir_all(dir.join("src")).expect("mkdir src");
    fs::write(
        dir.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
    )
    .expect("write member Cargo.toml");
    fs::write(dir.join("src/lib.rs"), "// fixture\n").expect("write lib.rs");
    fs::write(dir.join("src/main.rs"), "fn main() {}\n").expect("write main.rs");
}

/// Build a `cargo-each each` invocation against the fixture at `manifest`.
fn each(manifest: &Path) -> Command {
    let mut cmd = Command::cargo_bin("cargo-each").expect("binary");
    cmd.arg("each").arg("--manifest-path").arg(manifest);
    cmd
}

fn sealed_containment_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();

    *AVAILABLE.get_or_init(|| cargo_gamma_process::containment().is_ok())
}

fn timeout_refusal() -> impl Predicate<str> {
    predicate::str::contains("timeout requires sealed process-tree containment").and(predicate::str::contains("child was not started"))
}

fn rust_version_fixture(root_floor: Option<&str>, members: &[(&str, Option<&str>)]) -> (TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let member_names = members.iter().map(|(name, _)| format!("\"{name}\"")).collect::<Vec<_>>().join(", ");
    let workspace_package = root_floor.map_or_else(String::new, |floor| format!("\n[workspace.package]\nrust-version = \"{floor}\"\n"));
    fs::write(
        root.join("Cargo.toml"),
        format!("[workspace]\nresolver = \"2\"\nmembers = [{member_names}]\n{workspace_package}"),
    )
    .expect("write workspace root");
    for (name, rust_version) in members {
        let declaration = match rust_version {
            Some("workspace") => "rust-version.workspace = true\n".to_owned(),
            Some(version) => format!("rust-version = \"{version}\"\n"),
            None => String::new(),
        };
        write_lib(root, name, "0.1.0", &declaration);
    }
    let manifest = root.join("Cargo.toml");
    (tmp, manifest)
}

fn single_package_fixture(rust_version: &str) -> (TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::create_dir_all(root.join("src")).expect("mkdir src");
    fs::write(
        root.join("Cargo.toml"),
        format!("[package]\nname = \"single\"\nversion = \"0.1.0\"\nedition = \"2021\"\nrust-version = \"{rust_version}\"\n"),
    )
    .expect("write package manifest");
    fs::write(root.join("src/lib.rs"), "// fixture\n").expect("write lib");
    let manifest = root.join("Cargo.toml");
    (tmp, manifest)
}

#[expect(
    clippy::too_many_lines,
    reason = "the embedded standalone probe stays together so rustc compiles one auditable cross-platform fixture"
)]
fn compile_execution_probe(directory: &Path) -> PathBuf {
    let source = directory.join("execution-probe.rs");
    let executable = directory.join(format!("execution-probe{}", std::env::consts::EXE_SUFFIX));
    fs::write(
        &source,
        r#"
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::process::{self, Command};
use std::thread;
use std::time::{Duration, Instant};

fn append(path: &str, value: &str) {
    let mut file = OpenOptions::new().create(true).append(true).open(path).expect("open log");
    writeln!(file, "{value}").expect("append log");
}

fn wait_for(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        if Instant::now() >= deadline {
            process::exit(90);
        }
        thread::sleep(Duration::from_millis(1));
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    match args[1].as_str() {
        "ordered" => {
            let name = &args[2];
            println!("{name}:start");
            thread::sleep(Duration::from_millis(if name == "alpha" { 250 } else { 20 }));
            println!("{name}:end");
            append(&args[3], name);
        }
        "fail-order" => {
            let name = &args[2];
            thread::sleep(Duration::from_millis(if name == "alpha" { 180 } else { 20 }));
            process::exit(if name == "alpha" { 7 } else { 9 });
        }
        "fail-stop" => {
            let name = &args[2];
            let sync_dir = std::path::Path::new(&args[3]);
            fs::write(sync_dir.join(format!("{name}.started")), "").expect("write start marker");
            match name.as_str() {
                "alpha" => {
                    wait_for(&sync_dir.join("beta.started"));
                    process::exit(7);
                }
                "beta" => {
                    wait_for(&sync_dir.join("alpha.started"));
                    process::exit(9);
                }
                _ => process::exit(0),
            }
        }
        "keep-going" => {
            let name = &args[2];
            append(&args[3], name);
            process::exit(if name == "alpha" { 7 } else { 0 });
        }
        "large-output" => {
            let name = &args[2];
            let size = 1_100_000;
            let stdout_byte = if name == "alpha" { b'A' } else { b'B' };
            let stderr_byte = if name == "alpha" { b'C' } else { b'D' };
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout, "{name}:stdout").expect("write stdout header");
            stdout.write_all(&vec![stdout_byte; size]).expect("write large stdout");
            let mut stderr = std::io::stderr().lock();
            writeln!(stderr, "{name}:stderr").expect("write stderr header");
            stderr.write_all(&vec![stderr_byte; size]).expect("write large stderr");
            process::exit(if name == "alpha" { 7 } else { 0 });
        }
        "timeout-fail-fast" => {
            if args[2] == "alpha" {
                thread::sleep(Duration::from_secs(5));
            } else {
                fs::write(&args[3], "later invocation ran").expect("write later marker");
            }
        }
        "timeout-keep-going" => {
            if args[2] == "alpha" {
                thread::sleep(Duration::from_secs(5));
            } else {
                fs::write(&args[3], "later invocation ran").expect("write later marker");
            }
        }
        "tree-parent" => {
            let marker = &args[2];
            Command::new(env::current_exe().expect("current exe"))
                .arg("tree-child")
                .arg(marker)
                .spawn()
                .expect("spawn tree child");
            thread::sleep(Duration::from_secs(5));
        }
        "tree-child" => {
            thread::sleep(Duration::from_millis(500));
            fs::write(&args[2], "survived").expect("write marker");
        }
        "background-parent" => {
            Command::new(env::current_exe().expect("current exe"))
                .arg("background-child")
                .arg(&args[2])
                .spawn()
                .expect("spawn background child");
        }
        "background-child" => {
            thread::sleep(Duration::from_millis(100));
            fs::write(&args[2], "completed").expect("write background marker");
        }
        other => panic!("unknown probe mode: {other}"),
    }
}
"#,
    )
    .expect("write execution probe");
    let output = std::process::Command::new("rustc")
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .output()
        .expect("rustc must be available to compile the execution probe");
    assert!(
        output.status.success(),
        "failed to compile execution probe:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    executable
}

#[cfg(windows)]
fn compile_probe(source: &Path, executable: &Path, marker: &str) {
    fs::write(source, format!("fn main() {{ println!(\"{marker}\"); }}\n")).expect("write probe source");
    let output = std::process::Command::new("rustc")
        .arg(source)
        .arg("-o")
        .arg(executable)
        .output()
        .expect("rustc must be available to compile the Windows resolution probe");
    assert!(
        output.status.success(),
        "failed to compile Windows resolution probe:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(windows)]
#[cfg_attr(miri, ignore = "spawns cargo-each and child processes; miri supports neither")]
#[test]
fn relative_child_program_uses_path_before_cargo_each_directory() {
    let (_workspace, manifest) = fixture();
    let layout = tempfile::tempdir().expect("resolution layout");
    let executable_dir = layout.path().join("executable");
    let path_dir = layout.path().join("path");
    fs::create_dir_all(&executable_dir).expect("create executable directory");
    fs::create_dir_all(&path_dir).expect("create PATH directory");

    let cargo_each = executable_dir.join("cargo-each.exe");
    fs::copy(assert_cmd::cargo::cargo_bin!("cargo-each"), &cargo_each).expect("copy cargo-each");

    let probe_name = "cargo-each-path-resolution-probe.exe";
    compile_probe(
        &layout.path().join("adjacent.rs"),
        &executable_dir.join(probe_name),
        "adjacent executable",
    );
    compile_probe(&layout.path().join("path.rs"), &path_dir.join(probe_name), "PATH executable");

    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let path =
        std::env::join_paths(std::iter::once(path_dir).chain(std::env::split_paths(&inherited))).expect("fixture PATH must be valid");

    Command::new(cargo_each)
        .arg("each")
        .arg("--manifest-path")
        .arg(manifest)
        .args(["--package", "alpha", "--once", "--", probe_name])
        .env("PATH", path)
        .assert()
        .success()
        .stdout(predicate::str::contains("PATH executable"))
        .stdout(predicate::str::contains("adjacent executable").not());
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn once_whole_workspace_expands_to_workspace_flag() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["--workspace", "--once", "--dry-run", "--", "cargo", "clippy", "{packages}"])
        .assert()
        .success()
        .stdout(predicate::str::contains("cargo clippy --workspace"));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn per_package_runs_once_per_selected_member() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["-p", "alpha", "-p", "gamma", "--dry-run", "--", "echo", "{name}"])
        .assert()
        .success()
        .stdout(predicate::str::contains("echo alpha").and(predicate::str::contains("echo gamma")));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn package_files_union_with_direct_packages_and_each_other() {
    let (tmp, manifest) = fixture();
    let first = tmp.path().join("first.packages");
    let second = tmp.path().join("second.packages");
    fs::write(&first, "alpha\n\ngamma@0.1\n").expect("write first package file");
    fs::write(&second, "beta\n").expect("write second package file");

    each(&manifest)
        .arg("--package-file")
        .arg(&first)
        .arg("--package-file")
        .arg(&second)
        .args(["--package", "delta", "--dry-run", "--", "echo", "{name}"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("echo alpha")
                .and(predicate::str::contains("echo beta"))
                .and(predicate::str::contains("echo delta"))
                .and(predicate::str::contains("echo gamma")),
        );
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn present_empty_package_file_is_an_explicit_empty_selection() {
    let (tmp, manifest) = default_members_fixture();
    let packages = tmp.path().join("empty.packages");
    fs::write(&packages, "").expect("write empty package file");

    each(&manifest)
        .arg("--package-file")
        .arg(packages)
        .args(["--dry-run", "--", "echo", "{name}"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("nothing to do"));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn package_file_precedence_matches_the_selection_contract() {
    let (tmp, manifest) = fixture();
    let packages = tmp.path().join("alpha.packages");
    fs::write(&packages, "alpha\n").expect("write package file");

    each(&manifest)
        .arg("--package-file")
        .arg(&packages)
        .args(["--workspace", "--dry-run", "--", "echo", "{name}"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("echo alpha")
                .and(predicate::str::contains("echo beta"))
                .and(predicate::str::contains("echo gamma")),
        );

    each(&manifest)
        .arg("--package-file")
        .arg(packages)
        .args(["--none", "--dry-run", "--", "echo", "{name}"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("nothing to do"));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn package_file_input_errors_fail_loudly() {
    let (tmp, manifest) = fixture();
    let invalid_utf8 = tmp.path().join("invalid-utf8.packages");
    fs::write(&invalid_utf8, [0xFF, 0xFE]).expect("write invalid UTF-8");
    each(&manifest)
        .arg("--package-file")
        .arg(&invalid_utf8)
        .args(["--dry-run", "--", "echo", "{name}"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("not valid UTF-8"));

    let malformed = tmp.path().join("malformed.packages");
    fs::write(&malformed, "alpha\n--workspace\n").expect("write malformed package file");
    each(&manifest)
        .arg("--package-file")
        .arg(&malformed)
        .args(["--dry-run", "--", "echo", "{name}"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("line 2").and(predicate::str::contains("command-line tokens")));

    let comment = tmp.path().join("comment.packages");
    fs::write(&comment, "#alpha\n").expect("write package file comment");
    each(&manifest)
        .arg("--package-file")
        .arg(&comment)
        .args(["--dry-run", "--", "echo", "{name}"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("comments are not supported"));

    let unmatched = tmp.path().join("unmatched.packages");
    fs::write(&unmatched, "does-not-exist\n").expect("write unmatched package file");
    each(&manifest)
        .arg("--package-file")
        .arg(&unmatched)
        .args(["--dry-run", "--", "echo", "{name}"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("did not match"));

    each(&manifest)
        .arg("--package-file")
        .arg(tmp.path().join("missing.packages"))
        .args(["--dry-run", "--", "echo", "{name}"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("could not read package file"));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn none_is_a_successful_noop() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["--none", "--once", "--dry-run", "--", "cargo", "test", "{packages}"])
        .assert()
        .success()
        .stderr(predicate::str::contains("nothing to do"));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn none_with_misused_placeholder_is_a_usage_error() {
    // Even with an empty selection, a per-package token under --once is a
    // usage error (exit 2), not a silent no-op.
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["--none", "--once", "--dry-run", "--", "echo", "{name}"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("{name}"));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn version_spec_partial_matches_over_metadata() {
    // End-to-end over real `cargo metadata`: a partial version qualifier
    // (cargo package-id-spec) resolves the member (fixtures are 0.1.0).
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["-p", "alpha@0.1", "--dry-run", "--", "echo", "{spec}"])
        .assert()
        .success()
        .stdout(predicate::str::contains("echo alpha@0.1.0"));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn version_spec_mismatch_is_a_usage_error_over_metadata() {
    // A non-matching version fails loudly (exit 2) rather than silently
    // selecting the name — the metadata -> selection -> exit-2 path.
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["-p", "alpha@9.9.9", "--dry-run", "--", "echo", "{name}"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("did not match"));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn prerelease_version_spec_matches_exactly_over_metadata() {
    // End-to-end over real `cargo metadata`, exercising the manifest ->
    // metadata -> selector projection of the prerelease/build-metadata rules
    // against the `delta` fixture member (version `1.2.3-beta.1+build`).
    let (_tmp, manifest) = fixture();
    // Exact prerelease matches; build metadata is ignored on the qualifier.
    each(&manifest)
        .args(["-p", "delta@1.2.3-beta.1", "--dry-run", "--", "echo", "{spec}"])
        .assert()
        .success()
        .stdout(predicate::str::contains("echo delta@1.2.3-beta.1+build"));
    // A qualifier without a prerelease does not match a prerelease version.
    each(&manifest)
        .args(["-p", "delta@1.2.3", "--dry-run", "--", "echo", "{name}"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("did not match"));
    // A different (shorter) prerelease does not match.
    each(&manifest)
        .args(["-p", "delta@1.2.3-beta", "--dry-run", "--", "echo", "{name}"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("did not match"));
    // Build metadata, when supplied, is an exact constraint.
    each(&manifest)
        .args(["-p", "delta@1.2.3-beta.1+wrong", "--dry-run", "--", "echo", "{name}"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("did not match"));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn malformed_version_spec_is_rejected_over_metadata() {
    // End-to-end: a malformed version qualifier cargo's grammar rejects (empty
    // prerelease/build suffix, leading-zero component) matches no member, so it
    // is a loud usage error (exit 2, "did not match") rather than silently
    // resolving `alpha` as its well-formed `0.1.0` prefix would.
    let (_tmp, manifest) = fixture();
    for spec in ["alpha@0.1.0-", "alpha@0.1.0+", "alpha@01.0.0"] {
        each(&manifest)
            .args(["-p", spec, "--dry-run", "--", "echo", "{name}"])
            .assert()
            .failure()
            .code(2)
            .stderr(predicate::str::contains("did not match"));
    }
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn multiple_filters_intersect() {
    // `--filter` is AND-combined: `lib` matches {alpha, gamma, delta, epsilon},
    // `bin` matches {beta, epsilon}; the intersection is only `epsilon` (both a
    // lib and a bin). A flip to `any()` would wrongly keep every member.
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args([
            "--workspace",
            "--filter",
            "lib",
            "--filter",
            "bin",
            "--dry-run",
            "--",
            "echo",
            "{name}",
        ])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("echo epsilon")
                .and(predicate::str::contains("echo alpha").not())
                .and(predicate::str::contains("echo beta").not())
                .and(predicate::str::contains("echo gamma").not()),
        );
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn boolean_filter_supports_grouped_or() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args([
            "--workspace",
            "--filter",
            "publishable and (feature:loom or target-kind:bin)",
            "--dry-run",
            "--",
            "echo",
            "{name}",
        ])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("echo alpha")
                .and(predicate::str::contains("echo beta"))
                .and(predicate::str::contains("echo epsilon"))
                .and(predicate::str::contains("echo gamma").not())
                .and(predicate::str::contains("echo delta").not()),
        );
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn richer_package_predicates_use_cargo_metadata() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args([
            "--workspace",
            "--filter",
            "target-kind:example",
            "--filter",
            "publishable",
            "--dry-run",
            "--",
            "echo",
            "{name}",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("echo beta").and(predicate::str::contains("echo gamma").not()));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn publishable_filter_excludes_publish_false() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args([
            "-p",
            "alpha",
            "-p",
            "gamma",
            "--filter",
            "publishable",
            "--dry-run",
            "--",
            "echo",
            "{name}",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("echo alpha").and(predicate::str::contains("echo gamma").not()));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn multiple_exclude_filters_union() {
    // `--exclude-filter` is OR-combined: dropping `bin` removes {beta, epsilon}
    // and dropping `metadata:role=script-only` removes {gamma}; their union
    // leaves {alpha, delta}. A flip to `all()` would drop nothing (no member
    // matches both exclusions).
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args([
            "--workspace",
            "--exclude-filter",
            "bin",
            "--exclude-filter",
            "metadata:role=script-only",
            "--dry-run",
            "--",
            "echo",
            "{name}",
        ])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("echo alpha")
                .and(predicate::str::contains("echo delta"))
                .and(predicate::str::contains("echo beta").not())
                .and(predicate::str::contains("echo gamma").not())
                .and(predicate::str::contains("echo epsilon").not()),
        );
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn filter_lib_drops_bin_only_member() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["--workspace", "--filter", "lib", "--dry-run", "--", "tool", "{name}"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("tool alpha")
                .and(predicate::str::contains("tool gamma"))
                .and(predicate::str::contains("tool beta").not()),
        );
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn exclude_filter_metadata_drops_matching_member() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args([
            "--workspace",
            "--exclude-filter",
            "metadata:role=script-only",
            "--dry-run",
            "--",
            "tool",
            "{name}",
        ])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("tool gamma")
                .not()
                .and(predicate::str::contains("tool alpha")),
        );
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn exclude_requires_workspace() {
    // `--exclude` is documented as requiring `--workspace`; clap enforces it,
    // so `--exclude` with an implicit / `-p` selection is a usage error (2).
    // Assert the message names `--workspace` so the test is tied to *this*
    // constraint, not any incidental exit-2 parse error.
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["-p", "alpha", "--exclude", "beta", "--dry-run", "--", "echo", "{name}"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("--workspace"));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn exclude_with_workspace_removes_member() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["--workspace", "--exclude", "beta", "--dry-run", "--", "echo", "{name}"])
        .assert()
        .success()
        .stdout(predicate::str::contains("echo alpha").and(predicate::str::contains("echo beta").not()));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn chdir_shows_crate_root_in_dry_run() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["-p", "alpha", "--chdir", "--dry-run", "--", "echo", "{name}"])
        .assert()
        .success()
        .stdout(predicate::str::contains("(cd ").and(predicate::str::contains("alpha")));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn chdir_with_once_is_a_usage_error() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["--workspace", "--chdir", "--once", "--dry-run", "--", "echo", "{packages}"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("--chdir").and(predicate::str::contains("--once")));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn per_target_mode_filters_required_features_and_substitutes_target() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args([
            "--workspace",
            "--each-target",
            "test",
            "--target-required-feature",
            "loom",
            "--dry-run",
            "--",
            "echo",
            "{name}::{target}",
        ])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("echo alpha::loom-test")
                .and(predicate::str::contains("beta").not())
                .and(predicate::str::contains("epsilon").not()),
        );
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn per_target_rejects_unknown_kind() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args([
            "--workspace",
            "--each-target",
            "unknown-kind",
            "--dry-run",
            "--",
            "echo",
            "{target}",
        ])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("invalid target kind"));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn per_target_and_once_are_mutually_exclusive() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args([
            "--workspace",
            "--each-target",
            "test",
            "--once",
            "--dry-run",
            "--",
            "echo",
            "{target}",
        ])
        .assert()
        .failure()
        .code(2);
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn target_required_feature_requires_target_mode() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args([
            "--workspace",
            "--target-required-feature",
            "loom",
            "--dry-run",
            "--",
            "echo",
            "{name}",
        ])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("--each-target"));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn unknown_selector_is_a_usage_error() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["-p", "does-not-exist", "--dry-run", "--", "echo", "{name}"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("did not match"));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn bad_filter_expression_is_a_usage_error() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["--workspace", "--filter", "nonsense", "--dry-run", "--", "echo", "{name}"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("invalid filter expression"));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn executes_command_and_propagates_success() {
    // Actually spawns a command (not --dry-run) to cover the execution path.
    // `cargo --version` is available on every CI runner and is a no-op.
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["--workspace", "--once", "--", "cargo", "--version"])
        .assert()
        .success();
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn executes_command_and_propagates_failure() {
    // A failing child command's exit code propagates (fail-fast). The exact
    // code is intentionally left unspecified here: it is cargo's
    // no-such-subcommand code, which is not a stable contract we want to pin.
    // The `exit_byte` unit tests cover the low-byte reduction arithmetic
    // directly.
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["-p", "alpha", "--", "cargo", "this-subcommand-does-not-exist"])
        .assert()
        .failure();
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn chdir_executes_from_crate_root() {
    // Observe that --chdir actually set the child's working directory:
    // `cargo locate-project` resolves the closest Cargo.toml from the CWD, so
    // it must report the *member's* manifest. A dropped or miswired
    // `current_dir` would report a different manifest and fail this assertion.
    let (_tmp, manifest) = fixture();
    // Match the `alpha/Cargo.toml` suffix rather than the full temp path, so
    // the assertion is robust to any canonicalization of the parent dirs.
    let member_suffix = format!("alpha{}Cargo.toml", std::path::MAIN_SEPARATOR);
    each(&manifest)
        .args([
            "-p",
            "alpha",
            "--chdir",
            "--",
            "cargo",
            "locate-project",
            "--message-format",
            "plain",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(member_suffix));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn keep_going_runs_all_members_then_fails() {
    // With --keep-going every member runs even though the command fails for
    // each; the overall exit is a flat 1 (the documented contract), not any
    // individual child's exit code.
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args([
            "-p",
            "alpha",
            "-p",
            "gamma",
            "--keep-going",
            "--",
            "cargo",
            "this-subcommand-does-not-exist",
        ])
        .assert()
        .failure()
        .code(1);
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn keep_going_treats_spawn_failure_as_invocation_failure() {
    // A program that cannot be spawned at all (the binary does not exist) is a
    // failed invocation under --keep-going, not an abort: every member is still
    // attempted and the exit is a flat 1 — not the exit-2 usage code the
    // fail-fast path returns for a spawn failure.
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args([
            "-p",
            "alpha",
            "-p",
            "gamma",
            "--keep-going",
            "--",
            "cargo-each-no-such-program-xyz",
            "{name}",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(
            predicate::str::contains("cargo each: alpha")
                .and(predicate::str::contains("cargo each: gamma"))
                .and(predicate::str::contains("failed to spawn")),
        );
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn spawn_failure_without_keep_going_is_a_usage_error() {
    // Without --keep-going, an un-spawnable program aborts as a cargo-each
    // failure (exit 2 via main.rs), distinct from a child that spawned and then
    // exited non-zero (which propagates its own code).
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["-p", "alpha", "--", "cargo-each-no-such-program-xyz"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("failed to spawn"));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn dry_run_quotes_arguments_with_whitespace() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["--workspace", "--once", "--dry-run", "--", "echo", "a b"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"a b\""));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn once_with_filter_uses_explicit_packages_not_workspace() {
    // A `--filter` narrows the whole workspace, so `{packages}` must expand to
    // an explicit `--package` list rather than a bare `--workspace`.
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args([
            "--workspace",
            "--filter",
            "lib",
            "--once",
            "--dry-run",
            "--",
            "cargo",
            "x",
            "{packages}",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("--package alpha").and(predicate::str::contains("--workspace").not()));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn once_with_boolean_filter_uses_explicit_packages_not_workspace() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args([
            "--workspace",
            "--filter",
            "feature:loom",
            "--once",
            "--dry-run",
            "--",
            "cargo",
            "x",
            "{packages}",
        ])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("--package alpha")
                .and(predicate::str::contains("--workspace").not())
                .and(predicate::str::contains("--package beta").not()),
        );
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn once_with_exclude_filter_uses_explicit_packages_not_workspace() {
    // An `--exclude-filter` also narrows the set, so `{packages}` must expand
    // to an explicit `--package` list, not `--workspace`.
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args([
            "--workspace",
            "--exclude-filter",
            "metadata:role=script-only",
            "--once",
            "--dry-run",
            "--",
            "cargo",
            "x",
            "{packages}",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("--package").and(predicate::str::contains("--workspace").not()));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn fail_fast_stops_before_running_later_members() {
    // Without --keep-going, a failure on the first member must stop the run:
    // cargo-each prints a per-member label to stderr before each command, so a
    // stopped run never prints the second member's label.
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["-p", "alpha", "-p", "gamma", "--", "cargo", "this-subcommand-does-not-exist"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cargo each: alpha").and(predicate::str::contains("cargo each: gamma").not()));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn dep_filter_selects_member_with_declared_dependency() {
    // End-to-end over real `cargo metadata`: the `dep:` predicate is fed by the
    // per-member dependency projection. Only `beta` declares `alpha` (as a path
    // dev-dependency), so `--filter dep:alpha` must select it and nothing else.
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["--workspace", "--filter", "dep:alpha", "--dry-run", "--", "echo", "{name}"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("echo beta")
                .and(predicate::str::contains("echo alpha").not())
                .and(predicate::str::contains("echo gamma").not()),
        );
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn bare_invocation_runs_only_default_members() {
    // With no explicit selection, cargo-each falls back to the workspace's
    // `default-members`, exercising the `default-members` projection end-to-end.
    let (_tmp, manifest) = default_members_fixture();
    each(&manifest)
        .args(["--dry-run", "--", "echo", "{name}"])
        .assert()
        .success()
        .stdout(predicate::str::contains("echo alpha").and(predicate::str::contains("echo beta").not()));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn workspace_rust_version_expands_in_all_modes_and_accepts_lower_members() {
    let (_tmp, manifest) = rust_version_fixture(Some("1.80"), &[("alpha", Some("1.70")), ("beta", Some("workspace"))]);
    each(&manifest)
        .args(["--workspace", "--dry-run", "--", "echo", "{name}:{workspace-rust-version}"])
        .assert()
        .success()
        .stdout(predicate::str::contains("echo alpha:1.80").and(predicate::str::contains("echo beta:1.80")));

    each(&manifest)
        .args([
            "--workspace",
            "--each-target",
            "lib",
            "--dry-run",
            "--",
            "echo",
            "{target}:{workspace-rust-version}",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(":1.80"));

    each(&manifest)
        .args([
            "--package",
            "alpha",
            "--once",
            "--dry-run",
            "--",
            "echo",
            "{workspace-rust-version}",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("echo 1.80"));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn workspace_rust_version_validation_is_lazy() {
    let (_tmp, manifest) = rust_version_fixture(Some("1.80"), &[("alpha", Some("1.70")), ("beta", None)]);
    each(&manifest)
        .args(["--workspace", "--dry-run", "--", "echo", "{name}"])
        .assert()
        .success();
    each(&manifest)
        .args([
            "--package",
            "alpha",
            "--once",
            "--dry-run",
            "--",
            "echo",
            "{workspace-rust-version}",
        ])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("beta").and(predicate::str::contains("rust-version")));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn workspace_rust_version_rejects_newer_members() {
    let (_tmp, manifest) = rust_version_fixture(Some("1.80"), &[("alpha", Some("1.81")), ("beta", Some("1.70"))]);
    each(&manifest)
        .args(["--workspace", "--once", "--dry-run", "--", "echo", "{workspace-rust-version}"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("alpha").and(predicate::str::contains("newer than")));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn workspace_rust_version_rejects_missing_or_invalid_root_floor() {
    let (_missing, missing_manifest) = rust_version_fixture(None, &[("alpha", Some("1.70"))]);
    each(&missing_manifest)
        .args(["--workspace", "--once", "--dry-run", "--", "echo", "{workspace-rust-version}"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("[workspace.package].rust-version"));

    let (_invalid, invalid_manifest) = rust_version_fixture(Some("2.0"), &[("alpha", Some("1.70"))]);
    each(&invalid_manifest)
        .args(["--workspace", "--dry-run", "--", "echo", "{name}"])
        .assert()
        .success();
    each(&invalid_manifest)
        .args(["--workspace", "--once", "--dry-run", "--", "echo", "{workspace-rust-version}"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("2.0").and(predicate::str::contains("Rust 1.x")));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn workspace_rust_version_uses_root_package_for_single_package_repository() {
    let (_tmp, manifest) = single_package_fixture("1.75");
    each(&manifest)
        .args(["--once", "--dry-run", "--", "echo", "{workspace-rust-version}"])
        .assert()
        .success()
        .stdout(predicate::str::contains("echo 1.75"));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn once_rejects_jobs_greater_than_one() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["--workspace", "--once", "--jobs", "2", "--dry-run", "--", "echo", "{packages}"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("--jobs").and(predicate::str::contains("--once")));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn parallel_output_is_buffered_in_plan_order() {
    let (tmp, manifest) = fixture();
    let probe = compile_execution_probe(tmp.path());
    let completion_log = tmp.path().join("completion.log");
    let output = each(&manifest)
        .args(["-p", "alpha", "-p", "beta", "--jobs", "2", "--"])
        .arg(&probe)
        .args(["ordered", "{name}"])
        .arg(&completion_log)
        .output()
        .expect("run cargo-each");
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 probe output");
    let alpha = stdout.find("alpha:start").expect("alpha output");
    let beta = stdout.find("beta:start").expect("beta output");
    assert!(alpha < beta, "buffered blocks must follow plan order:\n{stdout}");
    assert_eq!(
        fs::read_to_string(completion_log).expect("completion log"),
        "beta\nalpha\n",
        "the probe must finish out of order to prove cargo-each reordered complete blocks"
    );
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn parallel_fail_fast_chooses_failure_by_plan_order() {
    let (tmp, manifest) = fixture();
    let probe = compile_execution_probe(tmp.path());
    each(&manifest)
        .args(["-p", "alpha", "-p", "beta", "--jobs", "2", "--"])
        .arg(probe)
        .args(["fail-order", "{name}"])
        .assert()
        .failure()
        .code(7);
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn parallel_fail_fast_stops_launching_new_work() {
    let (tmp, manifest) = fixture();
    let probe = compile_execution_probe(tmp.path());
    let sync_dir = tmp.path().join("fail-stop");
    fs::create_dir(&sync_dir).expect("create synchronization directory");
    each(&manifest)
        .args(["--workspace", "--jobs", "2", "--"])
        .arg(probe)
        .args(["fail-stop", "{name}"])
        .arg(&sync_dir)
        .assert()
        .failure()
        .code(7);
    assert!(sync_dir.join("alpha.started").exists(), "the first initial worker must start");
    assert!(sync_dir.join("beta.started").exists(), "the second initial worker must start");
    for name in ["delta", "epsilon", "gamma"] {
        assert!(
            !sync_dir.join(format!("{name}.started")).exists(),
            "{name} must not launch after either synchronized initial worker fails"
        );
    }
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn parallel_keep_going_runs_the_complete_plan() {
    let (tmp, manifest) = fixture();
    let probe = compile_execution_probe(tmp.path());
    let launch_log = tmp.path().join("launch.log");
    each(&manifest)
        .args(["--workspace", "--jobs", "2", "--keep-going", "--"])
        .arg(probe)
        .args(["keep-going", "{name}"])
        .arg(&launch_log)
        .assert()
        .failure()
        .code(1);
    let launched = fs::read_to_string(launch_log).expect("launch log");
    for name in ["alpha", "beta", "delta", "epsilon", "gamma"] {
        assert!(launched.contains(name), "{name} must run under --keep-going:\n{launched}");
    }
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn parallel_spills_large_stdout_and_stderr_without_truncating_plan_order() {
    let (tmp, manifest) = fixture();
    let probe = compile_execution_probe(tmp.path());
    let output = each(&manifest)
        .args(["-p", "alpha", "-p", "beta", "--jobs", "2", "--keep-going", "--"])
        .arg(probe)
        .args(["large-output", "{name}"])
        .output()
        .expect("run cargo-each with large output");

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("probe stdout is ASCII");
    let stderr = String::from_utf8(output.stderr).expect("probe stderr is ASCII");
    assert!(stdout.find("alpha:stdout").expect("alpha stdout header") < stdout.find("beta:stdout").expect("beta stdout header"));
    assert!(stderr.find("alpha:stderr").expect("alpha stderr header") < stderr.find("beta:stderr").expect("beta stderr header"));
    assert_eq!(stdout.bytes().filter(|byte| *byte == b'A').count(), 1_100_000);
    assert_eq!(stdout.bytes().filter(|byte| *byte == b'B').count(), 1_100_000);
    assert_eq!(stderr.bytes().filter(|byte| *byte == b'C').count(), 1_100_000);
    assert_eq!(stderr.bytes().filter(|byte| *byte == b'D').count(), 1_100_000);
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn parallel_without_timeout_preserves_ordinary_background_descendants() {
    let (tmp, manifest) = fixture();
    let probe = compile_execution_probe(tmp.path());
    let marker = tmp.path().join("background-completed");
    each(&manifest)
        .args(["-p", "alpha", "--jobs", "2", "--"])
        .arg(probe)
        .arg("background-parent")
        .arg(&marker)
        .assert()
        .success();
    assert!(
        marker.exists(),
        "parallel execution without --timeout must not kill an ordinary background descendant"
    );
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn sequential_timeout_fail_fast_does_not_run_later_members() {
    let (tmp, manifest) = fixture();
    let probe = compile_execution_probe(tmp.path());
    let later_marker = tmp.path().join("later-invocation");
    let assertion = each(&manifest)
        .args(["-p", "alpha", "-p", "beta", "--timeout", "50ms", "--"])
        .arg(probe)
        .args(["timeout-fail-fast", "{name}"])
        .arg(&later_marker)
        .assert()
        .failure();
    if sealed_containment_available() {
        assertion.code(1).stderr(predicate::str::contains("timed out after 50ms"));
    } else {
        assertion.code(2).stderr(timeout_refusal());
    }
    assert!(
        !later_marker.exists(),
        "fail-fast or pre-spawn refusal must not launch the later member"
    );
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn sequential_timeout_keep_going_runs_later_members() {
    let (tmp, manifest) = fixture();
    let probe = compile_execution_probe(tmp.path());
    let later_marker = tmp.path().join("later-invocation");
    let assertion = each(&manifest)
        .args(["-p", "alpha", "-p", "beta", "--timeout", "50ms", "--keep-going", "--"])
        .arg(probe)
        .args(["timeout-keep-going", "{name}"])
        .arg(&later_marker)
        .assert()
        .failure();
    if sealed_containment_available() {
        assertion.code(1).stderr(predicate::str::contains("timed out after 50ms"));
        assert!(
            later_marker.exists(),
            "--keep-going must launch the member after a timed-out invocation"
        );
    } else {
        assertion.code(1).stderr(timeout_refusal());
        assert!(
            !later_marker.exists(),
            "unsealed containment must refuse every timed child before spawn"
        );
    }
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn timeout_terminates_the_complete_process_tree() {
    let (tmp, manifest) = fixture();
    let probe = compile_execution_probe(tmp.path());
    let marker = tmp.path().join("grandchild-survived");
    let assertion = each(&manifest)
        .args(["-p", "alpha", "--jobs", "2", "--timeout", "50ms", "--"])
        .arg(probe)
        .arg("tree-parent")
        .arg(&marker)
        .assert()
        .failure();
    if sealed_containment_available() {
        assertion.code(1).stderr(predicate::str::contains("timed out after 50ms"));
    } else {
        assertion.code(2).stderr(timeout_refusal());
    }
    std::thread::sleep(std::time::Duration::from_millis(700));
    assert!(
        !marker.exists(),
        "a timed-out grandchild must be terminated, and an unsupported timed child must never start"
    );
}
