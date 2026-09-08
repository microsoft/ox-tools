// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))] // miri can't sandbox FS ops these tests do (TempDir, assert_cmd, etc.)
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic-on-failure idioms are appropriate in tests"
)]

//! Snapshot tests for onboarding a TOML host that already declares the
//! region's table by hand.
//!
//! Each test stages one hand-written host, runs `cargo anvil` over it, and
//! snapshots a single report: the host as the user wrote it, the decision
//! anvil reached for every region of that host, any refusal it raised, and the
//! host as anvil left it. The assertions elsewhere pin individual properties —
//! that a file parses, that a key survives, that a refusal names a table. These
//! snapshots pin the thing a reader actually has to judge: what the user's file
//! looks like afterwards.
//!
//! The scenarios are the ones this behaviour was built and reviewed against:
//! adopting a hand-written table (issue #148), refusing a key both sides
//! declare, refusing residue that cannot stay in the table it came from,
//! keeping an unmanaged dotted assignment, and leaving a CRLF host's line
//! endings alone.
//!
//! Reviewed with `cargo insta review`; snapshots live under `tests/snapshots/`.

#![expect(clippy::unwrap_used, reason = "integration tests favor concise assertions over Result plumbing")]

use std::fmt::Write as _;
use std::path::Path;

use cargo_anvil::test_support::{Cli, RunOutcome, Target, run_update};
use cargo_anvil::{Artifact, Catalog, CliMeta, CommentSyntax, HostSelector, RegionId, RegionSpec};
use tempfile::TempDir;

/// A workspace with nothing in it but the manifest anvil needs to find, plus
/// the one hand-written host under test. Everything else in the tree is
/// anvil's own output and is left out of the report.
fn workspace_with(host_relpath: &str, host: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n",
    );
    write(
        &root.join("crates/alpha/Cargo.toml"),
        "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(&root.join("crates/alpha/src/lib.rs"), "");
    write(&root.join(host_relpath), host);
    tmp
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn run(tmp: &TempDir) -> RunOutcome {
    run_update(
        &cargo_anvil::Catalog::anvil(),
        &Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        },
        tmp.path(),
    )
    .unwrap()
}

/// Render the one thing worth reviewing: what the user wrote, what anvil
/// decided about it, and what the user is left with.
///
/// The written host is re-read from disk rather than taken from the plan, so
/// the snapshot shows the bytes that actually landed. It is parsed on the way
/// past: a host anvil cannot read back is the defect this whole path exists to
/// prevent, and a snapshot of a broken file would otherwise just be accepted
/// on review.
fn report(before: &str, tmp: &TempDir, host_relpath: &str, outcome: &RunOutcome) -> String {
    let after = std::fs::read_to_string(tmp.path().join(host_relpath)).unwrap();
    if let Err(error) = after.parse::<toml_edit::DocumentMut>() {
        panic!("{host_relpath} is not valid TOML after the run: {error}\n---\n{after}\n---");
    }

    let mut out = String::new();
    writeln!(out, "--- {host_relpath}, as the user wrote it ---").unwrap();
    out.push_str(before);
    ensure_trailing_newline(&mut out);

    writeln!(out, "\n--- decisions ---").unwrap();
    let mut regions: Vec<(&String, String)> = outcome
        .plan
        .items()
        .iter()
        .filter_map(|item| match &item.target {
            Target::Region { host, id } if host == host_relpath => Some((id, format!("{:?}", item.decision))),
            _ => None,
        })
        .collect();
    regions.sort();
    for (id, decision) in regions {
        writeln!(out, "{id}: {decision}").unwrap();
    }

    writeln!(out, "\n--- refusals ---").unwrap();
    let refusals: Vec<&String> = outcome
        .plan
        .refusals()
        .iter()
        .filter(|reason| reason.contains(host_relpath))
        .collect();
    if refusals.is_empty() {
        out.push_str("(none)\n");
    } else {
        for reason in refusals {
            writeln!(out, "{reason}").unwrap();
        }
    }

    writeln!(out, "\n--- {host_relpath}, as anvil left it ---").unwrap();
    out.push_str(&after);
    ensure_trailing_newline(&mut out);
    out
}

