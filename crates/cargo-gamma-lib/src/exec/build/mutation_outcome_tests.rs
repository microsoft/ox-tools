// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use compact_str::CompactString;

use super::*;
use crate::ops::collect::Shape;

fn plan(mutants: Vec<Mutant>) -> Plan {
    Plan {
        root: Utf8PathBuf::from("root"),
        files: Vec::new(),
        mutants,
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

fn mutant(ordinal: u32, package: &str, item: &str) -> Mutant {
    Mutant {
        id: format!("{ordinal:012x}").into(),
        ordinal,
        file: Arc::from(Utf8Path::new("src/lib.rs")),
        package: Arc::from(package),
        span: 0..1,
        line: ordinal as usize,
        end_line: ordinal as usize,
        column: 1,
        mutator: Arc::from("literal.bool_flip"),
        item_path: Arc::from(item),
        occurrence: 0,
        replacement_index: 0,
        original: CompactString::new("true"),
        replacement: CompactString::new("false"),
        shape: Shape::Expr,
        outcome: Outcome::Pending,
        suppression: None,
        expectation: None,
        test_timeout_multiplier: None,
        elapsed_ms: 0,
        killed_by: None,
        note: None,
    }
}

#[test]
fn converger_state_updates_are_exact_and_repeatable() {
    let mut converger = Converger::default();
    converger.target_discovery("artifact stream".to_owned());
    assert_eq!(converger.target_discovery.as_deref(), Some("artifact stream"));

    converger.rounds = 9;
    converger.per_round.extend([3, 2, 1]);
    converger.first_round = Some(Duration::from_secs(2));
    converger.begin_convergence();
    assert_eq!(converger.rounds, 0);
    assert!(converger.per_round.is_empty());
    assert_eq!(converger.first_round, None);

    let preflight = Preflight::narrow(vec!["dropped".to_owned()], "discovery".to_owned());
    assert!(!preflight.whole_workspace);
    assert_eq!(preflight.dropped, ["dropped"]);
    assert_eq!(preflight.discovery, "discovery");
}

#[test]
fn build_rounds_attribute_withdrawals_to_packages_without_dividing_elapsed_time() {
    let plan = plan(vec![
        mutant(1, "alpha", "alpha::one"),
        mutant(2, "beta", "beta::one"),
        mutant(3, "alpha", "alpha::two"),
    ]);

    let round = build_round(Duration::from_secs(7), &plan, [3, 1]);

    assert_eq!(round.elapsed, Duration::from_secs(7));
    assert_eq!(round.withdrew, 2);
    assert_eq!(round.packages.len(), 1);
    assert_eq!(&*round.packages[0].package, "alpha");
    assert_eq!(round.packages[0].mutants, 2);
}

#[test]
fn probe_sets_respect_sentinel_withdrawal_package_hint_and_order() {
    let sentinel = mutant(0, "a", "sentinel");
    let first = mutant(3, "a", "one");
    let second = mutant(1, "a", "two");
    let other = mutant(2, "b", "three");
    let mut converger = Converger::guided(HashSet::from_iter([first.id.clone(), second.id.clone(), other.id.clone()]));
    let _ = converger.withdrawn.insert(3);
    let _ = converger.probed.insert(2);
    let plan = plan(vec![sentinel, first, second, other]);

    let selected = vec!["a".to_owned()];
    let (candidates, deferred) = converger.probe_sets(&plan, Some(&selected));
    assert_eq!(candidates, [1]);
    assert_eq!(deferred, HashSet::from_iter([2, 3]));
}

#[test]
fn abandonment_and_settlement_touch_only_the_requested_live_population() {
    let mut plan = plan(vec![
        mutant(0, "a", "sentinel"),
        mutant(3, "a", "one"),
        mutant(1, "b", "two"),
        mutant(2, "a", "three"),
    ]);
    let mut converger = Converger::default();
    let _ = converger.withdrawn.insert(3);
    let packages = vec!["a".to_owned()];
    let abandoned = converger.abandon(&mut plan, Some(&packages), &error!("reason"));

    assert_eq!(abandoned.reason, "reason");
    assert_eq!(abandoned.ordinals, [2]);
    assert_eq!(plan.mutants[2].outcome, Outcome::Pending);
    assert_eq!(plan.mutants[3].outcome, Outcome::NotBuilt);
    assert!(
        plan.mutants[3]
            .note
            .as_deref()
            .is_some_and(|note| note.contains("could not be made to compile"))
    );

    converger.settle(&mut plan);
    assert_eq!(plan.mutants[1].outcome, Outcome::CompileError);
    assert_eq!(plan.mutants[3].outcome, Outcome::NotBuilt);
    assert!(
        plan.mutants[3]
            .note
            .as_deref()
            .is_some_and(|note| note.contains("could not compile together"))
    );
}

#[test]
fn final_abandonment_reports_population_already_excluded_by_isolation() {
    let mut plan = plan(vec![mutant(1, "a", "one"), mutant(2, "a", "two")]);
    let mut converger = Converger::default();
    let _ = converger.withdrawn.insert(1);
    let _ = converger.abandoned.insert(1);
    let _ = converger.unresolved.insert(1, "subject (lib)".to_owned());

    let abandoned = converger.abandon_run(&mut plan, &error!("build failed"));
    converger.settle(&mut plan);

    assert_eq!(abandoned.ordinals, [1, 2]);
    assert!(plan.mutants.iter().all(|mutant| mutant.outcome == Outcome::NotBuilt));
}

#[test]
fn direct_compiler_blame_dominates_overlapping_nonverdict_isolation_state() {
    let mut plan = plan(vec![mutant(1, "a", "one")]);
    let mut converger = Converger::default();
    let _ = converger.withdrawn.insert(1);
    let _ = converger.abandoned.insert(1);
    let _ = converger.unresolved.insert(1, "subject (lib)".to_owned());
    let _ = converger.interactions.insert(1, vec![1, 2]);
    let _ = converger.census.insert(1, CompilerReason::isolated());

    converger.settle(&mut plan);

    assert_eq!(plan.mutants[0].outcome, Outcome::CompileError);
    assert!(plan.mutants[0].note.as_deref().is_some_and(|note| note.contains("isolated")));
}

#[test]
fn isolation_bisects_left_right_and_interacting_items() {
    let directory = crate::testing::workdir("build-isolation-outcomes-");
    let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 root");
    let work = Workspace::adopt(root.clone(), root.join("target"));
    let plan = plan(vec![
        mutant(3, "a", "second"),
        mutant(1, "a", "first"),
        mutant(4, "a", "third"),
        mutant(2, "a", "first"),
    ]);

    for (oracle, expected) in [
        (
            (|active: &HashSet<u32>| Some(active.contains(&1))) as SubsetOracle,
            Isolation::Blamed(vec![1]),
        ),
        (
            (|active: &HashSet<u32>| Some(active.contains(&4))) as SubsetOracle,
            Isolation::Blamed(vec![4]),
        ),
        (
            (|active: &HashSet<u32>| Some(active.contains(&1) && active.contains(&3))) as SubsetOracle,
            Isolation::Interaction {
                excluded: 1,
                members: vec![1, 3],
            },
        ),
    ] {
        let mut converger = Converger {
            subset_oracle: Some(oracle),
            ..Converger::default()
        };
        let isolated = converger
            .isolate(
                &work,
                &plan,
                None,
                &["build"],
                BuildLimits::default(),
                &mut crate::testing::Recorder::default(),
            )
            .expect("the pure oracle cannot fail")
            .expect("the population has a failing subset");

        assert_eq!(format!("{isolated:?}"), format!("{expected:?}"));
    }
}

#[test]
fn isolation_refuses_empty_pristine_and_indeterminate_populations() {
    let directory = crate::testing::workdir("build-isolation-negative-");
    let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 root");
    let work = Workspace::adopt(root.clone(), root.join("target"));

    for (population, oracle) in [
        (Vec::new(), (|_active: &HashSet<u32>| Some(true)) as SubsetOracle),
        (vec![mutant(1, "a", "one")], (|_active: &HashSet<u32>| Some(true)) as SubsetOracle),
        (vec![mutant(1, "a", "one")], (|_active: &HashSet<u32>| None) as SubsetOracle),
    ] {
        let mut converger = Converger {
            subset_oracle: Some(oracle),
            ..Converger::default()
        };
        let isolated = converger
            .isolate(
                &work,
                &plan(population),
                None,
                &["build"],
                BuildLimits::default(),
                &mut crate::testing::Recorder::default(),
            )
            .expect("the oracle cannot fail");
        assert_eq!(isolated.map(|value| format!("{value:?}")), None);
    }
}

#[test]
fn tally_groups_distinct_ordinals_and_orders_dense_groups_first() {
    let reason = |code: &str| CompilerReason {
        code: code.to_owned(),
        ..CompilerReason::default()
    };
    let plan = plan(vec![
        mutant(1, "a", "one"),
        Mutant {
            ordinal: 2,
            mutator: Arc::from("arith.add_to_sub"),
            ..mutant(2, "a", "two")
        },
        Mutant {
            ordinal: 3,
            mutator: Arc::from("arith.add_to_sub"),
            ..mutant(3, "a", "three")
        },
    ]);
    let converger = Converger {
        census: HashMap::from_iter([
            (1, reason("E0308")),
            (2, reason("E0277")),
            (3, reason("E0277")),
            (99, reason("E9999")),
        ]),
        ..Converger::default()
    };

    let tally = converger.tally(&plan);
    assert_eq!(tally[0].mutants, 2);
    assert_eq!(tally[0].code, "E0277");
    assert_eq!(tally[0].mutator, "arith.add_to_sub");
    assert_eq!(tally[1].mutants, 1);
    assert_eq!(tally[1].code, "E0308");
    assert_eq!(tally[2].mutator, "");
}

#[test]
fn rollback_diagnostic_keeps_only_the_five_most_recent_rounds() {
    let directory = crate::testing::workdir("build-rollback-diagnostic-");
    let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 root");
    let work = Workspace::adopt(root.clone(), root.join("target"));
    let error = Converger::rollback_limit_error(7, 7, &[9, 8, 7, 6, 5, 4, 3], &work, "");
    let text = error.to_string();

    assert!(text.contains("42 blamed during this build"), "{text}");
    assert!(text.contains("7, 6, 5, 4, 3"), "{text}");
    assert!(!text.contains("9, 8, 7, 6, 5"), "{text}");
}
