// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::path::Path;

use super::*;

fn proof_scope() -> BuildScope<'static> {
    BuildScope {
        roots: None,
        mutants: None,
        publish_progress: false,
    }
}

fn marked_proof_plan(work: &Workspace, mutants: usize, separate_items: bool) -> Plan {
    let mut plan = probe_plan(work, 0, mutants);
    for mutant in &mut plan.mutants {
        let ordinal = mutant.ordinal;
        mutant.replacement = format!("{{ let proof_marker_{ordinal}_end = 2; proof_marker_{ordinal}_end }}").into();
        let item = if separate_items {
            format!("item_{ordinal}")
        } else {
            "same_item".to_owned()
        };
        mutant.item_path = item.into();
    }
    plan
}

fn native_proof_script<T>(
    work: &Workspace,
    population: usize,
    required: &[u32],
    failure_at: Option<usize>,
    body: impl FnOnce() -> T,
) -> (T, Vec<Vec<u32>>) {
    let attempted = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let result = with_compiler(
        {
            let attempted = std::rc::Rc::clone(&attempted);
            let root = work.test_root();
            let required = required.to_vec();
            move |observed, arguments, budget, _| {
                assert_eq!(observed.test_root(), root);
                assert_eq!(arguments[0], "check");
                assert_eq!(budget, Some(core::time::Duration::from_millis(19)));
                let text = std::fs::read_to_string(root.join("src").join("lib.rs"))
                    .expect("the compiler callback reads the schema generated before invocation");
                let active = (1..=u32::try_from(population).expect("the bounded proof population fits in u32"))
                    .filter(|ordinal| text.contains(&format!("proof_marker_{ordinal}_end")))
                    .collect::<Vec<_>>();
                let failed = required.iter().all(|ordinal| active.contains(ordinal));
                let mut attempted = attempted.borrow_mut();
                attempted.push(active);
                assert!(attempted.len() <= MAX_ISOLATION_PROOFS, "proof orchestration must stay bounded");
                if failure_at == Some(attempted.len()) {
                    return Err(error!("scripted proof failure {}", attempted.len()));
                }
                Ok(Some(scripted_output(!failed, "proof stdout", "")))
            }
        },
        body,
    );
    let attempted = attempted.borrow().clone();
    (result, attempted)
}

fn proof_limits() -> BuildLimits {
    BuildLimits {
        timeout: Some(core::time::Duration::from_millis(19)),
        ..BuildLimits::default()
    }
}

fn minimization_cases() -> [MinimizationCase; 5] {
    [
        // A required prefix pair survives repeated half reductions, then rejects both singletons.
        (8, 0, vec![vec![1, 2, 3, 4], vec![1, 2], vec![1], vec![2]], vec![1, 2]),
        // Independent failures occupy opposite halves; retaining the first restarts reduction there.
        (8, 1, vec![vec![1, 2, 3, 4], vec![1, 2], vec![1]], vec![1]),
        // Member 4 masks failure 1; the competing 2/11 pair spans halves, forcing smaller chunks
        // until an unmasked prefix can be retained and reduced to its single failing member.
        (
            12,
            2,
            vec![
                vec![1, 2, 3, 4, 5, 6],
                vec![7, 8, 9, 10, 11, 12],
                vec![1, 2, 3],
                vec![1, 2],
                vec![1],
            ],
            vec![1],
        ),
        // The required 2/7 pair spans halves and quarter chunks: only chunk removal can shrink it,
        // and each successful removal must restart reduction before the final pair is minimal.
        (
            8,
            3,
            vec![
                vec![1, 2, 3, 4],
                vec![5, 6, 7, 8],
                vec![1, 2],
                vec![3, 4],
                vec![5, 6],
                vec![7, 8],
                vec![3, 4, 5, 6, 7, 8],
                vec![1, 2, 5, 6, 7, 8],
                vec![1, 2, 7, 8],
                vec![1],
                vec![2],
                vec![7],
                vec![8],
                vec![2, 7, 8],
                vec![2, 8],
                vec![2, 7],
            ],
            vec![2, 7],
        ),
        // A larger population places required 1/8 in separate three-member chunks, exercising
        // repeated complement reductions and repartitioning before singleton removals fail.
        (
            12,
            4,
            vec![
                vec![1, 2, 3, 4, 5, 6],
                vec![7, 8, 9, 10, 11, 12],
                vec![1, 2, 3],
                vec![4, 5, 6],
                vec![7, 8, 9],
                vec![10, 11, 12],
                vec![4, 5, 6, 7, 8, 9, 10, 11, 12],
                vec![1, 2, 3, 7, 8, 9, 10, 11, 12],
                vec![1, 2, 3, 10, 11, 12],
                vec![1, 2, 3, 7, 8, 9],
                vec![1, 2],
                vec![3, 7],
                vec![8, 9],
                vec![3, 7, 8, 9],
                vec![1, 2, 8, 9],
                vec![1],
                vec![2],
                vec![8],
                vec![9],
                vec![2, 8, 9],
                vec![1, 8, 9],
                vec![1, 9],
                vec![1, 8],
            ],
            vec![1, 8],
        ),
    ]
}

