// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::Write;

use super::cli::HintsArgs;
use super::dispatch::EXIT_OK;
use super::host::Host;
use crate::discover::{Hints, RunRecord, hints_path};
use crate::report::{Styler, quantity};

/// Implements `hints`.
///
/// Promoting rather than copying is the whole point of the command existing. A documented `cp` from
/// the scratch record would put a file in version control with a format nobody owns, no version, no
/// provenance, entries for mutants that were edited away three months ago, and — worst — every
/// verdict the record holds, which the next run would then be reading from somebody else's machine.
/// Everything this does that a copy does not is one of those problems:
///
/// - it admits only the tiers that cannot move a score, and drops the rest on the floor;
/// - it uses the workspace-relative identities persisted by the selected campaign instead of
///   rediscovering and reparsing the current population;
/// - it stamps the format version and the tool that wrote it;
/// - it writes atomically and reads back what it wrote.
///
/// Errors are surfaced rather than absorbed, which is the reverse of how the artifact is read. A
/// run consults the file automatically and must never fail over it, so reading is best-effort.
/// Writing it is something somebody asked for: incremental promotion therefore refuses an existing
/// generation it cannot understand, while `--replace` is the explicit permission to discard one.
/// Publication compares against the exact bytes used for the merge so concurrent writers cannot
/// silently replace one another.
/// Implements `hints` with the configuration generation dispatch already resolved.
#[cfg_attr(coverage_nightly, coverage(off))]
pub(super) fn hints<H: Host>(host: &mut H, args: &HintsArgs, styler: Styler) -> crate::Result<i32> {
    let selected = crate::paths::physical(&args.dir)?;
    let (root, base, _lock) = crate::exec::claim_campaign_state(&selected, args.cache_dir.as_deref())?;
    let record = promotion_record(
        args.replace,
        || crate::exec::partial_testing_record(&base),
        |strict| {
            if strict {
                RunRecord::load_required(&base)
            } else {
                Ok(RunRecord::load(&base))
            }
        },
    )?;
    let (existing, generation) = Hints::load_for_promotion(&root, args.replace)?;
    let (promoted, omitted) = Hints::promoted_record(&record);
    let merged = if args.replace {
        Hints::merged(&existing, promoted, true, &root)?
    } else {
        Hints::merged_record(&existing, promoted, &record, &root)?
    };
    finish_promotion(
        host,
        args,
        PreparedPromotion {
            root: &root,
            base: &base,
            existing: &existing,
            generation: generation.as_deref(),
            merged: &merged,
            omitted,
        },
        styler,
    )
}

/// Borrowed preparation snapshot for final preview/publication under the caller-held claim.
/// `generation` identifies the loaded `existing` artifact from which `merged` was derived.
#[derive(Clone, Copy)]
struct PreparedPromotion<'a> {
    root: &'a camino::Utf8Path,
    base: &'a camino::Utf8Path,
    existing: &'a Hints,
    generation: Option<&'a str>,
    merged: &'a Hints,
    omitted: usize,
}

