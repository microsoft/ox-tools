// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use camino::Utf8PathBuf;

use super::*;
use crate::exec::nextest::Harness;
use crate::exec::sweep::tests::{NARROW, one_mutant_plan, ranked_binaries, ranked_sweep};
use crate::exec::verdict::{with_binary_runner, with_shared_runner};

fn workspace() -> Workspace {
    Workspace::adopt(r"C:\gamma-stage-fixture".into(), r"C:\gamma-stage-fixture\target".into())
}

fn scripted_harness(binaries: Vec<TestBinary>) -> (Workspace, Plan, Vec<TestBinary>) {
    let work = workspace();
    let plan = one_mutant_plan(work.root.clone());
    (work, plan, binaries)
}

fn passed() -> BinaryRun {
    BinaryRun {
        verdict: Verdict::Passed,
        reach: ReachObservation::Reached,
    }
}

fn step(
    work: &Workspace,
    binaries: &[TestBinary],
    hint: Option<&Killer>,
    candidates: &[Candidate],
    phase: Stage,
    state: &mut ProbeState,
    tally: &Tally,
) -> Option<Judgement> {
    judge(
        work,
        7,
        &binaries.iter().collect::<Vec<_>>(),
        hint,
        candidates,
        None,
        None,
        None,
        ranked_sweep(),
        tally,
        phase,
        state,
    )
}

fn binaries(count: usize) -> Vec<TestBinary> {
    (0..count)
        .map(|index| TestBinary {
            target: format!("binary-{index}"),
            ..ranked_binaries()[0].clone()
        })
        .collect()
}

fn learning_probe(
    learning: &FileLearning,
    binary: &TestBinary,
    only: Only<'static>,
    verdict: Verdict,
    reach: ReachObservation,
    kind: Option<ProbeKind>,
) {
    let mut scripted = Some(BinaryRun { verdict, reach });
    let mut observed = false;
    let work = workspace();
    let reachable = [binary];
    let tally = Tally::default();
    with_binary_runner(
        move |_, attempt, _| {
            assert_eq!(attempt.active, Some(7));
            assert_eq!(attempt.only, only);
            scripted.take().expect("one probe must consume exactly one scripted result")
        },
        || {
            let execution = Execution {
                work: &work,
                ordinal: 7,
                reachable: &reachable,
                learning: Some((learning, "function")),
                negative: None,
                timeout_multiplier: None,
                sweep: ranked_sweep(),
                tally: &tally,
            };
            let Ok(result) = execution.execute(0, only, kind, 1) else {
                panic!("the scripted result must not abandon containment")
            };
            assert_eq!(result.reach, reach);
            observed = true;
        },
    );
    assert!(observed);
    assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
}

fn feedback<T>(candidates: &[RankedCandidate<T>]) -> Vec<(u32, u32, u32, u32)> {
    candidates
        .iter()
        .map(|candidate| (candidate.hits, candidate.misses, candidate.canonical_passes, candidate.samples))
        .collect()
}

#[test]
fn whole_binary_feedback_separates_reach_observations_passes_failures_and_timeouts() {
    use ReachObservation::{NotReached, Reached};

    let binary = &binaries(1)[0];
    for (verdict, reach, item, file, canonical) in [
        (Verdict::Passed, Reached, vec![(0, 1, 1, 2)], vec![(0, 0, 1, 1)], 1),
        (Verdict::Passed, NotReached, vec![], vec![], 1),
        (
            Verdict::Failed(Some("caught".into())),
            Reached,
            vec![(1, 0, 0, 2)],
            vec![(0, 0, 0, 1)],
            0,
        ),
        (Verdict::Failed(Some("caught".into())), NotReached, vec![], vec![], 0),
        (Verdict::TimedOut, Reached, vec![(0, 0, 0, 1)], vec![(0, 0, 0, 1)], 0),
        (Verdict::TimedOut, NotReached, vec![], vec![], 0),
    ] {
        let learning = FileLearning::new();
        learning.publish("function", &killed_by(killer_for(binary, "candidate")));
        learning_probe(&learning, binary, Only::All, verdict, reach, Some(ProbeKind::Item));
        let state = learning.locked();
        assert_eq!(feedback(&state.items["function"].reached), item);
        assert_eq!(feedback(&state.reached_file), file);
        assert_eq!(feedback(&state.binaries), [(0, 0, canonical, 0)]);
        assert_eq!(feedback(&state.items["function"].exact), [(0, 0, 0, 0)]);
    }
}

#[test]
fn full_binary_inference_updates_only_the_tier_that_selected_it() {
    let binary = &binaries(1)[0];
    for (kind, item, file, binary_feedback) in [
        (None, (0, 0, 1, 1), (0, 0, 1, 1), (0, 0, 1, 0)),
        (Some(ProbeKind::Item), (0, 1, 1, 2), (0, 0, 1, 1), (0, 0, 1, 0)),
        (Some(ProbeKind::Reach), (0, 0, 1, 1), (0, 1, 1, 2), (0, 0, 1, 0)),
        (Some(ProbeKind::File), (0, 0, 1, 1), (0, 0, 1, 1), (0, 1, 1, 1)),
    ] {
        let learning = FileLearning::new();
        learning.publish("function", &killed_by(killer_for(binary, "candidate")));
        learning_probe(&learning, binary, Only::All, Verdict::Passed, ReachObservation::Reached, kind);
        let state = learning.locked();
        assert_eq!(feedback(&state.items["function"].reached), [item]);
        assert_eq!(feedback(&state.reached_file), [file]);
        assert_eq!(feedback(&state.binaries), [binary_feedback]);
    }
}

#[test]
fn individual_probe_feedback_requires_a_conclusive_result_for_the_selected_test() {
    let binary = &binaries(1)[0];
    for (verdict, expected) in [
        (Verdict::Passed, (0, 1, 0, 1)),
        (Verdict::Failed(Some("candidate".into())), (1, 0, 0, 1)),
        (Verdict::Failed(Some("unrelated".into())), (0, 0, 0, 0)),
        (Verdict::Failed(None), (0, 0, 0, 0)),
        (Verdict::TimedOut, (0, 0, 0, 0)),
    ] {
        let learning = FileLearning::new();
        learning.publish("function", &killed_by(killer_for(binary, "candidate")));
        learning_probe(
            &learning,
            binary,
            Only::One("candidate"),
            verdict,
            ReachObservation::Reached,
            Some(ProbeKind::Item),
        );
        let state = learning.locked();
        assert_eq!(feedback(&state.items["function"].exact), [expected]);
        assert_eq!(feedback(&state.items["function"].reached), []);
        assert_eq!(feedback(&state.reached_file), []);
        assert_eq!(feedback(&state.binaries), [(0, 0, 0, 0)]);
    }
}