type MinimizationCase = (usize, u8, Vec<Vec<u32>>, Vec<u32>);

#[derive(Default)]
struct ProofEvents {
    observations: Vec<(usize, usize, Vec<std::path::PathBuf>)>,
    warnings: Vec<String>,
}

impl Events for ProofEvents {
    fn phase(&mut self, _verb: &str, _detail: &str) {}

    fn mutant(&mut self, _mutant: &Mutant) {}

    fn warn(&mut self, warning: &str) {
        self.warnings.push(warning.to_owned());
    }

    fn wants_isolation_evidence(&self) -> bool {
        true
    }

    fn isolation_evidence(
        &mut self,
        active: usize,
        population: usize,
        written: &[&Path],
        _fresh: usize,
        _rebuilt: usize,
        _failed_targets: &[String],
    ) {
        self.observations
            .push((active, population, written.iter().map(|path| path.to_path_buf()).collect()));
    }
}
#[test]
fn later_contexts_keep_the_failed_schemas_item_coordinates_after_proofs_replace_guards() {
    let (_directory, work) = trivial_workspace("native-isolation-coordinate-snapshot");
    let mut plan = split_native_isolation_plan(&work);
    for mutant in &mut plan.mutants {
        mutant.item_path = format!("item_{}", mutant.ordinal).into();
    }
    let mut converger = Converger::default();
    let (guards, _) = converger.instrument_schema(&work, &plan, &HashSet::default()).unwrap();
    let diagnostics = [("first", 1), ("second", 3)]
        .map(|(name, ordinal)| {
            let mut diagnostic: serde_json::Value = serde_json::from_str(&unpositioned_native_context(name)).unwrap();
            diagnostic["message"]["spans"][0]["line_start"] = guards[&ordinal].1.site.start.line().into();
            diagnostic.to_string()
        })
        .join("\n");
    let attempted = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let results = with_compiler(
        {
            let attempted = std::rc::Rc::clone(&attempted);
            move |work, arguments, _, _| {
                let second = arguments.windows(2).any(|pair| pair == ["--test", "second"]);
                let source = ["first", "second"]
                    .map(|name| std::fs::read_to_string(work.test_root().join(format!("src/{name}.rs"))).unwrap())
                    .join("\n");
                let active = (1..=3)
                    .filter(|ordinal| source.contains(&format!("regression_marker_{ordinal}_end")))
                    .collect::<Vec<_>>();
                let failed = active.contains(&if second { 3 } else { 1 });
                attempted.borrow_mut().push((second, active));
                Ok(Some(scripted_output(!failed, "", "")))
            }
        },
        || {
            converger.isolate_scoped(
                &work,
                &plan,
                &diagnostics,
                &["check", "--tests"],
                None,
                proof_limits(),
                &mut ProofEvents::default(),
            )
        },
    )
    .unwrap();
    assert!(matches!(&results[0], Isolation::Blamed(ordinals) if ordinals == &[1]));
    assert!(matches!(&results[1], Isolation::Blamed(ordinals) if ordinals == &[3]));
    let attempted = attempted.borrow();
    assert_eq!(
        attempted.iter().find(|(second, active)| *second && !active.is_empty()),
        Some(&(true, vec![3])),
        "the later diagnostic must start with its original item, not the whole file"
    );
    assert_eq!(
        *attempted,
        [(false, vec![1]), (true, vec![3])],
        "proven diagnostic-local causes must return to ordinary convergence without clean broader-tier proofs"
    );
}

