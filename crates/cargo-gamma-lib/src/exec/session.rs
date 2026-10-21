// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::time::Duration;
use std::collections::BTreeMap;
use std::sync::Arc;

use super::build::Round;
use super::test_binary::TestBinary;

/// What happened during a run, beyond the verdicts written back onto the mutants.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Session {
    /// The sum of the test binaries' baseline durations, used to calibrate mutant budgets.
    pub baseline: Duration,

    /// How long the concurrently measured baseline took on the wall clock.
    pub baseline_wall: Duration,

    /// How many tests the baseline actually ran, or `None` if no harness announced a count.
    ///
    /// This is what ran rather than what exists: `--test-package`, `--include-test` and any filter
    /// passed through to the harness all narrow it. Carried on the session rather than only printed
    /// as the baseline finishes, because progress output resolves to whether a terminal is
    /// attached — so in CI, where "did my suite run at all" is the single most useful thing this
    /// figure answers, the transient line saying it is exactly the one nobody sees.
    pub tests: Option<usize>,

    /// The longest the baseline legitimately went without saying anything.
    pub quiet: Duration,

    /// The silence a mutant was allowed before it was presumed hung, when that was enabled.
    pub stall: Option<Duration>,

    /// How long the single build took.
    pub build: Duration,

    /// The largest peak memory any one test binary reached during the baseline.
    ///
    /// `None` when nothing measured it, which is both the default and what a host without an
    /// aggregate process-tree accounting facility can offer. Reported because it is the figure a
    /// memory ceiling is chosen from, and because a suite whose peak surprises its authors is worth
    /// knowing about whether or not a ceiling is being enforced.
    pub peak: Option<u64>,

    /// Whether this run actually metered memory, which is not always what was configured.
    ///
    /// Memory control is on by default, and a host without cgroup v2 delegation cannot provide it.
    /// A run that defaulted into it and could not have it degrades rather than stopping, so the
    /// configured policy is a request and this is the answer. Everything downstream reads this one,
    /// because asking the platform for accounting it already declined to give would fail every
    /// mutant in the sweep.
    pub metered: bool,

    /// Why memory went unbounded, when it was meant to be bounded and could not be.
    ///
    /// Carried to the end of the run rather than printed when it is discovered, because progress
    /// output is suppressed when nothing is watching it — and a CI runner with no cgroup delegation
    /// is exactly the case where the protection is missing *and* nobody sees the transient line
    /// saying so.
    pub unbounded: Option<String>,

    /// How many mutants were withdrawn because they could not compile.
    pub withdrawn: usize,

    /// Why they were withdrawn, grouped by rustc error code and mutator, densest pair first.
    ///
    /// The count alone says whether unviability is expensive; only this says whether it is
    /// something a mutator could be taught to avoid, or an unavoidable cost of instrumenting the
    /// tree at all. Carried on the session so that asking the question needs no code change.
    pub census: Vec<crate::exec::Withdrawal>,

    /// How many rollback rounds were needed.
    pub rounds: u32,

    /// What each of those rounds cost and withdrew, oldest first.
    ///
    /// Carried alongside `build` because the total on its own cannot tell a build that compiled
    /// first time from one that spent most of its time converging. The two want very different
    /// remedies — a faster machine against fewer unviable mutants — and a run that does not say
    /// which it was leaves that choice to guesswork.
    pub rounds_taken: Vec<Round>,

    /// The test binaries that were run.
    pub binaries: Vec<TestBinary>,

    /// Where the run put its Cargo artifacts and reusable campaign state.
    ///
    /// The path rather than the size, because the size is a walk of a directory holding every build
    /// artifact of every round, and only the diagnostics dump ever prints it. See
    /// [`footprint`](crate::exec::footprint), which turns this into that figure on demand.
    ///
    /// Worth reporting at all because the disk is a real operating cost rather than a curiosity: a
    /// large workspace can leave tens of gigabytes here, which is more than the free space on a
    /// common CI runner, and a job whose next step fails for want of disk deserves to know where the
    /// disk went.
    pub scratch: camino::Utf8PathBuf,

    /// How many test targets `--include-test` or `--exclude-test` kept out of the oracle.
    ///
    /// Zero unless one of those was given. Reported because a narrowed oracle is the single most
    /// consequential thing that can happen to a score without appearing anywhere in it: a survivor
    /// here may be a mutant the excluded target would have caught, and a reader who did not write
    /// the `gamma.toml` has no other way to know the suite was not asked in full.
    pub filtered: usize,

    /// Whether the run had to build test targets it knew it would never consult.
    ///
    /// Building only the packages whose tests can reach a mutant is the cheaper thing to do, but
    /// cargo unifies features over the packages it is asked to build, so a test target that only
    /// compiles because some other package switches a feature on will not compile on its own. When
    /// that happens the selection is abandoned and the whole workspace is built, and the run says
    /// so: the scope the user asked for did not survive contact with their feature graph.
    pub widened: bool,

    /// What the stale build-ordering hints put in front of the compiler, and what came of it.
    ///
    /// A record whose build context no longer matches still knows which mutants failed to compile
    /// for it, and that knowledge is allowed to decide what the compiler sees first — never what
    /// gets built, judged or scored. These are the two facts that say whether it is paying: how
    /// many mutants were front-loaded, and how many of those the compiler then refused. See
    /// [`crate::exec::OrderingHints`] for why there is no "rounds saved" figure here.
    pub ordering: crate::exec::OrderingHints,

    /// What each phase of the run cost, gathered a clock at a time as the phases finish.
    ///
    /// The aggregates above cannot be taken apart after the fact — [`Self::build`] folds the copy,
    /// the preflight and the compile into one number. Only a clock started at
    /// each phase can say where the time went, so each is timed once, as it runs, and left here for
    /// the diagnostic reporter to surface. Nothing else reads it, and the run behaves identically whether or
    /// not anyone ever does.
    pub phases: Phases,
}

