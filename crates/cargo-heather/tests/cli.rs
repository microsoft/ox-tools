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
const HEADER_TEXT_HASH: &str = "# Copyright (c) Microsoft Corporation.\n# Licensed under the MIT License.\n";
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

#[test]
fn scanner_warns_on_unresolvable_exclude_entry() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    // Mix a valid exclude ("vendor") with an invalid one ("nonexistent_dir").
    let cfg = format!("{CONFIG_MIT}exclude = [\"vendor\", \"nonexistent_dir\"]\n");
    write(&p.join(".cargo-heather.toml"), &cfg);
    write(&p.join("a.rs"), &format!("{HEADER_TEXT}\nfn a() {{}}\n"));
    write(&p.join("vendor/lib.rs"), "fn lib() {}\n"); // would fail if scanned

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    // Valid exclude still works — vendor/lib.rs is excluded, only a.rs is checked.
    assert!(out.status.success(), "valid excludes must still work: {stderr}");
    assert!(stderr.contains("Checking 1 file(s)"), "{stderr}");
    // Warning is emitted for the unresolvable entry.
    assert!(
        stderr.contains("Warning: exclude entry 'nonexistent_dir'"),
        "should warn about unresolvable exclude: {stderr}"
    );
    assert!(stderr.contains("will be ignored"), "warning must mention it is ignored: {stderr}");
}

#[test]
fn scanner_includes_legacy_hash_comment_file_types() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CONFIG_MIT);
    write(
        &p.join("build.ps1"),
        &format!("#!/usr/bin/env pwsh\n{HEADER_TEXT_HASH}\nWrite-Host 'ok'\n"),
    );
    // `.psd1` is plain PowerShell data syntax (used for module manifests
    // and config-style hashtables) and `.psm1` is a PowerShell module file;
    // both use the same `#` line-comment syntax as `.ps1` and must be
    // scanned alongside it.
    write(&p.join("Settings.psd1"), &format!("{HEADER_TEXT_HASH}\n@{{ Name = 'Settings' }}\n"));
    write(&p.join("Module.psm1"), &format!("{HEADER_TEXT_HASH}\nfunction Get-Foo {{ }}\n"));
    write(&p.join("recipes.just"), &format!("{HEADER_TEXT_HASH}\nbuild:\n    cargo build\n"));
    write(&p.join("justfile"), &format!("{HEADER_TEXT_HASH}\ntest:\n    cargo test\n"));
    write(&p.join("constants.env"), &format!("{HEADER_TEXT_HASH}\nRUST_LATEST=1.88.0\n"));

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(
        out.status.success(),
        "legacy hash-comment files should be checked and pass: {stderr}"
    );
    assert!(stderr.contains("Checking 6 file(s)"), "{stderr}");
    assert!(
        stderr.contains("All 6 file(s) have correct license headers."),
        "summary missing for legacy hash-comment files: {stderr}"
    );
}

#[test]
fn scanner_reports_missing_header_in_legacy_hash_comment_file_type() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CONFIG_MIT);
    write(&p.join("build.ps1"), "#!/usr/bin/env pwsh\nWrite-Host 'missing header'\n");

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(!out.status.success(), "missing PowerShell header should fail: {stderr}");
    assert!(stderr.contains("MISSING header: build.ps1"), "missing log absent: {stderr}");
}

#[test]
fn scanner_reports_missing_header_in_powershell_data_file() {
    // Regression guard: `.psd1` must be classified as PowerShell so a
    // missing header is reported (rather than the file being silently
    // skipped, which was the pre-fix behaviour).
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CONFIG_MIT);
    write(&p.join("Settings.psd1"), "@{ Name = 'Settings' }\n");

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(!out.status.success(), "missing .psd1 header should fail: {stderr}");
    assert!(stderr.contains("MISSING header: Settings.psd1"), "missing log absent: {stderr}");
}

#[test]
fn scanner_reports_missing_header_in_powershell_module_file() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CONFIG_MIT);
    write(&p.join("Module.psm1"), "function Get-Foo { }\n");

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(!out.status.success(), "missing .psm1 header should fail: {stderr}");
    assert!(stderr.contains("MISSING header: Module.psm1"), "missing log absent: {stderr}");
}

#[test]
fn scanner_ignores_unsupported_extension_even_when_hash_comments_would_work() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CONFIG_MIT);
    write(&p.join("a.rs"), &format!("{HEADER_TEXT}\nfn a() {{}}\n"));
    write(
        &p.join("notes.txt"),
        "Copyright (c) Microsoft Corporation.\nLicensed under the MIT License.\n",
    );

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "unsupported .txt file must not be scanned: {stderr}");
    assert!(stderr.contains("Checking 1 file(s)"), "{stderr}");
}

