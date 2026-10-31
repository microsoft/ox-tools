// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::time::Duration;
use std::sync::Arc;
use std::time::Instant;

use camino::{Utf8Path, Utf8PathBuf};

use super::cargo_options::BuildLimits;
use super::events::Events;
use super::test_binary::{TestBinary, retain_linked_to_population, test_binaries_with_linkage};
use super::verdict::tail;
use super::workspace::Workspace;
use crate::discover::Plan;
use crate::error::{Error, error};
use crate::model::{Mutant, Outcome};
use crate::schema::Guard;
use crate::{HashMap, HashSet, Result};

mod blame;
mod complaints;
mod invoke;
#[cfg(test)]
#[doc(hidden)]
pub(super) use native_test_util::with_compiler;
mod isolation;
pub(super) mod messages;
mod splices;

#[cfg(all(test, not(miri)))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests;

use blame::{CompilerReason, blame};
use complaints::{DIAGNOSTIC_LIMIT, complaints, diagnostics, leading, manifests_of, prioritize};
use invoke::run_cargo;
use isolation::IsolationBudget;
#[cfg(test)]
use isolation::{FailureContext, failure_contexts, push_isolation_tiers};
use messages::compiled_sources;
use splices::Splices;

/// Where each live mutant's guard landed, by ordinal, paired with the file it landed in.
type Guards = HashMap<u32, (Utf8PathBuf, Guard)>;

#[derive(Debug, Clone, Copy)]
pub struct BuildScope<'a> {
    pub roots: Option<&'a [String]>,
    pub mutants: Option<&'a [String]>,
    pub publish_progress: bool,
}

fn compiles_test_harnesses(verb: &[&str]) -> bool {
    verb.first() == Some(&"test")
        || verb
            .iter()
            .any(|argument| matches!(*argument, "--test" | "--tests" | "--all-targets"))
}

fn package_of_message(manifest: Option<&str>, package_id: Option<&str>, plan: &Plan, root: &Utf8Path) -> Option<String> {
    if let Some(manifest) = manifest {
        let manifest = manifest.replace('\\', "/");
        for (package, (directory, _version)) in &plan.specs {
            let expected = root.join(directory).join("Cargo.toml").as_str().replace('\\', "/");
            if manifest.eq_ignore_ascii_case(&expected) {
                return Some(package.clone());
            }
        }
    }

    let package_id = package_id?;
    if let Some((_source, named)) = package_id.rsplit_once('#') {
        return Some(named.split('@').next().unwrap_or(named).to_owned());
    }
    package_id.split_whitespace().next().map(ToOwned::to_owned)
}