fn finish_promotion<H: Host>(host: &mut H, args: &HintsArgs, promotion: PreparedPromotion<'_>, styler: Styler) -> crate::Result<i32> {
    let PreparedPromotion {
        root,
        base,
        existing,
        generation,
        merged,
        omitted,
    } = promotion;
    let changes = merged.changes_from(existing);

    if omitted > 0 {
        writeln!(
            host.error(),
            "{} {} was outside the current campaign population or had no persisted source path and could not be promoted; generalized knowledge was preserved",
            styler.warning(),
            quantity(omitted, "exact hint")
        )?;
    }

    if merged.is_empty() && existing.is_empty() && generation.is_none() && !args.replace {
        writeln!(
            host.error(),
            "{} nothing to promote: no run under `{base}` has recorded a killing test, build-order hint, or generalized test-order hint for the current population",
            styler.verb("Finished")
        )?;

        return Ok(EXIT_OK);
    }

    let path = hints_path(root);

    if args.dry_run {
        writeln!(
            host.error(),
            "{} `{path}`: {}, {}, {}, and {}",
            styler.verb("Preview"),
            format_args!("{} added", quantity(changes.added, "record")),
            format_args!("{} updated", quantity(changes.updated, "record")),
            format_args!("{} removed", quantity(changes.removed, "record")),
            format_args!("{} preserved", quantity(changes.preserved, "record"))
        )?;

        return Ok(EXIT_OK);
    }

    let promotion = if !args.replace && generation.is_some() && changes.is_empty() {
        merged.counts()
    } else {
        merged.write_from_locked(&path, generation)?
    };

    let verb = if promotion.changed { "Wrote" } else { "Unchanged" };

    writeln!(
        host.error(),
        "{} `{path}`: {}, {}, {}, and {}",
        styler.verb(verb),
        format_args!("{} added", quantity(changes.added, "record")),
        format_args!("{} updated", quantity(changes.updated, "record")),
        format_args!("{} removed", quantity(changes.removed, "record")),
        format_args!("{} preserved", quantity(changes.preserved, "record"))
    )?;

    Ok(EXIT_OK)
}

