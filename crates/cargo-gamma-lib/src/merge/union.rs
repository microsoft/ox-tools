// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The union that turns per-shard reports into one score.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use super::merged::Merged;
use super::status::{NEVER_RUN, scoring};
use super::verdict::Verdict;
use crate::elements::{FileResult, MergeProvenance, MutantResult, Report, RunInfo, SourceProvenance, VerdictProvenance};
use crate::model::Scoring;

/// Merges reports, keeping the most recent verdict per mutant ID.
///
/// `now` and `window` are passed in rather than read from the clock so that the freshness rule is
/// testable and so that re-merging the same inputs twice gives the same answer.
#[must_use]
pub fn merge(reports: &[(String, Report)], now: u64, window: Option<u64>) -> Merged {
    let mut latest: BTreeMap<&str, Verdict<'_>> = BTreeMap::new();
    let mut out = Merged::default();
    let current = populations(reports);
    let sources = sources(reports);

    // Withdrawal is a fact about a mutant, not about the inputs that mentioned it. Counting a
    // sighting per input made ten nightly reports of the same three withdrawn ids read as thirty,
    // and inflated the one figure whose whole job is to say whether the inputs span commits further
    // apart than the reader thinks — the figure that is also how passing the same file twice would
    // otherwise become visible.
    let mut withdrawn: BTreeSet<&str> = BTreeSet::new();

    out.unchecked = reports
        .iter()
        .flat_map(|(_, report)| report.files.keys())
        .filter(|path| !current.contains_key(path.as_str()))
        .collect::<BTreeSet<_>>()
        .len();

    out.shard_count = rotation(reports);

    for (name, report) in reports {
        let input_name = name.as_str();

        if let Some(shard) = report.config.as_ref().and_then(|config| config.shard) {
            let _ = out.shards_seen.insert(shard.index);

            if out.shard_count.is_some_and(|count| count != shard.count) {
                out.inconsistent.push(name.clone());
            }
        }

        for (path, file) in &report.files {
            let Some(source) = sources.get(path.as_str()) else {
                continue;
            };
            let (source_at, source_origin, source_lineage) = source_provenance(input_name, report, path, file);

            for mutant in &file.mutants {
                // A file whose current population is known admits exactly the ids in it. When no
                // input supplies a complete population, absence says nothing about whether the code
                // still exists, so every id remains admissible.
                if current.get(path.as_str()).is_some_and(|population| {
                    !population.ids.contains(mutant.id.as_str())
                        && rank(source_origin, source_at, &source_lineage)
                            <= rank(population.origin, population.started_at, &population.lineage)
                }) {
                    let _ = withdrawn.insert(mutant.id.as_str());
                    continue;
                }

                let (tested_at, origin, lineage) = verdict_provenance(input_name, report, mutant);
                let compatible = file.source == source.file.source && file.language == source.file.language;
                retain(
                    &mut latest,
                    Candidate {
                        mutant,
                        path,
                        tested_at,
                        origin,
                        lineage,
                        compatible,
                        source_rank: (source.started_at, source.origin, source.lineage.clone()),
                    },
                );
            }
        }
    }

    let mut files: BTreeMap<&str, Vec<&Verdict<'_>>> = BTreeMap::new();

    out.withdrawn = withdrawn.len();

    // The dissenters are named in whatever order the inputs arrived in, and a shell glob chooses
    // that; sorting is what stops the same rotation reading differently on two machines.
    out.inconsistent.sort();

    for verdict in latest.values() {
        if verdict.presentation.is_none() {
            out.incompatible += 1;
            continue;
        }

        if verdict.mutant.status == NEVER_RUN {
            out.never_tested += 1;
        } else if window.is_some_and(|window| now.saturating_sub(verdict.tested_at) > window) {
            out.stale += 1;
        } else {
            out.fresh += 1;
        }

        // A status the schema does not define is left out of the fraction rather than counted
        // against it. `read` refuses such a document, so this is reachable only from a `Report`
        // assembled in memory — and a merge that guessed at a word it cannot interpret would be
        // guessing with the score.
        match scoring(&verdict.mutant.status) {
            Some(Scoring::Detected) => {
                out.valid += 1;
                out.detected += 1;
            }
            Some(Scoring::Undetected) => out.valid += 1,
            Some(Scoring::Excluded) | None => {}
        }

        files.entry(verdict.file).or_default().push(verdict);
    }

    out.report = rebuild(reports, &sources, files);
    out
}

struct Candidate<'report> {
    mutant: &'report MutantResult,
    path: &'report str,
    tested_at: u64,
    origin: &'report str,
    lineage: String,
    compatible: bool,
    source_rank: (u64, &'report str, String),
}

/// Retains the newest verdict and the newest presentation that fits the selected source.
fn retain<'report>(latest: &mut BTreeMap<&'report str, Verdict<'report>>, candidate: Candidate<'report>) {
    let Candidate {
        mutant,
        path,
        tested_at,
        origin,
        lineage,
        compatible,
        source_rank,
    } = candidate;
    let entry = latest.entry(mutant.id.as_str());

    match entry {
        Entry::Vacant(slot) => {
            let _ = slot.insert(Verdict {
                mutant,
                file: path,
                tested_at,
                origin,
                lineage,
                presentation: compatible.then_some(mutant),
                presentation_rank: compatible.then_some(source_rank),
            });
        }

        // The winner is whichever verdict ranks higher, decided the same way everywhere else in
        // the merge: a real verdict always outranks `Pending`, then the newer run wins, then an
        // equal-timestamp tie falls to the report name. A newer listing cannot blank an older
        // earned verdict, but it can supply its presentation when it fits the selected source.
        Entry::Occupied(mut slot) => {
            let held = slot.get();
            let winner = verdict_rank(&mutant.status, origin, tested_at, &lineage)
                > verdict_rank(&held.mutant.status, held.origin, held.tested_at, &held.lineage);
            let newer_presentation = compatible
                && held
                    .presentation_rank
                    .as_ref()
                    .is_none_or(|(at, origin, lineage)| rank(source_rank.1, source_rank.0, &source_rank.2) > rank(origin, *at, lineage));

            if winner {
                let (file, presentation, presentation_rank) = if newer_presentation {
                    (path, Some(mutant), Some(source_rank))
                } else {
                    (held.file, held.presentation, held.presentation_rank.clone())
                };

                let _ = slot.insert(Verdict {
                    mutant,
                    file,
                    tested_at,
                    origin,
                    lineage,
                    presentation,
                    presentation_rank,
                });
            } else if newer_presentation {
                let held = slot.get_mut();
                held.file = path;
                held.presentation = Some(mutant);
                held.presentation_rank = Some(source_rank);
            }
        }
    }
}

