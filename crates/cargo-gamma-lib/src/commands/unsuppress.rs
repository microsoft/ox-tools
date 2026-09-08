// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;

use camino::{Utf8Path, Utf8PathBuf};

use super::cli::UnsuppressArgs;
use super::dispatch::EXIT_OK;
use super::host::Host;
use super::suppress::{Written, recoverable, reject_external_sources, reverted};
use crate::discover::Plan;
use crate::error::error;
use crate::exec::CargoOptions;
use crate::report::{Styler, quantity};

/// Implements `unsuppress`.
///
/// Discovery on its own decides everything here, so nothing is built and no test is run: a directive
/// is idle when the mutants it would govern are not there, which is a fact about the source. That
/// makes this the cheap counterpart to `suppress`, which cannot know what it wants to write until a
/// run has watched every mutant misbehave.
///
/// The safety argument is the mirror of that module's. Writing a directive is dangerous because it
/// might suppress more than intended; removing one is dangerous because it might have been holding
/// something down that the report failed to notice. Both are answered the same way — do it, discover
/// again, compare the suppressed sets, and put the tree back if anything moved.
#[cfg(test)]
pub(super) fn unsuppress<H: Host>(host: &mut H, args: &UnsuppressArgs, styler: Styler) -> crate::Result<i32> {
    let config = crate::config::Config::resolve(&args.select)?;
    let cargo = config.cargo_options();

    unsuppress_with_cargo(host, args, styler, &cargo)
}

/// Implements `unsuppress` with the configuration generation dispatch already resolved.
pub(super) fn unsuppress_with_cargo<H: Host>(
    host: &mut H,
    args: &UnsuppressArgs,
    styler: Styler,
    cargo: &CargoOptions,
) -> crate::Result<i32> {
    let selection = args.select.selection()?;
    let before = crate::discover::plan_for_build(&args.select, &selection, args.select.shard()?, cargo, &mut |_| {})?;

    if before.idle.is_empty() {
        writeln!(
            host.error(),
            "{} nothing to remove: every skip directive in scope suppressed something",
            styler.verb("Finished")
        )?;

        return Ok(EXIT_OK);
    }

    let (removable, declined) = sort_out(&before)?;

    report_declined(host, &declined, styler)?;

    if removable.is_empty() {
        return Ok(EXIT_OK);
    }

    let written = remove_all(host, args, &before, &removable)?;

    let removed: usize = removable.values().map(|removal| removal.lines.len()).sum();

    if !args.apply {
        writeln!(
            host.error(),
            "{} {} in {} would be removed; pass `--apply` to do it",
            styler.verb("Preview"),
            quantity(removed, "skip directive"),
            quantity(removable.len(), "file")
        )?;

        return Ok(EXIT_OK);
    }

    verify_or_revert_with_cargo(host, args, &before, removed, written, styler, cargo)
}

/// Rewrites every file with an idle directive in it, putting back whatever it wrote if any of them
/// fails.
///
/// The mirror of `suppress`'s compensation, for the same reason. A read, a parse or a write can
/// fail on file N with files 1 to N-1 already rewritten, and stopping there would leave directives
/// deleted out of somebody's source by a command that reported failure — the outcome `--apply` is a
/// deliberate opt-in for, arrived at by accident.
fn remove_all<H: Host>(host: &mut H, args: &UnsuppressArgs, before: &Plan, removable: &Removals) -> crate::Result<Written> {
    if args.apply {
        let paths: Vec<&Utf8Path> = removable.keys().map(Utf8PathBuf::as_path).collect();

        reject_external_sources(&before.root, &paths)?;
        recoverable(&before.root, &paths, args.allow_dirty)?;
    }

    let mut written = Written::new();

    match remove_directives(host, args, before, removable, &mut written) {
        Ok(()) => Ok(written),

        Err(cause) => Err(reverted(&before.root, written, cause)),
    }
}

