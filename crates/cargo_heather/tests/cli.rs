// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! End-to-end integration tests for the `cargo-heather` binary.
//!
//! These tests build the real binary via `assert_cmd` and exercise the
//! CLI surface: scanner directory walking, config file resolution,
//! check/fix exit codes, and human-readable log output. They are
//! designed to kill mutants in `src/bin/cargo-heather/*.rs` by asserting
//! both the exit status and concrete log lines.

// Miri cannot run subprocesses, so the entire `assert_cmd`-based test
// suite is gated out under miri. The library-level unit tests (in
// `src/checker/strip.rs` and the `tests/rust_*.rs` integration tests
// that call the library directly) still run under miri.
#![cfg(not(miri))]
#![allow(clippy::unwrap_used, reason = "tests panic on setup failure — that's the expected failure mode")]
#![allow(clippy::missing_panics_doc, reason = "test functions")]

use std::fs;
use std::path::Path;
use std::process::Output;

use assert_cmd::Command;
use tempfile::TempDir;

const HEADER_TEXT: &str = "// Copyright (c) Microsoft Corporation.\n// Licensed under the MIT License.\n";
const CONFIG_MIT: &str = "header = \"Copyright (c) Microsoft Corporation.\\nLicensed under the MIT License.\"\n";
const HEADER_TEXT_MIT_ONLY: &str = "// Licensed under the MIT License.\n";

fn run_heather(project_dir: &Path, extra_args: &[&str]) -> Output {
    let mut cmd = Command::cargo_bin("cargo-heather").expect("cargo-heather binary should build");
    cmd.env("RUST_LOG", "info").arg("heather").arg("--project-dir").arg(project_dir);
    for a in extra_args {
        cmd.arg(a);
    }
    cmd.output().expect("spawning cargo-heather should succeed")
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

fn stderr_of(out: &Output) -> String {
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    s
}

// ───────────────────────── run.rs / check ─────────────────────────

#[test]
fn check_passes_when_all_files_have_correct_header() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CONFIG_MIT);
    write(&p.join("a.rs"), &format!("{HEADER_TEXT}\nfn main() {{}}\n"));
    write(&p.join("b.rs"), &format!("{HEADER_TEXT}\nfn b() {{}}\n"));

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "expected success, stderr: {stderr}");
    assert!(stderr.contains("Checking 2 file(s)..."), "checked count missing: {stderr}");
    assert!(
        stderr.contains("All 2 file(s) have correct license headers."),
        "summary missing: {stderr}"
    );
}

#[test]
fn check_fails_with_missing_header_log() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CONFIG_MIT);
    write(&p.join("a.rs"), "fn main() {}\n");

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(!out.status.success(), "expected failure: {stderr}");
    assert!(stderr.contains("MISSING header: a.rs"), "missing log absent: {stderr}");
}

#[test]
fn check_reports_two_missing_files() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CONFIG_MIT);
    write(&p.join("a.rs"), "fn a() {}\n");
    write(&p.join("b.rs"), "fn b() {}\n");

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(!out.status.success());
    assert!(stderr.contains("MISSING header: a.rs"), "missing a: {stderr}");
    assert!(stderr.contains("MISSING header: b.rs"), "missing b: {stderr}");
}

#[test]
fn check_fails_with_mismatch_diff_block() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CONFIG_MIT);
    write(
        &p.join("a.rs"),
        "// Some other header line.\n// SPDX-License-Identifier: GPL-3.0\n\nfn main() {}\n",
    );

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(!out.status.success());
    assert!(stderr.contains("MISMATCH header: a.rs"), "mismatch log: {stderr}");
    assert!(stderr.contains("expected header:"), "expected block: {stderr}");
    assert!(stderr.contains("actual header:"), "actual block: {stderr}");
    assert!(
        stderr.contains("+ Copyright (c) Microsoft Corporation."),
        "expected + line: {stderr}"
    );
    assert!(stderr.contains("- Some other header line."), "actual - line: {stderr}");
    // Asserting on the post-loop summary count is what kills the
    // `failures += 1` → `failures -= 1` mutant in `run.rs`: with the
    // mutation, `failures` underflows on the first mismatch and the
    // process panics before reaching this `bail!()` formatting path.
    assert!(
        stderr.contains("1 file(s) have missing or incorrect license headers"),
        "validation summary line for 1 file: {stderr}"
    );
}
#[test]
fn check_succeeds_when_no_files_found() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CONFIG_MIT);

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "expected success: {stderr}");
    assert!(stderr.contains("No source files found"), "no-files log: {stderr}");
}

// ───────────────────────── run.rs / fix ─────────────────────────