fn promotion_record(
    replace: bool,
    partial: impl FnOnce() -> crate::Result<Option<RunRecord>>,
    completed: impl FnOnce(bool) -> crate::Result<RunRecord>,
) -> crate::Result<RunRecord> {
    if replace {
        return completed(true);
    }
    match partial()? {
        Some(record) => Ok(record),
        None => completed(false),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use serde_json::{from_value, json, to_value};

    use super::*;
    use crate::error::error;

    fn recovered_nonempty_hints() -> Hints {
        let mutant = crate::model::Mutant {
            file: camino::Utf8PathBuf::from("lib.rs").into(),
            outcome: crate::model::Outcome::CompileError,
            ..crate::fixtures::mutant()
        };
        let population = [mutant];
        let mut wire = to_value(RunRecord::default()).unwrap();
        wire["files"] = json!([{
            "path": "lib.rs", "digest": "controlled", "size": 0,
            "mutants": [{"id": population[0].id, "outcome": population[0].outcome}]
        }]);
        let record = from_value(wire).unwrap();
        let hints = Hints::promoted(&record, &population);
        assert!(!hints.is_empty());
        hints
    }

    #[test]
    fn recovered_empty_promotion_previews_removal_of_existing_knowledge_instead_of_claiming_nothing_to_promote() {
        let root = camino::Utf8Path::new("never-read-promotion-root");
        let existing = recovered_nonempty_hints();
        let merged = Hints::default();
        let mut host = crate::testing::Sink::default();
        let exit = finish_promotion(
            &mut host,
            &HintsArgs {
                dir: root.to_owned(),
                cache_dir: None,
                replace: false,
                dry_run: true,
            },
            PreparedPromotion {
                root,
                base: root,
                existing: &existing,
                generation: None,
                merged: &merged,
                omitted: 0,
            },
            Styler::new(false),
        )
        .unwrap();
        assert_eq!(exit, EXIT_OK);
        assert!(host.out().is_empty());
        assert_eq!(
            host.err(),
            format!(
                "     Preview `{}`: 0 records added, 0 records updated, 1 record removed, and 0 records preserved\n",
                hints_path(root)
            )
        );
    }

    #[test]
    fn recovered_nothing_to_promote_returns_its_diagnostic_writer_error_without_touching_a_hint_file() {
        let root = camino::Utf8Path::new("never-read-promotion-root");
        let empty = Hints::default();
        let result = finish_promotion(
            &mut crate::testing::BrokenHost,
            &HintsArgs {
                dir: root.to_owned(),
                cache_dir: None,
                replace: false,
                dry_run: false,
            },
            PreparedPromotion {
                root,
                base: root,
                existing: &empty,
                generation: None,
                merged: &empty,
                omitted: 0,
            },
            Styler::new(false),
        );
        assert_eq!(result.unwrap_err().to_string(), "I/O error: broken pipe");
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "native publication failure requires filesystem access; pure promotion tests remain enabled"
    )]
    fn recovered_unchanged_promotion_without_a_loaded_generation_still_attempts_publication_and_returns_its_native_error() {
        let root = camino::Utf8Path::new("invalid\0promotion-root");
        let existing = recovered_nonempty_hints();
        let merged = existing.clone();
        let mut host = crate::testing::Sink::default();
        let result = finish_promotion(
            &mut host,
            &HintsArgs {
                dir: root.to_owned(),
                cache_dir: None,
                replace: false,
                dry_run: false,
            },
            PreparedPromotion {
                root,
                base: root,
                existing: &existing,
                generation: None,
                merged: &merged,
                omitted: 0,
            },
            Styler::new(false),
        );
        assert_eq!(result.unwrap_err().message(), format!("could not write `{}`", hints_path(root)));
        assert!(host.out().is_empty());
        assert!(host.err().is_empty());
    }

    #[cfg(all(feature = "system-tests", not(miri)))]
    mod native_acceptance {
        use super::*;
        use crate::testing::{Sink, fails_at_every_line};

        fn promotion_args(root: &camino::Utf8Path, replace: bool, dry_run: bool) -> HintsArgs {
            HintsArgs {
                dir: root.to_owned(),
                cache_dir: None,
                replace,
                dry_run,
            }
        }

        fn promotion_fixture(root: &camino::Utf8Path, revision: char, id: Option<&str>) -> (Hints, String) {
            let files = id.map_or_else(
                || "[]".to_owned(),
                |id| format!("\n- path: src/lib.rs\n  killers: []\n  mutants:\n  - id: {id}\n    unviable: true"),
            );
            let text = format!(
                "version: 4\ntool: cargo-gamma fixture\ncontext:\n  repo_sha: '{}'\n  generated_on: '2026-09-16'\nfiles: {files}\n",
                revision.to_string().repeat(40)
            );
            std::fs::write(hints_path(root), &text).unwrap();
            let (hints, generation) = Hints::load_for_promotion(root, false).unwrap();
            assert_eq!(hints.is_empty(), id.is_none());
            assert_eq!(generation.as_deref(), Some(text.as_str()));
            (hints, text)
        }

        fn survivor_campaign(outcome: Option<crate::model::Outcome>) -> (tempfile::TempDir, HintsArgs, Option<String>) {
            let directory = crate::testing::workdir("hints-actual-campaign-");
            crate::testing::write_project(
                directory.path(),
                &[
                    (
                        "Cargo.toml",
                        "[package]\nname = \"subject\"\nversion = \"0.0.0\"\nedition = \"2024\"\n[workspace]\n",
                    ),
                    ("src/lib.rs", "pub fn value() -> i32 { 42 }\n"),
                ],
            );
            let root = camino::Utf8Path::from_path(directory.path()).expect("fixture roots are UTF-8");
            let root = crate::paths::physical(root).expect("fixture roots can be normalized");
            let base = root.join("target/campaign");
            std::fs::create_dir_all(&base).expect("campaign fixture directory can be created");
            crate::exec::mark_cache_owned_for_test(&root, &base);
            let parsed =
                crate::parse::SourceFile::parse("src/lib.rs", "pub fn value() -> i32 { 42 }\n".to_owned()).expect("fixture Rust is valid");
            let selection = crate::ops::Selection::parse("literal.int_to_zero").expect("fixture mutator exists");
            let candidates = crate::ops::collect_candidates(&parsed, &selection);
            let mut mutants = crate::ops::into_mutants(&parsed, "subject", candidates);
            if let Some(outcome) = outcome {
                for mutant in &mut mutants {
                    mutant.outcome = outcome;
                }
            } else {
                mutants.clear();
            }
            let id = mutants.first().map(|mutant| mutant.id.to_string());
            let context = crate::discover::record_context(&crate::discover::RecordContext {
                toolchain: Some("fixture"),
                ..crate::discover::RecordContext::default()
            })
            .expect("the fixture has a named compiler identity");
            RunRecord::from_run(&root, &mutants, &context, &[root.join("src")]).store(&base, &root);
            RunRecord::load_required(&base).expect("the real record constructor published a readable campaign");
            let args = HintsArgs {
                dir: root,
                cache_dir: Some(base),
                replace: false,
                dry_run: false,
            };
            (directory, args, id)
        }

        #[test]
        fn survivor_actual_hint_command_keeps_color_and_returns_native_path_lock_and_partial_ledger_errors() {
            let (_directory, mut args, _) = survivor_campaign(None);
            let mut host = Sink::default();
            assert_eq!(
                hints(&mut host, &args, Styler::new(true)).expect("an empty retained campaign is valid"),
                EXIT_OK
            );
            assert!(host.err().contains("\x1b["), "{}", host.err());
            assert!(host.err().contains("nothing to promote"), "{}", host.err());
            let root = args.dir.clone();
            args.dir = root.join("\0");
            let error = hints(&mut Sink::default(), &args, Styler::new(false))
                .expect_err("an embedded NUL is a native physical-path error before any campaign lookup");
            assert!(error.to_string().contains("could not resolve"), "{error}");
            args.dir = root.clone();
            {
                let _lock = crate::exec::claim_workspace(&root).expect("the fixture holds the actual workspace lock");
                hints(&mut Sink::default(), &args, Styler::new(false)).expect_err("an active workspace cannot be claimed for promotion");
            }
            let ledger = args
                .cache_dir
                .as_ref()
                .expect("fixture uses an explicit campaign cache")
                .join("testing-phase.json");
            std::fs::write(&ledger, "[").expect("an interrupted campaign ledger can be malformed");
            let error =
                hints(&mut Sink::default(), &args, Styler::new(false)).expect_err("an existing corrupt partial ledger must not be ignored");
            assert!(error.to_string().contains("testing"), "{error}");
        }

        #[test]
        fn survivor_actual_hint_command_distinguishes_incremental_and_replacement_knowledge() {
            let (_directory, mut args, id) = survivor_campaign(Some(crate::model::Outcome::CompileError));
            let id = id.expect("the actual parsed source produced an unviable mutant");
            for replace in [false, true] {
                let _existing = promotion_fixture(&args.dir, 'a', Some("deadbeefcafe"));
                args.replace = replace;
                hints(&mut Sink::default(), &args, Styler::new(false))
                    .expect("actual offline promotion can capture the enclosing repository identity");
                let published = std::fs::read_to_string(hints_path(&args.dir)).expect("promotion published its checked artifact");
                assert!(published.contains(&id), "{published}");
                assert_eq!(published.contains("deadbeefcafe"), !replace, "{published}");
            }
        }

        #[test]
        fn survivor_actual_hint_command_returns_native_provenance_failures_from_both_merge_policies() {
            let (_directory, mut args, _) = survivor_campaign(Some(crate::model::Outcome::CompileError));
            std::fs::write(args.dir.join(".git"), "not a gitdir directive\n").expect("a real invalid repository boundary can be created");
            for replace in [false, true] {
                args.replace = replace;
                let mut host = Sink::default();
                let error = hints(&mut host, &args, Styler::new(false))
                    .expect_err("changed knowledge cannot invent commit provenance when native git refuses the repository");
                assert!(
                    error.to_string().contains("HEAD") && error.to_string().contains("hints provenance"),
                    "{error}"
                );
                assert!(host.err().is_empty(), "a failed merge must not claim successful promotion");
                assert!(!hints_path(&args.dir).exists(), "failed provenance must not publish an artifact");
            }
        }

        #[test]
        fn survivor_hint_promotion_preview_and_empty_replacement_have_distinct_exact_outputs_and_effects() {
            let directory = crate::testing::workdir("hints-preview-boundaries-");
            let root = camino::Utf8Path::from_path(directory.path()).unwrap();
            let path = hints_path(root);
            let (empty, _) = promotion_fixture(root, 'a', None);
            std::fs::remove_file(&path).unwrap();
            let absent = Hints::default();
            for (replace, dry_run, expected) in [
                (
                    false,
                    false,
                    format!(
                        "Finished nothing to promote: no run under `{root}` has recorded a killing test, build-order hint, or generalized test-order hint for the current population\n"
                    ),
                ),
                (
                    true,
                    true,
                    format!("Preview `{path}`: 0 records added, 0 records updated, 0 records removed, and 0 records preserved\n"),
                ),
                (
                    true,
                    false,
                    format!("Wrote `{path}`: 0 records added, 0 records updated, 0 records removed, and 0 records preserved\n"),
                ),
            ] {
                let mut host = Sink::default();
                assert_eq!(
                    finish_promotion(
                        &mut host,
                        &promotion_args(root, replace, dry_run),
                        PreparedPromotion {
                            root,
                            base: root,
                            existing: &absent,
                            generation: None,
                            merged: &empty,
                            omitted: 0,
                        },
                        Styler::new(false),
                    )
                    .unwrap(),
                    EXIT_OK
                );
                assert_eq!(host.err().trim_start(), expected);
                assert!(host.out().is_empty());
                assert_eq!(path.exists(), replace && !dry_run);
            }
            let (merged, _) = promotion_fixture(root, 'b', Some("deadbeefcafe"));
            std::fs::remove_file(&path).unwrap();
            let mut host = Sink::default();
            finish_promotion(
                &mut host,
                &promotion_args(root, false, true),
                PreparedPromotion {
                    root,
                    base: root,
                    existing: &absent,
                    generation: None,
                    merged: &merged,
                    omitted: 1,
                },
                Styler::new(false),
            )
            .unwrap();
            assert_eq!(
                host.err(),
                format!(
                    "     warning 1 exact hint was outside the current campaign population or had no persisted source path and could not be promoted; generalized knowledge was preserved\n     Preview `{path}`: 1 record added, 0 records updated, 0 records removed, and 0 records preserved\n"
                )
            );
            assert!(!path.exists(), "preview must not publish its promoted knowledge");
        }

        #[test]
        fn survivor_hint_publication_distinguishes_new_changed_unchanged_and_replaced_provenance() {
            let directory = crate::testing::workdir("hints-publication-generations-");
            let root = camino::Utf8Path::from_path(directory.path()).unwrap();
            let path = hints_path(root);
            let (existing, old) = promotion_fixture(root, 'a', Some("deadbeefcafe"));
            let (refreshed, new) = promotion_fixture(root, 'b', Some("deadbeefcafe"));
            let (changed, _) = promotion_fixture(root, 'c', Some("abcdefabcdef"));
            let empty = Hints::default();
            for (replace, generation, previous, merged, expected_verb, added, removed, preserved) in [
                (false, None, &empty, &existing, "Wrote", 1, 0, 0),
                (false, Some(old.as_str()), &existing, &existing, "Unchanged", 0, 0, 1),
                (false, Some(old.as_str()), &existing, &changed, "Wrote", 1, 1, 0),
                (true, Some(old.as_str()), &existing, &refreshed, "Wrote", 0, 0, 1),
            ] {
                if let Some(generation) = generation {
                    std::fs::write(&path, generation).unwrap();
                } else {
                    std::fs::remove_file(&path).unwrap();
                }
                let mut host = Sink::default();
                finish_promotion(
                    &mut host,
                    &promotion_args(root, replace, false),
                    PreparedPromotion {
                        root,
                        base: root,
                        existing: previous,
                        generation,
                        merged,
                        omitted: 0,
                    },
                    Styler::new(false),
                )
                .unwrap();
                assert_eq!(
                    host.err().trim_start(),
                    format!(
                        "{expected_verb} `{path}`: {added} {} added, 0 records updated, {removed} {} removed, and {preserved} {} preserved\n",
                        if added == 1 { "record" } else { "records" },
                        if removed == 1 { "record" } else { "records" },
                        if preserved == 1 { "record" } else { "records" },
                    )
                );
                let published = std::fs::read_to_string(&path).unwrap();
                if replace {
                    assert!(published.contains("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"));
                    assert_ne!(published, old);
                    assert!(new.contains("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"));
                } else if added == 0 {
                    assert_eq!(published, old, "unchanged incremental promotion retains exact generation bytes");
                } else {
                    let observed = Hints::load(root).changes_from(merged);
                    assert!(observed.is_empty());
                    assert_eq!(observed.preserved, 1);
                }
            }
        }

        #[test]
        fn survivor_hint_promotion_returns_publication_and_each_diagnostic_writer_failure() {
            let directory = crate::testing::workdir("hints-writer-boundaries-");
            let root = camino::Utf8Path::from_path(directory.path()).unwrap();
            let path = hints_path(root);
            let (merged, _) = promotion_fixture(root, 'a', Some("deadbeefcafe"));
            let existing = Hints::default();
            std::fs::remove_file(&path).unwrap();
            for dry_run in [false, true] {
                fails_at_every_line(2, |host| {
                    if path.exists() {
                        std::fs::remove_file(&path).unwrap();
                    }
                    finish_promotion(
                        host,
                        &promotion_args(root, false, dry_run),
                        PreparedPromotion {
                            root,
                            base: root,
                            existing: &existing,
                            generation: None,
                            merged: &merged,
                            omitted: 1,
                        },
                        Styler::new(false),
                    )
                    .map(|_| ())
                });
            }
            if path.exists() {
                std::fs::remove_file(&path).unwrap();
            }
            std::fs::create_dir(&path).unwrap();
            let failure = finish_promotion(
                &mut Sink::default(),
                &promotion_args(root, false, false),
                PreparedPromotion {
                    root,
                    base: root,
                    existing: &existing,
                    generation: None,
                    merged: &merged,
                    omitted: 0,
                },
                Styler::new(false),
            )
            .expect_err("a directory collision must reject hint publication without panicking");
            assert!(failure.to_string().contains("hints"));
            assert!(path.is_dir());
        }
    }

    #[test]
    fn replacement_requires_completed_evidence_without_reading_partial_state() {
        let failure = promotion_record(
            true,
            || panic!("replacement must never consult the interrupted campaign"),
            |strict| {
                assert!(strict, "replacement must strictly load a completed record");
                Err(error!("missing completed ledger"))
            },
        )
        .unwrap_err();
        assert_eq!(failure.message(), "missing completed ledger");
        let record = promotion_record(
            true,
            || panic!("replacement must never consult the interrupted campaign"),
            |strict| {
                assert!(strict);
                Ok(RunRecord::default())
            },
        )
        .unwrap();
        assert_eq!(to_value(record).unwrap(), to_value(RunRecord::default()).unwrap());
    }

    #[test]
    fn ordinary_promotion_prefers_partial_evidence_and_keeps_completed_fallback_best_effort() {
        let partial = promotion_record(
            false,
            || Ok(Some(RunRecord::default())),
            |_| panic!("partial evidence must take precedence over older completed evidence"),
        )
        .unwrap();
        assert_eq!(to_value(partial).unwrap(), to_value(RunRecord::default()).unwrap());
        let completed = promotion_record(
            false,
            || Ok(None),
            |strict| {
                assert!(!strict, "ordinary promotion retains best-effort completed-record loading");
                Ok(RunRecord::default())
            },
        )
        .unwrap();
        assert_eq!(to_value(completed).unwrap(), to_value(RunRecord::default()).unwrap());
        let failure = promotion_record(
            false,
            || Err(error!("corrupt interrupted checkpoint")),
            |_| panic!("a corrupt interrupted checkpoint must not fall back to old evidence"),
        )
        .unwrap_err();
        assert_eq!(failure.message(), "corrupt interrupted checkpoint");
    }
}