/// Rewrites each file without its idle directives, recording what it held before in `written`.
///
/// Every step is fallible and every step uses `?`, so this stops where it fails: what it has
/// already changed is in `written`, and putting that back is the caller's business. Leaving those
/// files rewritten after the command has failed would be a directive silently deleted from
/// somebody's source — the same hazard `--apply` is a deliberate opt-in for.
fn remove_directives<H: Host>(
    host: &mut H,
    args: &UnsuppressArgs,
    before: &Plan,
    removable: &Removals,
    written: &mut Written,
) -> crate::Result<()> {
    for (path, removal) in removable {
        let absolute = before.root.join(path);
        let source = crate::parse::strip_bom(&removal.text);
        let mut after = crate::fix::remove(source, &removal.lines);

        if removal.text.len() != source.len() {
            after.insert(0, crate::parse::BOM);
        }

        // Parsing before writing, not after: a file that does not parse must never reach the disk,
        // because the revert path is only as good as the copy it holds.
        let _ = syn::parse_file(&after)
            .map_err(|cause| error!("removing the directives would leave {absolute} unparseable").caused_by(cause))?;

        if args.apply {
            // The path discovery accepted is lexical, because it has to preserve how a source is
            // named in reports. Publishing is physical: a source symlink that leaves the
            // workspace must never turn an edit requested here into a write elsewhere.
            let destination = crate::paths::require_within(&absolute, &before.root, "a source edit")?;

            // Read again, not to work from, but to check that the text the line numbers were
            // validated against is still the text on disk. The generation check in `sort_out`
            // protects the planned directive; this one covers a save after that check but before
            // publication. A line delete must never take out whatever moved into its position.
            let current = fs::read_to_string(&destination).map_err(|cause| error!("could not read `{absolute}`").caused_by(cause))?;

            if current != removal.text {
                return Err(error!(
                    "`{absolute}` changed since the run that planned this edit; nothing was removed from it"
                ));
            }

            // `current` is the generation the directive lines were validated against. Checking it
            // again after staging catches changes through that comparison. A non-cooperating save
            // in the syscall interval after comparison is outside the publication API's guarantee.
            match crate::elements::write_if_unchanged(&before.root, &destination, Some(&current), &after)? {
                crate::elements::Publication::Conflict => {
                    return Err(error!(
                        "`{absolute}` changed while this command was preparing to publish its edit; the editor's bytes were left alone"
                    ));
                }
                crate::elements::Publication::Published => {
                    written.push(super::suppress::WrittenFile::new(destination, current, after));
                }
                crate::elements::Publication::PublishedUndurable(cause) => {
                    // The directive is already gone from the visible generation. Keep its
                    // rollback state before reporting that the parent directory was not synced.
                    written.push(super::suppress::WrittenFile::new(destination, current, after));
                    return Err(cause);
                }
            }
        } else {
            write!(host.results(), "{}", crate::fix::diff(path, &removal.text, &after))?;
        }
    }

    Ok(())
}

/// What is to be deleted from each file, gathered so that one file is read and rewritten once.
type Removals = BTreeMap<Utf8PathBuf, Removal>;

/// The lines to delete from one file, and the text they were decided against.
///
/// The text travels with the line numbers because a line number means nothing without it: the
/// validation that decided line 42 holds a directive and the delete that takes line 42 out are two
/// operations over a file anybody else may write between them, and re-reading it for the second one
/// is what lets them disagree.
#[derive(Debug)]
struct Removal {
    /// The one-based lines holding a directive that a plain line delete removes cleanly.
    lines: BTreeSet<usize>,

    /// The file's contents at the moment those lines were validated.
    text: String,
}

/// Splits the idle directives into the ones a line delete removes cleanly and the ones it does not.
///
/// Grouped by file rather than kept flat, because the removal reads and rewrites each file once and
/// the line numbers within it have to be applied together.
fn sort_out(plan: &Plan) -> crate::Result<(Removals, Vec<&crate::suppress::Idle>)> {
    let mut removable = Removals::new();
    let mut declined = Vec::new();
    let mut sources: BTreeMap<&Utf8PathBuf, (String, Vec<String>)> = BTreeMap::new();

    for idle in &plan.idle {
        let (text, lines) = match sources.entry(&idle.file) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let path = plan.root.join(&idle.file);
                let text = fs::read_to_string(&path).map_err(|cause| error!("could not read `{path}`").caused_by(cause))?;
                let source = crate::parse::strip_bom(&text);
                let Some(recorded) = plan.digests.get(&idle.file) else {
                    return Err(error!(
                        "`{path}` has no recorded generation for this planned edit; the planned directive was left alone"
                    ));
                };

                // A removable-looking current line is not enough: an editor could have replaced
                // the idle directive with a live one at the same line. The digest binds the line
                // deletion to the exact source discovery classified as idle.
                if crate::discover::digest(source.as_bytes()) != *recorded {
                    return Err(error!(
                        "`{path}` changed since the run that planned this edit; the planned directive was left alone. Re-run to plan against the file as it is now"
                    ));
                }

                let split = source.lines().map(str::to_owned).collect();

                entry.insert((text, split))
            }
        };

        if idle
            .line
            .checked_sub(1)
            .and_then(|index| lines.get(index))
            .is_some_and(|line| crate::fix::removable(line))
        {
            // The text is carried out with the line numbers rather than read again by the removal,
            // so that what was validated and what is edited are one and the same bytes.
            let entry = removable.entry(idle.file.clone()).or_insert_with(|| Removal {
                lines: BTreeSet::new(),
                text: text.clone(),
            });

            let _ = entry.lines.insert(idle.line);
        } else {
            declined.push(idle);
        }
    }

    Ok((removable, declined))
}