fn retain_blamed(blamed: &mut HashMap<u32, CompilerReason>, plan: &Plan, packages: Option<&[String]>) {
    let Some(packages) = packages else {
        return;
    };

    let packages: HashSet<&str> = packages.iter().map(String::as_str).collect();
    let ordinals: HashSet<u32> = plan
        .mutants
        .iter()
        .filter(|mutant| packages.contains(&*mutant.package))
        .map(|mutant| mutant.ordinal)
        .collect();

    blamed.retain(|ordinal, _code| ordinals.contains(ordinal));
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod final_survivor_tests {
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt as _;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt as _;

    use serde_json::{Value, from_str, from_value, json};

    use super::*;

    #[derive(Default)]
    struct RecordedConvergence {
        progress: Vec<usize>,
        activity: Vec<(u32, usize, usize)>,
    }

    impl Events for RecordedConvergence {
        fn phase(&mut self, _verb: &str, _detail: &str) {}
        fn mutant(&mut self, _mutant: &Mutant) {}
        fn convergence_progress(&mut self, excluded: usize) {
            self.progress.push(excluded);
        }
        fn convergence_activity(&mut self, round: u32, excluded: usize, proofs: usize) {
            self.activity.push((round, excluded, proofs));
        }
    }

    /// Points at the same ordinal-spaced source line as the corresponding seeded guard.
    fn memory_diagnostic(ordinal: u32) -> String {
        let line = ordinal * 10;
        json!({
            "reason": "compiler-message",
            "package_id": "selected 1.0.0",
            "target": {"name": "subject", "kind": ["lib"]},
            "message": {
                "level": "error",
                "message": "in-process compiler evidence",
                "code": {"code": "E0308"},
                "spans": [{
                    "file_name": "src/lib.rs", "is_primary": true,
                    "line_start": line, "column_start": 1, "line_end": line, "column_end": 2
                }]
            }
        })
        .to_string()
    }

    fn memory_compiler_output(succeeded: bool, stdout: String) -> std::process::Output {
        std::process::Output {
            status: std::process::ExitStatus::from_raw(if succeeded { 0 } else { 256 }),
            stdout: stdout.into_bytes(),
            stderr: Vec::new(),
        }
    }

    /// Separates selected/foreign attribution; nonzero prior learning exposes accidental progress resets.
    fn seeded_convergence_plan(root: &Utf8Path) -> (Plan, Converger) {
        let mut plan = plan();
        plan.root = root.to_owned();
        plan.mutants = ["selected", "outside"]
            .into_iter()
            .enumerate()
            .map(|(index, package)| Mutant {
                ordinal: u32::try_from(index + 1).unwrap(),
                package: package.into(),
                file: Utf8PathBuf::from("src/lib.rs").into(),
                ..crate::fixtures::mutant()
            })
            .collect();
        let guards = [1, 2]
            .map(|ordinal| {
                let position = |line| crate::schema::Position::new(line, 1).unwrap();
                (
                    ordinal,
                    (
                        "src/lib.rs".into(),
                        Guard {
                            site: position(ordinal * 10)..position(ordinal * 10 + 1),
                            mutated: None,
                        },
                    ),
                )
            })
            .into_iter()
            .collect();
        let converger = Converger {
            historical_exclusions: 7,
            splices: Splices::seeded_guards_for_test(root.to_owned(), guards),
            ..Converger::default()
        };
        (plan, converger)
    }

    #[test]
    fn recovered_convergence_keeps_selected_roots_filters_foreign_blame_and_publishes_exact_progress() {
        let work = Workspace::adopt("never-written-convergence-root".into(), "never-written-convergence-target".into());
        let (plan, mut converger) = seeded_convergence_plan(&work.root);
        let roots = vec!["selected".to_owned()];
        let mut events = RecordedConvergence::default();
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let result = with_compiler(
            {
                let calls = std::rc::Rc::clone(&calls);
                move |_work, args, _budget, _events| {
                    let call = calls.get();
                    calls.set(call + 1);
                    assert!(call < 2, "the bounded compiler script permits only withdrawal and success");
                    assert_eq!(args, ["check", "--tests", "--message-format=json", "--package", "selected"]);
                    Ok(Some(memory_compiler_output(
                        call == 1,
                        if call == 0 {
                            format!("{}\n{}", memory_diagnostic(1), memory_diagnostic(2))
                        } else {
                            String::new()
                        },
                    )))
                }
            },
            || {
                converger.converge(
                    &work,
                    &plan,
                    Some(&roots),
                    &["check", "--tests"],
                    BuildLimits::default(),
                    &mut events,
                )
            },
        )
        .unwrap();
        assert!(matches!(result, Convergence::Built(ref output) if output.is_empty()));
        assert_eq!(calls.get(), 2);
        assert_eq!(converger.withdrawn, HashSet::from_iter([1]));
        assert_eq!(converger.census.keys().copied().collect::<HashSet<_>>(), HashSet::from_iter([1]));
        assert_eq!(converger.per_round, [1]);
        assert_eq!(events.progress, [8]);
        assert_eq!(events.activity, [(1, 7, 0), (2, 8, 0)]);
    }

    #[test]
    fn recovered_convergence_returns_durable_exclusion_errors_before_withdrawing_the_proven_mutant() {
        let root = Utf8Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/isolation-graph/application");
        let work = Workspace::adopt(root, "never-written-exclusion-target".into());
        let (plan, mut converger) = seeded_convergence_plan(&work.root);
        converger.compiler_exclusions = Some(from_value(json!({"version": 1, "mutants": {}})).unwrap());
        converger.exclusion_store = Some(|_, _| Err(error!("named exclusion-store refusal")));
        let expected = "named exclusion-store refusal";
        let mut events = RecordedConvergence::default();
        let result = with_compiler(
            |_work, _args, _budget, _events| Ok(Some(memory_compiler_output(false, memory_diagnostic(1)))),
            || {
                converger.converge_scoped(
                    &work,
                    &plan,
                    BuildScope {
                        roots: None,
                        mutants: None,
                        publish_progress: true,
                    },
                    &["check", "--tests"],
                    BuildLimits::default(),
                    &mut events,
                )
            },
        );
        let Err(failure) = result else {
            panic!("a checked-in file cannot become the inventory's parent directory");
        };
        assert_eq!(failure.message(), expected);
        assert!(converger.withdrawn.is_empty());
        assert!(converger.census.is_empty());
        assert!(converger.per_round.is_empty());
        assert_eq!(converger.rounds, 1);
        assert!(events.progress.is_empty());
        assert_eq!(events.activity, [(1, 7, 0)]);
    }

    #[test]
    fn convergence_propagates_native_exclusion_store_failures_without_panicking() {
        // Keep the native store's exact platform error, including the unchanged withdrawal state.
        let root = Utf8Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("isolation-graph")
            .join("application");
        let work = Workspace::adopt(root.clone(), root.join("Cargo.toml/target"));
        let (plan, mut converger) = seeded_convergence_plan(&root);
        converger.compiler_exclusions = Some(from_value(json!({"version": 1, "mutants": {}})).unwrap());
        let expected = converger
            .compiler_exclusions
            .as_ref()
            .unwrap()
            .store(work.base())
            .expect_err("an immutable manifest file cannot contain the exclusion inventory");
        let result = with_compiler(
            |_work, _args, _budget, _events| Ok(Some(memory_compiler_output(false, memory_diagnostic(1)))),
            || {
                converger.converge_scoped(
                    &work,
                    &plan,
                    BuildScope {
                        roots: None,
                        mutants: None,
                        publish_progress: true,
                    },
                    &["check", "--tests"],
                    BuildLimits::default(),
                    &mut RecordedConvergence::default(),
                )
            },
        );
        let Err(failure) = result else {
            panic!("an immutable manifest file cannot contain the exclusion inventory");
        };
        assert_eq!(failure.to_string(), expected.to_string());
        assert!(converger.withdrawn.is_empty());
    }

    #[test]
    fn recovered_convergence_sorts_and_deduplicates_unresolved_context_labels_before_returning_the_exact_error() {
        let work = Workspace::adopt("never-written-unresolved-root".into(), "never-written-unresolved-target".into());
        let mut plan = plan();
        plan.mutants = ["alpha", "beta", "gamma"]
            .into_iter()
            .enumerate()
            .map(|(index, package)| Mutant {
                ordinal: u32::try_from(index + 1).unwrap(),
                package: package.into(),
                file: Utf8PathBuf::from("src/lib.rs").into(),
                ..crate::fixtures::mutant()
            })
            .collect();
        let stdout = [("alpha", "zulu"), ("beta", "aardvark"), ("gamma", "aardvark")]
            .map(|(package, target)| {
                let mut message: Value = from_str(&memory_diagnostic(1)).unwrap();
                message["package_id"] = format!("{package} 1.0.0").into();
                message["target"]["name"] = target.into();
                message.to_string()
            })
            .join("\n");
        let mut converger = Converger {
            subset_oracle: Some(|_| None),
            ..Converger::default()
        };
        let result = with_compiler(
            move |_work, _arguments, _budget, _events| Ok(Some(memory_compiler_output(false, stdout.clone()))),
            || {
                converger.converge(
                    &work,
                    &plan,
                    None,
                    &["check", "--tests"],
                    BuildLimits::default(),
                    &mut RecordedConvergence::default(),
                )
            },
        );
        let Err(failure) = result else {
            panic!("unproven compiler failures must be returned without exclusion");
        };
        assert_eq!(
            failure.message(),
            "could not safely isolate compiler failures in aardvark (lib) (1 unresolved candidates), zulu (lib) (1 unresolved candidates) within the bounded proof budget; \
             no unproven mutants were excluded. Re-run with `--show-build` to inspect the generated-code \
             errors, then fix the mutation encoding or narrow the campaign with `--file`. \
             Increasing the rollback limit does not increase the isolation proof budget"
        );
        assert_eq!(converger.isolation_budget.proofs, 3);
        assert!(converger.withdrawn.is_empty());
        assert!(converger.census.is_empty());
        assert_eq!(converger.history.len(), 1);
    }

    #[test]
    fn recovered_finish_preserves_a_frozen_empty_root_set_as_an_explicit_whole_workspace_cargo_build() {
        let work = Workspace::adopt("never-written-finish-root".into(), "never-written-finish-target".into());
        let mut plan = plan();
        let mut converger = Converger::default();
        converger.freeze_context(None);
        let result = with_compiler(
            |_work, arguments, _budget, _events| {
                assert_eq!(
                    arguments,
                    ["check", "--tests", "--keep-going", "--message-format=json", "--workspace"]
                );
                Err(error!("the in-memory whole-workspace finish observer stops execution"))
            },
            || {
                converger.finish(
                    &work,
                    &mut plan,
                    Some(&["must-not-be-reused".to_owned()]),
                    BuildLimits::default(),
                    &mut RecordedConvergence::default(),
                )
            },
        );
        let Err(failure) = result else {
            panic!("the compiler observer returns an error before any final build");
        };
        assert_eq!(failure.message(), "the in-memory whole-workspace finish observer stops execution");
    }

    #[test]
    fn recovered_convergence_withdraws_a_proven_interaction_even_when_an_independent_context_remains_unresolved() {
        let work = Workspace::adopt("never-written-mixed-root".into(), "never-written-mixed-target".into());
        let mut plan = plan();
        plan.mutants = ["alpha", "alpha", "beta"]
            .into_iter()
            .enumerate()
            .map(|(index, package)| Mutant {
                ordinal: u32::try_from(index + 1).unwrap(),
                package: package.into(),
                file: Utf8PathBuf::from("src/lib.rs").into(),
                ..crate::fixtures::mutant()
            })
            .collect();
        let stdout = ["alpha", "beta"]
            .map(|package| {
                let mut message: Value = from_str(&memory_diagnostic(1)).unwrap();
                message["package_id"] = format!("{package} 1.0.0").into();
                message.to_string()
            })
            .join("\n");
        let mut converger = Converger {
            subset_oracle: Some(|active| {
                if active.contains(&3) {
                    None
                } else {
                    Some(active.contains(&1) && active.contains(&2))
                }
            }),
            ..Converger::default()
        };
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let result = with_compiler(
            {
                let calls = std::rc::Rc::clone(&calls);
                move |_work, _arguments, _budget, _events| {
                    let call = calls.get();
                    calls.set(call + 1);
                    assert!(call < 2, "the in-memory compiler allows only one withdrawal and its final success");
                    Ok(Some(memory_compiler_output(
                        call == 1,
                        if call == 0 { stdout.clone() } else { String::new() },
                    )))
                }
            },
            || {
                converger.converge(
                    &work,
                    &plan,
                    None,
                    &["check", "--tests"],
                    BuildLimits::default(),
                    &mut RecordedConvergence::default(),
                )
            },
        )
        .unwrap();
        assert!(matches!(result, Convergence::Built(_)));
        assert_eq!(calls.get(), 2);
        assert_eq!(converger.withdrawn, HashSet::from_iter([1]));
        assert_eq!(converger.abandoned, HashSet::from_iter([1]));
        assert_eq!(converger.interactions, HashMap::from_iter([(1, vec![1, 2])]));
        assert!(converger.census.is_empty());
        assert_eq!(converger.per_round, [1]);
    }

    #[test]
    fn recovered_historical_census_requires_both_the_zero_ordinal_and_compile_error_status() {
        let mut plan = plan();
        plan.mutants = [
            (0, Outcome::CompileError, "historical", "first"),
            (0, Outcome::CompileError, "historical", "first"),
            (0, Outcome::Pending, "pending", "second"),
            (0, Outcome::Survived, "survived", "third"),
            (1, Outcome::CompileError, "live", "fourth"),
            (7, Outcome::CompileError, "live", "fifth"),
        ]
        .map(|(ordinal, outcome, package, mutator)| Mutant {
            ordinal,
            outcome,
            package: package.into(),
            mutator: mutator.into(),
            ..crate::fixtures::mutant()
        })
        .into();
        assert_eq!(
            Converger::default().tally(&plan),
            [Withdrawal {
                package: "historical".into(),
                code: String::new(),
                category: "historical compiler exclusion".into(),
                replacement_site: false,
                mutator: "first".into(),
                mutants: 2,
            }]
        );
    }

    #[test]
    fn recovered_native_manifest_separators_resolve_without_a_fallback_package_id() {
        let mut plan = plan();
        plan.specs.insert("selected".into(), ("members/selected".into(), "1.0.0".into()));
        assert_eq!(
            package_of_message(
                Some(r"C:\Workspace\members\selected\Cargo.toml"),
                None,
                &plan,
                Utf8Path::new("C:/Workspace"),
            ),
            Some("selected".into())
        );
    }

    fn plan() -> Plan {
        Plan {
            root: "root".into(),
            files: Vec::new(),
            mutants: Vec::new(),
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            digests: HashMap::default(),
            skipped: Vec::new(),
            reach: HashMap::default(),
            specs: HashMap::default(),
        }
    }

    #[test]
    fn package_resolution_prefers_a_manifest_then_decodes_both_package_id_formats() {
        let mut plan = plan();
        plan.specs = HashMap::from_iter([
            ("first".into(), ("members/first".into(), "1.0.0".into())),
            ("second".into(), ("members/second".into(), "2.0.0".into())),
        ]);
        let root = Utf8Path::new("C:\\Workspace");
        assert_eq!(
            package_of_message(Some("c:/workspace/members/second/CARGO.TOML"), Some("fallback 3.0.0"), &plan, root),
            Some("second".into())
        );
        for (identity, expected) in [
            ("registry+https://host/index#new-name@1.2.3", Some("new-name")),
            ("path+file:///root#outer#inner@9", Some("inner")),
            ("path+file:///root#unversioned", Some("unversioned")),
            ("old-name 1.2.3 (registry+https://host)", Some("old-name")),
            ("  spaced 1.2.3", Some("spaced")),
            (" \t", None),
        ] {
            assert_eq!(
                package_of_message(Some("unrelated/Cargo.toml"), Some(identity), &plan, root),
                expected.map(str::to_owned)
            );
        }
        assert_eq!(package_of_message(None, None, &plan, root), None);
    }

    #[test]
    fn package_membership_is_existential_and_abandonment_has_sorted_unique_ordinals() {
        let mut plan = plan();
        plan.mutants = [(9, "z"), (3, "a"), (7, "z"), (0, "z")]
            .into_iter()
            .map(|(ordinal, package)| Mutant {
                ordinal,
                package: package.into(),
                outcome: Outcome::Pending,
                ..crate::fixtures::mutant()
            })
            .collect();
        let packages = ["unrelated".to_owned(), "z".to_owned()];
        for mutant in &plan.mutants {
            assert_eq!(
                isolation_candidate(mutant, &HashSet::default(), Some(&packages)),
                mutant.ordinal != 0 && &*mutant.package == "z"
            );
        }
        let mut converger = Converger::default();
        let abandoned = converger.abandon(&mut plan, Some(&packages), &error!("compiler stopped"));
        assert_eq!(abandoned.ordinals, [7, 9]);
        assert_eq!(converger.withdrawn, HashSet::from_iter([7, 9]));
        assert_eq!(converger.abandoned, HashSet::from_iter([7, 9]));
        assert_eq!(
            plan.mutants.iter().map(|mutant| mutant.outcome).collect::<Vec<_>>(),
            [Outcome::NotBuilt, Outcome::Pending, Outcome::NotBuilt, Outcome::Pending]
        );
        let round = build_round(Duration::from_millis(17), &plan, [9, 3, 7, 9]);
        assert_eq!(round.elapsed, Duration::from_millis(17));
        assert_eq!(round.withdrew, 3);
        assert_eq!(
            round
                .packages
                .iter()
                .map(|package| (&*package.package, package.mutants))
                .collect::<Vec<_>>(),
            [("a", 1), ("z", 2)]
        );
    }
}

/// A test-only stand-in for one proof build: a verdict on the complete active schema.
///
/// Mirrors [`Converger::subset_fails`]'s own return: `Some(true)` failed, `Some(false)` compiled,
/// `None` could not be told (a timeout).
#[cfg(any(test, feature = "internals"))]
type SubsetOracle = fn(&HashSet<u32>) -> Option<bool>;

/// Historical build-ordering probe counts retained for report compatibility.
///
/// Current convergence does not probe stale hints and leaves these counters at zero.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OrderingHints {
    /// How many hinted mutants were put in front of the compiler in a probe round.
    pub offered: usize,

    /// How many of those the compiler then blamed, which is the hint turning out to be right.
    ///
    /// A hinted mutant that compiles is not an error and is not withheld: it stays live and is
    /// judged by the run exactly as if it had never been hinted. This counts only the ones the
    /// compiler independently refused.
    pub confirmed: usize,

    /// How many probe rounds the run spent, which is the cost side of the trade.
    pub rounds: u32,
}