#[test]
fn identical_proof_commands_share_exact_subsets_only_within_one_failed_invocation() {
    let (_directory, work) = trivial_workspace("native-isolation-shared-command");
    let plan = split_native_isolation_plan(&work);
    let diagnostics = ["first", "second"]
        .map(|name| {
            let mut diagnostic: serde_json::Value = serde_json::from_str(&unpositioned_native_context(name)).unwrap();
            diagnostic["target"]["kind"] = serde_json::json!(["lib"]);
            diagnostic.to_string()
        })
        .join("\n");
    let observed = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let mut converger = Converger::default();
    with_compiler(
        {
            let observed = std::rc::Rc::clone(&observed);
            move |work, arguments, _, _| {
                assert!(!arguments.iter().any(|argument| argument == "--test"));
                let source = ["first", "second"]
                    .map(|name| std::fs::read_to_string(work.test_root().join(format!("src/{name}.rs"))).unwrap())
                    .join("\n");
                let active = (1..=3)
                    .filter(|ordinal| source.contains(&format!("regression_marker_{ordinal}_end")))
                    .collect::<Vec<_>>();
                let failed = active.contains(&1);
                observed.borrow_mut().push(active);
                Ok(Some(scripted_output(!failed, "", "")))
            }
        },
        || {
            for _ in 0..2 {
                converger.withdrawn.clear();
                converger.census.clear();
                observed.borrow_mut().clear();
                let results = converger
                    .isolate_scoped(
                        &work,
                        &plan,
                        &diagnostics,
                        &["check", "--tests"],
                        None,
                        proof_limits(),
                        &mut ProofEvents::default(),
                    )
                    .unwrap();
                assert!(
                    results
                        .iter()
                        .any(|result| matches!(result, Isolation::Blamed(ordinals) if ordinals == &[1]))
                );
                let observed = observed.borrow();
                assert!(observed.iter().all(|active| !active.is_empty()));
                let unique = observed.iter().collect::<HashSet<_>>();
                assert_eq!(
                    unique.len(),
                    observed.len(),
                    "identical exact-active schemas must not launch duplicate builds"
                );
            }
        },
    );
}