/// What each phase of a run cost, so a fixed or testing total can be read apart into its parts.
///
/// Copy and preflight timings are components of [`Session::build`]. Compilation and bookkeeping
/// account for the remaining preparation time; the sweep records mutant execution separately.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Phases {
    /// Frozen pre-sweep exact hints for the entire testable population, not assignment-time hints.
    ///
    /// Absent in older campaigns; their original coverage cannot be reconstructed on resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_hints: Option<InitialHints>,
    /// What duplicating the workspace into the scratch tree cost, before a line of it was
    /// instrumented. Part of [`Session::build`].
    pub copy: Duration,

    /// What the preflight cost: proving the tree compiles at all before any mutant was staged. Part
    /// of [`Session::build`].
    pub preflight: Duration,

    /// What the sweep cost and how it spent its launches, or `None` when nothing was swept.
    pub sweep: Option<SweepCost>,
}

/// Sweep wall time, legacy run counters and file-only attempt accounting.
///
/// The launch count is what turns the cost model's `build + Σ(launch + prefix)` from a formula into
/// a measurement; the probe count is what says whether the killer hints are earning
/// their keep, since a probe is a launch the run only made because a hint pointed at it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SweepCost {
    /// Local-only confirmation identities: complete in summaries, incremental in journal entries.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub confirmed_initial_killers: std::collections::BTreeSet<crate::model::MutantId>,
    /// Attempt accounting added after legacy launch counters; absent in older records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accounting: Option<SweepAccounting>,
    /// How long the sweep took, across every mutant and every binary it ran.
    pub elapsed: Duration,

    /// Legacy requested binary runs, excluding additional confirmation attempts.
    pub launches: usize,

    /// How many of those launches were hint-directed probes rather than ordinary binary runs.
    pub probes: usize,

    /// Exact mutant-id probes attempted and confirmed.
    pub exact_probes: usize,
    pub exact_hits: usize,

    /// Inferred item, binary and reach-cluster test selections attempted and confirmed.
    #[serde(rename = "generalized_probes")]
    pub inferred_probes: usize,
    #[serde(rename = "generalized_hits")]
    pub inferred_hits: usize,
    #[serde(rename = "generalized_rejected")]
    pub inferred_rejected: usize,

    /// Hint candidates available when mutants were assigned.
    pub exact_candidates: usize,
    #[serde(rename = "generalized_candidates")]
    pub inferred_candidates: usize,
    pub item_candidates: usize,
    pub reach_candidates: usize,
    pub file_candidates: usize,
    pub item_probes: usize,
    pub item_hits: usize,
    pub reach_probes: usize,
    pub reach_hits: usize,
    pub file_probes: usize,
    pub file_hits: usize,
    pub item_rejected: usize,
    pub reach_rejected: usize,
    pub file_rejected: usize,

    /// Final reachability decisions made before canonical binary execution.
    pub whole_selections: usize,
    pub uncovered_selections: usize,

    /// Package wall-clock spans measured from the beginning of the sweep.
    pub packages: Vec<PackageSweepCost>,

    /// Canonical binary launches avoided by in-run reach and killer learning.
    pub launches_saved: usize,

    /// Launches avoided specifically by reach observations harvested during this sweep.
    pub reach_launches_saved: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PackageSweepCost {
    pub package: Arc<str>,
    pub mutants: usize,
    pub first_started_ms: u64,
    pub settled_ms: u64,
}

