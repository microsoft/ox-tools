// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for cargo-ensure-no-unused-workspace-deps.
//!
//! Each test builds a throwaway workspace on disk and runs the compiled binary
//! against it, so the behaviour under test is the one users get, including the
//! `cargo metadata` call that enumerates members.

// Miri cannot run these tests because they spawn subprocesses and use temp directories.
#![cfg(not(miri))]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic-on-failure idioms are appropriate in tests"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// Path to the binary under test.
fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_cargo-ensure-no-unused-workspace-deps"))
}

/// Write a workspace root manifest plus one member per entry in `members`.
///
/// Each member is `(name, manifest body)`; the body is appended to a generated
/// `[package]` section so tests only spell out the dependency tables they care
/// about.
fn workspace(root: &str, members: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().expect("failed to create temp dir");
    fs::write(dir.path().join("Cargo.toml"), root).expect("failed to write workspace manifest");

    for (name, body) in members {
        let member = dir.path().join(name);
        fs::create_dir_all(member.join("src")).expect("failed to create member dir");
        fs::write(member.join("src").join("lib.rs"), "").expect("failed to write member source");

        let manifest = format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n{body}");
        fs::write(member.join("Cargo.toml"), manifest).expect("failed to write member manifest");
    }

    dir
}

/// Run the tool against `manifest_path` with `args`.
fn run(manifest_path: &Path, args: &[&str]) -> Output {
    Command::new(binary())
        .arg("ensure-no-unused-workspace-deps")
        .arg("--manifest-path")
        .arg(manifest_path)
        .args(args)
        // Keep the run hermetic: `--no-deps` never resolves, so nothing should
        // reach the network, and this makes a regression there fail loudly.
        .env("CARGO_NET_OFFLINE", "true")
        .output()
        .expect("failed to execute the binary")
}