#[test]
fn sweep_has_four_draining_phases() {
    assert_eq!(Stage::sequence(), [Stage::Exact, Stage::Inferred, Stage::Binaries, Stage::Fallback]);
}

#[test]
fn staged_later_exact_kill_settles_without_earlier_binaries() {
    let binaries = ranked_binaries();
    let hint = killer_for(&binaries[1], "killer");
    let judged = with_binary_runner(
        |binary, attempt, confirm| {
            assert_eq!(binary.target, "later");
            assert_eq!(attempt.only, Only::One("killer"));
            assert!(confirm);
            BinaryRun {
                verdict: Verdict::Failed(Some("killer".into())),
                reach: ReachObservation::Reached,
            }
        },
        || {
            step(
                &workspace(),
                &binaries,
                Some(&hint),
                &[],
                Stage::Exact,
                &mut ProbeState::default(),
                &Tally::default(),
            )
        },
    );
    assert!(matches!(judged, Some(Judgement::Reached(Outcome::Killed, Some(found), None)) if found == hint));
}

#[test]
fn staged_libtest_runs_six_ranked_cases_individually_and_skips_passing_duplicates() {
    let binaries = binaries(4);
    let hint = killer_for(&binaries[1], "passed-exact");
    let candidates = [
        (1, "passed-exact"),
        (2, "best"),
        (0, "second"),
        (3, "third"),
        (2, "another"),
        (0, "tail"),
        (2, "best"),
        (1, "sixth"),
        (3, "after-six"),
        (2, "also-after-six"),
    ]
    .map(|(index, test)| Candidate::Exact(killer_for(&binaries[index], test)));
    let mut calls = Vec::new();
    let mut state = ProbeState::default();
    let tally = Tally::default();
    with_binary_runner(
        move |binary, attempt, _| {
            calls.push((binary.target.clone(), format!("{:?}", attempt.only)));
            if calls.len() == 7 {
                assert_eq!(
                    calls,
                    [
                        ("binary-1".into(), "One(\"passed-exact\")".into()),
                        ("binary-0".into(), "One(\"second\")".into()),
                        ("binary-0".into(), "One(\"tail\")".into()),
                        ("binary-1".into(), "One(\"sixth\")".into()),
                        ("binary-2".into(), "One(\"also-after-six\")".into()),
                        ("binary-2".into(), "One(\"another\")".into()),
                        ("binary-2".into(), "One(\"best\")".into()),
                    ]
                );
            }
            passed()
        },
        || {
            assert!(step(&workspace(), &binaries, Some(&hint), &[], Stage::Exact, &mut state, &tally).is_none());
            assert!(step(&workspace(), &binaries, None, &candidates, Stage::Inferred, &mut state, &tally).is_none());
        },
    );
    assert_eq!(tally.launches.load(Ordering::Relaxed), 7);
    assert_eq!(state.passed_tests.len(), 7);
    assert_eq!(tally.inferred_probes.load(Ordering::Relaxed), 6);
    assert_eq!(tally.accounting.lock().unwrap().duplicate_launches_avoided, 1);
}

#[test]
fn staged_case_cap_ignores_unreachable_filtered_duplicate_and_binary_candidates() {
    for nextest in [false, true] {
        let mut work = workspace();
        work.set_test_args(vec!["test-".into()]);
        if nextest {
            work.set_runner(Harness::fake(&[]));
        }
        let binaries = binaries(1);
        let mut unreachable = killer_for(&binaries[0], "unreachable");
        unreachable.package = "not-selected".into();
        let mut candidates = vec![
            Candidate::FileBinary(BinaryIdentity::from_binary(&binaries[0])),
            Candidate::Exact(unreachable),
            Candidate::Exact(killer_for(&binaries[0], "filtered")),
        ];
        for index in 0..9 {
            let candidate = Candidate::Exact(killer_for(&binaries[0], &format!("test-{index}")));
            candidates.push(candidate.clone());
            candidates.push(candidate);
        }
        let tally = Tally::default();
        let mut state = ProbeState::default();
        let mut index = 0;
        with_binary_runner(
            move |_, attempt, _| {
                assert!(index < 6);
                assert_eq!(attempt.only, Only::One(&format!("test-{index}")));
                index += 1;
                passed()
            },
            || {
                assert!(step(&work, &binaries, None, &candidates, Stage::Inferred, &mut state, &tally).is_none());
            },
        );
        assert_eq!(tally.launches.load(Ordering::Relaxed), 6);
        assert_eq!(state.passed_tests.len(), 6);
    }
}

#[test]
fn staged_six_case_cap_keeps_every_candidate_for_full_binary_inference() {
    let binaries = binaries(9);
    let candidates: Vec<_> = binaries
        .iter()
        .enumerate()
        .map(|(index, binary)| Candidate::Exact(killer_for(binary, &format!("test-{index}"))))
        .collect();
    let tally = Tally::default();
    let mut state = ProbeState::default();
    let mut calls = 0;
    let judged = with_binary_runner(
        move |binary, attempt, _| {
            if calls < 6 {
                assert_eq!(binary.target, format!("binary-{calls}"));
                assert_eq!(attempt.only, Only::One(&format!("test-{calls}")));
            } else {
                assert_eq!(binary.target, format!("binary-{}", calls - 6));
                assert_eq!(attempt.only, Only::All);
            }
            calls += 1;
            if calls == 15 {
                BinaryRun {
                    verdict: Verdict::Failed(Some("test-8".into())),
                    reach: ReachObservation::Reached,
                }
            } else {
                passed()
            }
        },
        || {
            assert!(step(&workspace(), &binaries, None, &candidates, Stage::Inferred, &mut state, &tally).is_none());
            step(&workspace(), &binaries, None, &candidates, Stage::Binaries, &mut state, &tally)
        },
    );
    assert!(matches!(judged, Some(Judgement::Reached(Outcome::Killed, Some(found), None)) if found.test == "test-8"));
    assert_eq!(tally.launches.load(Ordering::Relaxed), 15);
    assert_eq!(state.passed_tests.len(), 6);
}