/// Names the idle directives that were left alone, and says why.
///
/// Silence here would be the worst outcome: the run has just reported these as suppressing nothing,
/// and a removal that quietly skips them leaves the user believing they are gone.
fn report_declined<H: Host>(host: &mut H, declined: &[&crate::suppress::Idle], styler: Styler) -> crate::Result<()> {
    if declined.is_empty() {
        return Ok(());
    }

    writeln!(
        host.error(),
        "{} {} share a line with something else and must be removed by hand",
        styler.verb("Skipping"),
        quantity(declined.len(), "skip directive")
    )?;

    for idle in declined {
        writeln!(host.error(), "  {}:{}: skip({})", idle.file, idle.line, idle.selectors)?;
    }

    Ok(())
}

/// Re-runs discovery over the edited tree and reverts unless nothing about the population moved.
///
/// The check is exact, and that is what makes this safe to run unattended. A directive that
/// suppressed nothing cannot, by removing it, change anything at all: the same mutants must be
/// found, and the same ones must still be suppressed. Anything else means the report that named the
/// directive was wrong, and the tree goes back.
#[cfg(test)]
fn verify_or_revert<H: Host>(
    host: &mut H,
    args: &UnsuppressArgs,
    before: &Plan,
    removed: usize,
    written: Written,
    styler: Styler,
) -> crate::Result<i32> {
    let config = crate::config::Config::resolve(&args.select)?;
    let cargo = config.cargo_options();

    verify_or_revert_with_cargo(host, args, before, removed, written, styler, &cargo)
}