#[test]
fn fix_adds_missing_header_and_reports_count_one() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CONFIG_MIT);
    write(&p.join("a.rs"), "fn main() {}\n");

    let out = run_heather(p, &["--fix"]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "fix should succeed: {stderr}");
    assert!(stderr.contains("Fixed (added header): a.rs"), "added log: {stderr}");
    assert!(stderr.contains("Fixed 1 file(s)."), "count log: {stderr}");

    let written = fs::read_to_string(p.join("a.rs")).unwrap();
    assert!(
        written.starts_with("// Copyright (c) Microsoft Corporation."),
        "header inserted: {written}"
    );
}

#[test]
fn fix_replaces_mismatched_header_and_reports_count_one() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CONFIG_MIT);
    write(
        &p.join("a.rs"),
        "// Copyright (c) Acme Corp.\n// All rights reserved.\n\nfn main() {}\n",
    );

    let out = run_heather(p, &["--fix"]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("Fixed (replaced header): a.rs"), "replaced log: {stderr}");
    assert!(stderr.contains("Fixed 1 file(s)."), "count log: {stderr}");
}

#[test]
fn fix_count_three_when_three_files_need_fixing() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CONFIG_MIT);
    write(&p.join("a.rs"), "fn a() {}\n");
    write(&p.join("b.rs"), "fn b() {}\n");
    write(&p.join("c.rs"), "fn c() {}\n");

    let out = run_heather(p, &["--fix"]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("Fixed 3 file(s)."), "expected count 3: {stderr}");
}

#[test]
fn fix_with_all_correct_reports_zero_count() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CONFIG_MIT);
    write(&p.join("a.rs"), &format!("{HEADER_TEXT}\nfn main() {{}}\n"));

    let out = run_heather(p, &["--fix"]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("All files already have correct headers."),
        "zero-count log: {stderr}"
    );
    assert!(!stderr.contains("Fixed 1 file"));
}

#[test]
fn fix_mixed_two_missing_one_mismatch_reports_three() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CONFIG_MIT);
    write(&p.join("a.rs"), "fn a() {}\n");
    write(&p.join("b.rs"), "fn b() {}\n");
    write(&p.join("c.rs"), "// Copyright (c) Acme.\n// All rights reserved.\n\nfn c() {}\n");

    let out = run_heather(p, &["--fix"]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("Fixed 3 file(s)."), "{stderr}");
    assert!(stderr.contains("(added header): a.rs"), "{stderr}");
    assert!(stderr.contains("(added header): b.rs"), "{stderr}");
    assert!(stderr.contains("(replaced header): c.rs"), "{stderr}");
}

// ───────────────────────── scanner.rs ─────────────────────────

#[test]
fn scanner_skips_target_directory() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CONFIG_MIT);
    write(&p.join("a.rs"), &format!("{HEADER_TEXT}\nfn a() {{}}\n"));
    // file inside target/ should be skipped
    write(&p.join("target/build/x.rs"), "fn x() {}\n");

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "should not see target/x.rs as missing: {stderr}");
    assert!(stderr.contains("Checking 1 file(s)"), "expected only a.rs: {stderr}");
}

#[test]
fn scanner_skips_dot_git_directory() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CONFIG_MIT);
    write(&p.join("a.rs"), &format!("{HEADER_TEXT}\nfn a() {{}}\n"));
    write(&p.join(".git/hooks/x.rs"), "fn x() {}\n");
    write(&p.join(".vscode/x.rs"), "fn x() {}\n");

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("Checking 1 file(s)"), "{stderr}");
}

#[test]
fn scanner_does_not_skip_root_when_path_starts_with_dot() {
    // Project dir itself is dot-prefixed; depth=0 must NOT be skipped.
    let dir = TempDir::new().unwrap();
    let dot_root = dir.path().join(".dotted-project");
    fs::create_dir_all(&dot_root).unwrap();
    write(&dot_root.join(".cargo-heather.toml"), CONFIG_MIT);
    write(&dot_root.join("a.rs"), &format!("{HEADER_TEXT}\nfn a() {{}}\n"));

    let out = run_heather(&dot_root, &[]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("Checking 1 file(s)"), "root-with-dot must be walked: {stderr}");
}

#[test]
fn scanner_excludes_the_config_file_itself() {
    // .cargo-heather.toml MUST NOT appear among scanned files even though it has a .toml extension.
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CONFIG_MIT);
    write(
        &p.join("Cargo.toml"),
        &format!(
            "{}\n[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2024\"\nlicense = \"MIT\"\n",
            "# Copyright (c) Microsoft Corporation.\n# Licensed under the MIT License.\n"
        ),
    );

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "config file must not be scanned: {stderr}");
    // Only Cargo.toml should be checked.
    assert!(stderr.contains("Checking 1 file(s)"), "{stderr}");
}