/// The stable ranking that decides, without ever consulting input order, which of two reports a
/// choice prefers.
///
/// A report ranks first by when its run started and then, when that ties — the collision this whole
/// change exists to make deterministic, and the case of a report with no config, whose start is
/// taken as zero — by its name, then by its persisted lineage. The lineage distinguishes separate
/// reports that happened to use the same name and time before they were staged. Population, base
/// document, and source text all rank by this one key, so an equal-timestamp tie resolves to the
/// same report at every one of them and a merged file's mutants are never rendered over a different
/// report's source.
const fn rank<'name, 'lineage>(name: &'name str, started_at: u64, lineage: &'lineage str) -> (u64, &'name str, &'lineage str) {
    (started_at, name, lineage)
}

/// How strongly a candidate verdict should be preferred, with the greatest winning.
///
/// A real verdict outranks `Pending` however much older it is: the report carrying the `Pending` is
/// usually a listing of what still exists rather than a run, and letting it blank a verdict an
/// actual run earned would make the merged score a report about nothing. Only below that does
/// recency matter, and an equal-timestamp tie falls to [`rank`] so the winner is the same whichever
/// order the inputs arrived in.
fn verdict_rank<'name, 'lineage>(
    status: &str,
    name: &'name str,
    tested_at: u64,
    lineage: &'lineage str,
) -> (bool, (u64, &'name str, &'lineage str)) {
    (status != NEVER_RUN, rank(name, tested_at, lineage))
}

/// The complete set of mutant ids each file currently admits, where an input says so.
///
/// Only an unsharded report can answer this: it lists every mutant of every file it covers, so an
/// id it does not mention is an id the code no longer produces. A sharded report lists one slice of
/// the population, and reading its silence as a withdrawal would erase most of the rotation.
///
/// Two unsharded reports with the same timestamp describe the same commit, so one is as good as the
/// other; the tie is settled by [`rank`] rather than by which was named first, which is what keeps
/// the population — and so the whole score — independent of the order the inputs were listed.
struct Population<'report> {
    started_at: u64,
    origin: &'report str,
    lineage: String,
    ids: BTreeSet<&'report str>,
}

/// A file's source and language as one presentation unit.
#[derive(Clone)]
struct Source<'report> {
    file: &'report FileResult,
    started_at: u64,
    origin: &'report str,
    lineage: String,
}

fn populations(reports: &[(String, Report)]) -> BTreeMap<&str, Population<'_>> {
    let mut newest: BTreeMap<&str, Population<'_>> = BTreeMap::new();

    for (name, report) in reports {
        if report.config.as_ref().is_some_and(|config| config.shard.is_some() || config.merged) {
            continue;
        }

        let at = started_at(report);

        for (path, file) in &report.files {
            let ids: BTreeSet<&str> = file.mutants.iter().map(|mutant| mutant.id.as_str()).collect();

            match newest.entry(path.as_str()) {
                Entry::Vacant(slot) => {
                    let _ = slot.insert(Population {
                        started_at: at,
                        origin: name,
                        lineage: source_lineage(name, file),
                        ids,
                    });
                }

                Entry::Occupied(mut slot) => {
                    let held = slot.get();

                    let lineage = source_lineage(name, file);

                    if rank(name, at, &lineage) > rank(held.origin, held.started_at, &held.lineage) {
                        let _ = slot.insert(Population {
                            started_at: at,
                            origin: name,
                            lineage,
                            ids,
                        });
                    }
                }
            }
        }
    }

    newest
}

/// The shard count the merged rotation is measured against.
///
/// The largest any input claims, which is a choice the argument order cannot reach: first-wins made
/// `merge a.json b.json` and `merge b.json a.json` report different rotation coverage and different
/// missing shards, in a file built end to end around the opposite property — `populations`,
/// `rebuild` and `verdict_rank` all route through [`rank`] for exactly that reason.
///
/// The maximum rather than the newest input's count, because it is the only order-free rule that
/// also keeps coverage a fraction of at most one: every input's index is below its own count, so
/// taking the largest count leaves every index seen inside the rotation being counted up to. Taking
/// the newest input's count could put a shard from a wider rotation past the end of a narrower one,
/// which is how "3 of 2 shards seen, 150% of the rotation" gets printed. Inputs that disagree are
/// still named in [`Merged::inconsistent`], because a merge across two different partitions of the
/// population is a coverage figure nobody should read.
fn rotation(reports: &[(String, Report)]) -> Option<u32> {
    reports
        .iter()
        .filter_map(|(_, report)| {
            let config = report.config.as_ref()?;

            config.shard
        })
        .map(|shard| shard.count)
        .max()
}

/// When a report's run started, or zero when it does not say.
fn started_at(report: &Report) -> u64 {
    report.config.as_ref().map_or(0, |config| config.started_at)
}

/// Finds the provenance of the source a report renders for one file.
fn source_provenance<'report>(name: &'report str, report: &'report Report, path: &str, file: &FileResult) -> (u64, &'report str, String) {
    report
        .config
        .as_ref()
        .and_then(|config| config.merge_provenance.as_ref())
        .and_then(|provenance| provenance.sources.get(path))
        .map_or_else(
            || (started_at(report), name, source_lineage(name, file)),
            |provenance| {
                let lineage = if provenance.lineage.is_empty() {
                    source_lineage(provenance.origin.as_str(), file)
                } else {
                    provenance.lineage.clone()
                };

                (provenance.started_at, provenance.origin.as_str(), lineage)
            },
        )
}

/// Finds the run that established one verdict.
fn verdict_provenance<'report>(name: &'report str, report: &'report Report, mutant: &MutantResult) -> (u64, &'report str, String) {
    let provenance = report
        .config
        .as_ref()
        .and_then(|config| config.merge_provenance.as_ref())
        .and_then(|provenance| provenance.verdicts.get(mutant.id.as_str()));

    provenance.map_or_else(
        || (started_at(report), name, verdict_lineage(name, mutant)),
        |provenance| {
            let lineage = if provenance.lineage.is_empty() {
                verdict_lineage(provenance.origin.as_str(), mutant)
            } else {
                provenance.lineage.clone()
            };

            (provenance.started_at, provenance.origin.as_str(), lineage)
        },
    )
}

/// Gives an original source or verdict a stable identity that survives staged merges.
fn lineage(parts: &[&[u8]]) -> String {
    let mut hasher = blake3::Hasher::new();

    for part in parts {
        let _ = hasher.update(&(part.len() as u64).to_le_bytes());
        let _ = hasher.update(part);
    }

    hasher.finalize().to_hex().to_string()
}

fn source_lineage(name: &str, file: &FileResult) -> String {
    lineage(&[b"source", name.as_bytes(), file.language.as_bytes(), file.source.as_bytes()])
}

fn verdict_lineage(name: &str, mutant: &MutantResult) -> String {
    let encoded = serde_json::to_vec(mutant).unwrap_or_default();

    lineage(&[b"verdict", name.as_bytes(), &encoded])
}