#[test]
fn fix_inserts_powershell_header_after_shebang() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CONFIG_MIT);
    write(&p.join("build.ps1"), "#!/usr/bin/env pwsh\nWrite-Host 'missing header'\n");

    let out = run_heather(p, &["--fix"]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "PowerShell fix should succeed: {stderr}");

    let fixed = fs::read_to_string(p.join("build.ps1")).unwrap();
    assert!(
        fixed.starts_with(&format!("#!/usr/bin/env pwsh\n{HEADER_TEXT_HASH}\n")),
        "fix must preserve shebang and insert header after it. Got: {fixed:?}"
    );
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

// ─────────────── custom (non-SPDX) header via `header = "..."` ───────────────
//
// These tests cover the case where a project supplies a fully custom
// header text via the `header` field of `.cargo-heather.toml` whose
// content is NOT one of the SPDX licenses registered in
// `crates/cargo-heather/src/license.rs`. The tool must accept arbitrary
// header text — proprietary licenses, internal-use notices, etc. — and
// must NOT treat the `header` field as an SPDX identifier.

/// A multi-line, fully custom header that does not match any SPDX
/// identifier or any header text in the registered license registry.
/// It contains the word "Copyright" so the header-detection heuristic
/// recognises it as a license header (see
/// `looks_like_license_header` in `src/checker/extract.rs`).
const CUSTOM_HEADER_TOML: &str = "header = \"Copyright (c) Acme Corp 2026.\\nAll rights reserved. Proprietary - internal use only.\"\n";

const CUSTOM_HEADER_RS: &str = "\
// Copyright (c) Acme Corp 2026.
// All rights reserved. Proprietary - internal use only.
";

const CUSTOM_HEADER_HASH: &str = "\
# Copyright (c) Acme Corp 2026.
# All rights reserved. Proprietary - internal use only.
";

#[test]
fn custom_header_check_passes_when_files_match() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CUSTOM_HEADER_TOML);
    write(&p.join("a.rs"), &format!("{CUSTOM_HEADER_RS}\nfn a() {{}}\n"));
    write(&p.join("b.rs"), &format!("{CUSTOM_HEADER_RS}\nfn b() {{}}\n"));

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "custom header must be accepted: {stderr}");
    assert!(
        stderr.contains("All 2 file(s) have correct license headers."),
        "summary missing for custom header: {stderr}"
    );
}

#[test]
fn custom_header_check_fails_when_file_has_wrong_header() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CUSTOM_HEADER_TOML);
    // File has the standard MS-MIT header, which does NOT match the
    // configured custom proprietary header.
    write(&p.join("a.rs"), &format!("{HEADER_TEXT}\nfn a() {{}}\n"));

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(!out.status.success(), "wrong header must be rejected: {stderr}");
    assert!(stderr.contains("MISMATCH header: a.rs"), "expected MISMATCH log: {stderr}");
    // The expected/actual diff must reference the custom header text.
    assert!(
        stderr.contains("Copyright (c) Acme Corp 2026."),
        "diff must show expected custom header: {stderr}"
    );
    assert!(
        stderr.contains("Licensed under the MIT License."),
        "diff must show actual (wrong) header: {stderr}"
    );
}

#[test]
fn custom_header_check_fails_when_file_missing_header() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CUSTOM_HEADER_TOML);
    write(&p.join("a.rs"), "fn a() {}\n");

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(!out.status.success(), "missing header must fail: {stderr}");
    assert!(stderr.contains("MISSING header: a.rs"), "expected MISSING log: {stderr}");
}

#[test]
fn custom_header_fix_inserts_header_in_rust_file() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CUSTOM_HEADER_TOML);
    write(&p.join("a.rs"), "fn a() {}\n");

    let out = run_heather(p, &["--fix"]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "fix must succeed: {stderr}");

    let fixed = fs::read_to_string(p.join("a.rs")).unwrap();
    assert!(
        fixed.starts_with(CUSTOM_HEADER_RS),
        "fix must insert custom header at top of Rust file. Got: {fixed:?}"
    );
    assert!(fixed.contains("fn a() {}"), "fix must preserve original code: {fixed:?}");
}