#[test]
fn staged_nextest_runs_six_ranked_individual_cases_without_a_binary_group_limit() {
    let mut work = workspace();
    work.set_runner(Harness::fake(&[]));
    let binaries = binaries(4);
    let candidates = (0..9)
        .map(|index| Candidate::Exact(killer_for(&binaries[index % 4], &format!("test-{index}"))))
        .collect::<Vec<_>>();
    let mut index = 0;
    let expected = [(0, 0), (0, 4), (0, 8), (1, 1), (1, 5), (2, 2)];
    let tally = Tally::default();
    with_binary_runner(
        move |binary, attempt, _| {
            assert!(index < 6);
            let (target, test) = expected[index];
            assert_eq!(binary.target, format!("binary-{target}"));
            assert_eq!(attempt.only, Only::One(&format!("test-{test}")));
            index += 1;
            passed()
        },
        || {
            assert!(
                step(
                    &work,
                    &binaries,
                    None,
                    &candidates,
                    Stage::Inferred,
                    &mut ProbeState::default(),
                    &tally
                )
                .is_none()
            );
        },
    );
    assert_eq!(tally.launches.load(Ordering::Relaxed), 6);
}

#[test]
fn staged_inconclusive_exact_case_can_be_retried_but_inferred_kills_settle_immediately() {
    let binaries = ranked_binaries();
    let hint = killer_for(&binaries[1], "killer");
    let candidates = [
        Candidate::Exact(hint.clone()),
        Candidate::Exact(killer_for(&binaries[0], "must-not-run")),
    ];
    let mut state = ProbeState::default();
    let tally = Tally::default();
    tally.named_kills.lock().unwrap().record(99, &killed_by(hint.clone()));
    let mut first = true;
    let judged = with_binary_runner(
        move |binary, attempt, _| {
            assert_eq!(binary.target, "later");
            let verdict = if first {
                first = false;
                assert_eq!(attempt.only, Only::One("killer"));
                Verdict::TimedOut
            } else {
                assert_eq!(attempt.only, Only::One("killer"));
                Verdict::Failed(Some("killer".into()))
            };
            BinaryRun {
                verdict,
                reach: ReachObservation::Reached,
            }
        },
        || {
            assert!(step(&workspace(), &binaries, Some(&hint), &[], Stage::Exact, &mut state, &tally).is_none());
            step(&workspace(), &binaries, None, &candidates, Stage::Inferred, &mut state, &tally)
        },
    );
    assert!(matches!(judged, Some(Judgement::Reached(Outcome::Killed, Some(found), None)) if found == hint));
    assert_eq!(tally.launches.load(Ordering::Relaxed), 2);
}

#[test]
fn staged_inferred_binary_phase_has_no_cap_and_fallback_reuses_every_full_outcome() {
    let binaries = binaries(5);
    let candidates = [3, 1, 4, 0, 3].map(|index| Candidate::FileBinary(BinaryIdentity::from_binary(&binaries[index])));
    let expected = ["binary-3", "binary-1", "binary-4", "binary-0", "binary-2"];
    let mut index = 0;
    let mut state = ProbeState::default();
    let tally = Tally::default();
    let judged = with_binary_runner(
        move |binary, attempt, _| {
            assert_eq!(attempt.only, Only::All);
            assert_eq!(binary.target, expected[index]);
            index += 1;
            passed()
        },
        || {
            assert!(step(&workspace(), &binaries, None, &candidates, Stage::Binaries, &mut state, &tally).is_none());
            assert_eq!(tally.launches.load(Ordering::Relaxed), 4);
            step(&workspace(), &binaries, None, &[], Stage::Fallback, &mut state, &tally)
        },
    );
    assert!(matches!(judged, Some(Judgement::Reached(Outcome::Survived, None, None))));
    assert_eq!(tally.launches.load(Ordering::Relaxed), 5);
    assert_eq!(tally.accounting.lock().unwrap().duplicate_launches_avoided, 4);
}

#[test]
fn staged_later_full_kill_overrides_an_earlier_timeout_without_repeating_the_timed_out_binary() {
    let binaries = ranked_binaries();
    let candidates = [Candidate::FileBinary(BinaryIdentity::from_binary(&binaries[0]))];
    let mut state = ProbeState::default();
    let tally = Tally::default();
    let judged = with_binary_runner(
        |binary, attempt, _| {
            assert_eq!(attempt.only, Only::All);
            BinaryRun {
                verdict: if binary.target == "earlier" {
                    Verdict::TimedOut
                } else {
                    Verdict::Failed(Some("killer".into()))
                },
                reach: ReachObservation::Reached,
            }
        },
        || {
            assert!(step(&workspace(), &binaries, None, &candidates, Stage::Binaries, &mut state, &tally).is_none());
            step(&workspace(), &binaries, None, &[], Stage::Fallback, &mut state, &tally)
        },
    );
    assert!(matches!(judged, Some(Judgement::Reached(Outcome::Killed, Some(found), None)) if found.target == "later"));
    assert_eq!(tally.launches.load(Ordering::Relaxed), 2);
}

#[test]
fn staged_nonpassing_full_results_do_not_become_survivors_and_do_not_block_later_kills() {
    for fail_later in [false, true] {
        let binaries = ranked_binaries();
        let candidates = binaries
            .iter()
            .map(|b| Candidate::FileBinary(BinaryIdentity::from_binary(b)))
            .collect::<Vec<_>>();
        let mut state = ProbeState::default();
        let tally = Tally::default();
        let judged = with_binary_runner(
            move |binary, _, _| BinaryRun {
                verdict: if binary.target == "earlier" {
                    Verdict::MemoryLimit { peak: Some(50), limit: 37 }
                } else if fail_later {
                    Verdict::Failed(Some("killer".into()))
                } else {
                    Verdict::Passed
                },
                reach: ReachObservation::Reached,
            },
            || {
                let inferred = step(&workspace(), &binaries, None, &candidates, Stage::Binaries, &mut state, &tally);
                inferred.or_else(|| step(&workspace(), &binaries, None, &[], Stage::Fallback, &mut state, &tally))
            },
        );
        assert!(
            matches!(judged, Some(Judgement::Reached(outcome, _, _)) if outcome == if fail_later { Outcome::Killed } else { Outcome::OutOfMemory })
        );
        assert_eq!(tally.launches.load(Ordering::Relaxed), 2);
    }
}