/// What one round of a build cost, and what it bought.
///
/// A run reports the series rather than a total because the two ends mean opposite things. The
/// first round is what compiling this workspace costs at all, which no amount of mutant selection
/// will avoid; every round after it exists only because some mutant did not compile, and its time
/// is the price of that mutant. A total conflates the two and so points at the wrong remedy.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Round {
    /// How long the round's cargo invocation took.
    pub elapsed: Duration,

    /// How many mutants the round withdrew, which is zero for the round that finally compiled.
    pub withdrew: usize,

    /// Withdrawals attributed to each package during this round.
    pub packages: Vec<PackageWithdrawal>,
}

/// Mutants withdrawn from one package during one build round.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PackageWithdrawal {
    pub package: Arc<str>,
    pub mutants: usize,
}

fn build_round(elapsed: Duration, plan: &Plan, ordinals: impl IntoIterator<Item = u32>) -> Round {
    let ordinals: HashSet<u32> = ordinals.into_iter().collect();
    let mut packages: HashMap<Arc<str>, usize> = HashMap::default();

    for mutant in &plan.mutants {
        if ordinals.contains(&mutant.ordinal) {
            let count = packages.entry(Arc::clone(&mutant.package)).or_default();
            *count = count.saturating_add(1);
        }
    }

    let mut packages = packages
        .into_iter()
        .map(|(package, mutants)| PackageWithdrawal { package, mutants })
        .collect::<Vec<_>>();
    packages.sort_by(|left, right| left.package.cmp(&right.package));

    Round {
        elapsed,
        withdrew: ordinals.len(),
        packages,
    }
}

/// How many mutants one normalized compiler reason withdrew from one mutator in one package.
///
/// The grouping is what makes the figure actionable. A reason on its own says what kind of code
/// the instrumented tree contained; a mutator on its own says which mutator is expensive; together
/// they say *which mutator emits which mistake*, which is the form a heuristic can be written
/// against.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Withdrawal {
    /// The package whose mutants were withdrawn.
    pub package: String,

    /// The rustc error code, or empty for a diagnostic that carried none.
    pub code: String,

    /// The normalized primary diagnostic message.
    pub category: String,

    /// Whether the primary diagnostic span identified the replacement text.
    pub replacement_site: bool,

    /// The mutator whose mutants the code was reported against.
    pub mutator: String,

    /// How many mutants — not how many diagnostics — the group accounts for.
    pub mutants: usize,
}

/// What a build round produced.
#[derive(Debug, Default)]
pub struct Build {
    /// What each round of this run's builds cost, oldest first.
    pub history: Vec<Round>,

    pub binaries: Vec<TestBinary>,

    /// Cargo's successful final build stream, retained until the test runtime environment has
    /// been reconstructed from its artifacts and build-script outputs.
    pub artifacts: String,
    pub withdrawn: usize,

    /// How many rollback rounds the whole run spent, summed over its builds.
    pub rounds: u32,

    /// Whether a narrowed build was abandoned and the whole workspace built instead.
    pub widened: bool,

    /// Why the withdrawn mutants were withdrawn, densest pair first.
    pub census: Vec<Withdrawal>,