fn ensure_trailing_newline(out: &mut String) {
    if !out.ends_with('\n') {
        out.push('\n');
    }
}

/// Issue #148, end to end. A repository that already keeps its own
/// `[advisories]` table gets the managed region spliced into that table rather
/// than beside it: one header, the managed keys, and the accepted advisory the
/// managed body says nothing about carried out to just after the region, where
/// TOML still reads it as an `[advisories]` setting.
///
/// The bug this replaced appended a second `[advisories]` header, which
/// `cargo deny` rejects outright — and wrote it to disk and recorded it in the
/// manifest before anything noticed.
#[test]
fn adopts_a_hand_written_table() {
    let before = "\
# We accept this one until upstream ships a fix.
[advisories]
ignore = [\"RUSTSEC-9999-0001\"]
";
    let tmp = workspace_with("deny.toml", before);
    let outcome = run(&tmp);

    insta::assert_snapshot!("adopts_a_hand_written_table", report(before, &tmp, "deny.toml", &outcome));
}

/// A key both sides declare, with different values, has no output that keeps
/// both: TOML forbids repeating it inside one table, and picking either value
/// silently discards a decision somebody made. Anvil refuses that one region
/// and leaves the user's value exactly as it was.
///
/// The refusal is scoped to the region, not to the host or the run — the
/// snapshot shows `anvil-deny-licenses`, `-bans` and `-sources` onboarding into
/// the same file in the same pass. That is what makes refusing an acceptable
/// answer rather than a wall in front of adoption: the user reconciles one
/// table and re-runs.
#[test]
fn refuses_a_key_both_sides_declare() {
    let before = "\
[advisories]
yanked = \"warn\"
";
    let tmp = workspace_with("deny.toml", before);
    let outcome = run(&tmp);

    insta::assert_snapshot!("refuses_a_key_both_sides_declare", report(before, &tmp, "deny.toml", &outcome));
}

/// Residue is re-emitted after the region's closing sentinel, so TOML reads it
/// as a setting of whichever table the body opens **last**. The shipped
/// spellcheck body opens `[Hunspell]` and then `[Hunspell.quirks]`, so a
/// hand-written `[Hunspell]` key that the body does not declare would come back
/// as `Hunspell.quirks.<key>` — valid TOML that parses cleanly and that
/// cargo-spellcheck never reads.
///
/// Anvil refuses instead, naming both tables and the edit that clears it. A
/// file that quietly means something else is exactly the failure mode this PR
/// rejects elsewhere, when it disproves issue #148's dotted-key option.
#[test]
fn refuses_residue_that_cannot_stay_in_its_table() {
    let before = "\
[Hunspell]
transform_regex = [\"^[0-9]+$\"]
";
    let tmp = workspace_with("spellcheck.toml", before);
    let outcome = run(&tmp);

    insta::assert_snapshot!(
        "refuses_residue_that_cannot_stay_in_its_table",
        report(before, &tmp, "spellcheck.toml", &outcome)
    );
}

/// Dotted assignments that share a prefix are one dotted sub-table to the
/// parser, not several independent keys. Locating each leaf by the prefix key
/// gave every leaf but the last an empty slice, so `rust.a_custom_lint` was
/// deleted with its table and re-emitted as nothing — silent data loss in the
/// one direction the design forbids outright.
///
/// The snapshot shows the house rule surviving as residue while
/// `rust.unsafe_op_in_unsafe_fn`, which the managed body declares identically,
/// is dropped as covered.
#[test]
fn keeps_an_unmanaged_dotted_lint() {
    let before = "\
[workspace]
resolver = \"2\"
members = [\"crates/*\"]

[workspace.lints]
# House rule, not in anvil's catalog.
rust.a_custom_lint = \"warn\"
rust.unsafe_op_in_unsafe_fn = \"warn\"
";
    let tmp = workspace_with("Cargo.toml", before);
    let outcome = run(&tmp);

    insta::assert_snapshot!("keeps_an_unmanaged_dotted_lint", report(before, &tmp, "Cargo.toml", &outcome));
}

/// Two regions *of the catalog* on one host that declare the same table. Each
/// is invisible to the other's parser check — the backstop masks every other
/// managed region before it parses — so both used to plan a write and compose a
/// `deny.toml` with two `[licenses]` headers.
///
/// This refusal reads differently from the others on purpose. Both regions are
/// anvil's own, so there is no edit to the host that resolves it; telling the
/// user to reconcile a table they never wrote would send them chasing nothing.
/// It asks for a bug report instead, and the first region still onboards.
#[test]
fn refuses_two_regions_claiming_one_table() {
    let tmp = workspace_with("deny.toml", "");
    std::fs::remove_file(tmp.path().join("deny.toml")).unwrap();

    let catalog = Catalog::builder(CliMeta::new("anvil"))
        .with_artifact(Artifact::region(RegionSpec {
            host: HostSelector::Path("deny.toml".to_owned()),
            id: RegionId::new("anvil-licenses-basics"),
            body: "[licenses]\nallow = [\"MIT\"]\n".to_owned(),
            syntax: CommentSyntax::Hash,
        }))
        .with_artifact(Artifact::region(RegionSpec {
            host: HostSelector::Path("deny.toml".to_owned()),
            id: RegionId::new("anvil-licenses-threshold"),
            body: "[licenses]\nconfidence-threshold = 0.93\n".to_owned(),
            syntax: CommentSyntax::Hash,
        }))
        .build()
        .unwrap();

    let outcome = run_update(
        &catalog,
        &Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        },
        tmp.path(),
    )
    .unwrap();

    insta::assert_snapshot!(
        "refuses_two_regions_claiming_one_table",
        report("(anvil creates this file)\n", &tmp, "deny.toml", &outcome)
    );
}

/// A CRLF host keeps the bytes the user wrote. Two separate places used to
/// break that: the gap check after the region recognised only `\n`, so a CRLF
/// file gained a stray `\n`; and the residue's own terminator was trimmed and
/// replaced with a literal `\n`, ending the relocated block in LF.
///
/// What this pins is that **the user's** line endings are not rewritten — the
/// carried-over comment and the relocated `ignore` line are still CRLF in the
/// output. The managed body between the sentinels is LF, and deliberately so:
/// it is the template's own bytes, and `.anvil.lock` checksums normalize line
/// endings precisely so a `core.autocrlf=true` checkout does not read as a user
/// edit. Rendering the body per host would make the same region hash
/// differently on different machines.
///
/// Line endings are shown as markers because that is the whole subject here —
/// a snapshot of the raw bytes would show two files that look identical.
#[test]
fn preserves_a_crlf_host() {
    let before = "# We accept this one until upstream ships a fix.\r\n[advisories]\r\nignore = [\"RUSTSEC-9999-0001\"]\r\n";
    let tmp = workspace_with("deny.toml", before);
    let outcome = run(&tmp);

    let rendered = report(before, &tmp, "deny.toml", &outcome);
    insta::assert_snapshot!("preserves_a_crlf_host", show_line_endings(&rendered));
}

/// Make line endings visible so a CRLF/LF difference is reviewable rather than
/// invisible.
fn show_line_endings(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    let mut rest = text;
    while let Some(at) = rest.find('\n') {
        let (line, tail) = rest.split_at(at);
        if let Some(line) = line.strip_suffix('\r') {
            out.push_str(line);
            out.push_str("<CRLF>\n");
        } else {
            out.push_str(line);
            out.push_str("<LF>\n");
        }
        rest = &tail[1..];
    }
    out.push_str(rest);
    out
}