/// Exact identities captured before any mutant was assigned, retained only in local campaign state.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct InitialHints {
    pub population: usize,
    pub killers: crate::HashMap<crate::model::MutantId, crate::discover::Killer>,
}

impl InitialHints {
    pub(super) fn capture(plan: &crate::discover::Plan, killers: &super::killers::Killers) -> Self {
        let mutants = plan
            .mutants
            .iter()
            .filter(|mutant| mutant.ordinal != 0 && mutant.outcome == crate::model::Outcome::Pending);
        let mut captured = Self::default();
        for mutant in mutants {
            captured.population += 1;
            if let Some(killer) = killers.hint(&mutant.id) {
                captured.killers.insert(mutant.id.clone(), killer.clone());
            }
        }
        captured
    }
}

/// Summed binary-attempt costs, including concurrent workers, never campaign wall time.
///
/// Runner launch attempts are recorded before execution and may fail before spawning a child.
/// Results and elapsed time follow at settlement. Interrupted attempts may lack both.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AttemptCost {
    /// Runner invocations, including pre-spawn refusals; not physical subprocess creations.
    #[serde(rename = "launchAttempts", alias = "launches")]
    pub launches: usize,
    pub execution_ms: u64,
    pub hits: usize,
    pub misses: usize,
    pub inconclusive: usize,
}

impl AttemptCost {
    pub(super) fn add(&mut self, other: &Self) {
        self.launches = self.launches.saturating_add(other.launches);
        self.execution_ms = self.execution_ms.saturating_add(other.execution_ms);
        self.hits = self.hits.saturating_add(other.hits);
        self.misses = self.misses.saturating_add(other.misses);
        self.inconclusive = self.inconclusive.saturating_add(other.inconclusive);
    }
}

/// File-only sweep accounting. Legacy counters keep their historical meanings.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SweepAccounting {
    /// False when earlier campaign segments predate this accounting, not an indication of settlement.
    pub complete: bool,
    /// Unknown for legacy campaigns; coverage is frozen once, not added on resume.
    pub initial_population: Option<usize>,
    pub initial_exact_hints: Option<usize>,
    /// Distinct mutants whose initially recorded test actually failed, independent of verdict credit.
    pub initial_killer_confirmed: Option<usize>,
    pub exact: AttemptCost,
    pub inferred: AttemptCost,
    pub fallback: AttemptCost,
    /// Additional executions for resource, kill and enumeration confirmation.
    ///
    /// Disjoint from the three primary execution tiers above.
    pub confirmation: AttemptCost,
    pub duplicate_launches_avoided: usize,
    pub live_refresh_failures: usize,
}

impl SweepAccounting {
    pub(super) fn segment(initial: Option<&InitialHints>, complete: bool) -> Self {
        Self {
            complete,
            initial_population: initial.map(|hints| hints.population),
            initial_exact_hints: initial.map(|hints| hints.killers.len()),
            initial_killer_confirmed: initial.map(|_| 0),
            ..Self::default()
        }
    }