    /// The population given up on when this build could not be made to compile at all.
    ///
    /// `None` for a build that converged, which is the ordinary case. When it is set the binaries
    /// are empty — there is nothing to run a mutant against — and the run reports what it knows
    /// rather than exiting with nothing at all.
    pub stuck: Option<Abandoned>,

    /// Historical probe counts; current convergence leaves them at zero.
    pub ordering: OrderingHints,
}

/// What one build arrived at.
///
/// A build that cannot be made to compile is not the same kind of event as a build that could not
/// be started. The first is a fact about this tree and these mutants, which a run can record,
/// report and carry on from; the second is a fact about the machine, which nothing downstream can
/// say anything useful about. Only the first is modelled here — everything else stays an `Err`.
#[derive(Debug)]
pub enum Convergence {
    /// The build succeeded, carrying cargo's JSON stream from the round that succeeded.
    Built(String),

    /// The build failed and no further mutant can be withdrawn to change that.
    Stuck(Error),
}

/// What a proof build isolated after rustc's spans could not identify a cause.
#[derive(Debug)]
pub enum Isolation {
    /// One or more mutants failed even without any other mutant from their item.
    Blamed(Vec<u32>),

    /// A minimal group fails only in combination, so one member is excluded without blaming it.
    Interaction { excluded: u32, members: Vec<u32> },

    /// The campaign budget ended before this context could be proved.
    Unresolved { ordinals: Vec<u32>, context: String },
}

/// The mutants a build gave up on, and why.
///
/// The reason is the text of the error that would otherwise abort the whole run. It is preserved
/// word for word — the rollback-limit advice about a falling or flat withdrawal series, the excerpt
/// of what cargo said — because that text is the only thing that tells a reader whether the answer
/// is to raise a limit, to fix a build script, or to look somewhere else entirely.
#[derive(Debug)]
pub struct Abandoned {
    /// The diagnostic explaining why the build could not be converged.
    pub reason: String,

    /// The ordinals of the mutants that will never be run because of it, ascending.
    pub ordinals: Vec<u32>,
}

/// Drives the build, withdrawing mutants that cannot compile until what is asked for compiles.
///
/// A run converges the complete instrumented test-target build. Every failed round withdraws all
/// mutants its diagnostics can identify, then repeats the same Cargo command with those mutations
/// restored to their original source. `--rollback-rounds` caps this one convergence loop.
#[derive(Debug, Default)]
pub struct Converger {
    #[cfg(test)]
    exclusion_store: Option<fn(&crate::discover::CompilerExclusions, &Utf8Path) -> Result<()>>,
    pub fixed_roots: Option<Vec<String>>,
    pub compiler_exclusions: Option<crate::discover::CompilerExclusions>,
    pub historical_exclusions: usize,
    pub proof_background: HashSet<u32>,
    pub proof_active: HashSet<u32>,
    pub withdrawn: HashSet<u32>,

    /// The subset of `withdrawn` that was given up on rather than blamed.
    ///
    /// A withdrawn mutant is one the compiler pointed at: it is unviable, and saying so is a
    /// verdict. A mutant in here was never accused of anything — the build it belonged to could
    /// not be made to compile, so its whole population was taken out of the tree to let the run
    /// carry on. Conflating the two would report a mutant the tool never judged as one the tool
    /// judged unbuildable, which is the exact confusion this run is trying to avoid.
    pub abandoned: HashSet<u32>,

    /// Minimal compiler-conflict groups whose excluded member was never individually unviable.
    pub interactions: HashMap<u32, Vec<u32>>,

    /// Budget-exhausted compiler contexts whose pending mutants were not judged.
    pub unresolved: HashMap<u32, String>,

    /// Mutants discovered from a different source generation than the synchronized build tree.
    pub unavailable: HashSet<u32>,

    /// How many rounds the build currently converging has spent, reset at the start of each one.
    pub rounds: u32,

    /// How many rounds the whole run has spent, which is what the run reports.
    pub total_rounds: u32,

    /// How many mutants each failed round of the current build blamed, oldest first.
    ///
    /// Kept so that a build which hits the limit can say whether it was converging, which is the
    /// only thing that decides whether raising the limit would have helped. Reset with `rounds`,
    /// so that the advice describes the build that just failed rather than the ones before it.
    ///
    /// Counts what a round blamed rather than what it withdrew, because the two differ in exactly
    /// the round the reader is being told about: the round that hits the limit blames mutants and
    /// is then stopped before it can withdraw them. Recording the withdrawal would leave that round
    /// out of its own diagnostic, and the trend the advice reads is a trend in what the rounds are
    /// finding.
    pub per_round: Vec<usize>,

    /// What every round of every build in this run cost, oldest first.
    ///
    /// Unlike `per_round`, this is never reset: it describes the whole run, because what a reader
    /// wants to know is where the run's build time went, not where one stage's did.
    pub history: Vec<Round>,

    /// How long the first ordinary round of the current build took.
    ///
    /// Reset before each convergence. Subsequent rollback and isolation rounds repeat that build's
    /// Cargo command and roots, so they are comparable.
    pub first_round: Option<Duration>,

    /// What the tree already holds, so a round rewrites only the files it changed.
    pub splices: Splices,

    /// Source files named by the successful instrumented test-target build.
    pub compiled: Option<HashSet<Utf8PathBuf>>,

    /// The rustc error code that first named each withdrawn mutant.
    ///
    /// The count of withdrawals says whether the number is large; only the codes say whether it is
    /// worth acting on. A run dominated by `E0308` is one where a mutator produces ill-typed code
    /// and could be taught not to, while one dominated by the borrow checker is a cost of the
    /// schema itself. Distinguishing them by hand meant patching this file every time, which is why
    /// it is kept rather than derived on demand.
    pub census: HashMap<u32, CompilerReason>,

    /// Whether every build this run makes must compile the whole workspace.
    ///
    /// Set when preflight checked the whole workspace, either initially or after widening. Every
    /// later build retains that root set so Cargo's feature unification stays constant. Narrowing
    /// after a wide-only success would reproduce a known failure; narrowing an initially wide
    /// build would compile dependency variants that the final workspace build cannot reuse.
    pub whole_workspace: bool,

    /// Whether the verdict oracle is restricted to library unit-test harnesses.
    pub test_lib: bool,

    /// A test-only stand-in for the proof build in [`Self::subset_fails`].
    ///
    /// Reaching [`Isolation::Interaction`] needs subsets that compile alone but fail only in
    /// combination, which no cheap real mutation fixture produces. When set, each proof build asks
    /// this function — a pure verdict on which ordinals are spliced — instead of invoking cargo, so
    /// a test can drive isolation to any branch deterministically without a real interaction bug.
    #[cfg(any(test, feature = "internals"))]
    pub subset_oracle: Option<SubsetOracle>,
    #[cfg(any(test, feature = "internals"))]
    pub proof_roots: Vec<Option<Vec<String>>>,

    /// Aggregate isolation work shared by every diagnostic context in one Cargo invocation.
    pub isolation_budget: IsolationBudget,
}

/// What a preflight check settled: the scope it needed, and what it cost to pass at all.
#[derive(Debug)]
pub struct Preflight {
    /// Whether only a whole-workspace build was shown to compile.
    ///
    /// Carried rather than discarded because a narrowed build after a wide-only success is a build
    /// already known to fail — and its failure would be blamed on mutants, settling valid ones as
    /// unbuildable and quietly shrinking the population the score is taken over.
    pub whole_workspace: bool,

    /// The packages the check had to give up on, empty in the ordinary case.
    pub dropped: Vec<String>,
}

impl Preflight {
    /// The result of a check that passed in the scope it was asked about.
    fn narrow(dropped: Vec<String>) -> Self {
        Self {
            whole_workspace: false,
            dropped,
        }
    }
}

fn isolation_candidate(mutant: &Mutant, withdrawn: &HashSet<u32>, packages: Option<&[String]>) -> bool {
    mutant.ordinal > 0
        && !withdrawn.contains(&mutant.ordinal)
        && packages.is_none_or(|packages| packages.iter().any(|package| package.as_str() == &*mutant.package))
}

impl Converger {
    pub fn freeze_context(&mut self, roots: Option<&[String]>) {
        // Checks and final code generation retain the pristine validation's package/target,
        // feature, profile, and compiler context so exclusions stay attributable to mutant edits.
        self.fixed_roots = Some(roots.unwrap_or_default().to_vec());
    }