/// `(success, stdout, stderr)` of a run.
fn outcome(output: &Output) -> (bool, String, String) {
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn passes_when_every_entry_is_inherited() {
    let dir = workspace(
        "[workspace]\nmembers = [\"member\"]\n\n[workspace.dependencies]\nserde = \"1\"\n",
        &[("member", "[dependencies]\nserde = { workspace = true }\n")],
    );
    let manifest = dir.path().join("Cargo.toml");

    let (success, stdout, _) = outcome(&run(&manifest, &[]));

    assert!(success, "an inherited catalog should pass");
    assert!(stdout.contains("All 1 workspace dependency"), "unexpected stdout: {stdout}");
}

#[test]
fn fails_and_lists_entries_nobody_inherits() {
    let dir = workspace(
        "[workspace]\nmembers = [\"member\"]\n\n[workspace.dependencies]\nserde = \"1\"\nonce_cell = \"1\"\nsmallvec = \"1\"\n",
        &[("member", "[dependencies]\nserde = { workspace = true }\n")],
    );
    let manifest = dir.path().join("Cargo.toml");

    let (success, _, stderr) = outcome(&run(&manifest, &[]));

    assert!(!success, "uninherited entries should fail the run");
    assert!(
        stderr.contains("Found 2 unused workspace dependencies"),
        "unexpected stderr: {stderr}"
    );
    assert!(stderr.contains("- once_cell"), "unexpected stderr: {stderr}");
    assert!(stderr.contains("- smallvec"), "unexpected stderr: {stderr}");
    assert!(!stderr.contains("- serde"), "the inherited entry should not be reported: {stderr}");
}

#[test]
fn counts_a_single_entry_in_the_singular() {
    let dir = workspace(
        "[workspace]\nmembers = [\"member\"]\n\n[workspace.dependencies]\nonce_cell = \"1\"\n",
        &[("member", "")],
    );
    let manifest = dir.path().join("Cargo.toml");

    let (success, _, stderr) = outcome(&run(&manifest, &[]));

    assert!(!success);
    assert!(
        stderr.contains("Found 1 unused workspace dependency in"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn recognizes_dev_build_and_target_tables() {
    let root =
        "[workspace]\nmembers = [\"member\"]\n\n[workspace.dependencies]\ntempfile = \"3\"\ncc = \"1\"\nlibc = \"0.2\"\nwinapi = \"0.3\"\n";
    let body = concat!(
        "[dev-dependencies]\ntempfile = { workspace = true }\n\n",
        "[build-dependencies]\ncc = { workspace = true }\n\n",
        "[target.'cfg(unix)'.dependencies]\nlibc = { workspace = true }\n\n",
        "[target.'cfg(windows)'.dev-dependencies]\nwinapi = { workspace = true }\n",
    );
    let dir = workspace(root, &[("member", body)]);
    let manifest = dir.path().join("Cargo.toml");

    let (success, stdout, stderr) = outcome(&run(&manifest, &[]));

    assert!(success, "every table form should count as inheritance: {stderr}");
    assert!(stdout.contains("All 4 workspace dependencies"), "unexpected stdout: {stdout}");
}

#[test]
fn recognizes_the_dotted_inheritance_form() {
    let dir = workspace(
        "[workspace]\nmembers = [\"member\"]\n\n[workspace.dependencies]\nserde = \"1\"\n",
        &[("member", "[dependencies]\nserde.workspace = true\n")],
    );
    let manifest = dir.path().join("Cargo.toml");

    let (success, _, stderr) = outcome(&run(&manifest, &[]));

    assert!(success, "the dotted form is inheritance too: {stderr}");
}

#[test]
fn a_declaration_that_does_not_inherit_does_not_count() {
    // The member declares `serde` itself rather than drawing it from the
    // catalog, so the catalog entry is still inherited by nobody.
    let dir = workspace(
        "[workspace]\nmembers = [\"member\"]\n\n[workspace.dependencies]\nserde = \"1\"\n",
        &[("member", "[dependencies]\nserde = { version = \"1\" }\n")],
    );
    let manifest = dir.path().join("Cargo.toml");

    let (success, _, stderr) = outcome(&run(&manifest, &[]));

    assert!(!success, "a self-declared dependency does not inherit");
    assert!(stderr.contains("- serde"), "unexpected stderr: {stderr}");
}

#[test]
fn counts_the_root_package_as_a_member() {
    let dir = TempDir::new().expect("failed to create temp dir");
    fs::create_dir_all(dir.path().join("src")).expect("failed to create src");
    fs::write(dir.path().join("src").join("lib.rs"), "").expect("failed to write source");
    fs::write(
        dir.path().join("Cargo.toml"),
        concat!(
            "[workspace]\n\n",
            "[workspace.dependencies]\nserde = \"1\"\n\n",
            "[package]\nname = \"root\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n",
            "[dependencies]\nserde = { workspace = true }\n",
        ),
    )
    .expect("failed to write manifest");

    let (success, _, stderr) = outcome(&run(&dir.path().join("Cargo.toml"), &[]));

    assert!(success, "a workspace root that is itself a package inherits too: {stderr}");
}

#[test]
fn honors_the_allow_list_and_reports_stale_entries() {
    let root = concat!(
        "[workspace]\nmembers = [\"member\"]\n\n",
        "[workspace.metadata.ensure-no-unused-workspace-deps]\nallowed = [\"kept\", \"stale\"]\n\n",
        "[workspace.dependencies]\nkept = \"1\"\nstale = \"1\"\n",
    );
    let dir = workspace(root, &[("member", "[dependencies]\nstale = { workspace = true }\n")]);
    let manifest = dir.path().join("Cargo.toml");

    let (success, stdout, stderr) = outcome(&run(&manifest, &[]));

    assert!(success, "an allowed entry does not fail the run: {stderr}");
    assert!(stdout.contains("explicitly allowed"), "unexpected stdout: {stdout}");
    assert!(
        stderr.contains("'stale' is allowed but is inherited or not declared"),
        "unexpected stderr: {stderr}"
    );
    assert!(
        !stderr.contains("'kept' is allowed"),
        "the load-bearing allow entry is not stale: {stderr}"
    );
}

#[test]
fn fix_removes_the_entries_and_keeps_the_rest_intact() {
    let root = concat!(
        "[workspace]\nmembers = [\"member\"]\n\n",
        "[workspace.dependencies]\n",
        "# --- kept ---\n",
        "serde = { version = \"1\", default-features = false }\n",
        "once_cell = \"1\"\n",
    );
    let dir = workspace(root, &[("member", "[dependencies]\nserde = { workspace = true }\n")]);
    let manifest = dir.path().join("Cargo.toml");

    let (success, stdout, stderr) = outcome(&run(&manifest, &["--fix"]));

    assert!(success, "--fix should succeed: {stderr}");
    assert!(
        stdout.contains("Removed 1 unused workspace dependency"),
        "unexpected stdout: {stdout}"
    );

    let fixed = fs::read_to_string(&manifest).expect("failed to read the fixed manifest");
    assert!(!fixed.contains("once_cell"), "the unused entry should be gone: {fixed}");
    assert!(
        fixed.contains("serde = { version = \"1\", default-features = false }"),
        "the survivor keeps its formatting: {fixed}"
    );
    assert!(fixed.contains("# --- kept ---"), "comments on survivors are preserved: {fixed}");

    // A fixed manifest is clean on the next run.
    let (success, _, _) = outcome(&run(&manifest, &[]));
    assert!(success, "the fixed manifest should pass");
}

#[test]
fn fix_carries_a_removed_group_header_to_the_next_survivor() {
    let root = concat!(
        "[workspace]\nmembers = [\"member\"]\n\n",
        "[workspace.dependencies]\n",
        "# --- unused group ---\n",
        "once_cell = \"1\"\n",
        "# --- kept group ---\n",
        "serde = \"1\"\n",
    );
    let dir = workspace(root, &[("member", "[dependencies]\nserde = { workspace = true }\n")]);
    let manifest = dir.path().join("Cargo.toml");

    let (success, _, stderr) = outcome(&run(&manifest, &["--fix"]));
    assert!(success, "--fix should succeed: {stderr}");

    let fixed = fs::read_to_string(&manifest).expect("failed to read the fixed manifest");
    let header = fixed.find("# --- kept group ---").expect("the surviving header is kept");
    let survivor = fixed.find("serde =").expect("the survivor is kept");
    let carried = fixed
        .find("# --- unused group ---")
        .expect("the removed entry's comment is carried");

    // Position matters, not mere presence: the carried comment has to land
    // ahead of the next surviving entry. Leaving it at the end of the table
    // would also keep it in the file, while silently moving it out of place.
    assert!(
        carried < header,
        "the carried comment must precede the next entry's own decor: {fixed}"
    );
    assert!(header < survivor, "the header still introduces its group: {fixed}");
}

#[test]
fn fix_keeps_a_trailing_header_behind_the_last_survivor() {
    let root = concat!(
        "[workspace]\nmembers = [\"member\"]\n\n",
        "[workspace.dependencies]\n",
        "serde = \"1\"\n",
        "# --- external dependencies ---\n",
        "once_cell = \"1\"\n",
    );
    let dir = workspace(root, &[("member", "[dependencies]\nserde = { workspace = true }\n")]);
    let manifest = dir.path().join("Cargo.toml");

    let (success, _, stderr) = outcome(&run(&manifest, &["--fix"]));
    assert!(success, "--fix should succeed: {stderr}");

    let fixed = fs::read_to_string(&manifest).expect("failed to read the fixed manifest");
    let survivor = fixed.find("serde =").expect("the survivor is kept");
    let header = fixed.find("# --- external dependencies ---").expect("the carried header is kept");
    assert!(
        survivor < header,
        "the trailing header must not be hoisted above the survivor: {fixed}"
    );

    // The trailing branch reports too: this is the path that used to claim a
    // carry whether or not the append had actually happened.
    assert!(
        stderr.contains("Carried 1 comment line from 'once_cell' onto 'serde'"),
        "the trailing carry must be reported: {stderr}"
    );
}

#[test]
fn fix_on_a_fully_unused_catalog_empties_the_table() {
    let root = "[workspace]\nmembers = [\"member\"]\n\n[workspace.dependencies]\n# --- all of it ---\nonce_cell = \"1\"\n";
    let dir = workspace(root, &[("member", "")]);
    let manifest = dir.path().join("Cargo.toml");

    let (success, stdout, stderr) = outcome(&run(&manifest, &["--fix"]));

    assert!(success, "--fix should succeed: {stderr}");
    assert!(
        stdout.contains("Removed 1 unused workspace dependency"),
        "unexpected stdout: {stdout}"
    );

    let fixed = fs::read_to_string(&manifest).expect("failed to read the fixed manifest");
    assert!(!fixed.contains("once_cell"), "the entry should be gone: {fixed}");
    assert!(fixed.contains("[workspace.dependencies]"), "the empty table stays: {fixed}");
}

#[test]
fn fix_keeps_an_allowed_entry_while_removing_the_others() {
    // The destructive path must honour the allow-list too: reporting suppresses
    // an allowed finding, and `--fix` has to leave that entry on disk.
    let root = concat!(
        "[workspace]\nmembers = [\"member\"]\n\n",
        "[workspace.metadata.ensure-no-unused-workspace-deps]\nallowed = [\"kept\"]\n\n",
        "[workspace.dependencies]\nkept = \"1\"\nonce_cell = \"1\"\nserde = \"1\"\n",
    );
    let dir = workspace(root, &[("member", "[dependencies]\nserde = { workspace = true }\n")]);
    let manifest = dir.path().join("Cargo.toml");

    let (success, stdout, stderr) = outcome(&run(&manifest, &["--fix"]));

    assert!(success, "--fix should succeed: {stderr}");
    assert!(
        stdout.contains("Removed 1 unused workspace dependency"),
        "unexpected stdout: {stdout}"
    );

    let fixed = fs::read_to_string(&manifest).expect("failed to read the fixed manifest");
    assert!(fixed.contains("kept = \"1\""), "an allowed entry must survive --fix: {fixed}");
    assert!(fixed.contains("serde = \"1\""), "an inherited entry must survive --fix: {fixed}");
    assert!(!fixed.contains("once_cell"), "the ordinary unused entry should be gone: {fixed}");
}

#[test]
fn member_globs_and_exclusions_follow_cargo() {
    // The reason this tool shells out to `cargo metadata` rather than reading
    // `members` itself: globs expand and `exclude` subtracts. A literal reading
    // of `members` would treat the excluded package as a member and call
    // `only_excluded_uses` inherited.
    let dir = TempDir::new().expect("failed to create temp dir");
    fs::write(
        dir.path().join("Cargo.toml"),
        concat!(
            "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/excluded\"]\n\n",
            "[workspace.dependencies]\nboth_use = \"1\"\nonly_excluded_uses = \"1\"\n",
        ),
    )
    .expect("failed to write workspace manifest");

    for (name, dep) in [("included", "both_use"), ("excluded", "only_excluded_uses")] {
        let member = dir.path().join("crates").join(name);
        fs::create_dir_all(member.join("src")).expect("failed to create member dir");
        fs::write(member.join("src").join("lib.rs"), "").expect("failed to write member source");
        fs::write(
            member.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n{dep} = {{ workspace = true }}\n"
            ),
        )
        .expect("failed to write member manifest");
    }

    let (success, _, stderr) = outcome(&run(&dir.path().join("Cargo.toml"), &[]));

    assert!(!success, "the entry only the excluded package inherits is unused");
    assert!(stderr.contains("- only_excluded_uses"), "unexpected stderr: {stderr}");
    assert!(
        !stderr.contains("- both_use"),
        "the glob-expanded member's entry is inherited: {stderr}"
    );
}

#[test]
fn a_stale_allow_entry_is_reported_against_an_empty_catalog() {
    // The degenerate boundary: with no catalog at all, every allowed name
    // suppresses nothing, so every one of them is stale.
    let root = concat!(
        "[workspace]\nmembers = [\"member\"]\n\n",
        "[workspace.metadata.ensure-no-unused-workspace-deps]\nallowed = [\"old\"]\n",
    );
    let dir = workspace(root, &[("member", "")]);
    let manifest = dir.path().join("Cargo.toml");

    let (success, stdout, stderr) = outcome(&run(&manifest, &[]));

    assert!(success, "an empty catalog still passes");
    assert!(stdout.contains("declares no workspace dependencies"), "unexpected stdout: {stdout}");
    assert!(
        stderr.contains("'old' is allowed but"),
        "a stale allow entry must still be reported: {stderr}"
    );
}

#[test]
fn fix_reports_which_comments_moved_and_where() {
    // A note about one entry and a group header are indistinguishable, so the
    // relocation has to be visible in the run's output.
    let root = concat!(
        "[workspace]\nmembers = [\"member\"]\n\n",
        "[workspace.dependencies]\n",
        "# pinned to 1.2 until upstream #42 is fixed\n",
        "# revisit after the 2.0 release\n",
        "once_cell = \"1\"\n",
        "serde = \"1\"\n",
    );
    let dir = workspace(root, &[("member", "[dependencies]\nserde = { workspace = true }\n")]);
    let manifest = dir.path().join("Cargo.toml");

    let (success, _, stderr) = outcome(&run(&manifest, &["--fix"]));

    assert!(success, "--fix should succeed: {stderr}");
    // The count has to be the real number of comment lines, not a placeholder:
    // it tells the reviewer how much text to re-read.
    assert!(
        stderr.contains("Carried 2 comment lines from 'once_cell' onto 'serde'"),
        "the move must be reported with its size: {stderr}"
    );
}

#[test]
fn fix_reports_comments_dropped_with_an_emptied_table() {
    let root = "[workspace]\nmembers = [\"member\"]\n\n[workspace.dependencies]\n# --- all of it ---\nonce_cell = \"1\"\n";
    let dir = workspace(root, &[("member", "")]);
    let manifest = dir.path().join("Cargo.toml");

    let (success, _, stderr) = outcome(&run(&manifest, &["--fix"]));

    assert!(success, "--fix should succeed: {stderr}");
    assert!(
        stderr.contains("Dropped 1 comment line from 'once_cell': no surviving entry could carry them."),
        "the drop must be reported: {stderr}"
    );
}

#[test]
fn fix_reports_a_drop_when_the_last_survivor_cannot_carry_comments() {
    // Only a plain value has a suffix to append to. A dotted survivor is an
    // `Item::Table`, so the comment cannot be attached and is dropped -- and
    // the report has to say so rather than claim a move that did not happen.
    let root = concat!(
        "[workspace]\nmembers = [\"member\"]\n\n",
        "[workspace.dependencies]\n",
        "serde.version = \"1\"\n",
        "# pinned to 1.2 until upstream #42 is fixed\n",
        "once_cell = \"1\"\n",
    );
    let dir = workspace(root, &[("member", "[dependencies]\nserde = { workspace = true }\n")]);
    let manifest = dir.path().join("Cargo.toml");

    let (success, _, stderr) = outcome(&run(&manifest, &["--fix"]));

    assert!(success, "--fix should succeed: {stderr}");

    let fixed = fs::read_to_string(&manifest).expect("failed to read the fixed manifest");
    assert!(!fixed.contains("pinned to 1.2"), "the comment really is gone: {fixed}");
    assert!(
        stderr.contains("Dropped 1 comment line from 'once_cell'"),
        "a comment that was dropped must not be reported as carried: {stderr}"
    );
    assert!(!stderr.contains("Carried"), "nothing was carried here: {stderr}");
}

#[test]
fn fix_does_not_duplicate_a_sub_table_survivors_own_comment() {
    // The survivor's own comment lives on its table decor. Reading it there and
    // writing it back onto the key would leave both in the document, so `--fix`
    // would emit a line the user never wrote.
    let root = concat!(
        "[workspace]\nmembers = [\"member\"]\n\n",
        "[workspace.dependencies]\n",
        "# --- group header ---\n",
        "once_cell = \"1\"\n\n",
        "# note about serde\n",
        "[workspace.dependencies.serde]\nversion = \"1\"\n",
    );
    let dir = workspace(root, &[("member", "[dependencies]\nserde = { workspace = true }\n")]);
    let manifest = dir.path().join("Cargo.toml");

    let (success, _, stderr) = outcome(&run(&manifest, &["--fix"]));

    assert!(success, "--fix should succeed: {stderr}");

    let fixed = fs::read_to_string(&manifest).expect("failed to read the fixed manifest");
    assert_eq!(
        fixed.matches("# note about serde").count(),
        1,
        "the survivor's own comment must appear exactly once: {fixed}"
    );
    assert!(fixed.contains("# --- group header ---"), "the carried header survives: {fixed}");
    assert!(
        stderr.contains("Carried 1 comment line from 'once_cell' onto 'serde'"),
        "the move must be reported: {stderr}"
    );
}

#[test]
fn fix_carries_onto_a_dotted_survivor() {
    // A dotted entry keeps its leading comments on its first inner key, so that
    // is where a carry has to land. Writing to the outer key renders nothing
    // and would lose the text while still reporting a move.
    let root = concat!(
        "[workspace]\nmembers = [\"member\"]\n\n",
        "[workspace.dependencies]\n",
        "# pinned to 1.2 until upstream #42 is fixed\n",
        "old = \"1\"\n",
        "serde.version = \"1\"\n",
        "zzz = \"1\"\n",
    );
    let dir = workspace(
        root,
        &[(
            "member",
            "[dependencies]\nserde = { workspace = true }\nzzz = { workspace = true }\n",
        )],
    );
    let manifest = dir.path().join("Cargo.toml");

    let (success, _, stderr) = outcome(&run(&manifest, &["--fix"]));

    assert!(success, "--fix should succeed: {stderr}");

    let fixed = fs::read_to_string(&manifest).expect("failed to read the fixed manifest");
    let comment = fixed.find("# pinned to 1.2").expect("the carried comment survives");
    let survivor = fixed.find("serde.version").expect("the dotted survivor is kept");
    assert!(comment < survivor, "the comment introduces the entry it landed on: {fixed}");
    assert!(
        stderr.contains("Carried 1 comment line from 'old' onto 'serde'"),
        "the move must be reported: {stderr}"
    );
}

#[test]
fn fix_carries_a_comment_from_a_removed_dotted_entry() {
    // The removed entry is dotted, so its comment hangs off the inner key.
    // Reading only the outer key or the table decor would drop it unreported.
    let root = concat!(
        "[workspace]\nmembers = [\"member\"]\n\n",
        "[workspace.dependencies]\n",
        "# note about old\n",
        "old.version = \"1\"\n",
        "serde = \"1\"\n",
    );
    let dir = workspace(root, &[("member", "[dependencies]\nserde = { workspace = true }\n")]);
    let manifest = dir.path().join("Cargo.toml");

    let (success, _, stderr) = outcome(&run(&manifest, &["--fix"]));

    assert!(success, "--fix should succeed: {stderr}");

    let fixed = fs::read_to_string(&manifest).expect("failed to read the fixed manifest");
    assert!(!fixed.contains("old.version"), "the unused entry should be gone: {fixed}");
    assert!(fixed.contains("# note about old"), "its comment must not vanish: {fixed}");
    assert!(
        stderr.contains("Carried 1 comment line from 'old' onto 'serde'"),
        "the move must be reported: {stderr}"
    );
}

#[test]
fn fix_carries_a_comment_from_a_removed_sub_table_entry() {
    // A `[workspace.dependencies.name]` entry keeps its comments on the table,
    // not on the key, so reading only the key decor would drop them unremarked.
    let root = concat!(
        "[workspace]\nmembers = [\"member\"]\n\n",
        "[workspace.dependencies]\nserde = \"1\"\n\n",
        "# pinned to 1.2 until upstream #42 is fixed\n",
        "[workspace.dependencies.once_cell]\nversion = \"1\"\n",
    );
    let dir = workspace(root, &[("member", "[dependencies]\nserde = { workspace = true }\n")]);
    let manifest = dir.path().join("Cargo.toml");

    let (success, _, stderr) = outcome(&run(&manifest, &["--fix"]));

    assert!(success, "--fix should succeed: {stderr}");

    let fixed = fs::read_to_string(&manifest).expect("failed to read the fixed manifest");
    assert!(!fixed.contains("once_cell"), "the unused entry should be gone: {fixed}");
    assert!(fixed.contains("pinned to 1.2"), "the sub-table's comment must survive: {fixed}");
    assert!(
        stderr.contains("Carried 1 comment line from 'once_cell' onto 'serde'"),
        "the move must be reported: {stderr}"
    );
}

#[test]
fn a_manifest_without_a_workspace_table_passes_with_a_note() {
    let dir = TempDir::new().expect("failed to create temp dir");
    let manifest = dir.path().join("Cargo.toml");
    fs::write(&manifest, "[package]\nname = \"solo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").expect("failed to write manifest");

    let (success, _, stderr) = outcome(&run(&manifest, &[]));

    assert!(success, "a non-workspace manifest has no catalog to be wrong about");
    assert!(stderr.contains("has no [workspace] table"), "unexpected stderr: {stderr}");
}

#[test]
fn require_workspace_rejects_a_manifest_without_a_workspace_table() {
    let dir = TempDir::new().expect("failed to create temp dir");
    let manifest = dir.path().join("Cargo.toml");
    fs::write(&manifest, "[package]\nname = \"solo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").expect("failed to write manifest");

    let (success, _, stderr) = outcome(&run(&manifest, &["--require-workspace"]));

    assert!(!success, "--require-workspace makes the missing table an error");
    assert!(stderr.contains("has no [workspace] table"), "unexpected stderr: {stderr}");
}

#[test]
fn an_empty_catalog_passes() {
    let dir = workspace("[workspace]\nmembers = [\"member\"]\n", &[("member", "")]);
    let manifest = dir.path().join("Cargo.toml");

    let (success, stdout, _) = outcome(&run(&manifest, &[]));

    assert!(success, "no catalog means nothing to check");
    assert!(stdout.contains("declares no workspace dependencies"), "unexpected stdout: {stdout}");
}

#[test]
fn a_missing_manifest_is_an_error() {
    let dir = TempDir::new().expect("failed to create temp dir");
    let manifest = dir.path().join("Cargo.toml");

    let (success, _, stderr) = outcome(&run(&manifest, &[]));

    assert!(!success, "a missing manifest is a real failure");
    assert!(stderr.contains("failed to read"), "unexpected stderr: {stderr}");
}

#[test]
fn an_unparsable_manifest_is_an_error() {
    let dir = TempDir::new().expect("failed to create temp dir");
    let manifest = dir.path().join("Cargo.toml");
    fs::write(&manifest, "[workspace\n").expect("failed to write manifest");

    let (success, _, stderr) = outcome(&run(&manifest, &[]));

    assert!(!success, "a broken manifest is a real failure");
    assert!(stderr.contains("failed to parse"), "unexpected stderr: {stderr}");
}

#[test]
fn a_workspace_cargo_cannot_load_is_an_error() {
    // `members` names a directory that has no manifest, so `cargo metadata`
    // fails. Reporting that beats silently treating the member as inheriting
    // nothing, which would accuse every entry it inherits.
    let dir = workspace(
        "[workspace]\nmembers = [\"member\", \"ghost\"]\n\n[workspace.dependencies]\nserde = \"1\"\n",
        &[("member", "[dependencies]\nserde = { workspace = true }\n")],
    );
    let manifest = dir.path().join("Cargo.toml");

    let (success, _, stderr) = outcome(&run(&manifest, &[]));

    assert!(!success, "an unloadable workspace is a real failure");
    assert!(
        stderr.contains("failed to enumerate the workspace members"),
        "unexpected stderr: {stderr}"
    );
}