#[test]
fn scoped_native_proofs_prefer_file_then_package_tiers_and_check_remaining_context() {
    for mode in 0..4 {
        let (_directory, work) = trivial_workspace("native-isolation-tier-order");
        let mut plan = split_native_isolation_plan(&work);
        if mode != 0 {
            for mutant in &mut plan.mutants[1..] {
                mutant.package = "dependency".into();
            }
            plan.reach
                .insert("trivial".into(), ["trivial".into(), "dependency".into()].into_iter().collect());
        }
        let mut context: serde_json::Value =
            serde_json::from_str(&unpositioned_native_context("first")).expect("the fixture serializes a valid compiler diagnostic");
        if mode == 1 {
            context["message"]["spans"][0]["file_name"] = "src/unrelated.rs".into();
        } else if mode == 2 {
            let mut second = context["message"]["spans"][0].clone();
            second["file_name"] = "src/second.rs".into();
            context["message"]["spans"]
                .as_array_mut()
                .expect("fixture spans are an array")
                .push(second);
        } else if mode == 3 {
            context["message"]["spans"][0]["file_name"] = "src/second.rs".into();
        }
        let attempted = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut converger = Converger::default();
        let mut events = ProofEvents::default();
        let result = with_candidate_policy(
            move |count| count <= if mode == 3 { 1 } else { MAX_ISOLATION_CANDIDATES },
            || {
                with_compiler(
                    {
                        let attempted = std::rc::Rc::clone(&attempted);
                        move |work, _, _, _| {
                            let source = ["first", "second"]
                                .map(|name| {
                                    std::fs::read_to_string(work.test_root().join(format!("src/{name}.rs")))
                                        .expect("the split fixture has both native source files")
                                })
                                .join("\n");
                            let active = (1..=3)
                                .filter(|ordinal| source.contains(&format!("regression_marker_{ordinal}_end")))
                                .collect::<Vec<_>>();
                            let failed = active.contains(&1) && (mode != 2 || !active.contains(&2));
                            let mut attempted = attempted.borrow_mut();
                            attempted.push(active);
                            assert!(attempted.len() <= 8, "a three-mutant tier proof must remain bounded");
                            Ok(Some(scripted_output(!failed, "", "")))
                        }
                    },
                    || {
                        converger.isolate_scoped(
                            &work,
                            &plan,
                            &context.to_string(),
                            &["check", "--tests"],
                            None,
                            proof_limits(),
                            &mut events,
                        )
                    },
                )
            },
        )
        .expect("native tier proofs use an infallible compiler callback");
        if mode == 2 {
            assert!(result.is_empty(), "a complete non-reproducing population ends this context");
            assert_eq!(*attempted.borrow(), [vec![1, 2, 3]]);
        } else {
            assert_eq!(result.len(), 1, "resolution must not continue into later tiers");
            assert!(matches!(&result[0], Isolation::Blamed(ordinals) if ordinals == &[1]));
            let expected = if mode == 3 { vec![vec![1]] } else { vec![vec![1], vec![2, 3]] };
            assert_eq!(*attempted.borrow(), expected);
        }
        assert_eq!(events.warnings.len(), if mode == 3 { 2 } else { 0 });
    }
}

#[test]
fn exhausted_contexts_report_every_context_and_a_proof_cap_stops_before_considering_another_tier() {
    for local in [MAX_ISOLATION_PROOFS - 1, MAX_ISOLATION_PROOFS] {
        for invocation in [MAX_INVOCATION_ISOLATION_PROOFS - 1, MAX_INVOCATION_ISOLATION_PROOFS] {
            assert_eq!(
                isolation_proofs_exhausted(local, invocation),
                local == MAX_ISOLATION_PROOFS || invocation == MAX_INVOCATION_ISOLATION_PROOFS
            );
        }
    }
    let (_directory, work) = trivial_workspace("native-isolation-proof-cap");
    let plan = split_native_isolation_plan(&work);
    let mut converger = Converger::default();
    converger.isolation_budget.contexts = MAX_ISOLATION_CONTEXTS;
    let mut events = ProofEvents::default();
    let contexts = ["first", "second"].map(unpositioned_native_context).join("\n");
    let result = converger
        .isolate_scoped(&work, &plan, &contexts, &["check", "--tests"], None, proof_limits(), &mut events)
        .expect("exhausted context admission cannot execute a compiler");
    assert_eq!(result.len(), 2, "one exhausted context must not abandon the next");
    assert!(result.iter().all(|result| matches!(result, Isolation::Unresolved { .. })));
    assert_eq!(events.warnings.len(), 2);
    let visited = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let mut converger = Converger::default();
    converger.isolation_budget.proofs = MAX_INVOCATION_ISOLATION_PROOFS - 1;
    let mut events = ProofEvents::default();
    let result = with_candidate_policy(
        {
            let visited = std::rc::Rc::clone(&visited);
            move |count| {
                visited.borrow_mut().push(count);
                true
            }
        },
        || {
            with_compiler(
                |_, _, _, _| Ok(Some(scripted_output(true, "", ""))),
                || {
                    converger.isolate_scoped(
                        &work,
                        &plan,
                        &unpositioned_native_context("first"),
                        &["check", "--tests"],
                        None,
                        proof_limits(),
                        &mut events,
                    )
                },
            )
        },
    )
    .expect("the last admitted pristine proof succeeds");
    assert_eq!(result.len(), 1);
    assert!(matches!(result[0], Isolation::Unresolved { .. }));
    assert_eq!(
        *visited.borrow(),
        [1, 1],
        "the exhausted proof budget forbids even considering the next tier"
    );
    assert_eq!(events.warnings.len(), 1);
    assert!(events.warnings[0].contains("1 local and 2048 invocation proof builds"));
}