#[test]
fn fallback_uses_kill_counts_without_changing_nonpassing_outcome_precedence() {
    let binaries = ranked_binaries();
    let tally = Tally::default();
    tally.binary_kills.lock().unwrap().record(99, &binaries[1]);
    let mut calls = Vec::new();
    let judged = with_binary_runner(
        move |binary, attempt, _| {
            assert_eq!(attempt.only, Only::All);
            calls.push(binary.target.clone());
            if calls.len() == 2 {
                assert_eq!(calls, ["later", "earlier"]);
            }
            BinaryRun {
                verdict: if binary.target == "earlier" {
                    Verdict::MemoryLimit { peak: Some(50), limit: 37 }
                } else {
                    Verdict::TimedOut
                },
                reach: ReachObservation::Reached,
            }
        },
        || {
            step(
                &workspace(),
                &binaries,
                None,
                &[],
                Stage::Fallback,
                &mut ProbeState::default(),
                &tally,
            )
        },
    );
    assert!(matches!(judged, Some(Judgement::Reached(Outcome::OutOfMemory, None, _))));
    assert_eq!(tally.launches.load(Ordering::Relaxed), 2);
}

#[test]
fn only_confirmed_kill_verdicts_train_fallback_order_including_unnamed_kills() {
    let binaries = ranked_binaries();
    let reachable = binaries.iter().collect::<Vec<_>>();
    let hint = killer_for(&binaries[1], "probe");
    for (verdict, killed) in [
        (Verdict::Passed, false),
        (Verdict::TimedOut, false),
        (Verdict::MemoryLimit { peak: Some(50), limit: 37 }, false),
        (Verdict::Flaky(None), false),
        (Verdict::Failed(Some("probe".into())), true),
        (Verdict::Failed(None), true),
        (Verdict::TestEnumerationFailed("enumeration failed".into()), true),
    ] {
        let tally = Tally::default();
        let mut verdict = Some(verdict);
        with_binary_runner(
            move |_, _, _| BinaryRun {
                verdict: verdict.take().expect("the exact-hint stage launches only its one selected case"),
                reach: ReachObservation::Reached,
            },
            || {
                step(
                    &workspace(),
                    &binaries,
                    Some(&hint),
                    &[],
                    Stage::Exact,
                    &mut ProbeState::default(),
                    &tally,
                )
            },
        );
        let order = tally.binary_kills.lock().unwrap().order(8, &reachable);
        assert_eq!(order, if killed { [1, 0] } else { [0, 1] });
    }
}

#[test]
fn staged_unnamed_individual_failure_identifies_the_selected_case() {
    let binaries = ranked_binaries();
    let candidates = ["one", "two"].map(|test| Candidate::Exact(killer_for(&binaries[1], test)));
    let judged = with_binary_runner(
        |_, attempt, _| {
            assert_eq!(attempt.only, Only::One("one"));
            BinaryRun {
                verdict: Verdict::Failed(None),
                reach: ReachObservation::Reached,
            }
        },
        || {
            step(
                &workspace(),
                &binaries,
                None,
                &candidates,
                Stage::Inferred,
                &mut ProbeState::default(),
                &Tally::default(),
            )
        },
    );
    assert!(matches!(judged, Some(Judgement::Reached(Outcome::Killed, Some(found), None)) if found == killer_for(&binaries[1], "one")));
}

#[test]
fn staged_containment_failure_aborts_in_every_phase() {
    for &stage in Stage::sequence() {
        let binaries = ranked_binaries();
        let hint = killer_for(&binaries[1], "killer");
        let candidates = [Candidate::Exact(hint.clone())];
        let tally = Tally::default();
        let judged = with_binary_runner(
            |_, _, _| BinaryRun {
                verdict: Verdict::Unmetered("containment lost".into()),
                reach: ReachObservation::Unknown,
            },
            || {
                step(
                    &workspace(),
                    &binaries,
                    Some(&hint),
                    &candidates,
                    stage,
                    &mut ProbeState::default(),
                    &tally,
                )
            },
        );
        assert!(matches!(judged, Some(Judgement::Abandoned(reason)) if reason == "containment lost"));
        assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
    }
}