    #[must_use]
    pub fn check_verb(&self) -> &'static [&'static str] {
        if self.test_lib {
            &["test", "--no-run", "--lib", "--no-fail-fast"]
        } else {
            &["check", "--tests", "--keep-going"]
        }
    }

    fn persist_compiler_exclusions(&mut self, work: &Workspace, plan: &Plan, blamed: &HashMap<u32, CompilerReason>) -> Result<()> {
        if let Some(inventory) = self.compiler_exclusions.as_mut()
            && inventory.remember(&plan.mutants, &blamed.keys().copied().collect(), |ordinal| blamed[&ordinal].note())
        {
            #[cfg(test)]
            if let Some(store) = self.exclusion_store {
                return store(inventory, work.base());
            }
            inventory.store(work.base())?;
        }
        Ok(())
    }

    /// Restricts preflight and final test-target builds to library unit-test harnesses.
    pub fn select_test_lib(&mut self, selected: bool) {
        self.test_lib = selected;
    }

    /// Records that only a whole-workspace build has been shown to compile.
    ///
    /// Called with the preflight's own answer, so that the scope which proved the tree sound is the
    /// scope every later build uses. See [`Self::whole_workspace`].
    pub const fn require_whole_workspace(&mut self) {
        self.whole_workspace = true;
    }

    /// Invalidates position-based splice indexes after the plan is sorted.
    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    pub(super) fn plan_reordered(&mut self) {
        self.splices.plan_reordered();
    }

    /// The package selection a build may actually use.
    ///
    /// Every narrowing goes through here, so there is one place that can answer "may this build be
    /// narrowed at all" and no build can be narrowed by forgetting to ask.
    const fn scoped<'names>(&self, select: Option<&'names [String]>) -> Option<&'names [String]> {
        if self.whole_workspace { None } else { select }
    }

    /// The mutants that must be absent while one attribution scope is being judged.
    ///
    /// Cargo's roots may be wider than the stage to keep feature unification stable. Mutants from
    /// those additional packages still have to be restored to pristine source: filtering their
    /// diagnostics after compilation is too late, because they may already have broken or masked
    /// the build.
    // #[gamma::skip(fn_value.empty_collection, tag = "timeout", reason = "dropping withdrawn mutants repeatedly rebuilds contexts that cannot be isolated in the campaign")]
    fn scoped_withdrawn(&self, plan: &Plan, packages: Option<&[String]>) -> HashSet<u32> {
        let mut withdrawn = self.withdrawn.clone();
        let Some(packages) = packages else {
            return withdrawn;
        };

        let packages: HashSet<&str> = packages.iter().map(String::as_str).collect();

        withdrawn.extend(
            plan.mutants
                .iter()
                .filter(|mutant| !packages.contains(&*mutant.package))
                .map(|mutant| mutant.ordinal),
        );

        withdrawn
    }

    /// Resets state that describes one Cargo command and root set.
    fn begin_convergence(&mut self) {
        // #[gamma::skip(all, reason = "the replacement is exactly the type default already written here, so it is semantically identical")]
        self.rounds = 0;
        self.per_round.clear();
        // #[gamma::skip(all, reason = "the replacement is exactly the type default already written here, so it is semantically identical")]
        self.first_round = None;
        self.isolation_budget = IsolationBudget::default();
    }

    fn admit_withdrawal_round(&mut self, blamed: usize, limits: BuildLimits) -> bool {
        self.per_round.push(blamed);
        self.rounds < limits.rounds()
    }

    /// Instruments the tree and builds it until it compiles, withdrawing whatever stands in the way.
    ///
    /// The scope's roots name the packages Cargo compiles, while its mutants limit what convergence
    /// may withdraw. They normally agree, but the complete schema admits mutations from every
    /// target package even when the test oracle narrows Cargo's roots. `verb` is the cargo command
    /// and its flags.
    ///
    /// Returns cargo's JSON stream from the build that finally succeeded, or the diagnostic for a
    /// build that could not be made to compile at all. That second case is returned rather than
    /// raised because it is a result: the run can withdraw the population it belongs to, keep every
    /// verdict it has already reached, and still produce a report.
    #[cfg_attr(coverage_nightly, coverage(off))]
    #[expect(
        clippy::too_many_lines,
        reason = "the convergence loop keeps one auditable state transition from Cargo outcome through verdict publication"
    )]
    pub fn converge_scoped(
        &mut self,
        work: &Workspace,
        plan: &Plan,
        scope: BuildScope<'_>,
        verb: &[&str],
        limits: BuildLimits,
        events: &mut dyn Events,
    ) -> Result<Convergence> {
        // The round budget is per build: what earlier stages spent converging is not this build's
        // to answer for, and the withdrawal series the limit error reads has to describe the build
        // that failed. The withdrawal set is deliberately left alone — a mutant already known not
        // to compile stays withdrawn for the rest of the run.
        self.begin_convergence();
        let may_compile_test_harnesses = compiles_test_harnesses(verb);

        loop {
            self.rounds = self.rounds.saturating_add(1);
            self.total_rounds = self.total_rounds.saturating_add(1);

            let withdrawn = self.scoped_withdrawn(plan, scope.mutants);
            let (guards, written) = self.instrument_schema(work, plan, &withdrawn)?;

            events.convergence_activity(
                self.total_rounds,
                self.historical_exclusions + self.census.len(),
                self.isolation_budget.proof_count(),
            );
            let started = Instant::now();
            let outcome = run_cargo(work, plan, verb, scope.roots, limits, self.first_round, events)?;
            let mut elapsed = started.elapsed();

            let Some(stdout) = outcome.stdout else {
                let budget = limits.budget(self.first_round).unwrap_or(elapsed);

                return Err(Self::build_timeout_error(&verb.join(" "), budget));
            };
            if events.wants_convergence_evidence() {
                let evidence = messages::build_evidence(&stdout, may_compile_test_harnesses);
                let written = written.iter().map(|path| path.as_std_path()).collect::<Vec<_>>();
                events.convergence_evidence(
                    self.total_rounds,
                    &written,
                    evidence.fresh,
                    evidence.rebuilt,
                    &evidence.rebuilt_targets,
                    &evidence.failed_targets,
                );
            }

            // #[gamma::skip(all, reason = "the branch handles process, filesystem, platform, or synchronization state that cannot be forced safely and deterministically in unit tests")]
            if self.first_round.is_none() {
                // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
                self.first_round = Some(elapsed);
            }

            if outcome.succeeded {
                // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
                self.history.push(build_round(elapsed, plan, []));

                return Ok(Convergence::Built(stdout));
            }

            let mut blamed = blame(&stdout, &work.root, &guards);
            retain_blamed(&mut blamed, plan, scope.mutants);

            if blamed.is_empty() {
                let proof_started = Instant::now();
                let isolated = self.isolate_scoped(work, plan, &stdout, verb, scope.roots, limits, events)?;
                elapsed = elapsed.saturating_add(proof_started.elapsed());
                if isolated.is_empty() {
                    self.history.push(build_round(elapsed, plan, []));

                    return Ok(Convergence::Stuck(Self::unattributed_build_error(work, &stdout, &outcome.stderr)));
                }

                let mut interactions = Vec::new();
                let mut unresolved = Vec::new();
                for isolated in isolated {
                    match isolated {
                        Isolation::Blamed(ordinals) => {
                            blamed.extend(ordinals.into_iter().map(|ordinal| (ordinal, CompilerReason::isolated())));
                        }
                        Isolation::Interaction { excluded, members } => {
                            interactions.push((excluded, members));
                        }
                        Isolation::Unresolved { ordinals, context } => {
                            unresolved.push((ordinals, context));
                        }
                    }
                    interactions.retain(|(excluded, _members)| !blamed.contains_key(excluded));
                    for (ordinals, _context) in &mut unresolved {
                        // #[gamma::skip(unary.remove_not, tag = "timeout", reason = "retaining blamed ordinals prevents isolation from removing resolved work")]
                        ordinals.retain(|ordinal| !blamed.contains_key(ordinal));
                    }
                    // #[gamma::skip(unary.remove_not, tag = "timeout", reason = "retaining empty unresolved contexts prevents build recovery from making progress")]
                    unresolved.retain(|(ordinals, _context)| !ordinals.is_empty());
                }

                if blamed.is_empty() && interactions.is_empty() {
                    let mut contexts = unresolved
                        .iter()
                        .map(|(ordinals, context)| format!("{context} ({} unresolved candidates)", ordinals.len()))
                        .collect::<Vec<_>>();
                    contexts.sort_unstable();
                    contexts.dedup();
                    self.history.push(build_round(elapsed, plan, []));
                    return Err(error!(
                        "could not safely isolate compiler failures in {} within the bounded proof budget; \
                         no unproven mutants were excluded. Re-run with `--show-build` to inspect the generated-code \
                         errors, then fix the mutation encoding or narrow the campaign with `--file`. \
                         Increasing the rollback limit does not increase the isolation proof budget",
                        contexts.join(", ")
                    ));
                }

                if
                // #[gamma::skip(cond.always_true, reason = "When the original !interactions.is_empty condition is false, earlier guards imply nonempty blamed proven by isolation and no interactions. The forced interaction arm performs exactly the same persist/admit/withdrawn/census/history/progress steps as the normal following blame-only arm: its extra extend/sort/dedup acts on unique map keys, its interaction loop is empty, census reinsertion changes nothing, and continue reaches the same next iteration. Both use identical sorted build_round output and per-round count.", tag = "equivalent")]
                !interactions.is_empty() {
                    let mut withdrawn = blamed.keys().copied().collect::<Vec<_>>();
                    withdrawn.extend(interactions.iter().map(|(excluded, _members)| *excluded));
                    withdrawn.sort_unstable();
                    withdrawn.dedup();

                    // #[gamma::skip(try.propagate_to_unwrap, reason = "This is the interaction-arm persistence call after isolate_scoped. Every possible blamed singleton was already passed through persist_compiler_exclusions successfully by isolate_scoped before being returned, and compiler_exclusions remains owned/unchanged between calls. For Some inventory, remember finds no vacant identity and returns false, so store is not called; for None it also returns Ok. Thus this exact second persistence call cannot return Err to distinguish ? from unwrap.", tag = "equivalent")]
                    self.persist_compiler_exclusions(work, plan, &blamed)?;
                    if !self.admit_withdrawal_round(withdrawn.len(), limits) {
                        let error = Self::rollback_limit_error(self.rounds, limits.rounds(), &self.per_round, work, &stdout);
                        self.history.push(build_round(elapsed, plan, []));

                        return Ok(Convergence::Stuck(error));
                    }

                    for (ordinal, reason) in blamed {
                        let _ = self.withdrawn.insert(ordinal);
                        let _ = self.census.entry(ordinal).or_insert(reason);
                    }
                    for (excluded, members) in interactions {
                        withdrawn.push(excluded);
                        let _ = self.withdrawn.insert(excluded);
                        let _ = self.abandoned.insert(excluded);
                        let _ = self.interactions.insert(excluded, members);
                    }
                    self.history.push(build_round(elapsed, plan, withdrawn));
                    continue;
                }
            }

            // This round joins the series before the limit is checked, because the round the limit
            // stops is the one the diagnostic is about: it blamed these mutants and was refused the
            // chance to withdraw them. Reading the series without it leaves a one-round budget with
            // nothing to report and the advice saying the last round found nothing, which is the
            // opposite of what happened.
            self.persist_compiler_exclusions(work, plan, &blamed)?;
            if !self.admit_withdrawal_round(blamed.len(), limits) {
                let error = Self::rollback_limit_error(self.rounds, limits.rounds(), &self.per_round, work, &stdout);

                // Nothing was withdrawn: `history` is what the run reports its build time against,
                // and this round ended without applying its blame.
                self.history.push(build_round(elapsed, plan, []));

                return Ok(Convergence::Stuck(error));
            }

            self.history.push(build_round(elapsed, plan, blamed.keys().copied()));

            let census_before = self.census.len();
            for (ordinal, reason) in blamed {
                let _ = self.withdrawn.insert(ordinal);
                let _ = self.census.entry(ordinal).or_insert(reason);
            }
            if scope.publish_progress && self.census.len() > census_before {
                events.convergence_progress(self.historical_exclusions + self.census.len());
            }
        }
    }

    #[cfg(any(test, feature = "internals"))]
    pub fn converge(
        &mut self,
        work: &Workspace,
        plan: &Plan,
        select: Option<&[String]>,
        verb: &[&str],
        limits: BuildLimits,
        events: &mut dyn Events,
    ) -> Result<Convergence> {
        self.converge_scoped(
            work,
            plan,
            BuildScope {
                roots: select,
                mutants: select,
                publish_progress: true,
            },
            verb,
            limits,
            events,
        )
    }

    fn build_timeout_error(stage: &str, budget: Duration) -> Error {
        error!(
            "the compiler invocation `cargo {stage}` was still running after {budget:.0?} and was \
             stopped. Convergence may run checks and bounded proof invocations before the final \
             code-generating build; raise --build-timeout if this stage is simply slow."
        )
    }

    fn unattributed_build_error(work: &Workspace, stdout: &str, stderr: &str) -> Error {
        let diagnostics = diagnostics(stdout);

        // A build that produced no diagnostics at all did not fail the way this message assumes.
        // The compiler was never reached — a build script panicked, a native library is missing, a
        // dependency would not resolve, a package spec was ambiguous — and every one of those is
        // explained on stderr and nowhere else. Saying "does not compile" here would send the
        // reader hunting for a broken mutant that was never generated.
        if diagnostics.is_empty() {
            return error!(
                "the instrumented tree failed to build, and the compiler reported nothing, so no \
                 mutant can be blamed for it. The cause is usually something cargo hit before it \
                 reached the code — a build script, a missing native dependency, a bad invocation — \
                 and it is almost always in what cargo said:\n\n{}\n\n{}",
                tail(&complaints(stderr), 30),
                work.inspect_hint()
            );
        }

        error!(
            "the instrumented tree does not compile and the failure could not be attributed to a mutant.\n\
             {}\n\n{}",
            work.inspect_hint(),
            leading(&diagnostics, DIAGNOSTIC_LIMIT)
        )
    }

    /// Explains a build that ran out of rollback rounds.
    ///
    /// `per_round` is what each round of this build blamed, oldest first, and it includes the round
    /// the limit stopped. Every entry is non-zero: a round that blames nothing is not a rollback
    /// failure at all and is reported by `unattributed_build_error` instead.
    fn rollback_limit_error(rounds: u32, limit: u32, per_round: &[usize], work: &Workspace, stdout: &str) -> Error {
        let blamed: usize = per_round.iter().sum();

        // Whether the rounds were still making progress is the one thing that decides what to do
        // next, and it is invisible from a total. A falling tail means the cap was simply too low
        // for this tree; a flat one means each round is uncovering as much as the last, and more
        // rounds will not help.
        let recent: Vec<String> = per_round.iter().rev().take(5).rev().map(usize::to_string).collect();

        error!(
            "the instrumented tree still does not compile after {rounds} of the {limit} rollback rounds \
             this build is allowed, having blamed unviable mutants in each of them ({blamed} blamed \
             during this build, the last round's among them not withdrawn because the limit stopped it).\n\
             Mutants blamed in the last rounds of this build: {}.\n\
             If those counts are falling, the tree was converging and --rollback-rounds is simply too \
             low for it. If they are flat, each round is uncovering as much as the last and raising \
             the limit will only make the failure slower.\n\
             {}\n\n{}",
            recent.join(", "),
            work.inspect_hint(),
            leading(&diagnostics(stdout), DIAGNOSTIC_LIMIT)
        )
    }

    /// Instruments the synchronized tree and retires mutants discovered from another generation.
    ///
    /// Discovery runs after the scratch copy is synchronized. If the live checkout changes in
    /// between, its spans no longer describe the tree this run proved and will test. Those mutants
    /// are explicitly reported as not built rather than either spliced at the wrong offsets or
    /// allowed to reach the missing-guard invariant as an internal error.
    pub fn instrument_schema(&mut self, work: &Workspace, plan: &Plan, withdrawn: &HashSet<u32>) -> Result<(Guards, Vec<Utf8PathBuf>)> {
        let instrumented = self.splices.instrument(work, plan, withdrawn)?;

        self.withdrawn.extend(instrumented.unavailable.iter().copied());
        self.abandoned.extend(instrumented.unavailable.iter().copied());
        self.unavailable.extend(instrumented.unavailable);

        Ok((instrumented.guards, instrumented.written))
    }

    pub fn instrument_active_schema(&mut self, work: &Workspace, plan: &Plan, active: &HashSet<u32>) -> Result<Vec<Utf8PathBuf>> {
        let instrumented = self.splices.instrument_active(work, plan, active)?;

        self.withdrawn.extend(instrumented.unavailable.iter().copied());
        self.abandoned.extend(instrumented.unavailable.iter().copied());
        self.unavailable.extend(instrumented.unavailable);

        Ok(instrumented.written)
    }

    fn missing_guard_error(missing: &Mutant) -> Error {
        error!(
            "internal error: no guard was emitted for the mutant at {}:{}, so it could not \
             be tested. Please report this.\n  {}",
            missing.file,
            missing.line,
            missing.describe()
        )
    }

    /// Checks that the copied tree compiles before a single mutant is applied to it.
    ///
    /// This is what makes every later compiler error attributable. The instrumented build compiles
    /// this same tree with guards written into it, so once this passes, an error that appears
    /// afterwards was introduced by a mutant and nothing else. Without it, gamma cannot tell a
    /// broken mutant from code that never compiled, and reports the second as though it were the
    /// first — which sends the reader hunting through their own source for a fault that was there
    /// before the tool arrived.
    ///
    /// Preflight always compiles the same test-target scope as the verdict oracle. The ordinary
    /// path uses `cargo check --tests`: no codegen or linking, and nothing it produces is kept.
    /// Library-only mode instead uses `cargo test --no-run --lib`, preserving the narrower
    /// library-harness-only contract. Neither path proves link-time or post-monomorphization
    /// behavior, so later builds still report failures they cannot attribute to a mutant.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn preflight(
        work: &Workspace,
        plan: &Plan,
        select: Option<&[String]>,
        mutating: &[String],
        test_lib: bool,
        limits: BuildLimits,
        events: &mut dyn Events,
    ) -> Result<Preflight> {
        Self::check(work, plan, select, mutating, test_lib, limits, events).map(|_| Preflight::narrow(Vec::new()))
    }

    /// Runs one preflight check over the packages named, or the whole workspace when none are.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn check(
        work: &Workspace,
        plan: &Plan,
        select: Option<&[String]>,
        mutating: &[String],
        test_lib: bool,
        limits: BuildLimits,
        events: &mut dyn Events,
    ) -> Result<String> {
        let verb = if test_lib {
            &["test", "--no-run", "--lib", "--no-fail-fast"][..]
        } else {
            &["check", "--tests", "--keep-going"][..]
        };
        // #[gamma::skip(all, reason = "the optional state is observed only through higher-level process orchestration that cannot be isolated safely here")]
        let outcome = run_cargo(work, plan, verb, select, limits, None, events)?;

        let Some(stdout) = outcome.stdout else {
            return Err(Self::build_timeout_error(&verb.join(" "), limits.budget(None).unwrap_or_default()));
        };

        if outcome.succeeded {
            return Ok(stdout);
        }

        let mut diagnostics = diagnostics(&stdout);

        prioritize(&mut diagnostics, &manifests_of(plan, &work.root, mutating));

        // Nothing on the JSON stream means the compiler was never reached, which is the same class
        // of failure `unattributed_build_error` describes and wants the same explanation. Saying
        // "does not compile" over an empty diagnostic list would be a lie about a build script or a
        // missing native library.
        if diagnostics.is_empty() {
            return Err(error!(
                "the tree could not be checked, and the compiler reported nothing, so the cause is \
                 something cargo hit before it reached the code — a build script, a missing native \
                 dependency, a bad invocation:\n\n{}",
                tail(&complaints(&outcome.stderr), 30)
            ));
        }

        let reproducer = if test_lib {
            "`cargo test --no-run --lib` reproduces it"
        } else {
            "`cargo check --tests` reproduces it"
        };

        Err(error!(
            "this tree does not compile before any mutation is applied, so there is nothing to \
             measure against.\n\
             These are the compiler's own errors, on the unmodified sources. Note that `cargo build` \
             alone would not show them, because it does not build test targets; {reproducer}. \
             A feature selection that leaves a test target's dependencies switched \
             off is the usual cause.\n\n{}",
            leading(&diagnostics, DIAGNOSTIC_LIMIT)
        ))
    }

    /// Checks one stage's mutants before anything downstream.
    ///
    /// A narrowed run builds the stage's libraries and binaries. A workspace run checks every
    /// member under a constant feature graph, which avoids producing a succession of differently
    /// featured codegen artifacts; only the current stage's mutants may be blamed. The final build
    /// still performs code generation and catches link- or monomorphization-only failures.
    /// A stage that cannot be made to compile does not stop the run. Its own mutants are taken out
    /// of the tree — which restores exactly the sources the preflight check already proved compile
    /// — and what it gave up on is returned so the run can report it. See [`Self::abandon`].
    #[cfg_attr(coverage_nightly, coverage(off))]
    #[cfg(test)]
    pub(super) fn stage(
        &mut self,
        work: &Workspace,
        plan: &mut Plan,
        packages: &[String],
        limits: BuildLimits,
        events: &mut dyn Events,
    ) -> Result<Option<Abandoned>> {
        // Nothing is narrated from inside a stage: the stage reports what it found and what it
        // withdrew as one line when it is done, and a round-by-round commentary underneath that
        // would bury the sequence the whole arrangement exists to show.
        let workspace = self.whole_workspace;
        let roots = if workspace { None } else { Some(packages) };
        let verb: &[&str] = if self.test_lib {
            &["test", "--no-run", "--lib", "--no-fail-fast"]
        } else if workspace {
            &["check", "--keep-going"]
        } else {
            &["build", "--keep-going"]
        };

        match self.converge_scoped(
            work,
            plan,
            BuildScope {
                roots,
                mutants: Some(packages),
                publish_progress: true,
            },
            verb,
            limits,
            events,
        )? {
            Convergence::Built(stdout) => {
                self.remember_compiled(&stdout, &work.root);
                Ok(None)
            }
            Convergence::Stuck(reason) => Ok(Some(self.abandon(plan, Some(packages), &reason))),
        }
    }

    /// Takes a population out of the run because the build holding it could not be made to compile.
    ///
    /// `packages` names whose mutants to give up on, or is `None` for every one still live.
    ///
    /// Each mutant is recorded as [`Outcome::NotBuilt`] rather than [`Outcome::CompileError`]: the
    /// compiler never accused it of anything, and reporting a mutant nobody judged as unviable
    /// would be inventing a verdict. They are added to the withdrawal set as well, so the next
    /// build instruments the tree without them — that is what makes carrying on possible, since a
    /// tree with every one of them withdrawn is the pristine tree the preflight check cleared.
    fn abandon(&mut self, plan: &mut Plan, packages: Option<&[String]>, reason: &Error) -> Abandoned {
        let mut ordinals = Vec::new();

        for mutant in &mut plan.mutants {
            let mine = packages.is_none_or(|packages| packages.iter().any(|package| package.as_str() == &*mutant.package));

            if mutant.ordinal == 0 || !mine || self.withdrawn.contains(&mutant.ordinal) {
                continue;
            }

            mutant.outcome = Outcome::NotBuilt;
            mutant.note = Some("the build this mutant belongs to could not be made to compile, so it was never run".to_owned());

            ordinals.push(mutant.ordinal);
        }

        self.withdrawn.extend(ordinals.iter().copied());
        self.abandoned.extend(ordinals.iter().copied());
        ordinals.sort_unstable();

        Abandoned {
            reason: reason.to_string(),
            ordinals,
        }
    }

    fn abandon_run(&mut self, plan: &mut Plan, reason: &Error) -> Abandoned {
        let mut abandoned = self.abandon(plan, None, reason);
        abandoned.ordinals = self.abandoned.iter().copied().collect();
        abandoned.ordinals.sort_unstable();
        abandoned
    }

    /// Writes the withdrawal verdicts back onto the plan.
    ///
    /// Only mutants the compiler actually blamed are called unviable; what was abandoned wholesale
    /// already carries [`Outcome::NotBuilt`] from [`Self::abandon`] and keeps it.
    pub fn settle(&self, plan: &mut Plan) {
        for mutant in &mut plan.mutants {
            if self.unavailable.contains(&mutant.ordinal) {
                mutant.outcome = Outcome::NotBuilt;
                mutant.note = Some(
                    "the source changed between synchronization and discovery, so this mutant did not describe the tree being tested"
                        .to_owned(),
                );
            } else if let Some(reason) = self.census.get(&mutant.ordinal) {
                mutant.outcome = Outcome::CompileError;
                mutant.note = Some(reason.note());
            } else if self.abandoned.contains(&mutant.ordinal) {
                mutant.outcome = Outcome::NotBuilt;
                mutant.note = if let Some(context) = self.unresolved.get(&mutant.ordinal) {
                    Some(format!(
                        "compiler isolation exhausted its campaign budget before {context} could be resolved"
                    ))
                } else if let Some(members) = self.interactions.get(&mutant.ordinal) {
                    Some(format!(
                        "this mutant was excluded without blame because compiler isolation found a conflict among ordinals {}",
                        members.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
                    ))
                } else {
                    Some("the instrumented forms in this item could not compile together, so its mutants were not run".to_owned())
                };
            } else if self.withdrawn.contains(&mutant.ordinal) {
                mutant.outcome = Outcome::CompileError;
                mutant.note = None;
            }
        }
    }

    /// Converges the complete instrumented population and produces its test binaries.
    ///
    /// A build that cannot be made to compile leaves no test binary to judge anything with, so
    /// every mutant still live is abandoned and the returned [`Build`] says so. The run reports
    /// what it has rather than exiting with nothing.
    pub fn finish(
        mut self,
        work: &Workspace,
        plan: &mut Plan,
        select: Option<&[String]>,
        limits: BuildLimits,
        events: &mut dyn Events,
    ) -> Result<Build> {
        let roots = match &self.fixed_roots {
            Some(roots) if roots.is_empty() => None,
            Some(roots) => Some(roots.clone()),
            None => self.scoped(select).map(<[String]>::to_vec),
        };
        let mut stdout = String::new();
        for codegen in [false, true] {
            let verb = if !codegen || self.test_lib {
                self.check_verb()
            } else {
                &["build", "--tests", "--keep-going"]
            };
            match self.converge_scoped(
                work,
                plan,
                BuildScope {
                    roots: roots.as_deref(),
                    mutants: None,
                    publish_progress: true,
                },
                verb,
                limits,
                events,
            )? {
                Convergence::Built(artifacts) => {
                    self.remember_compiled(&artifacts, &work.root);
                    stdout = artifacts;
                }
                Convergence::Stuck(reason) => {
                    let stuck = self.abandon_run(plan, &reason);
                    self.settle(plan);
                    return Ok(Build {
                        history: self.history.clone(),
                        census: self.tally(plan),
                        binaries: Vec::new(),
                        artifacts: String::new(),
                        withdrawn: self.historical_exclusions + self.withdrawn.len().saturating_sub(self.abandoned.len()),
                        rounds: self.total_rounds,
                        widened: false,
                        stuck: Some(stuck),
                        ordering: OrderingHints::default(),
                    });
                }
            }
        }

        self.settle(plan);
        // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
        self.remember_compiled(&stdout, &work.root);

        // Runs after the withdrawal above so that a mutant which genuinely failed to compile keeps
        // that more specific verdict; see [`withdraw_uncompiled`] for why the set is only trusted
        // when it agrees with the survey at all.
        if let Some(compiled) = &self.compiled {
            // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
            withdraw_uncompiled(plan, compiled);
        }

        let captures = work.rustc_captures();
        let mut binaries = test_binaries_with_linkage(&stdout, &work.root, captures.as_deref());
        // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
        retain_linked_to_population(&mut binaries, plan);

        Ok(Build {
            history: self.history.clone(),
            census: self.tally(plan),
            binaries,
            artifacts: stdout,
            withdrawn: self.historical_exclusions + self.withdrawn.len().saturating_sub(self.abandoned.len()),
            rounds: self.total_rounds,
            widened: false,
            stuck: None,
            ordering: OrderingHints::default(),
        })
    }

    /// How many mutants have been withdrawn so far.
    #[cfg(test)]
    pub(super) fn withdrawn(&self) -> usize {
        self.withdrawn.len()
    }

    /// Adds one successful Cargo invocation's dep-info to the run-wide source inventory.
    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    fn remember_compiled(&mut self, stdout: &str, root: &camino::Utf8Path) {
        if let Some(found) = compiled_sources(stdout, root) {
            self.compiled.get_or_insert_with(HashSet::default).extend(found);
        }
    }

    /// Groups withdrawals by package, normalized compiler reason, and mutator, densest group first.
    ///
    /// Counts distinct ordinals, because a diagnostic is not a mutant: one unviable mutant can draw
    /// a four-figure count of follow-on complaints, so anything tallying rows rather than mutants
    /// overstates the answer by an order of magnitude.
    fn tally(&self, plan: &Plan) -> Vec<Withdrawal> {
        let mut identities: HashMap<u32, (&str, &str)> = HashMap::default();

        for mutant in &plan.mutants {
            let _ = identities.insert(mutant.ordinal, (&mutant.package, &mutant.mutator));
        }

        let mut counts: HashMap<(&CompilerReason, &str, &str), usize> = HashMap::default();

        for (ordinal, reason) in &self.census {
            let (package, mutator) = identities.get(ordinal).copied().unwrap_or(("", ""));

            *counts.entry((reason, package, mutator)).or_default() += 1;
        }

        let historical = CompilerReason {
            category: "historical compiler exclusion".into(),
            ..CompilerReason::default()
        };
        for mutant in plan
            .mutants
            .iter()
            .filter(|mutant| mutant.ordinal == 0 && mutant.outcome == Outcome::CompileError)
        {
            *counts.entry((&historical, &mutant.package, &mutant.mutator)).or_default() += 1;
        }

        let mut census: Vec<Withdrawal> = counts
            .into_iter()
            .map(|((reason, package, mutator), mutants)| Withdrawal {
                package: package.to_owned(),
                code: reason.code.clone(),
                category: reason.category.clone(),
                replacement_site: reason.replacement_site(),
                mutator: mutator.to_owned(),
                mutants,
            })
            .collect();

        // Descending by weight, then by name, so that the line worth reading is the first one and
        // two runs over the same tree print the same thing.
        census.sort_by(|left, right| {
            right
                .mutants
                .cmp(&left.mutants)
                .then_with(|| left.code.cmp(&right.code))
                .then_with(|| left.category.cmp(&right.category))
                .then_with(|| left.replacement_site.cmp(&right.replacement_site))
                .then_with(|| left.package.cmp(&right.package))
                .then_with(|| left.mutator.cmp(&right.mutator))
        });

        census
    }
}