#[test]
fn minimization_uses_the_exact_bounded_native_query_order_for_prefix_competing_and_unmasked_failures() {
    for (count, mode, expected, members) in minimization_cases() {
        let (_directory, work) = trivial_workspace("native-minimization-query-order");
        let plan = marked_proof_plan(&work, count, true);
        let attempted = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut converger = Converger::default();
        let mut proofs = Proofs::default();
        let population = plan.mutants.iter().collect::<Vec<_>>();
        let result = with_compiler(
            {
                let root = work.test_root();
                let attempted = std::rc::Rc::clone(&attempted);
                move |_, _, _, _| {
                    let source = std::fs::read_to_string(root.join("src/lib.rs")).unwrap();
                    let active = (1..=u32::try_from(count).unwrap())
                        .filter(|ordinal| source.contains(&format!("proof_marker_{ordinal}_end")))
                        .collect::<Vec<_>>();
                    let has = |ordinal| active.contains(&ordinal);
                    let failed = match mode {
                        0 => has(1) && has(2),
                        1 => has(1) || has(5),
                        2 => (has(1) && !has(4)) || (has(2) && has(11)),
                        3 => has(2) && has(7),
                        _ => has(1) && has(8),
                    };
                    let mut attempted = attempted.borrow_mut();
                    attempted.push(active);
                    assert!(attempted.len() <= MAX_ISOLATION_PROOFS);
                    Ok(Some(scripted_output(!failed, "", "")))
                }
            },
            || {
                converger.minimize_interaction(
                    &work,
                    &plan,
                    proof_scope(),
                    &["check"],
                    proof_limits(),
                    &mut ProofEvents::default(),
                    &population,
                    &population,
                    &mut proofs,
                )
            },
        )
        .unwrap();
        assert_eq!(result, Some(members), "mode={mode}");
        assert_eq!(*attempted.borrow(), expected, "mode={mode}");
        assert_eq!(proofs.local, expected.len());
        assert_eq!(converger.isolation_budget.proofs, expected.len());
    }
}

#[test]
fn actual_isolation_proofs_propagate_each_early_bisection_and_minimization_transport_failure() {
    for separate_items in [false, true] {
        for failed_call in 1..=8 {
            let (_directory, work) = trivial_workspace("exec-isolation-transport-");
            let plan = marked_proof_plan(&work, 4, separate_items);
            let mut converger = Converger::default();
            let (result, attempted) = native_proof_script(&work, 4, &[1, 4], Some(failed_call), || {
                converger.isolate(&work, &plan, None, &["check"], proof_limits(), &mut ProofEvents::default())
            });
            let failure = result.expect_err("the selected proof must propagate its transport failure through the full isolation caller");
            assert_eq!(failure.message(), format!("scripted proof failure {failed_call}"));
            assert_eq!(attempted.len(), failed_call);
            if failed_call > 1 {
                assert_eq!(attempted[0], [1, 2, 3, 4], "initial validation replaces the pristine proof");
            }
            if failed_call > 2 {
                assert_eq!(attempted[1], [1, 2]);
            }
        }
    }
}