    pub(super) fn add(&mut self, current: &Self) {
        self.complete &= current.complete;
        self.exact.add(&current.exact);
        self.inferred.add(&current.inferred);
        self.fallback.add(&current.fallback);
        self.confirmation.add(&current.confirmation);
        self.duplicate_launches_avoided = self.duplicate_launches_avoided.saturating_add(current.duplicate_launches_avoided);
        self.live_refresh_failures = self.live_refresh_failures.saturating_add(current.live_refresh_failures);
    }

    fn combine(previous: Option<Self>, current: Option<Self>) -> Option<Self> {
        match (previous, current) {
            (Some(mut prior), Some(current)) => {
                prior.add(&current);
                Some(prior)
            }
            (None, Some(mut current)) => {
                current.complete = false;
                current.initial_population = None;
                current.initial_exact_hints = None;
                current.initial_killer_confirmed = None;
                Some(current)
            }
            (Some(mut prior), None) => {
                prior.complete = false;
                Some(prior)
            }
            (None, None) => None,
        }
    }
}

impl SweepCost {
    pub(crate) fn live_json(&self) -> serde_json::Result<String> {
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Live<'a> {
            accounting: Option<&'a SweepAccounting>,
            schema_version: u32,
            sweep_wall_ms: u64,
        }
        serde_json::to_string_pretty(&Live {
            accounting: self.accounting.as_ref(),
            schema_version: 1,
            sweep_wall_ms: u64::try_from(self.elapsed.as_millis()).unwrap_or(u64::MAX),
        })
    }

    pub(super) fn combine(previous: Option<Self>, current: Option<Self>) -> Option<Self> {
        let keyed_confirmations = match (&previous, &current) {
            (Some(previous), Some(current)) => [previous, current].into_iter().all(|cost| {
                cost.accounting
                    .as_ref()
                    .and_then(|accounting| accounting.initial_killer_confirmed)
                    .unwrap_or(0)
                    <= cost.confirmed_initial_killers.len()
            }),
            _ => true,
        };
        let mut total = Self::combine_checkpoint(previous, current);
        if let Some(accounting) = total.as_mut().and_then(|cost| cost.accounting.as_mut()) {
            accounting.complete &= keyed_confirmations;
        }
        total
    }

    /// Combines cumulative counters whose confirmation identities are journaled separately.
    pub(super) fn combine_checkpoint(previous: Option<Self>, current: Option<Self>) -> Option<Self> {
        let Some(mut total) = previous else { return current };
        let Some(current) = current else { return Some(total) };
        let previous_confirmed = total
            .accounting
            .as_ref()
            .and_then(|cost| cost.initial_killer_confirmed)
            .unwrap_or(0);
        let current_confirmed = current
            .accounting
            .as_ref()
            .and_then(|cost| cost.initial_killer_confirmed)
            .unwrap_or(0);
        let offset = u64::try_from(total.elapsed.as_millis()).unwrap_or(u64::MAX);
        total.elapsed = total.elapsed.saturating_add(current.elapsed);
        total.accounting = SweepAccounting::combine(total.accounting, current.accounting);
        total.confirmed_initial_killers.extend(current.confirmed_initial_killers);
        if let Some(accounting) = &mut total.accounting
            && let Some(confirmed) = &mut accounting.initial_killer_confirmed
        {
            *confirmed = total.confirmed_initial_killers.len().max(previous_confirmed).max(current_confirmed);
        }
        macro_rules! add_counts {
            ($($field:ident),+ $(,)?) => {
                $(total.$field = total.$field.saturating_add(current.$field);)+
            };
        }
        add_counts!(
            launches,
            probes,
            exact_probes,
            exact_hits,
            inferred_probes,
            inferred_hits,
            inferred_rejected,
            exact_candidates,
            inferred_candidates,
            item_candidates,
            reach_candidates,
            file_candidates,
            item_probes,
            item_hits,
            reach_probes,
            reach_hits,
            file_probes,
            file_hits,
            item_rejected,
            reach_rejected,
            file_rejected,
            whole_selections,
            uncovered_selections,
            launches_saved,
            reach_launches_saved,
        );
        let mut packages: BTreeMap<_, _> = total
            .packages
            .into_iter()
            .map(|package| (Arc::clone(&package.package), package))
            .collect();
        for mut package in current.packages {
            package.first_started_ms = package.first_started_ms.saturating_add(offset);
            package.settled_ms = package.settled_ms.saturating_add(offset);
            if let Some(prior) = packages.get_mut(&package.package) {
                prior.mutants = prior.mutants.saturating_add(package.mutants);
                prior.first_started_ms = prior.first_started_ms.min(package.first_started_ms);
                prior.settled_ms = prior.settled_ms.max(package.settled_ms);
            } else {
                packages.insert(Arc::clone(&package.package), package);
            }
        }
        total.packages = packages.into_values().collect();
        Some(total)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use core::time::Duration;

    use super::{AttemptCost, PackageSweepCost, SweepAccounting, SweepCost};

    #[test]
    fn live_sweep_json_preserves_nulls_field_names_and_pretty_output() {
        let mut cost = SweepCost {
            elapsed: Duration::from_millis(123),
            ..SweepCost::default()
        };
        assert_eq!(
            cost.live_json().unwrap(),
            "{\n  \"accounting\": null,\n  \"schemaVersion\": 1,\n  \"sweepWallMs\": 123\n}"
        );
        cost.elapsed = Duration::MAX;
        cost.accounting = Some(SweepAccounting {
            complete: true,
            initial_population: Some(11),
            initial_exact_hints: Some(7),
            initial_killer_confirmed: Some(3),
            exact: AttemptCost {
                launches: 5,
                execution_ms: 17,
                hits: 2,
                misses: 1,
                inconclusive: 2,
            },
            duplicate_launches_avoided: 4,
            live_refresh_failures: 6,
            ..SweepAccounting::default()
        });
        let live: serde_json::Value = serde_json::from_str(&cost.live_json().unwrap()).unwrap();
        let empty = serde_json::json!({
            "launchAttempts": 0, "executionMs": 0, "hits": 0, "misses": 0, "inconclusive": 0
        });
        assert_eq!(
            live,
            serde_json::json!({
                "schemaVersion": 1,
                "sweepWallMs": u64::MAX,
                "accounting": {
                    "complete": true,
                    "initialPopulation": 11,
                    "initialExactHints": 7,
                    "initialKillerConfirmed": 3,
                    "exact": {"launchAttempts": 5, "executionMs": 17, "hits": 2, "misses": 1, "inconclusive": 2},
                    "inferred": empty,
                    "fallback": empty,
                    "confirmation": empty,
                    "duplicateLaunchesAvoided": 4,
                    "liveRefreshFailures": 6
                }
            })
        );
    }

    #[test]
    fn telemetry_unkeyed_historical_confirmation_totals_are_preserved_without_inventing_new_confirmations() {
        let previous = SweepCost {
            accounting: Some(SweepAccounting {
                complete: true,
                initial_population: Some(20),
                initial_exact_hints: Some(10),
                initial_killer_confirmed: Some(5),
                ..SweepAccounting::default()
            }),
            ..SweepCost::default()
        };
        let current = SweepCost {
            confirmed_initial_killers: ["possibly-repeated", "possibly-new"].into_iter().map(Into::into).collect(),
            accounting: Some(SweepAccounting {
                complete: true,
                initial_killer_confirmed: Some(2),
                ..SweepAccounting::default()
            }),
            ..SweepCost::default()
        };
        let combined = SweepCost::combine(Some(previous), Some(current)).unwrap().accounting.unwrap();
        assert_eq!(
            combined.initial_killer_confirmed,
            Some(5),
            "unknown overlap cannot justify either resetting or inflating a saved total"
        );
        assert!(
            !combined.complete,
            "unkeyed historical confirmations are only a preserved lower bound"
        );
    }

    #[test]
    fn telemetry_resume_adds_work_but_never_adds_population_coverage_or_resets_legacy_costs() {
        let accounting = SweepAccounting {
            complete: true,
            initial_population: Some(100),
            initial_exact_hints: Some(80),
            initial_killer_confirmed: Some(3),
            exact: AttemptCost {
                launches: 7,
                execution_ms: 700,
                misses: 4,
                ..AttemptCost::default()
            },
            confirmation: AttemptCost {
                launches: 3,
                execution_ms: 100,
                ..AttemptCost::default()
            },
            duplicate_launches_avoided: 2,
            ..SweepAccounting::default()
        };
        let prior = SweepCost {
            elapsed: Duration::from_millis(100),
            launches: 17,
            accounting: Some(accounting.clone()),
            confirmed_initial_killers: ["a", "b", "c"].into_iter().map(Into::into).collect(),
            ..SweepCost::default()
        };
        let segment = SweepCost {
            elapsed: Duration::from_millis(50),
            launches: 4,
            accounting: Some(SweepAccounting {
                initial_killer_confirmed: Some(6),
                ..accounting
            }),
            confirmed_initial_killers: ["a", "b", "c", "d", "e", "f"].into_iter().map(Into::into).collect(),
            ..SweepCost::default()
        };
        let total = SweepCost::combine(Some(prior), Some(segment)).unwrap();
        let actual = total.accounting.as_ref().unwrap();
        assert!(actual.complete);
        assert_eq!(
            (
                actual.initial_population,
                actual.initial_exact_hints,
                actual.initial_killer_confirmed
            ),
            (Some(100), Some(80), Some(6))
        );
        assert_eq!(
            (actual.exact.launches, actual.exact.execution_ms, actual.exact.misses),
            (14, 1400, 8)
        );
        assert_eq!(
            (
                actual.confirmation.launches,
                actual.confirmation.execution_ms,
                actual.duplicate_launches_avoided
            ),
            (6, 200, 4)
        );
        assert_eq!(
            total.elapsed,
            Duration::from_millis(150),
            "summed concurrent attempts are not wall time"
        );
        let restored: SweepCost = serde_json::from_value(serde_json::to_value(&total).unwrap()).unwrap();
        let settled = SweepCost::combine(Some(restored), None).unwrap();
        assert_eq!(
            serde_json::to_value(&settled).unwrap(),
            serde_json::to_value(&total).unwrap(),
            "settled resume adds nothing"
        );
        let mut legacy = serde_json::to_value(&total).unwrap();
        legacy.as_object_mut().unwrap().remove("accounting");
        let legacy: SweepCost = serde_json::from_value(legacy).unwrap();
        assert!(legacy.accounting.is_none());
        let resumed = SweepCost::combine(Some(legacy), Some(total)).unwrap();
        assert_eq!(resumed.launches, 42, "old counters are retained, not silently reset");
        let incomplete = resumed.accounting.unwrap();
        assert!(!incomplete.complete, "new observations cannot recover legacy costs");
        assert_eq!(
            (
                incomplete.initial_population,
                incomplete.initial_exact_hints,
                incomplete.initial_killer_confirmed
            ),
            (None, None, None),
            "legacy coverage remains unknown"
        );
    }

    fn cost(count: usize, elapsed: u64, packages: &[(&str, usize, u64, u64)]) -> SweepCost {
        let mut value = serde_json::to_value(SweepCost::default()).unwrap();
        for field in value.as_object_mut().unwrap().values_mut() {
            if field.is_number() {
                *field = serde_json::json!(count);
            }
        }
        let mut cost: SweepCost = serde_json::from_value(value).unwrap();
        cost.elapsed = Duration::from_millis(elapsed);
        cost.packages = packages
            .iter()
            .map(|&(package, mutants, first_started_ms, settled_ms)| PackageSweepCost {
                package: package.into(),
                mutants,
                first_started_ms,
                settled_ms,
            })
            .collect();
        cost
    }

    #[test]
    fn inferred_test_counters_preserve_checkpoint_field_names() {
        let mut legacy = serde_json::to_value(SweepCost::default()).unwrap();
        for (field, count) in [
            ("generalized_probes", 7),
            ("generalized_hits", 3),
            ("generalized_rejected", 5),
            ("generalized_candidates", 11),
        ] {
            legacy[field] = serde_json::json!(count);
        }
        let restored: SweepCost = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(restored.inferred_probes, 7);
        assert_eq!(restored.inferred_hits, 3);
        assert_eq!(restored.inferred_rejected, 5);
        assert_eq!(restored.inferred_candidates, 11);
        assert_eq!(serde_json::to_value(restored).unwrap(), legacy);
    }

    #[test]
    fn cumulative_sweep_merges_every_counter_and_offsets_package_spans() {
        let prior = cost(2, 100, &[("b", 2, 10, 90)]);
        let segment = cost(3, 50, &[("b", 1, 5, 40), ("a", 2, 0, 50)]);
        let expected = cost(5, 150, &[("a", 2, 100, 150), ("b", 3, 10, 140)]);
        let total = SweepCost::combine(Some(prior), Some(segment)).unwrap();
        assert_eq!(serde_json::to_value(&total).unwrap(), serde_json::to_value(&expected).unwrap());
        let third = cost(1, 10, &[("a", 1, 0, 10)]);
        let expected = cost(6, 160, &[("a", 3, 100, 160), ("b", 3, 10, 140)]);
        let total = SweepCost::combine(Some(total), Some(third)).unwrap();
        assert_eq!(serde_json::to_value(&total).unwrap(), serde_json::to_value(&expected).unwrap());
        let unchanged = SweepCost::combine(Some(total), None).unwrap();
        assert_eq!(serde_json::to_value(unchanged).unwrap(), serde_json::to_value(expected).unwrap());
        assert!(SweepCost::combine(None, None).is_none());
        let fresh = cost(1, 10, &[]);
        assert_eq!(
            serde_json::to_value(SweepCost::combine(None, Some(fresh)).unwrap()).unwrap(),
            serde_json::to_value(cost(1, 10, &[])).unwrap(),
        );
    }

    #[test]
    fn cumulative_sweep_merges_large_overlapping_package_sets_in_name_order() {
        let mut prior = cost(1, 100, &[]);
        prior.packages = (0..1000)
            .rev()
            .map(|index| PackageSweepCost {
                package: format!("package-{index:04}").into(),
                mutants: 1,
                first_started_ms: 10,
                settled_ms: 90,
            })
            .collect();
        let mut segment = cost(1, 50, &[]);
        segment.packages = (500..1500)
            .rev()
            .map(|index| PackageSweepCost {
                package: format!("package-{index:04}").into(),
                mutants: 2,
                first_started_ms: 5,
                settled_ms: 40,
            })
            .collect();
        let total = SweepCost::combine(Some(prior), Some(segment)).unwrap();
        assert_eq!(total.packages.len(), 1500);
        for (index, package) in total.packages.iter().enumerate() {
            assert_eq!(package.package.as_ref(), format!("package-{index:04}"));
            assert_eq!(
                package.mutants,
                if index < 500 {
                    1
                } else if index < 1000 {
                    3
                } else {
                    2
                }
            );
            assert_eq!(package.first_started_ms, if index < 1000 { 10 } else { 105 });
            assert_eq!(package.settled_ms, if index < 500 { 90 } else { 140 });
        }
    }

    #[test]
    fn cumulative_sweep_saturates_counts_elapsed_and_package_offsets() {
        let prior = cost(usize::MAX, 1, &[("a", usize::MAX, 0, u64::MAX)]);
        let mut segment = cost(1, 1, &[("a", 1, u64::MAX, u64::MAX)]);
        segment.elapsed = Duration::MAX;
        let total = SweepCost::combine(Some(prior), Some(segment)).unwrap();
        let mut expected = cost(usize::MAX, 1, &[("a", usize::MAX, 0, u64::MAX)]);
        expected.elapsed = Duration::MAX;
        assert_eq!(serde_json::to_value(total).unwrap(), serde_json::to_value(expected).unwrap());
    }
}