#[test]
fn custom_header_fix_inserts_header_in_toml_file() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CUSTOM_HEADER_TOML);
    // dot_toml = true so the `.cargo-heather.toml` itself is also a
    // candidate; but we explicitly target a regular Cargo.toml here.
    write(
        &p.join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );

    let out = run_heather(p, &["--fix"]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "fix must succeed for TOML: {stderr}");

    let fixed = fs::read_to_string(p.join("Cargo.toml")).unwrap();
    assert!(
        fixed.starts_with(CUSTOM_HEADER_HASH),
        "fix must insert hash-style custom header at top of TOML file. Got: {fixed:?}"
    );
}

#[test]
fn custom_header_fix_replaces_wrong_header() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CUSTOM_HEADER_TOML);
    // File starts with the standard MS-MIT header that must be replaced.
    write(&p.join("a.rs"), &format!("{HEADER_TEXT}\nfn a() {{}}\n"));

    let out = run_heather(p, &["--fix"]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "fix must succeed: {stderr}");

    let fixed = fs::read_to_string(p.join("a.rs")).unwrap();
    assert!(
        fixed.starts_with(CUSTOM_HEADER_RS),
        "fix must replace wrong header with custom one. Got: {fixed:?}"
    );
    // The old (wrong) MIT line must no longer be present at the top.
    assert!(
        !fixed.starts_with("// Copyright (c) Microsoft Corporation."),
        "old header must be stripped: {fixed:?}"
    );
}

#[test]
fn custom_header_recheck_after_fix_passes() {
    // End-to-end: fix then re-check on the same tree must succeed,
    // proving the custom header round-trips through fix and check.
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), CUSTOM_HEADER_TOML);
    write(&p.join("a.rs"), "fn a() {}\n");
    write(&p.join("b.rs"), "fn b() {}\n");

    let fix_out = run_heather(p, &["--fix"]);
    assert!(fix_out.status.success(), "fix must succeed: {}", stderr_of(&fix_out));

    let check_out = run_heather(p, &[]);
    let stderr = stderr_of(&check_out);
    assert!(check_out.status.success(), "re-check after fix must succeed: {stderr}");
    assert!(
        stderr.contains("All 2 file(s) have correct license headers."),
        "re-check summary missing: {stderr}"
    );
}

#[test]
fn config_unknown_spdx_license_errors_clearly() {
    // `license = "FOO-BAR"` is an SPDX-style identifier that is NOT in
    // the tool's registered license registry. The tool must surface a
    // clear `unknown SPDX license identifier` error rather than treating
    // the unknown id as a custom header.
    //
    // This complements the `header = "..."` custom-header tests above:
    // the two config keys have different semantics — `license` is
    // looked up in the SPDX registry; `header` is taken verbatim — and
    // a typo in `license` must NOT silently succeed.
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), "license = \"FOO-BAR\"\n");
    write(&p.join("a.rs"), "fn a() {}\n");

    let out = run_heather(p, &[]);
    let stderr = stderr_of(&out);
    assert!(!out.status.success(), "unknown SPDX id must fail: {stderr}");
    assert!(
        stderr.contains("unknown SPDX license identifier"),
        "expected UnknownLicense error wording: {stderr}"
    );
    assert!(stderr.contains("FOO-BAR"), "error must echo the unknown id: {stderr}");
}

#[test]
fn fix_strips_long_existing_header_when_expected_is_short() {
    // Regression for the "expected shorter than existing header" review
    // comment on PR #16: previously, `--fix` bounded the strip by the
    // expected header's line count, so a file with a 5-line wrong header
    // configured with a 1-line MIT header ended up with the new header
    // followed by the 4 leftover lines from the old header. After the
    // fix, the entire existing header block is stripped and only the
    // new header + body remain.
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    write(&p.join(".cargo-heather.toml"), "header = \"Licensed under the MIT License.\"\n");
    let long_existing = "\
// Copyright 2024 Acme Corp.
// Licensed under the Apache License, Version 2.0 (the \"License\");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//     http://www.apache.org/licenses/LICENSE-2.0

fn main() {}
";
    write(&p.join("a.rs"), long_existing);

    let out = run_heather(p, &["--fix"]);
    let stderr = stderr_of(&out);
    assert!(out.status.success(), "fix should succeed, stderr: {stderr}");

    let fixed = fs::read_to_string(p.join("a.rs")).unwrap();
    assert_eq!(
        fixed, "// Licensed under the MIT License.\n\nfn main() {}\n",
        "fix must strip the entire 5-line wrong header, not just the first line"
    );
    // Belt-and-suspenders: no Apache leftovers.
    assert!(!fixed.contains("Apache"), "Apache leftovers in fixed file: {fixed}");
    assert!(!fixed.contains("Acme"), "Acme leftovers in fixed file: {fixed}");
}