#[test]
fn actual_isolation_proofs_report_minimal_cross_item_interactions_and_exact_cache_population() {
    let (_directory, work) = trivial_workspace("exec-isolation-native-interaction-");
    let plan = marked_proof_plan(&work, 4, true);
    let mut converger = Converger::default();
    let (result, attempted) = native_proof_script(&work, 4, &[1, 4], None, || {
        converger.isolate(&work, &plan, None, &["check"], proof_limits(), &mut ProofEvents::default())
    });
    let result = result.unwrap().expect("the finite full proof graph resolves the interaction");
    let Isolation::Interaction { excluded, members } = result else {
        panic!("both endpoint mutants are necessary: {result:?}")
    };
    assert_eq!(excluded, 1);
    assert_eq!(members, [1, 4]);
    assert!(attempted.contains(&vec![1]));
    assert!(attempted.contains(&vec![4]));
    assert_eq!(attempted.len(), 10);
    assert_eq!(
        attempted.iter().collect::<HashSet<_>>().len(),
        attempted.len(),
        "repeated schemas use cached proofs"
    );
    let active = vec![&plan.mutants[0]];
    let population = plan.mutants.iter().collect::<Vec<_>>();
    let mut events = ProofEvents::default();
    let (result, _) = native_proof_script(&work, 4, &[1, 4], None, || {
        converger.subset_fails(
            &work,
            &plan,
            proof_scope(),
            &["check"],
            proof_limits(),
            &mut events,
            &population,
            &active,
            &mut Proofs::default(),
        )
    });
    assert_eq!(result.unwrap(), Some(false));
    assert_eq!((events.observations[0].0, events.observations[0].1), (1, 4));
}

#[test]
fn empty_or_already_withdrawn_candidate_tiers_never_invoke_the_compiler() {
    let (_directory, work) = trivial_workspace("exec-isolation-no-candidates-");
    let plan = marked_proof_plan(&work, 2, true);
    for withdrawn in [HashSet::default(), HashSet::from_iter([1, 2])] {
        let eligible = if withdrawn.is_empty() {
            HashSet::default()
        } else {
            HashSet::from_iter([1, 2])
        };
        let mut converger = Converger {
            withdrawn,
            ..Converger::default()
        };
        let result = with_compiler(
            |_, _, _, _| panic!("an empty effective tier must not start a proof"),
            || {
                converger.isolate_candidates(
                    &work,
                    &plan,
                    proof_scope(),
                    &["check"],
                    proof_limits(),
                    &mut ProofEvents::default(),
                    &eligible,
                    &mut Proofs::default(),
                )
            },
        )
        .unwrap();
        assert!(matches!(result, IsolationAttempt::Inconclusive));
    }
}

#[test]
fn scoped_isolation_retains_nondefault_limits_and_reuses_endpoint_proofs_before_checking_the_remainder() {
    let (_directory, work) = trivial_workspace("exec-scoped-endpoint-proofs-");
    let plan = marked_proof_plan(&work, 5, true);
    let mut converger = Converger::default();
    let stdout = error_stream().replace("subject", "trivial");
    let (isolated, attempted) = native_proof_script(&work, 5, &[1, 5], None, || {
        converger.isolate_scoped(&work, &plan, &stdout, &["check"], None, proof_limits(), &mut ProofEvents::default())
    });
    let isolated = isolated.unwrap();
    assert_eq!(isolated.len(), 1);
    let Isolation::Interaction { excluded, members } = &isolated[0] else {
        panic!("endpoint interaction must be retained: {isolated:?}")
    };
    assert_eq!(*excluded, 1);
    assert_eq!(members, &[1, 5]);
    assert_eq!(attempted.len(), 13);
    assert_eq!(converger.isolation_budget.proofs, 13);
    assert_eq!(converger.isolation_budget.contexts, 1);
    assert_eq!(attempted.iter().collect::<HashSet<_>>().len(), 13);
    assert_eq!(&attempted[..3], &[vec![1, 2, 3, 4, 5], vec![1, 2], vec![3, 4, 5]]);
}