/// Selects one source generation per file before choosing verdicts.
fn sources(reports: &[(String, Report)]) -> BTreeMap<&str, Source<'_>> {
    let mut newest = BTreeMap::new();

    for (name, report) in reports {
        for (path, file) in &report.files {
            let (started_at, origin, lineage) = source_provenance(name, report, path, file);
            let candidate = Source {
                file,
                started_at,
                origin,
                lineage,
            };

            match newest.entry(path.as_str()) {
                Entry::Vacant(slot) => {
                    let _ = slot.insert(candidate);
                }
                Entry::Occupied(mut slot)
                    if rank(origin, started_at, &candidate.lineage)
                        > rank(slot.get().origin, slot.get().started_at, &slot.get().lineage) =>
                {
                    let _ = slot.insert(candidate);
                }
                Entry::Occupied(_slot) => {}
            }
        }
    }

    newest
}

/// Rebuilds a report document from the merged verdicts.
///
/// Source text is taken from whichever input had it. A file's source can differ between reports from
/// different commits; the newest is not necessarily the one that matches every verdict, and there is
/// no honest way to reconcile that, so the most recent report that contains the file wins and the
/// freshness accounting is what tells the reader how much to trust it. When two reports are equally
/// recent the winner is settled by [`rank`], the same tie-break the population used, so the source
/// and the mutants rendered over it come from one coherent choice rather than from opposite ends of
/// the argument list.
fn rebuild(reports: &[(String, Report)], sources: &BTreeMap<&str, Source<'_>>, files: BTreeMap<&str, Vec<&Verdict<'_>>>) -> Option<Report> {
    let base = reports
        .iter()
        .max_by(|left, right| rank(&left.0, started_at(&left.1), "").cmp(&rank(&right.0, started_at(&right.1), "")))
        .map(|(_, report)| report)?;

    let mut provenance = MergeProvenance::default();
    let mut newest = 0;

    let merged_files = files
        .into_iter()
        .filter_map(|(path, mutants)| {
            let source = sources.get(path)?;
            let _ = provenance.sources.insert(
                path.to_owned(),
                SourceProvenance {
                    started_at: source.started_at,
                    origin: source.origin.to_owned(),
                    lineage: source.lineage.clone(),
                },
            );
            newest = newest.max(source.started_at);

            let mut mutants: Vec<MutantResult> = mutants
                .into_iter()
                .filter_map(|verdict| {
                    let presentation = verdict.presentation?;
                    newest = newest.max(verdict.tested_at);

                    // The winning verdict supplies the identity and outcome fields; the selected
                    // presentation supplies the rendering ones. Cloning `verdict.mutant` wholesale
                    // and then overwriting four of its fields from `presentation` cloned each of
                    // those fields twice — once into the throwaway copy, once for real — so the
                    // fields are picked from their real source directly instead.
                    let mutant = MutantResult {
                        id: verdict.mutant.id.clone(),
                        mutator_name: presentation.mutator_name.clone(),
                        location: presentation.location,
                        status: verdict.mutant.status.clone(),
                        replacement: presentation.replacement.clone(),
                        description: presentation.description.clone(),
                        status_reason: verdict.mutant.status_reason.clone(),
                        duration: verdict.mutant.duration,
                        killed_by: verdict.mutant.killed_by.clone(),
                    };

                    let _ = provenance.verdicts.insert(
                        mutant.id.to_string(),
                        VerdictProvenance {
                            started_at: verdict.tested_at,
                            origin: verdict.origin.to_owned(),
                            lineage: verdict.lineage.clone(),
                        },
                    );

                    Some(mutant)
                })
                .collect();

            mutants.sort_by(|left, right| {
                left.location
                    .start
                    .line
                    .cmp(&right.location.start.line)
                    .then(left.id.cmp(&right.id))
            });

            Some((
                path.to_owned(),
                FileResult {
                    source: source.file.source.clone(),
                    language: source.file.language.clone(),
                    mutants,
                },
            ))
        })
        .collect();

    let tests = reports
        .iter()
        .map(|(_, report)| {
            let config = report.config.as_ref()?;
            config.tests
        })
        .try_fold(usize::MAX, |lowest, tests| tests.map(|tests| lowest.min(tests)));
    let not_built = reports
        .iter()
        .filter_map(|(_, report)| {
            let config = report.config.as_ref()?;
            config.not_built
        })
        .fold(0, usize::saturating_add);
    let dropped_test_packages = reports
        .iter()
        .filter_map(|(_, report)| report.config.as_ref())
        .flat_map(|config| config.dropped_test_packages.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    Some(Report {
        schema_version: base.schema_version.clone(),
        thresholds: base.thresholds,
        project_root: base.project_root.clone(),
        framework: base.framework.clone(),
        files: merged_files,
        config: Some(RunInfo {
            started_at: newest,
            merged: true,
            shard: None,
            tests: tests.filter(|_| !reports.is_empty()),
            not_built: (not_built > 0).then_some(not_built),
            dropped_test_packages,
            merge_provenance: Some(provenance),
        }),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::elements::{Location, Position};
    use crate::fixtures;
    use crate::fixtures::{mutant_result_at as mutant, report_with as report};

    /// One day, in seconds.
    const DAY: u64 = 86_400;

    /// Like [`report`], but with a chosen source text, so a test can tell which input a merged
    /// file's source was drawn from.
    fn report_with_source(shard: Option<(u32, u32)>, started_at: u64, source: &str, mutants: Vec<MutantResult>) -> Report {
        let mut built = fixtures::report_with(shard, started_at, mutants);

        built
            .files
            .get_mut("src/lib.rs")
            .expect("the fixture always seeds this file")
            .source = source.to_owned();

        built
    }

    /// The pretty-printed merged document, which reversing the inputs must reproduce byte for byte.
    fn rendered(merged: &Merged) -> String {
        let document = merged.report.as_ref().expect("a merge of real reports produces a document");

        crate::elements::to_json(document).expect("the merged document serializes")
    }

    #[test]
    fn shards_union_into_one_population() {
        let merged = merge(
            &[
                ("a".to_owned(), report(Some((0, 2)), 100, vec![mutant("aaa", 1, "Killed")])),
                ("b".to_owned(), report(Some((1, 2)), 200, vec![mutant("bbb", 2, "Survived")])),
            ],
            300,
            None,
        );

        assert_eq!(merged.valid, 2);
        assert_eq!(merged.detected, 1);
        assert!((merged.score() - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn the_most_recent_verdict_wins() {
        // The whole point of merging a rotation: last night's answer for a mutant is superseded by
        // tonight's, not averaged with it.
        let merged = merge(
            &[
                ("old".to_owned(), report(Some((0, 2)), 100, vec![mutant("aaa", 1, "Survived")])),
                ("new".to_owned(), report(Some((0, 2)), 200, vec![mutant("aaa", 1, "Killed")])),
            ],
            300,
            None,
        );

        assert_eq!(merged.valid, 1);
        assert_eq!(merged.detected, 1);
    }

    #[test]
    fn an_older_report_never_overwrites_a_newer_one() {
        // Argument order is whatever the shell's glob produced, so it must not decide the answer.
        let merged = merge(
            &[
                ("new".to_owned(), report(Some((0, 2)), 200, vec![mutant("aaa", 1, "Killed")])),
                ("old".to_owned(), report(Some((0, 2)), 100, vec![mutant("aaa", 1, "Survived")])),
            ],
            300,
            None,
        );

        assert_eq!(merged.detected, 1);
    }

    #[test]
    fn the_source_text_comes_from_the_newest_report_not_the_last_argument() {
        // A glob orders by filename, which has nothing to do with when a run happened. Taking the
        // last one would render fresh verdicts over source from an older commit.
        let mut newer = report(Some((0, 2)), 200, vec![mutant("aaa", 1, "Killed")]);

        newer
            .files
            .get_mut("src/lib.rs")
            .expect("the fixture always seeds this file")
            .source = "fn f() { todo!() }\n".to_owned();

        let merged = merge(
            &[
                ("a-new".to_owned(), newer),
                ("z-old".to_owned(), report(Some((1, 2)), 100, vec![mutant("bbb", 2, "Survived")])),
            ],
            300,
            None,
        );

        let document = merged.report.expect("a merge of two reports produces a document");
        let file = document.files.get("src/lib.rs").expect("the merged file survives");

        assert_eq!(file.source, "fn f() { todo!() }\n");
    }

    #[test]
    fn an_informative_verdict_outranks_a_newer_pending_in_either_order() {
        let older = || {
            let mut old = mutant("aaa", 2, "Killed");
            old.mutator_name = "relational.lt_to_le".into();
            old.replacement = Some("<=".into());
            old.description = Some("changed the old comparison".to_owned());
            old.status_reason = Some("tests::old".to_owned());
            old.duration = Some(17.0);
            old.killed_by = Some(vec!["tests::old".to_owned()]);

            report_with_source(None, 100, "fn old() {}\nfn target() { old < value }\n", vec![old])
        };
        let newer = || {
            let mut current = mutant("aaa", 4, "Pending");
            current.mutator_name = "relational.eq_to_ne".into();
            current.replacement = Some("!=".into());
            current.description = Some("changed the current comparison".to_owned());

            report_with_source(
                None,
                200,
                "fn added() {}\nfn also_added() {}\nfn another() {}\nfn target() { current == value }\n",
                vec![current],
            )
        };

        let forward = merge(&[("older".to_owned(), older()), ("newer".to_owned(), newer())], 300, None);
        let reversed = merge(&[("newer".to_owned(), newer()), ("older".to_owned(), older())], 300, None);

        assert_eq!(forward.detected, 1, "the older Killed must outrank the newer Pending");
        assert_eq!(forward.never_tested, 0, "the earned verdict is not counted as never run");
        assert_eq!(
            rendered(&forward),
            rendered(&reversed),
            "reversing the inputs changed the merged document"
        );

        let document = forward.report.expect("a document");
        let file = document.files.get("src/lib.rs").expect("the merged file survives");

        assert_eq!(file.mutants.len(), 1);
        let mutant = &file.mutants[0];

        assert_eq!(mutant.status, "Killed", "the informative verdict is the one rendered");
        assert_eq!(mutant.status_reason.as_deref(), Some("tests::old"));
        assert_eq!(mutant.duration, Some(17.0));
        assert_eq!(mutant.killed_by.as_deref(), Some(&["tests::old".to_owned()][..]));

        assert_eq!(mutant.location.start.line, 4);
        assert_eq!(mutant.location.end.line, 4);
        assert_eq!(mutant.mutator_name, "relational.eq_to_ne");
        assert_eq!(mutant.replacement.as_deref(), Some("!="));
        assert_eq!(mutant.description.as_deref(), Some("changed the current comparison"));
        assert_eq!(
            file.source, "fn added() {}\nfn also_added() {}\nfn another() {}\nfn target() { current == value }\n",
            "the presentation span and source must come from the same newer listing"
        );
    }

    #[test]
    fn an_equal_timestamp_conflict_resolves_the_same_way_in_either_order() {
        // Two full runs collide on the same one-second timestamp and disagree: different source, a
        // shared mutant with opposite verdicts, and a mutant unique to each. The old fold settled
        // the population by first-wins and the source by last-wins, so reversing the inputs changed
        // the score and rendered one report's mutants over the other report's source. One stable
        // tie-break now decides population, verdict, base, and source together, so the winning
        // report supplies all of them and the two orders agree byte for byte.
        let left = || {
            report_with_source(
                None,
                100,
                "fn left() {}\n",
                vec![mutant("shared", 1, "Killed"), mutant("only_left", 2, "Survived")],
            )
        };
        let right = || {
            report_with_source(
                None,
                100,
                "fn right() {}\n",
                vec![mutant("shared", 1, "Survived"), mutant("only_right", 3, "Killed")],
            )
        };

        let forward = merge(&[("left.json".to_owned(), left()), ("right.json".to_owned(), right())], 300, None);
        let reversed = merge(&[("right.json".to_owned(), right()), ("left.json".to_owned(), left())], 300, None);

        assert_eq!(
            rendered(&forward),
            rendered(&reversed),
            "an equal-timestamp tie must not depend on input order"
        );
        assert_eq!(forward.withdrawn, 1, "the losing report's unique mutant left the denominator");

        let document = forward.report.expect("a document");
        let file = document.files.get("src/lib.rs").expect("the merged file survives");
        let ids: Vec<&str> = file.mutants.iter().map(|found| found.id.as_str()).collect();

        // `right.json` is the greater name at an equal timestamp, so it wins the tie everywhere:
        // its population, its source, and its verdict for the shared mutant. Nothing from
        // `left.json` is rendered over `right.json`'s source.
        assert_eq!(
            file.source, "fn right() {}\n",
            "the source is drawn from the same report the population is"
        );
        assert_eq!(ids, vec!["shared", "only_right"], "only the winning report's population survives");

        let shared = file
            .mutants
            .iter()
            .find(|found| found.id == "shared")
            .expect("the shared mutant survives");

        assert_eq!(
            shared.status, "Survived",
            "the winning report's verdict for the shared mutant is the one kept"
        );
    }

    #[test]
    fn a_verdict_older_than_the_window_is_stale_but_still_counted() {
        // Dropping it would silently shrink the denominator, which raises the score by forgetting.
        let merged = merge(
            &[("a".to_owned(), report(Some((0, 2)), 0, vec![mutant("aaa", 1, "Survived")]))],
            40 * DAY,
            Some(30 * DAY),
        );

        assert_eq!(merged.stale, 1);
        assert_eq!(merged.fresh, 0);
        assert_eq!(merged.valid, 1);
    }

    #[test]
    fn a_never_run_mutant_is_counted_separately_and_never_as_killed() {
        // Counting untested code as passing is how a mutation score becomes a decoration.
        let merged = merge(
            &[(
                "a".to_owned(),
                report(Some((0, 2)), 100, vec![mutant("aaa", 1, "Killed"), mutant("bbb", 2, "Pending")]),
            )],
            200,
            None,
        );

        assert_eq!(merged.never_tested, 1);
        assert_eq!(merged.valid, 1, "a never-run mutant must stay out of the denominator");
        assert!((merged.score() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn unviable_and_suppressed_mutants_stay_out_of_the_denominator() {
        let merged = merge(
            &[(
                "a".to_owned(),
                report(
                    Some((0, 2)),
                    100,
                    vec![
                        mutant("aaa", 1, "Killed"),
                        mutant("bbb", 2, "CompileError"),
                        mutant("ccc", 3, "Ignored"),
                    ],
                ),
            )],
            200,
            None,
        );

        assert_eq!(merged.valid, 1);
    }

    #[test]
    fn a_timeout_counts_as_undetected() {
        let merged = merge(
            &[("a".to_owned(), report(Some((0, 2)), 100, vec![mutant("aaa", 1, "Timeout")]))],
            200,
            None,
        );

        assert_eq!(merged.detected, 0);
        assert_eq!(merged.valid, 1);
    }

    #[test]
    fn a_disagreeing_shard_count_is_reported_rather_than_reconciled() {
        // Two runs at different counts partitioned the population differently, so "shards seen" no
        // longer means what it says, and silently picking one would make the coverage number a lie.
        let merged = merge(
            &[
                ("a".to_owned(), report(Some((0, 4)), 100, vec![mutant("aaa", 1, "Killed")])),
                ("b".to_owned(), report(Some((1, 8)), 200, vec![mutant("bbb", 2, "Killed")])),
            ],
            300,
            None,
        );

        // The rotation is measured against the widest count any input claimed, so every index seen
        // is inside it; the input that partitioned the population differently is named.
        assert_eq!(merged.shard_count, Some(8));
        assert_eq!(merged.inconsistent, vec!["a".to_owned()]);
    }

    /// The rotation a merge reports is a property of the inputs, not of the order they were named.
    ///
    /// Every other choice in this file routes through `rank` so that a shell glob listing the same
    /// reports differently cannot change the answer; the shard count was the one that consulted
    /// argument order, and it decides both figures a team uses to judge whether the merged score
    /// covers the codebase.
    #[test]
    fn the_rotation_is_the_same_whichever_order_the_inputs_arrive_in() {
        let inputs = || {
            [
                ("a".to_owned(), report(Some((0, 4)), 100, vec![mutant("aaa", 1, "Killed")])),
                ("b".to_owned(), report(Some((1, 8)), 200, vec![mutant("bbb", 2, "Killed")])),
            ]
        };

        let forward = merge(&inputs(), 300, None);
        let mut reversed_inputs = inputs();

        reversed_inputs.reverse();

        let reversed = merge(&reversed_inputs, 300, None);

        assert_eq!(forward.shard_count, reversed.shard_count);
        assert_eq!(forward.missing_shards(), reversed.missing_shards());
        assert_eq!(forward.inconsistent, reversed.inconsistent);
        assert!(
            (forward.coverage() - reversed.coverage()).abs() < f64::EPSILON,
            "{} against {}",
            forward.coverage(),
            reversed.coverage()
        );
    }

    /// Whatever the inputs claim, the rotation cannot be more than fully covered.
    ///
    /// Coverage is what a team reads to decide whether a merged score can be trusted, and a figure
    /// above 100% is a figure that has lost its relationship with the shards it counts.
    #[test]
    fn rotation_coverage_never_exceeds_the_whole_rotation() {
        let merged = merge(
            &[
                ("a".to_owned(), report(Some((0, 2)), 100, vec![mutant("aaa", 1, "Killed")])),
                ("b".to_owned(), report(Some((1, 2)), 200, vec![mutant("bbb", 2, "Killed")])),
                ("c".to_owned(), report(Some((7, 8)), 300, vec![mutant("ccc", 3, "Killed")])),
            ],
            400,
            None,
        );

        assert!(merged.coverage() <= 100.0, "{}% of the rotation", merged.coverage());
    }

    /// A withdrawal is one mutant, however many inputs still name it.
    ///
    /// Every other figure on `Merged` counts distinct ids, and this one counted sightings: a
    /// nightly rotation holding three withdrawn ids in ten reports read as thirty withdrawals,
    /// which defeats the only judgement the figure exists to support — whether the inputs span
    /// commits further apart than the reader thinks.
    #[test]
    fn a_withdrawn_mutant_is_counted_once_however_many_inputs_named_it() {
        let stale = |name: &str, at: u64| {
            (
                name.to_owned(),
                report(
                    None,
                    at,
                    vec![
                        mutant("gone-1", 1, "Survived"),
                        mutant("gone-2", 2, "Survived"),
                        mutant("here", 3, "Killed"),
                    ],
                ),
            )
        };

        let merged = merge(
            &[
                stale("a", 100),
                stale("b", 200),
                stale("c", 300),
                ("current".to_owned(), report(None, 400, vec![mutant("here", 3, "Killed")])),
            ],
            500,
            None,
        );

        assert_eq!(merged.withdrawn, 2, "two ids were withdrawn, not six sightings of them");
        assert_eq!(merged.valid, 1);
    }

    /// A status the schema does not define never reaches the denominator.
    ///
    /// `merge` is the one command that reads documents it did not write, so a corrupt or misspelled
    /// status is a real input. Counting it as an undetected mutant charged the score for a word
    /// nothing can interpret; `RuntimeError` did the same while being a status the schema defines
    /// and explicitly keeps out of the denominator, so a single one made the printed score and the
    /// score the viewer computes from the merged document disagree.
    #[test]
    fn a_status_the_score_cannot_interpret_stays_out_of_the_fraction() {
        for status in ["RuntimeError", "Kiled"] {
            let merged = merge(
                &[(
                    "a".to_owned(),
                    report(Some((0, 2)), 100, vec![mutant("aaa", 1, "Killed"), mutant("bbb", 2, status)]),
                )],
                200,
                None,
            );

            assert_eq!(merged.valid, 1, "`{status}` must not be in the denominator");
            assert_eq!(merged.detected, 1);
            assert!(
                (merged.score() - 100.0).abs() < f64::EPSILON,
                "`{status}` scored {}%",
                merged.score()
            );
        }
    }

    #[test]
    fn the_merged_document_holds_every_mutant_in_line_order() {
        let merged = merge(
            &[
                ("a".to_owned(), report(Some((0, 2)), 100, vec![mutant("bbb", 9, "Killed")])),
                ("b".to_owned(), report(Some((1, 2)), 200, vec![mutant("aaa", 2, "Survived")])),
            ],
            300,
            None,
        );

        let report = merged.report.expect("a merge of two reports produces one");
        let mutants = &report.files["src/lib.rs"].mutants;

        assert_eq!(mutants.len(), 2);
        assert_eq!(mutants[0].location.start.line, 2);
        assert_eq!(mutants[1].location.start.line, 9);
    }

    #[test]
    fn merging_nothing_prints_a_perfect_score_but_has_nothing_to_grade() {
        let merged = merge(&[], 0, None);

        assert!(merged.report.is_none());
        assert_eq!(merged.valid, 0);

        // The printable score agrees with the `run` side that nothing scores 100%, but the gradeable
        // score is `None`, so no `--min-score` threshold can be passed by a merge that scored nothing.
        assert!((merged.score() - 100.0).abs() < f64::EPSILON);
        assert_eq!(merged.scored(), None);
    }

    #[test]
    fn a_merged_document_round_trips_through_json() {
        // `merge` reads what `run` wrote, so the two halves have to agree about the format. This is
        // the only test that exercises both directions.
        let merged = merge(
            &[("a".to_owned(), report(Some((0, 2)), 100, vec![mutant("aaa", 1, "Killed")]))],
            200,
            None,
        );

        let text = crate::elements::to_json(&merged.report.expect("a report")).expect("serializes");
        let parsed: Report = serde_json::from_str(&text).expect("the document must read back");

        assert_eq!(parsed.files["src/lib.rs"].mutants[0].id, "aaa");
        let config = parsed.config.expect("merge info survives");
        assert_eq!(config.started_at, 100);
        assert!(config.merged);
        assert!(config.shard.is_none());
        assert_eq!(
            config
                .merge_provenance
                .as_ref()
                .and_then(|provenance| provenance.verdicts.get("aaa"))
                .map(|provenance| provenance.started_at),
            Some(100)
        );
    }

    #[test]
    fn repeated_merges_keep_each_verdicts_original_freshness() {
        let first = || report(Some((0, 3)), 100, vec![mutant("old", 1, "Killed")]);
        let newest = || report(Some((1, 3)), 300, vec![mutant("new", 2, "Killed")]);
        let middle = || report(Some((2, 3)), 200, vec![mutant("old", 1, "Survived")]);

        let direct = merge(
            &[
                ("first".to_owned(), first()),
                ("newest".to_owned(), newest()),
                ("middle".to_owned(), middle()),
            ],
            400,
            None,
        );
        let staged_input = merge(&[("first".to_owned(), first()), ("newest".to_owned(), newest())], 400, None)
            .report
            .expect("the first stage writes a report");
        let staged = merge(&[("stage".to_owned(), staged_input), ("middle".to_owned(), middle())], 400, None);

        assert_eq!(rendered(&staged), rendered(&direct), "staging changed the selected verdict");
        assert_eq!(
            staged.report.as_ref().expect("a report").files["src/lib.rs"]
                .mutants
                .iter()
                .find(|mutant| mutant.id == "old")
                .map(|mutant| mutant.status.as_str()),
            Some("Survived")
        );
    }

    #[test]
    fn run_metadata_is_aggregated_and_survives_staging() {
        let configured = |id: &str, tests: Option<usize>, not_built: Option<usize>, dropped: &[&str]| {
            let mut report = report(Some((0, 2)), 100, vec![mutant(id, 1, "Killed")]);
            let config = report.config.as_mut().expect("run info");

            config.tests = tests;
            config.not_built = not_built;
            config.dropped_test_packages = dropped.iter().map(|package| (*package).to_owned()).collect();
            report
        };
        let merged = merge(
            &[
                ("a".to_owned(), configured("a", Some(12), Some(2), &["slow", "broken"])),
                ("b".to_owned(), configured("b", Some(8), Some(3), &["broken", "extra"])),
            ],
            200,
            None,
        )
        .report
        .expect("report");
        let config = merged.config.as_ref().expect("config");

        assert_eq!(config.tests, Some(8));
        assert_eq!(config.not_built, Some(5));
        assert_eq!(config.dropped_test_packages, ["broken", "extra", "slow"]);

        let staged = merge(
            &[
                ("stage".to_owned(), merged),
                ("unknown".to_owned(), configured("c", None, None, &["later"])),
            ],
            300,
            None,
        )
        .report
        .expect("report");
        let config = staged.config.expect("config");

        assert_eq!(config.tests, None);
        assert_eq!(config.not_built, Some(5));
        assert_eq!(config.dropped_test_packages, ["broken", "extra", "later", "slow"]);
    }

    #[test]
    fn staged_reports_with_colliding_inner_provenance_are_order_independent() {
        let left = || report_with_source(Some((0, 2)), 100, "fn left() {}\n", vec![mutant("shared", 1, "Killed")]);
        let right = || report_with_source(Some((1, 2)), 100, "fn right() {}\n", vec![mutant("shared", 1, "Survived")]);

        let expected_source = {
            let left = left();
            let right = right();
            let left_file = &left.files["src/lib.rs"];
            let right_file = &right.files["src/lib.rs"];

            if source_lineage("inner.json", left_file) > source_lineage("inner.json", right_file) {
                left_file.source.clone()
            } else {
                right_file.source.clone()
            }
        };
        let expected_status = {
            let left = left();
            let right = right();
            let left_mutant = &left.files["src/lib.rs"].mutants[0];
            let right_mutant = &right.files["src/lib.rs"].mutants[0];

            if verdict_lineage("inner.json", left_mutant) > verdict_lineage("inner.json", right_mutant) {
                left_mutant.status.clone()
            } else {
                right_mutant.status.clone()
            }
        };
        let stage = |report| {
            merge(&[("inner.json".to_owned(), report)], 200, None)
                .report
                .expect("the stage writes a report")
        };

        let forward = merge(
            &[
                ("left-stage.json".to_owned(), stage(left())),
                ("right-stage.json".to_owned(), stage(right())),
            ],
            300,
            None,
        );
        let reversed = merge(
            &[
                ("right-stage.json".to_owned(), stage(right())),
                ("left-stage.json".to_owned(), stage(left())),
            ],
            300,
            None,
        );

        assert_eq!(rendered(&forward), rendered(&reversed));

        let file = &forward.report.expect("a report").files["src/lib.rs"];

        assert_eq!(file.source, expected_source, "the source tie uses its retained lineage");
        assert_eq!(file.mutants[0].status, expected_status, "the verdict tie uses its retained lineage");
    }

    #[test]
    fn changed_source_excludes_a_verdict_without_a_compatible_presentation() {
        let old = report_with_source(None, 100, "fn old() {}\n", vec![mutant("old", 1, "Killed")]);
        let newer = report_with_source(Some((0, 2)), 200, "fn newer() {}\n", vec![mutant("new", 1, "Killed")]);

        let merged = merge(&[("old".to_owned(), old), ("newer".to_owned(), newer)], 300, None);
        let report = merged.report.expect("a report");
        let file = report.files.get("src/lib.rs").expect("the newer file survives");

        assert_eq!(file.source, "fn newer() {}\n");
        assert_eq!(file.mutants.len(), 1, "the old overlay cannot be rendered over changed source");
        assert_eq!(file.mutants[0].id, "new");
        assert_eq!(merged.valid, 1, "the summary must match the rendered population");
        assert_eq!(merged.incompatible, 1);
    }

    #[test]
    fn a_merged_file_keeps_its_selected_source_language() {
        let mut javascript = report_with_source(None, 100, "export const value = 1;\n", vec![mutant("js", 1, "Killed")]);

        javascript
            .files
            .get_mut("src/lib.rs")
            .expect("the fixture always seeds this file")
            .language = "javascript".to_owned();

        let merged = merge(&[("javascript".to_owned(), javascript)], 200, None);

        assert_eq!(
            merged.report.expect("a report").files["src/lib.rs"].language,
            "javascript",
            "the merger must not relabel non-Rust source"
        );
    }

    #[test]
    fn a_merged_document_is_not_a_population_authority_when_merged_again() {
        let document = merge(
            &[
                ("shard-a".to_owned(), report(Some((0, 2)), 100, vec![mutant("aaa", 1, "Killed")])),
                ("shard-b".to_owned(), report(Some((1, 2)), 200, vec![mutant("bbb", 2, "Survived")])),
            ],
            300,
            None,
        )
        .report
        .expect("a report");
        let current = report(None, 0, vec![mutant("aaa", 1, "Pending")]);

        let remerged = merge(&[("z-merged".to_owned(), document), ("a-current".to_owned(), current)], 300, None);

        assert_eq!(remerged.withdrawn, 0, "an older full report cannot withdraw a carried newer shard");
        let report = remerged.report.expect("a report");
        let ids: Vec<&str> = report.files["src/lib.rs"].mutants.iter().map(|mutant| mutant.id.as_str()).collect();

        assert_eq!(ids, vec!["aaa", "bbb"]);
    }

    #[test]
    fn a_verdict_for_edited_code_leaves_the_denominator() {
        // The old survivor's code was edited, so the newer full run does not produce its id at all.
        // Keeping it would go on depressing the score for a construct that no longer exists, which
        // is exactly what the README promises does not happen.
        let merged = merge(
            &[
                (
                    "old".to_owned(),
                    report(None, 100, vec![mutant("aaa", 1, "Survived"), mutant("keep", 2, "Killed")]),
                ),
                (
                    "new".to_owned(),
                    report(None, 200, vec![mutant("bbb", 1, "Pending"), mutant("keep", 2, "Pending")]),
                ),
            ],
            300,
            None,
        );

        assert_eq!(merged.withdrawn, 1, "the edited mutant was not withdrawn");
        assert_eq!(merged.valid, 1, "only the surviving construct counts");
        assert_eq!(merged.detected, 1);
        assert_eq!(merged.never_tested, 1, "the replacement construct has never been run");
        assert!((merged.score() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_withdrawn_mutant_leaves_the_rendered_document_too() {
        // A report that still lists the old mutant would render it over source it does not appear
        // in, which is worse than not mentioning it.
        let merged = merge(
            &[
                ("old".to_owned(), report(None, 100, vec![mutant("aaa", 1, "Survived")])),
                ("new".to_owned(), report(None, 200, vec![mutant("bbb", 1, "Pending")])),
            ],
            300,
            None,
        );

        let ids: Vec<&str> = merged.report.as_ref().expect("a report").files["src/lib.rs"]
            .mutants
            .iter()
            .map(|found| found.id.as_str())
            .collect();

        assert_eq!(ids, vec!["bbb"]);
    }

    #[test]
    fn a_shard_never_withdraws_anything() {
        // A shard lists its own slice of the population, so an id it does not mention may simply
        // belong to another shard. Reading that silence as a withdrawal would erase the rotation.
        let merged = merge(
            &[
                ("one".to_owned(), report(Some((0, 2)), 100, vec![mutant("aaa", 1, "Killed")])),
                ("two".to_owned(), report(Some((1, 2)), 200, vec![mutant("bbb", 2, "Killed")])),
            ],
            300,
            None,
        );

        assert_eq!(merged.withdrawn, 0);
        assert_eq!(merged.valid, 2);
    }

    /// Withdrawal is undecidable from shards alone, so the count says so rather than letting zero
    /// withdrawals read as evidence that nothing had changed.
    #[test]
    fn a_shard_only_merge_says_its_withdrawals_went_unchecked() {
        let merged = merge(
            &[
                ("one".to_owned(), report(Some((0, 2)), 100, vec![mutant("aaa", 1, "Killed")])),
                ("two".to_owned(), report(Some((1, 2)), 200, vec![mutant("bbb", 2, "Killed")])),
            ],
            300,
            None,
        );

        assert_eq!(merged.unchecked, 1, "both shards cover the one file, so one file went unchecked");
    }

    #[test]
    fn an_unsharded_merge_checks_every_file_it_covers() {
        let merged = merge(
            &[("one".to_owned(), report(None, 100, vec![mutant("aaa", 1, "Killed")]))],
            300,
            None,
        );

        assert_eq!(merged.unchecked, 0);
    }

    #[test]
    fn a_full_run_withdraws_from_a_shard_that_predates_it() {
        // This is the rotation's real shape: nightly shards, and one full population to say what
        // still exists.
        let merged = merge(
            &[
                ("night".to_owned(), report(Some((0, 4)), 100, vec![mutant("gone", 1, "Survived")])),
                ("today".to_owned(), report(None, 200, vec![mutant("here", 1, "Pending")])),
            ],
            300,
            None,
        );

        assert_eq!(merged.withdrawn, 1);
        assert_eq!(merged.valid, 0, "the only construct left has never been tested");
    }

    #[test]
    fn a_newer_shard_is_not_withdrawn_by_an_older_full_run() {
        // The full population is a statement about its own commit. A shard run afterwards may have
        // found a construct that did not exist then, and dropping it would lose a real verdict.
        let full = || report(None, 100, vec![mutant("aaa", 1, "Pending")]);
        let later = || report(Some((0, 4)), 200, vec![mutant("bbb", 2, "Killed")]);
        let forward = merge(&[("full".to_owned(), full()), ("later".to_owned(), later())], 300, None);
        let reversed = merge(&[("later".to_owned(), later()), ("full".to_owned(), full())], 300, None);

        assert_eq!(rendered(&forward), rendered(&reversed), "input order changed the merged report");

        for merged in [&forward, &reversed] {
            assert_eq!(
                merged.withdrawn, 0,
                "a newer shard is outside the older full population's authority"
            );
            assert!(
                merged.report.as_ref().expect("a merged report").files["src/lib.rs"]
                    .mutants
                    .iter()
                    .any(|mutant| mutant.id == "bbb" && mutant.status == "Killed"),
                "the newer shard verdict must survive"
            );
        }
    }

    #[test]
    fn the_newest_full_population_is_the_one_that_decides() {
        // Two full runs disagree about what exists; the later one is the current tree.
        let merged = merge(
            &[
                ("older".to_owned(), report(None, 100, vec![mutant("aaa", 1, "Killed")])),
                (
                    "newest".to_owned(),
                    report(None, 300, vec![mutant("aaa", 1, "Pending"), mutant("bbb", 2, "Pending")]),
                ),
                ("middle".to_owned(), report(None, 200, vec![mutant("ccc", 3, "Survived")])),
            ],
            400,
            None,
        );

        assert_eq!(merged.withdrawn, 1, "only `ccc` is gone");
        assert_eq!(merged.detected, 1, "`aaa` keeps the verdict it earned");
    }

    #[test]
    fn a_listing_does_not_erase_the_verdicts_it_is_merged_with() {
        // The current population usually comes from a listing, which reports every mutant as never
        // run. Newest-wins on its own would let that blank every verdict in the merge and report a
        // score of zero over a suite that had actually killed everything.
        let merged = merge(
            &[
                ("run".to_owned(), report(None, 100, vec![mutant("aaa", 1, "Killed")])),
                ("listing".to_owned(), report(None, 200, vec![mutant("aaa", 1, "Pending")])),
            ],
            300,
            None,
        );

        assert_eq!(merged.detected, 1, "the verdict survived the listing");
        assert_eq!(merged.never_tested, 0);
        assert!((merged.score() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_real_verdict_still_replaces_an_older_one() {
        // The rule above must not go so far that a genuine re-run cannot change a verdict.
        let merged = merge(
            &[
                ("old".to_owned(), report(None, 100, vec![mutant("aaa", 1, "Killed")])),
                ("new".to_owned(), report(None, 200, vec![mutant("aaa", 1, "Survived")])),
            ],
            300,
            None,
        );

        assert_eq!(merged.detected, 0, "the newer run said it survived");
        assert_eq!(merged.valid, 1);
    }

    #[test]
    fn a_file_no_full_run_covers_is_left_alone() {
        // Nothing here states a population for the file, so nothing may claim an id is withdrawn.
        let merged = merge(
            &[
                ("one".to_owned(), report(Some((0, 2)), 100, vec![mutant("aaa", 1, "Killed")])),
                ("two".to_owned(), report(Some((1, 2)), 200, vec![mutant("bbb", 2, "Survived")])),
            ],
            300,
            None,
        );

        assert_eq!(merged.withdrawn, 0);
        assert_eq!(merged.valid, 2);
    }

    /// The rebuild avoids cloning the base report's files into the merged output.
    #[test]
    fn rebuild_produces_same_document_as_before_clone_elimination() {
        let inputs = vec![
            ("a".to_owned(), report(Some((0, 2)), 100, vec![mutant("aaa", 1, "Killed")])),
            ("b".to_owned(), report(Some((1, 2)), 200, vec![mutant("bbb", 2, "Survived")])),
        ];

        let merged = merge(&inputs, 300, None);
        let document = merged.report.expect("produces a document");
        let text = crate::elements::to_json(&document).expect("serializes");
        let parsed: Report = serde_json::from_str(&text).expect("round-trips");

        assert_eq!(parsed.schema_version, "2");
        assert!(parsed.config.as_ref().unwrap().merged);
        assert_eq!(parsed.files["src/lib.rs"].mutants.len(), 2);
    }

    /// A winning verdict from one report can be rendered with a different report's presentation:
    /// the merged mutant must draw its outcome fields from the verdict that won and its rendering
    /// fields from the presentation that was selected, never a leftover from whichever one lost.
    ///
    /// The winning ("edited") report is newest but reports a different file source, so it cannot
    /// supply a trustworthy presentation for the selected source; the older, source-compatible
    /// ("baseline") report supplies the rendering instead, the same rule
    /// `changed_source_excludes_a_verdict_without_a_compatible_presentation` exercises for whether a
    /// verdict counts at all, applied here to which fields a *counted* verdict is rendered with.
    #[test]
    fn rebuild_mixes_the_winning_verdict_with_the_selected_presentation_field_by_field() {
        let baseline_mutant = MutantResult {
            id: "aaa".into(),
            mutator_name: "baseline.mutator".into(),
            location: Location {
                start: Position { line: 1, column: 1 },
                end: Position { line: 1, column: 9 },
            },
            status: "Pending".into(),
            replacement: Some("baseline-replacement".into()),
            description: Some("baseline description".to_owned()),
            status_reason: None,
            duration: None,
            killed_by: None,
        };
        let edited_mutant = MutantResult {
            id: "aaa".into(),
            mutator_name: "edited.mutator".into(),
            location: Location {
                start: Position { line: 5, column: 2 },
                end: Position { line: 5, column: 10 },
            },
            status: "Killed".into(),
            replacement: Some("edited-replacement".into()),
            description: Some("edited description".to_owned()),
            status_reason: Some("failed `edited_test`".to_owned()),
            duration: Some(1.0),
            killed_by: Some(vec!["edited_test".to_owned()]),
        };

        // "baseline" is the newest report, so its source is the one the merged document renders;
        // "edited" is older but carries the only real verdict, which a real verdict outranks
        // `Pending` regardless of timestamp.
        let baseline = report_with_source(None, 200, "fn f() {}\n", vec![baseline_mutant]);
        let edited = report_with_source(None, 100, "fn edited() {}\n", vec![edited_mutant]);

        let merged = merge(&[("edited".to_owned(), edited), ("baseline".to_owned(), baseline)], 300, None);

        let document = merged.report.expect("a report");
        let mutant = &document.files["src/lib.rs"].mutants[0];

        // Outcome fields: from the winning ("edited") verdict.
        assert_eq!(mutant.status, "Killed");
        assert_eq!(mutant.duration, Some(1.0));
        assert_eq!(mutant.killed_by.as_deref(), Some(&["edited_test".to_owned()][..]));
        assert_eq!(mutant.status_reason.as_deref(), Some("failed `edited_test`"));

        // Rendering fields: from the selected ("baseline") presentation, since the winning
        // verdict's own source does not match what the merged file renders.
        assert_eq!(mutant.mutator_name, "baseline.mutator");
        assert_eq!(mutant.location.start.line, 1);
        assert_eq!(mutant.replacement.as_deref(), Some("baseline-replacement"));
        assert_eq!(mutant.description.as_deref(), Some("baseline description"));
    }
}