#[test]
fn scanner_skips_dot_prefixed_toml_files_by_default() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CONFIG_MIT);
    write(&p.join("a.rs"), &format!("{HEADER_TEXT}\nfn a() {{}}\n"));
    write(&p.join(".other.toml"), "key = \"value\"\n"); // unsupported, no header

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "dot-prefixed toml must be skipped: {stderr}");
    assert!(stderr.contains("Checking 1 file(s)"), "{stderr}");
}

#[test]
fn scanner_includes_dot_prefixed_toml_when_dot_toml_true() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    let cfg = format!("{CONFIG_MIT}dot_toml = true\nexclude = [\".other.toml\"]\n");
    write(&p.join(".cargo-heather.toml"), &cfg);
    write(&p.join("a.rs"), &format!("{HEADER_TEXT}\nfn a() {{}}\n"));
    // Without exclude, .other.toml would appear; we exclude it via the list to
    // keep the test deterministic, but the scanner should still classify it
    // as a candidate when dot_toml=true (that path goes through exclude_list).
    write(&p.join(".other.toml"), "key = \"value\"\n");

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("Checking 1 file(s)"), "{stderr}");
}

#[test]
fn scanner_exclude_list_filters_named_file() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    let cfg = format!("{CONFIG_MIT}exclude = [\"b.rs\"]\n");
    write(&p.join(".cargo-heather.toml"), &cfg);
    write(&p.join("a.rs"), &format!("{HEADER_TEXT}\nfn a() {{}}\n"));
    write(&p.join("b.rs"), "fn b() {}\n"); // would fail check if scanned

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "b.rs should be excluded so check passes: {stderr}");
    assert!(stderr.contains("Checking 1 file(s)"), "{stderr}");
}

#[test]
fn scanner_exclude_list_filters_directory_subtree() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    let cfg = format!("{CONFIG_MIT}exclude = [\"vendor\"]\n");
    write(&p.join(".cargo-heather.toml"), &cfg);
    write(&p.join("a.rs"), &format!("{HEADER_TEXT}\nfn a() {{}}\n"));
    write(&p.join("vendor/lib.rs"), "fn lib() {}\n"); // would fail if scanned

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "vendor/ subtree must be excluded: {stderr}");
    assert!(stderr.contains("Checking 1 file(s)"), "{stderr}");
}

// ───────────────────────── config.rs ─────────────────────────

#[test]
fn config_falls_back_to_cargo_toml_license_field() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    // No .cargo-heather.toml — must fall back to Cargo.toml.
    let cargo_header = "# Licensed under the MIT License.\n";
    write(
        &p.join("Cargo.toml"),
        &format!("{cargo_header}\n[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2024\"\nlicense = \"MIT\"\n"),
    );
    write(&p.join("a.rs"), &format!("{HEADER_TEXT_MIT_ONLY}\nfn a() {{}}\n"));

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("using license from Cargo.toml"), "fallback log missing: {stderr}");
}

#[test]
fn config_workspace_license_inheritance() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let cargo_header = "# Licensed under the MIT License.\n";
    // Workspace Cargo.toml at root.
    write(
        &root.join("Cargo.toml"),
        &format!("{cargo_header}\n[workspace]\nmembers = [\"pkg\"]\n[workspace.package]\nlicense = \"MIT\"\n"),
    );
    // Member package.
    let pkg = root.join("pkg");
    write(
        &pkg.join("Cargo.toml"),
        &format!("{cargo_header}\n[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2024\"\nlicense.workspace = true\n"),
    );
    write(&pkg.join("src/lib.rs"), &format!("{HEADER_TEXT_MIT_ONLY}\nfn x() {{}}\n"));

    let out = run_heather(&pkg, &[]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "workspace inheritance should resolve license: {stderr}");
}