#[test]
fn staged_candidate_ranking_keeps_all_candidates_without_legacy_admission_filters() {
    let learning = FileLearning::new();
    let binaries = binaries(10);
    for (index, binary) in binaries.iter().enumerate() {
        learning.publish("function", &killed_by(killer_for(binary, &format!("test-{index}"))));
    }
    {
        let mut state = learning.locked();
        let candidates = &mut state.items.get_mut("function").unwrap().exact;
        candidates[0].misses = 10;
        candidates[1].hits = 5;
    }
    let candidates = learning.select_candidates("function");
    let named = candidates
        .iter()
        .filter_map(|candidate| match candidate {
            Candidate::Exact(killer) => Some(killer),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(named.len(), 10);
    assert_eq!(named[0].test, "test-1");
    assert_eq!(named[9].test, "test-0");
    let tally = Tally::default();
    tally.item_probes.store(8, Ordering::Relaxed);
    let mut index = 0;
    with_binary_runner(
        move |binary, attempt, _| {
            assert_eq!(binary.target, format!("binary-{index}"));
            assert_eq!(attempt.only, Only::One(&format!("test-{index}")));
            index += 1;
            passed()
        },
        || {
            assert!(
                step(
                    &workspace(),
                    &binaries,
                    None,
                    &candidates,
                    Stage::Inferred,
                    &mut ProbeState::default(),
                    &tally
                )
                .is_none()
            );
        },
    );
    assert_eq!(tally.launches.load(Ordering::Relaxed), 6);
}

#[test]
fn inferred_cases_merge_promoted_candidates_and_rank_global_kills_before_the_six_case_cap() {
    let binaries = binaries(1);
    let learning = FileLearning::new();
    for name in ["zero-c", "zero-b", "zero-a", "fresh", "tie-b", "tie-a", "z-high"] {
        learning.publish("function", &killed_by(killer_for(&binaries[0], name)));
    }
    learning.publish("unrelated-function", &killed_by(killer_for(&binaries[0], "not-eligible")));
    {
        let mut state = learning.locked();
        for candidate in &mut state.items.get_mut("function").unwrap().exact {
            candidate.seeds = u32::MAX;
            if candidate.identity.test == "z-high" {
                candidate.misses = u32::MAX;
                candidate.measured = Duration::from_secs(1000);
                candidate.samples = 1;
            } else if candidate.identity.test == "zero-c" {
                candidate.hits = u32::MAX;
                candidate.measured = Duration::ZERO;
                candidate.samples = 1;
            }
        }
    }
    let tally = Tally::default();
    let mut ordinal = 100;
    for (name, kills) in [("z-promoted", 9), ("z-high", 8), ("tie-b", 2), ("tie-a", 2), ("fresh", 1)] {
        for _ in 0..kills {
            tally
                .named_kills
                .lock()
                .unwrap()
                .record(ordinal, &killed_by(killer_for(&binaries[0], name)));
            ordinal += 1;
        }
    }
    for _ in 0..20 {
        tally
            .named_kills
            .lock()
            .unwrap()
            .record(ordinal, &killed_by(killer_for(&binaries[0], "not-eligible")));
        ordinal += 1;
    }
    let promoted = ["z-promoted", "z-high"].map(|name| Candidate::Exact(killer_for(&binaries[0], name)));
    let mut calls = Vec::new();
    with_binary_runner(
        move |_, attempt, _| {
            let Only::One(name) = attempt.only else {
                panic!("named inference must run individual cases")
            };
            calls.push(name.to_owned());
            if calls.len() == 6 {
                assert_eq!(calls, ["z-promoted", "z-high", "tie-a", "tie-b", "fresh", "zero-a"]);
            }
            passed()
        },
        || {
            assert!(
                judge_learning_step(
                    &workspace(),
                    7,
                    &binaries.iter().collect::<Vec<_>>(),
                    None,
                    &promoted,
                    &learning,
                    "function",
                    &SiteIdentity::from_mutant(&crate::fixtures::mutant()),
                    &NegativeLearning::default(),
                    true,
                    None,
                    ranked_sweep(),
                    &tally,
                    Stage::Inferred,
                    &mut ProbeState::default(),
                )
                .is_none()
            );
        },
    );
    assert_eq!(tally.launches.load(Ordering::Relaxed), 6);
}

#[test]
fn every_stage_credits_only_the_final_named_killer_not_resource_or_enumeration_failures() {
    let binaries = ranked_binaries();
    let reachable = binaries.iter().collect::<Vec<_>>();
    let hint = killer_for(&binaries[1], "probe");
    for &stage in Stage::sequence() {
        for (verdict, named) in [
            (Verdict::Passed, None),
            (Verdict::TimedOut, None),
            (Verdict::MemoryLimit { peak: Some(50), limit: 37 }, None),
            (Verdict::Flaky(Some("probe".into())), None),
            (Verdict::Unmetered("containment lost".into()), None),
            (Verdict::Failed(Some("probe".into())), Some("probe")),
            (Verdict::Failed(Some("actual".into())), Some("actual")),
            (
                Verdict::Failed(None),
                matches!(stage, Stage::Exact | Stage::Inferred).then_some("probe"),
            ),
            (Verdict::TestEnumerationFailed("enumeration failed".into()), None),
        ] {
            let tally = Tally::default();
            let mut verdict = Some(verdict);
            let candidates = [Candidate::Exact(hint.clone())];
            with_binary_runner(
                move |binary, _, _| {
                    if binary.target == "earlier" {
                        return passed();
                    }
                    BinaryRun {
                        verdict: verdict.take().expect("each stage launches the selected binary once"),
                        reach: ReachObservation::Reached,
                    }
                },
                || {
                    step(
                        &workspace(),
                        &binaries,
                        Some(&hint),
                        &candidates,
                        stage,
                        &mut ProbeState::default(),
                        &tally,
                    )
                },
            );
            let mut selections = [(0, named.unwrap_or("probe")), (1, named.unwrap_or("probe"))];
            tally.named_kills.lock().unwrap().rank(&mut selections, &reachable);
            assert_eq!(
                selections[0].0,
                usize::from(named.is_some()),
                "only a final named attribution should change named-test ordering in {stage:?}"
            );
        }
    }
}

#[test]
fn global_named_ranking_does_not_reorder_inferred_whole_binaries() {
    let binaries = ranked_binaries();
    let candidates = [
        Candidate::Exact(killer_for(&binaries[0], "first")),
        Candidate::Exact(killer_for(&binaries[1], "successful")),
    ];
    let tally = Tally::default();
    tally
        .named_kills
        .lock()
        .unwrap()
        .record(99, &killed_by(killer_for(&binaries[1], "successful")));
    let mut calls = Vec::new();
    let mut state = ProbeState::default();
    with_binary_runner(
        move |binary, attempt, _| {
            calls.push((binary.target.clone(), format!("{:?}", attempt.only)));
            if calls.len() == 4 {
                assert_eq!(
                    calls,
                    [
                        ("later".into(), "One(\"successful\")".into()),
                        ("earlier".into(), "One(\"first\")".into()),
                        ("earlier".into(), "All".into()),
                        ("later".into(), "All".into()),
                    ]
                );
            }
            passed()
        },
        || {
            assert!(step(&workspace(), &binaries, None, &candidates, Stage::Inferred, &mut state, &tally).is_none());
            assert!(step(&workspace(), &binaries, None, &candidates, Stage::Binaries, &mut state, &tally).is_none());
        },
    );
    assert_eq!(tally.launches.load(Ordering::Relaxed), 4);
}

#[test]
fn restored_named_kills_train_global_ranking_before_the_next_inferred_selection() {
    let (work, mut plan, binaries) = scripted_harness(ranked_binaries().to_vec());
    plan.mutants = (1..=2)
        .map(|ordinal| Mutant {
            ordinal,
            id: format!("restored-{ordinal}").into(),
            item_path: "shared".into(),
            ..plan.mutants[0].clone()
        })
        .collect();
    let successful = killer_for(&binaries[1], "z-successful");
    let learning = FileLearning::new();
    learning.publish("shared", &killed_by(killer_for(&binaries[0], "a-uncredited")));
    learning.publish("shared", &killed_by(successful.clone()));
    let mut generalized = GeneralizedHints::empty_supported();
    learning.persist(&plan.mutants[0].file, &mut generalized);
    let mut killers = Killers::default();
    killers.replace_generalized(generalized);
    let reach = Reachability::build(&plan, &binaries, &NARROW);
    let mut events = Recorder::default();
    with_shared_runner(
        Arc::new(|binary, attempt, _| {
            assert_eq!(attempt.active, Some(2));
            assert_eq!(binary.target, "later");
            assert_eq!(attempt.only, Only::One("z-successful"));
            BinaryRun {
                verdict: Verdict::Failed(Some("z-successful".into())),
                reach: ReachObservation::Reached,
            }
        }),
        || {
            test_all_with_accounting_hooks(
                &work,
                &mut plan,
                &reach,
                ranked_sweep(),
                true,
                &mut killers,
                &mut events,
                (
                    |_, _, _| Ok(()),
                    |ordinal| (ordinal == 1).then(|| killed_by(successful.clone())),
                    || {},
                ),
            )
        },
    )
    .unwrap()
    .unwrap();
    assert!(plan.mutants.iter().all(|mutant| mutant.outcome == Outcome::Killed));
    assert_eq!(killers.hint(&plan.mutants[1].id), Some(&successful));
}

#[derive(Default)]
struct Recorder {
    starts: usize,
    completed: usize,
    active: usize,
    phases: Vec<String>,
}

impl Events for Recorder {
    fn phase(&mut self, verb: &str, detail: &str) {
        assert_eq!(verb, "Testing");
        self.phases.push(detail.to_owned());
    }
    fn mutant_started(&mut self) {
        self.starts += 1;
        self.mutant_activity(true);
    }
    fn mutant(&mut self, _: &Mutant) {
        self.completed += 1;
        self.mutant_activity(false);
    }
    fn mutant_activity(&mut self, active: bool) {
        self.active = if active { self.active + 1 } else { self.active - 1 };
    }
}

#[test]
fn staged_campaign_publishes_phase_one_kills_before_inference_and_never_revisits_killed_mutants() {
    let (work, mut plan, binaries) = scripted_harness(ranked_binaries().to_vec());
    plan.mutants = (1..=4)
        .map(|ordinal| Mutant {
            ordinal,
            id: format!("phase-{ordinal}").into(),
            item_path: if ordinal == 1 || ordinal == 3 {
                "shared".into()
            } else {
                format!("function-{ordinal}").into()
            },
            ..plan.mutants[0].clone()
        })
        .collect();
    let mut killers = Killers::default();
    killers.record(plan.mutants[0].id.clone(), killer_for(&binaries[1], "killer"));
    killers.record(plan.mutants[1].id.clone(), killer_for(&binaries[0], "passes"));
    let reach = Reachability::build(&plan, &binaries, &NARROW);
    // This observes runner order, not a scheduler gate or a publication flag for other data.
    // SeqCst gives callback increments and later-phase assertions one global observation order.
    let exact_done = Arc::new(AtomicUsize::new(0));
    let mut events = Recorder::default();
    with_shared_runner(
        Arc::new({
            let exact_done = Arc::clone(&exact_done);
            move |binary, attempt, _| {
                match (attempt.active.unwrap(), attempt.only) {
                    (1, Only::One("killer")) => {
                        assert_eq!(binary.target, "later");
                        exact_done.fetch_add(1, Ordering::SeqCst);
                        return BinaryRun {
                            verdict: Verdict::Failed(Some("killer".into())),
                            reach: ReachObservation::Reached,
                        };
                    }
                    (2, Only::One("passes")) => {
                        exact_done.fetch_add(1, Ordering::SeqCst);
                    }
                    (3, Only::One("killer")) => {
                        assert_eq!(exact_done.load(Ordering::SeqCst), 2, "phase one must drain before inferred cases");
                        return BinaryRun {
                            verdict: Verdict::Failed(Some("killer".into())),
                            reach: ReachObservation::Reached,
                        };
                    }
                    (2 | 4, Only::All) => {
                        assert_eq!(exact_done.load(Ordering::SeqCst), 2, "exact hints must drain before full binaries");
                    }
                    unexpected => panic!("unexpected probe: {unexpected:?}"),
                }
                passed()
            }
        }),
        || {
            test_all(
                &work,
                &mut plan,
                &reach,
                Sweep { jobs: 2, ..ranked_sweep() },
                true,
                &mut killers,
                &mut events,
            )
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        plan.mutants.iter().map(|m| m.outcome).collect::<Vec<_>>(),
        [Outcome::Killed, Outcome::Survived, Outcome::Killed, Outcome::Survived]
    );
    assert_eq!((events.starts, events.completed, events.active), (4, 4, 0));
    assert!(events.phases.is_empty(), "internal testing stages must not emit phase messages");
    assert_eq!(killers.hint(&plan.mutants[2].id).unwrap().test, "killer");
}

#[test]
fn full_binary_discovery_teaches_queued_siblings_before_another_function_is_explored() {
    let (work, mut plan, binaries) = scripted_harness(ranked_binaries().to_vec());
    plan.mutants = (1..=4)
        .map(|ordinal| Mutant {
            ordinal,
            id: format!("learning-{ordinal}").into(),
            item_path: if ordinal < 4 { "shared".into() } else { "unexplored".into() },
            file: Utf8PathBuf::from(if ordinal < 4 { "src/a.rs" } else { "src/other.rs" }).into(),
            ..plan.mutants[0].clone()
        })
        .collect();
    let reach = Reachability::build(&plan, &binaries, &NARROW);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut killers = Killers::default();
    let mut events = Recorder::default();
    with_shared_runner(
        Arc::new({
            let calls = Arc::clone(&calls);
            move |binary, attempt, _| {
                let ordinal = attempt.active.unwrap();
                calls
                    .lock()
                    .unwrap()
                    .push((ordinal, binary.target.clone(), format!("{:?}", attempt.only)));
                match (ordinal, binary.target.as_str(), attempt.only) {
                    (1, "earlier", Only::All) | (4, _, Only::All) => passed(),
                    (1, "later", Only::All) | (2 | 3, "later", Only::One("discovered")) => BinaryRun {
                        verdict: Verdict::Failed(Some("discovered".into())),
                        reach: ReachObservation::Reached,
                    },
                    unexpected => panic!("unexpected selection: {unexpected:?}"),
                }
            }
        }),
        || {
            test_all(
                &work,
                &mut plan,
                &reach,
                Sweep { jobs: 1, ..ranked_sweep() },
                true,
                &mut killers,
                &mut events,
            )
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        *calls.lock().unwrap(),
        [
            (1, "earlier".into(), "All".into()),
            (1, "later".into(), "All".into()),
            (2, "later".into(), "One(\"discovered\")".into()),
            (3, "later".into(), "One(\"discovered\")".into()),
            (4, "later".into(), "All".into()),
            (4, "earlier".into(), "All".into()),
        ]
    );
    assert_eq!(
        plan.mutants.iter().map(|mutant| mutant.outcome).collect::<Vec<_>>(),
        [Outcome::Killed, Outcome::Killed, Outcome::Killed, Outcome::Survived]
    );
    for mutant in &plan.mutants[..3] {
        assert_eq!(killers.hint(&mutant.id).unwrap().test, "discovered");
    }
    assert_eq!((events.starts, events.completed, events.active), (4, 4, 0));
}

#[test]
fn end_to_end_work_reuses_exact_passes_after_the_global_hint_pass() {
    let (work, mut plan, binaries) = scripted_harness(ranked_binaries().to_vec());
    plan.mutants = (1..=3)
        .map(|ordinal| Mutant {
            ordinal,
            id: format!("exact-first-{ordinal}").into(),
            ..plan.mutants[0].clone()
        })
        .collect();
    let mut killers = Killers::default();
    for mutant in &plan.mutants[..2] {
        killers.record(mutant.id.clone(), killer_for(&binaries[1], "hinted"));
    }
    let reach = Reachability::build(&plan, &binaries, &NARROW);
    let calls = Arc::new(Mutex::new(Vec::new()));
    with_shared_runner(
        Arc::new({
            let calls = Arc::clone(&calls);
            move |binary, attempt, _| {
                let ordinal = attempt.active.unwrap();
                calls
                    .lock()
                    .unwrap()
                    .push((ordinal, binary.target.clone(), format!("{:?}", attempt.only)));
                match (ordinal, attempt.only) {
                    (2 | 3, Only::One("hinted")) => BinaryRun {
                        verdict: Verdict::Failed(Some("hinted".into())),
                        reach: ReachObservation::Reached,
                    },
                    (1, _) => passed(),
                    unexpected => panic!("unexpected selection: {unexpected:?}"),
                }
            }
        }),
        || {
            test_all(
                &work,
                &mut plan,
                &reach,
                Sweep { jobs: 1, ..ranked_sweep() },
                true,
                &mut killers,
                &mut Recorder::default(),
            )
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        *calls.lock().unwrap(),
        [
            (1, "later".into(), "One(\"hinted\")".into()),
            (2, "later".into(), "One(\"hinted\")".into()),
            (3, "later".into(), "One(\"hinted\")".into()),
            (1, "later".into(), "All".into()),
            (1, "earlier".into(), "All".into()),
        ]
    );
    assert_eq!(
        plan.mutants.iter().map(|mutant| mutant.outcome).collect::<Vec<_>>(),
        [Outcome::Survived, Outcome::Killed, Outcome::Killed]
    );
}

#[test]
fn staged_same_item_admission_and_probe_ownership_allow_independent_progress() {
    let binaries = Arc::new(ranked_binaries());
    let scheduler = Arc::new(Scheduler::new(
        (0..2)
            .map(|index| ScheduledWork {
                file: 0,
                item: "shared".into(),
                package: "subject".into(),
                cost: Duration::from_millis(1),
                sibling_benefit: 1,
                hinted: false,
                transfer: false,
                stable_order: index,
                resources: Vec::new(),
            })
            .collect(),
        1,
    ));
    let first = scheduler.reserve_next(&mut scheduler.state.lock().unwrap()).unwrap();
    assert_eq!(first, 0);
    let states = Arc::new([Mutex::new(PendingState::default()), Mutex::new(PendingState::default())]);
    let active = Arc::new(Mutex::new(crate::HashSet::default()));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let tally = Arc::new(Tally::default());
    with_shared_runner(
        Arc::new({
            let scheduler = Arc::clone(&scheduler);
            let states = Arc::clone(&states);
            let binaries = Arc::clone(&binaries);
            let active = Arc::clone(&active);
            let calls = Arc::clone(&calls);
            let tally = Arc::clone(&tally);
            move |binary, attempt, _| {
                let ordinal = attempt.active.unwrap();
                let index = usize::try_from(ordinal - 1).unwrap();
                assert!(matches!(states[index].try_lock(), Err(std::sync::TryLockError::WouldBlock)));
                assert!(active.lock().unwrap().insert(ordinal), "a mutant cannot overlap its own probes");
                calls.lock().unwrap().push((ordinal, binary.target.clone()));
                if ordinal == 1 && binary.target == "earlier" {
                    let second = {
                        let mut state = scheduler.state.lock().unwrap();
                        let second = scheduler.reserve_next(&mut state).expect("same-item work remains admissible");
                        assert_eq!(second, 1);
                        assert_eq!(state.active_items.get(&(0, Arc::from("shared"))), Some(&2));
                        assert_eq!(scheduler.reserve_next(&mut state), None);
                        second
                    };
                    let judged = judge(
                        &workspace(),
                        2,
                        &binaries.iter().collect::<Vec<_>>(),
                        None,
                        &[],
                        None,
                        None,
                        None,
                        ranked_sweep(),
                        &tally,
                        Stage::Fallback,
                        &mut lock_pending(states.as_slice(), second).probes,
                    );
                    assert!(matches!(judged, Some(Judgement::Reached(Outcome::Survived, None, None))));
                    assert_eq!(*active.lock().unwrap(), crate::HashSet::from_iter([1]));
                    scheduler.release(second, false);
                }
                assert!(active.lock().unwrap().remove(&ordinal));
                passed()
            }
        }),
        || {
            let judged = judge(
                &workspace(),
                1,
                &binaries.iter().collect::<Vec<_>>(),
                None,
                &[],
                None,
                None,
                None,
                ranked_sweep(),
                &tally,
                Stage::Fallback,
                &mut lock_pending(states.as_slice(), first).probes,
            );
            assert!(matches!(judged, Some(Judgement::Reached(Outcome::Survived, None, None))));
        },
    );
    scheduler.release(first, false);
    assert_eq!(
        *calls.lock().unwrap(),
        [
            (1, "earlier".into()),
            (2, "earlier".into()),
            (2, "later".into()),
            (1, "later".into()),
        ]
    );
    assert!(active.lock().unwrap().is_empty());
    assert_eq!(tally.launches.load(Ordering::Relaxed), 4);
    assert_eq!(scheduler.state.lock().unwrap().active_files, [0]);
}

#[test]
fn staged_fallback_uses_linkage_and_never_discards_unknown_links_as_proven_absent() {
    let mut binaries = binaries(3);
    binaries[0].linked_sources = Some(crate::HashSet::from_iter([Utf8PathBuf::from("src/a.rs")]));
    binaries[1].linked_sources = Some(crate::HashSet::from_iter([Utf8PathBuf::from("src/other.rs")]));
    binaries[2].linked_sources = None;
    let (work, mut plan, binaries) = scripted_harness(binaries);
    let reach = Reachability::build(&plan, &binaries, &NARROW);
    let spent = with_shared_runner(
        Arc::new(|binary, _, _| {
            assert_ne!(binary.target, "binary-1", "compiler-proven unlinked binaries must not run");
            passed()
        }),
        || {
            test_all(
                &work,
                &mut plan,
                &reach,
                ranked_sweep(),
                true,
                &mut Killers::default(),
                &mut Recorder::default(),
            )
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(plan.mutants[0].outcome, Outcome::Survived);
    assert_eq!(spent.accounting.unwrap().fallback.launches, 2);
}

#[test]
fn sweep_fallback_processes_all_17_mutants_without_hints_or_memory_reservations() {
    let mut binaries = binaries(1);
    binaries[0].memory = Some(1_000);
    let (work, mut plan, binaries) = scripted_harness(binaries);
    plan.mutants = (1..=17)
        .map(|ordinal| Mutant {
            ordinal,
            id: format!("parallel-fallback-{ordinal}").into(),
            ..plan.mutants[0].clone()
        })
        .collect();
    let reach = Reachability::build(&plan, &binaries, &NARROW);
    let mut events = Recorder::default();
    let spent = with_shared_runner(
        Arc::new(|_, attempt, confirm| {
            assert_eq!(attempt.only, Only::All);
            assert_eq!(attempt.request.limit, Some(1_000));
            assert!(confirm);
            BinaryRun {
                verdict: Verdict::Failed(Some("killer".into())),
                reach: ReachObservation::Reached,
            }
        }),
        || {
            test_all(
                &work,
                &mut plan,
                &reach,
                Sweep {
                    jobs: 17,
                    ..ranked_sweep()
                },
                false,
                &mut Killers::default(),
                &mut events,
            )
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!((events.starts, events.completed, events.active), (17, 17, 0));
    assert!(plan.mutants.iter().all(|mutant| mutant.outcome == Outcome::Killed));
    assert_eq!(spent.accounting.unwrap().fallback.launches, 17);
}

#[test]
fn all_phases_run_for_17_admitted_mutants_without_aggregate_memory_reservations() {
    let mut binaries = binaries(1);
    binaries[0].memory = Some(1_000);
    let binary = &binaries[0];
    let reachable = [binary];
    let hint = killer_for(binary, "killer");
    let candidates = [Candidate::Exact(hint.clone())];
    for &phase in Stage::sequence() {
        let tally = Tally::default();
        let scheduler = Arc::new(Scheduler::new(
            (0..17)
                .map(|index| ScheduledWork {
                    file: 0,
                    item: "subject::f".into(),
                    package: "subject".into(),
                    cost: Duration::from_millis(1),
                    sibling_benefit: 16,
                    hinted: false,
                    transfer: false,
                    stable_order: index,
                    resources: Vec::new(),
                })
                .collect(),
            1,
        ));
        let mut admitted = Vec::new();
        launch_workers(17, || {
            let next = scheduler.reserve_next(&mut scheduler.state.lock().unwrap());
            admitted.push(next.expect("all configured mutants must be admitted without waiting"));
        });
        assert_eq!(admitted, (0..17).collect::<Vec<_>>());
        let runner = Arc::new({
            let scheduler = Arc::clone(&scheduler);
            move |_: &TestBinary, attempt: Attempt<'_>, confirm: bool| {
                assert_eq!(
                    attempt.only,
                    match phase {
                        Stage::Exact | Stage::Inferred => Only::One("killer"),
                        Stage::Binaries | Stage::Fallback => Only::All,
                    }
                );
                assert_eq!(attempt.request.limit, Some(1_000));
                assert!(confirm);
                let state = scheduler.state.lock().unwrap();
                assert_eq!(state.remaining_count, 0);
                assert_eq!(state.active_files, [17]);
                assert_eq!(state.active_items.get(&(0, Arc::from("subject::f"))), Some(&17));
                assert!(state.active_resources.is_empty());
                BinaryRun {
                    verdict: Verdict::Failed(Some("killer".into())),
                    reach: ReachObservation::Reached,
                }
            }
        });
        with_shared_runner(runner, || {
            for &index in &admitted {
                let judged = judge(
                    &workspace(),
                    u32::try_from(index + 1).unwrap(),
                    &reachable,
                    Some(&hint),
                    &candidates,
                    None,
                    None,
                    None,
                    ranked_sweep(),
                    &tally,
                    phase,
                    &mut ProbeState::default(),
                );
                assert!(matches!(judged, Some(Judgement::Reached(Outcome::Killed, _, _))));
            }
        });
        assert_eq!(tally.launches.load(Ordering::Relaxed), 17);
        for index in admitted {
            scheduler.release(index, false);
        }
        let state = scheduler.state.lock().unwrap();
        assert_eq!(state.active_files, [0]);
        assert!(state.active_items.is_empty());
        assert!(state.active_resources.is_empty());
    }
}

#[test]
fn staged_no_coverage_requires_no_binary() {
    let judged = step(
        &workspace(),
        &[],
        None,
        &[],
        Stage::Fallback,
        &mut ProbeState::default(),
        &Tally::default(),
    );
    assert!(matches!(judged, Some(Judgement::Reached(Outcome::NoCoverage, None, None))));
}