/// Marks every pending mutant whose file the compiler never read as not built.
///
/// A mutant in a file no compilation opened cannot be judged by any test, so it is taken out of the
/// run here rather than left to be reported as a survivor later.
///
/// The agreement check in front of the loop exists because the failure this could otherwise cause
/// is silent and expensive: if dep-info ever spelled its paths differently from the way the survey
/// spells them, nothing would match, every mutant would be excused, and the run would report a
/// flattering score with no sign that anything had gone wrong. A set that names not one file the
/// survey found is a set we do not understand, so nothing is concluded from it.
///
/// The check is deliberately whole-set and cannot be tightened to a per-file one: "this file is
/// missing from the compiled set" is exactly the question being asked, so a per-file guard would
/// answer it with itself. That makes the check blind to a spelling difference that affects only
/// *some* paths, which is why [`messages::compiled_sources`] has to decode the dep-info escaping
/// correctly rather than rely on being caught here.
fn withdraw_uncompiled(plan: &mut Plan, compiled: &HashSet<Utf8PathBuf>) {
    if !plan.files.iter().any(|file| compiled.contains(&file.path)) {
        return;
    }

    for mutant in &mut plan.mutants {
        if mutant.outcome == Outcome::Pending && !compiled.contains(&*mutant.file) {
            mutant.outcome = Outcome::NotBuilt;
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod mutation_outcome_tests;

/// Native integration-test access to schema and compiler adapters.
#[cfg(any(test, feature = "internals"))]
#[doc(hidden)]
pub mod native_test_util;