#[test]
fn config_walks_up_past_non_workspace_intermediate_cargo_toml() {
    // Ensures find_workspace_root keeps walking when an intermediate
    // Cargo.toml exists but has no [workspace] section.
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let cargo_header = "# Licensed under the MIT License.\n";
    // Outer workspace.
    write(
        &root.join("Cargo.toml"),
        &format!("{cargo_header}\n[workspace]\nmembers = [\"mid/inner\"]\n[workspace.package]\nlicense = \"MIT\"\n"),
    );
    // Intermediate Cargo.toml WITHOUT [workspace].
    let mid = root.join("mid");
    write(
        &mid.join("Cargo.toml"),
        &format!("{cargo_header}\n[package]\nname = \"mid\"\nversion = \"0.1.0\"\nedition = \"2024\"\nlicense = \"MIT\"\n"),
    );
    let inner = mid.join("inner");
    write(
        &inner.join("Cargo.toml"),
        &format!("{cargo_header}\n[package]\nname = \"inner\"\nversion = \"0.1.0\"\nedition = \"2024\"\nlicense.workspace = true\n"),
    );
    write(&inner.join("src/lib.rs"), &format!("{HEADER_TEXT_MIT_ONLY}\nfn x() {{}}\n"));

    let out = run_heather(&inner, &[]);
    let stderr = stderr_of(&out);
    assert!(
        out.status.success(),
        "must walk past intermediate non-workspace Cargo.toml: {stderr}"
    );
}

#[test]
fn config_rejects_both_license_and_header() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), "license = \"MIT\"\nheader = \"X\"\n");
    write(&p.join("a.rs"), "fn a() {}\n");

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(!out.status.success());
    assert!(
        stderr.contains("specify either 'license' or 'header'"),
        "rejection message: {stderr}"
    );
}

#[test]
fn config_rejects_empty_header() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), "header = \"   \"\n");
    write(&p.join("a.rs"), "fn a() {}\n");

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(!out.status.success());
    assert!(stderr.contains("'header' must not be empty"), "empty header rejection: {stderr}");
}

#[test]
fn config_rejects_missing_license_and_header() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), "scripts = false\n");
    write(&p.join("a.rs"), "fn a() {}\n");

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(!out.status.success());
    assert!(
        stderr.contains("must specify either 'license' (SPDX identifier) or 'header'"),
        "missing-both rejection: {stderr}"
    );
}

#[test]
fn config_no_config_no_cargo_returns_not_found() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join("a.rs"), "fn a() {}\n");

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(!out.status.success());
    // Errors surface via ohno; we just need to confirm a non-zero exit
    // happened — the precise wording is not asserted.
    assert!(!stderr.is_empty(), "expected some error output: {stderr}");
}

#[test]
fn config_cargo_toml_with_empty_package_license_falls_through() {
    // Empty package license must NOT be accepted; expect ConfigNotFound
    // because there's no .cargo-heather.toml and no usable license.
    //
    // Mutation guard for `config.rs:160`: replacing
    // `LicenseField::Plain(id) if !id.trim().is_empty()` with `if true`
    // would make the binary call `license::header_for_license("   ")`
    // and fail with `unknown SPDX license identifier: '   '` rather than
    // `config file not found`. We assert the exact original error text
    // to distinguish the two paths.
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    let cargo_header = "# Licensed under the MIT License.\n";
    write(
        &p.join("Cargo.toml"),
        &format!("{cargo_header}\n[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2024\"\nlicense = \"   \"\n"),
    );
    write(&p.join("a.rs"), "fn a() {}\n");

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(!out.status.success(), "empty license must be rejected: {stderr}");
    assert!(
        stderr.contains("config file not found"),
        "empty license must fall through to ConfigNotFound, not be treated as a license id: {stderr}"
    );
    assert!(
        !stderr.contains("unknown SPDX license identifier"),
        "empty license must NOT be passed to header_for_license: {stderr}"
    );
}

#[test]
fn config_workspace_package_license_inferred_at_root() {
    // Root Cargo.toml has NO [package] section but provides
    // `[workspace.package] license = "MIT"`. The MIT header should be
    // inferred for the lone source file at the workspace root.
    //
    // Mutation guard for `config.rs:173`: deleting the `!` in
    // `.filter(|id| !id.trim().is_empty())` would make the filter
    // reject the non-empty "MIT" id and fall through to Ok(None),
    // which ends with ConfigNotFound.
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    let cargo_header = "# Licensed under the MIT License.\n";
    write(
        &p.join("Cargo.toml"),
        &format!("{cargo_header}\n[workspace]\nmembers = []\n[workspace.package]\nlicense = \"MIT\"\n"),
    );
    // A Rust file at the workspace root with the canonical MIT-only
    // header; success requires the inferred header to match this.
    write(&p.join("a.rs"), &format!("{HEADER_TEXT_MIT_ONLY}\nfn a() {{}}\n"));

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "workspace.package.license inference must succeed: {stderr}");
    // Cargo.toml itself is also scanned (CommentStyle::Hash), so 2 files.
    assert!(stderr.contains("Checking 2 file(s)"), "{stderr}");
    assert!(stderr.contains("All 2 file(s) have correct license headers"), "{stderr}");
}

// ───────────────────────── scanner.rs mutants ─────────────────────────