#[test]
fn scoped_isolation_propagates_a_late_proof_failure_without_a_pristine_build() {
    let (_directory, work) = trivial_workspace("exec-scoped-proof-failure-");
    let plan = marked_proof_plan(&work, 5, true);
    let mut converger = Converger::default();
    let stdout = error_stream().replace("subject", "trivial");
    let (result, attempted) = native_proof_script(&work, 5, &[1, 5], Some(5), || {
        converger.isolate_scoped(&work, &plan, &stdout, &["check"], None, proof_limits(), &mut ProofEvents::default())
    });
    let failure = result.expect_err("a late file-tier proof error must propagate through scoped isolation");
    assert_eq!(failure.message(), "scripted proof failure 5");
    assert_eq!(attempted.len(), 5);
    assert_eq!(attempted[0], [1, 2, 3, 4, 5]);
    assert_eq!(attempted[1], [1, 2]);
}

#[test]
fn native_proof_adapter_preserves_limits_population_written_files_and_all_terminal_results() {
    let (_directory, work) = trivial_workspace("exec-native-proof-adapter-");
    let plan = probe_plan(&work, 0, 3);
    let limits = BuildLimits {
        timeout: Some(core::time::Duration::from_millis(19)),
        ..BuildLimits::default()
    };
    let mut converger = Converger::default();
    for (output, expected) in [(Some(true), Some(false)), (Some(false), Some(true)), (None, None)] {
        let mut events = ProofEvents::default();
        let result = with_compiler(
            move |_, arguments, budget, _| {
                assert_eq!(arguments[0], "check");
                assert_eq!(budget, limits.timeout);
                Ok(output.map(|success| scripted_output(success, "proof stdout", "")))
            },
            || {
                converger.run_proof(
                    &work,
                    &plan,
                    proof_scope(),
                    &["check"],
                    limits,
                    &mut events,
                    3,
                    &HashSet::from_iter([2]),
                )
            },
        )
        .unwrap();
        assert_eq!(result, expected);
        if output.is_some() {
            assert_eq!(events.observations.len(), 1);
            let (active, population, written) = &events.observations[0];
            assert_eq!((*active, *population), (1, 3));
            if output == Some(true) {
                assert_eq!(written, &[plan.files[0].path.as_std_path().to_path_buf()]);
            } else {
                assert!(written.is_empty(), "reusing an identical schema does not rewrite a clean file");
            }
        } else {
            assert!(events.observations.is_empty());
        }
    }
}

#[test]
fn native_proof_adapter_propagates_instrumentation_and_compiler_failures_without_publishing_evidence() {
    let (_directory, work) = trivial_workspace("exec-proof-errors-");
    let plan = probe_plan(&work, 0, 2);
    let mut converger = Converger::default();
    let mut events = ProofEvents::default();
    let failure = with_compiler(
        |_, _, _, _| Err(error!("proof transport failed")),
        || {
            converger.run_proof(
                &work,
                &plan,
                proof_scope(),
                &["check"],
                BuildLimits::default(),
                &mut events,
                2,
                &HashSet::from_iter([1]),
            )
        },
    )
    .expect_err("a compiler transport failure must propagate from the proof adapter");
    assert_eq!(failure.message(), "proof transport failed");
    assert!(events.observations.is_empty());
    std::fs::remove_file(work.test_root().join("src/lib.rs")).unwrap();
    let mut converger = Converger::default();
    let failure = with_compiler(
        |_, _, _, _| panic!("no compiler may run after source instrumentation failed"),
        || {
            converger.run_proof(
                &work,
                &plan,
                proof_scope(),
                &["check"],
                BuildLimits::default(),
                &mut events,
                2,
                &HashSet::from_iter([1]),
            )
        },
    )
    .expect_err("an unreadable copied source cannot be treated as a proof");
    assert_eq!(
        failure.message(),
        format!("could not read `{}`", work.test_root().join("src/lib.rs"))
    );
    assert!(events.observations.is_empty());
}

fn error_stream() -> String {
    serde_json::json!({
        "reason": "compiler-message",
        "package_id": "subject 0.0.0",
        "target": {"name": "subject", "kind": ["lib"]},
        "message": {
            "level": "error",
            "message": "failed",
            "code": {"code": "E0308"},
            "spans": [{"file_name": "src/lib.rs", "is_primary": true}]
        }
    })
    .to_string()
}