/// Verifies against the Cargo options that planned the edit.
fn verify_or_revert_with_cargo<H: Host>(
    host: &mut H,
    args: &UnsuppressArgs,
    before: &Plan,
    removed: usize,
    written: Written,
    styler: Styler,
    cargo: &CargoOptions,
) -> crate::Result<i32> {
    let verified = (|| {
        let selection = args.select.selection()?;
        let after = crate::discover::plan_for_build(&args.select, &selection, args.select.shard()?, cargo, &mut |_| {})?;

        Ok(crate::fix::verify(&before.mutants, &after.mutants, &BTreeSet::new()))
    })();
    let result = match verified {
        Ok(result) => result,
        Err(cause) => return Err(reverted(&before.root, written, cause)),
    };

    if result.is_clean() {
        writeln!(
            host.error(),
            "{} {} from {}",
            styler.verb("Removed"),
            quantity(removed, "skip directive"),
            quantity(written.len(), "file")
        )?;

        return Ok(EXIT_OK);
    }

    Err(reverted(
        &before.root,
        written,
        error!(
            "removing the directives changed what the run found ({} mutants stopped being suppressed, {} started)",
            result.released.len(),
            result.collateral.len()
        ),
    ))
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;
    use crate::fixtures::crate_dir;
    use crate::suppress::Idle;
    use crate::testing::Sink;
    #[cfg(unix)]
    use crate::testing::workdir;

    /// The first line of `source` marked for removal, as `sort_out` would have decided it.
    fn removal(source: &str) -> Removal {
        Removal {
            lines: core::iter::once(1).collect(),
            text: source.to_owned(),
        }
    }

    fn args(root: &Utf8PathBuf, apply: bool) -> UnsuppressArgs {
        UnsuppressArgs {
            select: crate::commands::SelectArgs {
                dir: root.clone(),
                ..crate::commands::SelectArgs::default()
            },
            apply,
            allow_dirty: false,
        }
    }

    /// The default has to be the preview, because deleting a directive that was in fact
    /// load-bearing turns a considered decision into a survivor nobody chose to accept.
    #[test]
    fn a_preview_shows_the_removal_and_leaves_the_file_alone() {
        let source = "// #[gamma::skip(arith)]\npub fn f(a: i32) -> bool { a > 1 }\n";
        let (_dir, root) = crate_dir("unsuppress-preview-", source);
        let mut host = Sink::default();

        let code = unsuppress(&mut host, &args(&root, false), Styler::new(false)).expect("preview");

        assert_eq!(code, EXIT_OK);
        assert!(host.out().contains("-// #[gamma::skip(arith)]"), "{}", host.out());
        assert!(host.err().contains("--apply"), "{}", host.err());
        assert_eq!(fs::read_to_string(root.join("src/lib.rs")).expect("source"), source);
    }

    #[test]
    fn applying_removes_the_directive_and_nothing_else() {
        let source = "// #[gamma::skip(arith)]\npub fn f(a: i32) -> bool { a > 1 }\n";
        let (_dir, root) = crate_dir("unsuppress-apply-", source);
        let mut host = Sink::default();

        let code = unsuppress(&mut host, &args(&root, true), Styler::new(false)).expect("apply");

        assert_eq!(code, EXIT_OK);
        assert_eq!(
            fs::read_to_string(root.join("src/lib.rs")).expect("source"),
            "pub fn f(a: i32) -> bool { a > 1 }\n"
        );
        assert!(host.err().contains("Removed 1 skip directive"), "{}", host.err());
    }

    #[cfg(unix)]
    #[test]
    fn applying_refuses_a_source_symlink_whose_referent_is_outside_the_workspace() {
        let source = "// #[gamma::skip(arith)]\npub fn f(a: i32) -> bool { a > 1 }\n";
        let (_workspace, root) = crate_dir("unsuppress-external-link-", source);
        let external = workdir("unsuppress-external-referent-");
        let outside = Utf8PathBuf::from_path_buf(external.path().join("outside.rs")).expect("UTF-8 path");
        let link = root.join("src/lib.rs");

        fs::write(&outside, source).expect("external source");
        fs::remove_file(&link).expect("replace source with link");
        std::os::unix::fs::symlink(outside.as_std_path(), link.as_std_path()).expect("source link");

        let failure = unsuppress(&mut Sink::default(), &args(&root, true), Styler::new(false))
            .expect_err("an external source referent must not be edited");

        assert!(failure.to_string().contains("outside"), "{failure}");
        assert_eq!(fs::read_to_string(&outside).expect("external source"), source);
    }

    /// A BOM is a marker for the whole file, not for the first directive. Removing that directive
    /// must retain the marker before the code that follows, whether the directive was on the first
    /// line or later in the file.
    #[test]
    fn bom_prefixed_files_keep_the_bom_when_removing_first_and_later_directives() {
        for (name, source, expected) in [
            (
                "first",
                "\u{feff}// #[gamma::skip(arith)]\npub fn f(a: i32) -> bool { a > 1 }\n",
                "\u{feff}pub fn f(a: i32) -> bool { a > 1 }\n",
            ),
            (
                "later",
                "\u{feff}//! A crate.\n// #[gamma::skip(arith)]\npub fn f(a: i32) -> bool { a > 1 }\n",
                "\u{feff}//! A crate.\npub fn f(a: i32) -> bool { a > 1 }\n",
            ),
        ] {
            let (_dir, root) = crate_dir(&format!("unsuppress-bom-{name}-"), source);
            let path = root.join("src/lib.rs");
            let mut host = Sink::default();

            let code = unsuppress(&mut host, &args(&root, true), Styler::new(false)).expect("apply");
            let after = fs::read_to_string(path).expect("source");

            assert_eq!(code, EXIT_OK, "{name}");
            assert_eq!(after, expected, "{name}");
            assert_eq!(after.chars().next(), Some(crate::parse::BOM), "{name}: {after:?}");
        }
    }

    #[test]
    fn a_verification_error_restores_every_removed_directive() {
        let original = "// #[gamma::skip(arith)]\npub fn f(a: i32) -> bool { a > 1 }\n";
        let (_dir, root) = crate_dir("unsuppress-verify-error-", original);
        let path = root.join("src/lib.rs");
        let removed = "pub fn f(a: i32) -> bool { a > 1 }\n";
        fs::write(&path, removed).expect("edited");
        let mut args = args(&root, true);
        args.select.mutators = Some("not.a.mutator".to_owned());
        let mut host = Sink::default();

        let failure = verify_or_revert(
            &mut host,
            &args,
            &Plan {
                skipped: Vec::new(),
                digests: crate::HashMap::default(),
                root,
                files: Vec::new(),
                mutants: Vec::new(),
                suppressed: 0,
                idle: Vec::new(),
                sharded_out: 0,
                settled_out: 0,
                reach: crate::HashMap::default(),
                specs: crate::HashMap::default(),
            },
            1,
            vec![super::super::suppress::WrittenFile::new(
                path.clone(),
                original.to_owned(),
                removed.to_owned(),
            )],
            Styler::new(false),
        )
        .expect_err("selection must fail");

        assert!(failure.to_string().contains("every edit has been reverted"), "{failure}");
        assert_eq!(fs::read_to_string(path).expect("restored"), original);
    }

    /// The quiet case. A directive that is still doing its job must not be touched, and the
    /// command must say so rather than printing an empty diff.
    #[test]
    fn a_directive_that_still_suppresses_something_is_left_alone() {
        let source = "// #[gamma::skip(arith)]\npub fn f(a: i32) -> i32 { a + 1 }\n";
        let (_dir, root) = crate_dir("unsuppress-live-", source);
        let mut host = Sink::default();

        let code = unsuppress(&mut host, &args(&root, true), Styler::new(false)).expect("nothing");

        assert_eq!(code, EXIT_OK);
        assert!(host.err().contains("nothing to remove"), "{}", host.err());
        assert_eq!(fs::read_to_string(root.join("src/lib.rs")).expect("source"), source);
    }

    /// A directive sharing its line with code cannot be deleted by deleting the line, and saying
    /// nothing about it would leave the user believing it was gone.
    #[test]
    fn a_directive_that_shares_its_line_is_named_rather_than_removed() {
        let source = "pub fn f(a: i32) -> bool { a > 1 } // #[gamma::skip(arith)]\n";
        let (_dir, root) = crate_dir("unsuppress-declined-", source);
        let mut host = Sink::default();

        let code = unsuppress(&mut host, &args(&root, true), Styler::new(false)).expect("declined");

        assert_eq!(code, EXIT_OK);
        assert!(host.err().contains("by hand"), "{}", host.err());
        assert!(host.err().contains("src/lib.rs:1"), "{}", host.err());
        assert_eq!(fs::read_to_string(root.join("src/lib.rs")).expect("source"), source);
    }

    /// The line numbers have to be applied together, or removing the first shifts the second.
    #[test]
    fn several_directives_in_one_file_are_all_removed() {
        let source =
            "// #[gamma::skip(arith)]\npub fn f(a: i32) -> bool { a > 1 }\n// #[gamma::skip(arith)]\npub fn g(a: i32) -> bool { a < 1 }\n";
        let (_dir, root) = crate_dir("unsuppress-several-", source);
        let mut host = Sink::default();

        let _ = unsuppress(&mut host, &args(&root, true), Styler::new(false)).expect("apply");

        let text = fs::read_to_string(root.join("src/lib.rs")).expect("source");

        assert!(!text.contains("gamma::skip"), "{text}");
        assert!(text.contains("fn f") && text.contains("fn g"), "{text}");
    }

    #[test]
    fn a_declined_directive_is_reported_with_its_place_and_selectors() {
        let idle = Idle {
            file: Utf8PathBuf::from("src/lib.rs"),
            line: 7,
            selectors: "arith".to_owned(),
            reason: None,
        };
        let mut host = Sink::default();

        report_declined(&mut host, &[&idle], Styler::new(false)).expect("note");

        let text = host.err();

        assert!(text.contains("1 skip directive"), "{text}");
        assert!(text.contains("src/lib.rs:7"), "{text}");
        assert!(text.contains("skip(arith)"), "{text}");
    }

    #[test]
    fn nothing_declined_says_nothing() {
        let mut host = Sink::default();

        report_declined(&mut host, &[], Styler::new(false)).expect("note");

        assert!(host.err().is_empty(), "{}", host.err());
    }

    /// The defence in depth, and the one no ordinary tree can reach: a directive reported as idle
    /// cannot change anything by being removed, so the only way to exercise the revert is to hand
    /// the verification a "before" that claims otherwise. If it ever fires for real, the report that
    /// named the directive was wrong, and the tree has to go back exactly as it was.
    #[test]
    fn a_removal_that_changes_what_is_suppressed_puts_every_file_back() {
        let source = "pub fn f(a: i32) -> i32 { a + 1 }\n";
        let (_dir, root) = crate_dir("unsuppress-revert-", source);
        let path = root.join("src/lib.rs");

        fs::write(&path, "pub fn f(a: i32) -> i32 { a - 1 }\n").expect("edited");

        let mut before = crate::discover::plan(
            &crate::commands::SelectArgs {
                dir: root.clone(),
                ..crate::commands::SelectArgs::default()
            },
            &crate::ops::registry::Selection::default_preset(),
            None,
            &mut |_| {},
        )
        .expect("plan");

        // A claim that something was suppressed, which the fresh discovery will not agree with.
        before.mutants[0].suppression = Some(crate::model::Suppression {
            channel: crate::model::Channel::Comment,
            reason: None,
            tag: None,
            line: Some(1),
        });

        let mut host = Sink::default();
        let written = vec![super::super::suppress::WrittenFile::new(
            path.clone(),
            source.to_owned(),
            "pub fn f(a: i32) -> i32 { a - 1 }\n".to_owned(),
        )];
        let error = verify_or_revert(&mut host, &args(&root, true), &before, 1, written, Styler::new(false)).unwrap_err();

        assert!(error.to_string().contains("reverted"), "{error}");
        assert_eq!(fs::read_to_string(&path).expect("source"), source, "the file was not put back");
    }

    /// A plan holding only the root, which is all the removal loop reads out of one.
    fn plan_at(root: &Utf8PathBuf) -> Plan {
        Plan {
            skipped: Vec::new(),
            digests: crate::HashMap::default(),
            root: root.clone(),
            files: Vec::new(),
            mutants: Vec::new(),
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            reach: crate::HashMap::default(),
            specs: crate::HashMap::default(),
        }
    }

    /// The two-file case the compensation exists for. A directive deleted out of the first file by
    /// a command that then failed is the worst of both worlds: the user is told nothing happened,
    /// and the reason somebody wrote that directive is gone from the tree.
    #[test]
    fn a_failure_on_the_second_file_puts_the_first_one_back_byte_for_byte() {
        let source = "// #[gamma::skip(arith)]\npub fn f(a: i32) -> bool { a > 1 }\n";
        let (_dir, root) = crate_dir("unsuppress-rollback-", source);
        let first = root.join("src/lib.rs");
        let original = fs::read(first.as_std_path()).expect("the original bytes");

        // Absent, and named so that it sorts after the first: reading it fails on every platform
        // and for every user, root included.
        let removable: Removals = [
            (Utf8PathBuf::from("src/lib.rs"), removal(source)),
            (Utf8PathBuf::from("src/zzz_gone.rs"), removal(source)),
        ]
        .into_iter()
        .collect();
        let mut host = Sink::default();

        let error = remove_all(&mut host, &args(&root, true), &plan_at(&root), &removable).expect_err("the second file");

        assert!(error.to_string().contains("zzz_gone.rs"), "{error}");
        assert!(error.to_string().contains("every edit has been reverted"), "{error}");
        assert_eq!(
            fs::read(first.as_std_path()).expect("the bytes afterwards"),
            original,
            "the first file was left with its directive removed"
        );
    }

    /// The same loop, with nothing in its way, does rewrite both files — so the test above is
    /// asserting that a rollback happened rather than that the loop never got started.
    #[test]
    fn both_files_are_rewritten_when_nothing_fails() {
        let source = "// #[gamma::skip(arith)]\npub fn f(a: i32) -> bool { a > 1 }\n";
        let (_dir, root) = crate_dir("unsuppress-both-", source);

        fs::write(root.join("src/other.rs"), source).expect("other");

        let removable: Removals = [
            (Utf8PathBuf::from("src/lib.rs"), removal(source)),
            (Utf8PathBuf::from("src/other.rs"), removal(source)),
        ]
        .into_iter()
        .collect();
        let mut host = Sink::default();

        let written = remove_all(&mut host, &args(&root, true), &plan_at(&root), &removable).expect("both files");

        assert_eq!(written.len(), 2);
        assert!(!fs::read_to_string(root.join("src/lib.rs")).expect("lib").contains("gamma::skip"));
        assert!(
            !fs::read_to_string(root.join("src/other.rs"))
                .expect("other")
                .contains("gamma::skip")
        );
    }

    /// A write that cannot be staged leaves the source exactly as it was, rather than truncated:
    /// the injected partial write, standing in for a full disk or a kill between the truncate and
    /// the replacement bytes.
    #[test]
    fn a_write_that_cannot_be_staged_leaves_the_source_untouched() {
        let source = "// #[gamma::skip(arith)]\npub fn f(a: i32) -> bool { a > 1 }\n";
        let (_dir, root) = crate_dir("unsuppress-partial-write-", source);
        let path = root.join("src/lib.rs");
        let original = fs::read(path.as_std_path()).expect("the original bytes");

        let scratch = root.join(".blocked-stage");
        fs::create_dir(scratch.as_std_path()).expect("block the staging file");
        crate::elements::next_scratch_path(scratch);

        let removable: Removals = core::iter::once((Utf8PathBuf::from("src/lib.rs"), removal(source))).collect();
        let mut host = Sink::default();

        let error = remove_all(&mut host, &args(&root, true), &plan_at(&root), &removable).expect_err("the write");

        assert!(error.to_string().contains("lib.rs"), "{error}");
        assert_eq!(
            fs::read(path.as_std_path()).expect("the bytes afterwards"),
            original,
            "a failed write did not leave the source alone"
        );
    }

    /// Validating a line and deleting it are two operations over a file the user may write between
    /// them. A save that shifts the contents by a line turns "delete the directive on line 1" into
    /// "delete line 1", which is now somebody's code. The source generation from discovery is the
    /// binding between the planned directive and that line, so it is checked before removals are
    /// even selected.
    #[test]
    fn a_file_that_changed_since_planning_is_left_alone_rather_than_edited_by_line_number() {
        let source = "// #[gamma::skip(arith)]\npub fn f(a: i32) -> bool { a > 1 }\n";
        let (_dir, root) = crate_dir("unsuppress-shifted-", source);
        let path = root.join("src/lib.rs");
        let mut plan = Plan {
            idle: vec![Idle {
                file: Utf8PathBuf::from("src/lib.rs"),
                line: 1,
                selectors: "arith".to_owned(),
                reason: None,
            }],
            ..plan_at(&root)
        };
        let _recorded = plan.digests.insert(
            Utf8PathBuf::from("src/lib.rs"),
            crate::discover::digest(crate::parse::strip_bom(source).as_bytes()),
        );

        // The user saves the file between discovery and selecting a directive to remove.
        let edited = "//! A crate.\n// #[gamma::skip(arith)]\npub fn f(a: i32) -> bool { a > 1 }\n";
        fs::write(&path, edited).expect("the concurrent save");

        let error = sort_out(&plan).expect_err("the changed file");

        assert!(
            error.to_string().contains("changed since the run that planned this edit"),
            "{error}"
        );
        assert_eq!(
            fs::read_to_string(&path).expect("the file afterwards"),
            edited,
            "the removal edited a file it had not examined"
        );
    }

    /// An editor can replace an idle directive with a live one without moving its line. Checking
    /// only that the current line is removable would then delete a new suppression and report a
    /// successful no-op verification. The planned source generation rejects the replacement.
    #[test]
    fn a_planned_idle_directive_replaced_with_a_live_one_is_retained() {
        let idle = "// #[gamma::skip(arith)]\npub fn f(a: i32) -> bool { a > 1 }\n";
        let live = "// #[gamma::skip(relational)]\npub fn f(a: i32) -> bool { a > 1 }\n";
        let (_dir, root) = crate_dir("unsuppress-replaced-directive-", idle);
        let path = root.join("src/lib.rs");
        let mut plan = Plan {
            idle: vec![Idle {
                file: Utf8PathBuf::from("src/lib.rs"),
                line: 1,
                selectors: "arith".to_owned(),
                reason: None,
            }],
            ..plan_at(&root)
        };
        let _recorded = plan.digests.insert(
            Utf8PathBuf::from("src/lib.rs"),
            crate::discover::digest(crate::parse::strip_bom(idle).as_bytes()),
        );

        fs::write(&path, live).expect("replacement directive");

        let error = sort_out(&plan).expect_err("the planned directive no longer exists");

        assert!(error.to_string().contains("planned directive was left alone"), "{error}");
        assert_eq!(fs::read_to_string(path).expect("source"), live);
    }

    /// Rechecking after staging closes the remaining interval after line validation: the editor's
    /// new generation wins, and the deletion is reported as a conflict rather than published.
    #[test]
    fn a_save_after_validation_and_before_unsuppress_publication_is_left_alone() {
        let source = "// #[gamma::skip(arith)]\npub fn f(a: i32) -> bool { a > 1 }\n";
        let (_dir, root) = crate_dir("unsuppress-publication-conflict-", source);
        let path = root.join("src/lib.rs");
        let editor = "//! saved by the editor\n// #[gamma::skip(arith)]\npub fn f(a: i32) -> bool { a > 1 }\n".to_owned();
        let editor_path = path.clone();
        let removable: Removals = core::iter::once((Utf8PathBuf::from("src/lib.rs"), removal(source))).collect();

        crate::elements::before_next_publication(move |_| {
            fs::write(editor_path, &editor).expect("the editor save");
        });

        let error = remove_all(&mut Sink::default(), &args(&root, true), &plan_at(&root), &removable)
            .expect_err("the generation changed after validation");

        assert!(
            error.to_string().contains("changed while this command was preparing to publish"),
            "{error}"
        );
        assert_eq!(
            fs::read_to_string(path).expect("the editor's bytes"),
            "//! saved by the editor\n// #[gamma::skip(arith)]\npub fn f(a: i32) -> bool { a > 1 }\n"
        );
    }

    /// The deletion is visible before syncing its directory can fail. It must be added to the
    /// compensation set first, so an error after the rename restores the directive.
    #[test]
    fn a_post_rename_unsuppress_sync_failure_is_reverted() {
        let source = "// #[gamma::skip(arith)]\npub fn f(a: i32) -> bool { a > 1 }\n";
        let (_dir, root) = crate_dir("unsuppress-sync-failure-", source);
        let path = root.join("src/lib.rs");
        let removable: Removals = core::iter::once((Utf8PathBuf::from("src/lib.rs"), removal(source))).collect();

        crate::elements::fail_next_directory_sync();

        let error =
            remove_all(&mut Sink::default(), &args(&root, true), &plan_at(&root), &removable).expect_err("the post-rename sync fails");

        assert!(error.to_string().contains("injected directory sync failure"), "{error}");
        assert!(error.to_string().contains("every edit has been reverted"), "{error}");
        assert_eq!(fs::read_to_string(path).expect("directive restored"), source);
    }

    /// Deleting a directive deletes the reason somebody wrote it, and an interrupt part-way through
    /// the loop leaves no artifact behind to reconstruct it from. Version control is the journal,
    /// so a file with nothing committed behind it is refused before the first delete.
    #[test]
    fn removing_from_a_file_with_uncommitted_changes_is_refused_before_anything_is_deleted() {
        let source = "// #[gamma::skip(arith)]\npub fn f(a: i32) -> bool { a > 1 }\n";
        let (_dir, root) = crate_dir("unsuppress-dirty-", source);
        let path = root.join("src/lib.rs");

        let started = std::process::Command::new("git")
            .arg("-C")
            .arg(root.as_std_path())
            .args(["init", "--quiet"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        if !started.is_ok_and(|status| status.success()) {
            return;
        }

        let removable: Removals = core::iter::once((Utf8PathBuf::from("src/lib.rs"), removal(source))).collect();
        let mut arguments = args(&root, true);

        let error = remove_all(&mut Sink::default(), &arguments, &plan_at(&root), &removable).expect_err("a dirty tree");

        assert!(error.is_usage(), "{error}");
        assert!(error.to_string().contains("--allow-dirty"), "{error}");
        assert_eq!(
            fs::read_to_string(&path).expect("afterwards"),
            source,
            "the refusal still edited the file"
        );

        arguments.allow_dirty = true;

        let _ = remove_all(&mut Sink::default(), &arguments, &plan_at(&root), &removable).expect("the override");

        assert!(!fs::read_to_string(&path).expect("afterwards").contains("gamma::skip"));
    }
}