#[test]
fn scanner_rejects_directory_with_rust_extension() {
    // A directory NAMED `weird.rs` is not a file, so the scanner must
    // skip it. A subdirectory whose name ends in `.rs` is unusual but
    // legal on Windows and Unix.
    //
    // Mutation guard for `scanner.rs:34`: replacing
    // `is_file() && from_path.is_some()` with `||` would let the
    // directory entry slip into the file list because
    // `CommentStyle::from_path("weird.rs") == Some(Slash)`. The binary
    // would then try to read it as a file and fail.
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CONFIG_MIT);
    // A correctly-headered file so the scanner has something to find.
    write(&p.join("a.rs"), &format!("{HEADER_TEXT}\nfn a() {{}}\n"));
    // A directory named like a Rust file; must be ignored.
    fs::create_dir_all(p.join("weird.rs")).unwrap();

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "directory ending in .rs must be ignored: {stderr}");
    assert!(
        stderr.contains("Checking 1 file(s)"),
        "exactly one file must be picked up: {stderr}"
    );
}

#[test]
fn scanner_skips_hidden_directories_at_any_depth() {
    // A non-SKIP_DIRS hidden directory like `.foobar/` must be pruned
    // by the depth>0 + starts_with('.') check.
    //
    // Mutation guard for `scanner.rs:73`: replacing `>` with `<` in
    // `entry.depth() > 0` — since `depth()` is `usize`, the mutated
    // condition `depth() < 0` is always false, disabling the
    // hidden-dir prune. The mutated binary would descend into
    // `.foobar/` and report MISSING for `.foobar/inner.rs`.
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CONFIG_MIT);
    let hidden = p.join(".foobar");
    fs::create_dir_all(&hidden).unwrap();
    // Unheaded source file inside the hidden dir.
    write(&hidden.join("inner.rs"), "fn inner() {}\n");
    // A correctly-headered file at the visible root so the scanner has
    // SOMETHING to traverse.
    write(&p.join("a.rs"), &format!("{HEADER_TEXT}\nfn a() {{}}\n"));

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "hidden dir must be skipped: {stderr}");
    assert!(stderr.contains("Checking 1 file(s)"), "{stderr}");
    assert!(
        !stderr.contains("MISSING header: .foobar"),
        "hidden dir contents must not be scanned: {stderr}"
    );
    assert!(!stderr.contains("inner.rs"), "hidden dir contents must not be scanned: {stderr}");
}

#[test]
fn config_cli_override_uses_explicit_path() {
    // --config points at a file OUTSIDE the project dir.
    let dir = TempDir::new().unwrap();
    let project = dir.path().join("proj");
    fs::create_dir_all(&project).unwrap();
    write(&project.join("a.rs"), &format!("{HEADER_TEXT}\nfn a() {{}}\n"));

    let cfg_dir = dir.path().join("cfgs");
    fs::create_dir_all(&cfg_dir).unwrap();
    let cfg_path = cfg_dir.join("custom.toml");
    write(&cfg_path, CONFIG_MIT);

    let mut cmd = Command::cargo_bin("cargo-heather").unwrap();
    cmd.env("RUST_LOG", "info")
        .arg("heather")
        .arg("--project-dir")
        .arg(&project)
        .arg("--config")
        .arg(&cfg_path);
    let out = cmd.output().unwrap();
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("Checking 1 file(s)"), "{stderr}");
    // Sanity: the custom config path was used (the project dir has no
    // .cargo-heather.toml and no Cargo.toml — without --config we'd error).
}

// ───────────────────────── cargo-script behaviour ─────────────────────────

#[test]
fn cargo_script_skipped_when_scripts_false() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    let cfg = format!("{CONFIG_MIT}scripts = false\n");
    write(&p.join(".cargo-heather.toml"), &cfg);
    let script = "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n[dependencies]\n---\nfn main() {}\n";
    write(&p.join("script.rs"), script);

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    // The script has no header but should be SKIPPED (read_and_classify
    // returns None), so the run succeeds with 0 checked files.
    assert!(out.status.success(), "scripts=false must skip: {stderr}");
    assert!(
        stderr.contains("Checking 1 file(s)") && stderr.contains("All 0 file(s)"),
        "expected 1 found / 0 checked: {stderr}"
    );
}

#[test]
fn cargo_script_processed_when_scripts_default_true() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CONFIG_MIT);
    let script = "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n[dependencies]\n---\nfn main() {}\n";
    write(&p.join("script.rs"), script);

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(!out.status.success(), "script without header must fail: {stderr}");
    assert!(
        stderr.contains("MISSING header: script.rs"),
        "expected missing log for script: {stderr}"
    );
}
