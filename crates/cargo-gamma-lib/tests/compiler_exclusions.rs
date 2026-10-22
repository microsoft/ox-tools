// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![cfg(not(miri))]

//! Native persistence and migration boundaries for the compiler-exclusion inventory.

use core::error::Error as _;
use std::fs;

use camino::{Utf8Path, Utf8PathBuf};
use cargo_gamma_lib::internals::discover::{CompilerExclusions, ContextDigest, Killer, Killers, RunRecord, Term, Trust};
use cargo_gamma_lib::internals::model::{Mutant, Outcome};
use cargo_gamma_lib::internals::ops::{Selection, collect_candidates, into_mutants};
use cargo_gamma_lib::internals::parse::SourceFile;
use cargo_gamma_lib::testing::ci_fixture::mutant;
use cargo_gamma_lib::testing::workdir;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

const FILE: &str = "compiler-exclusions.json";

fn fixture() -> Utf8PathBuf {
    Utf8Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("completed-recovery")
}

fn candidate(ordinal: u32) -> Mutant {
    Mutant {
        ordinal,
        ..mutant("lib.rs", ordinal as usize, "literal.bool_flip", Outcome::Pending)
    }
}

#[test]
fn policy_mismatches_preserve_publication_and_exact_hints_but_not_reusable_exclusions() {
    let root = fixture();
    let directory = workdir("compiler-policy-publication");
    let base = Utf8Path::from_path(directory.path()).unwrap();
    let source = SourceFile::parse("lib.rs", include_str!("fixtures/completed-recovery/lib.rs").to_owned()).unwrap();
    let selection = Selection::parse("all").unwrap();
    let mut population = into_mutants(&source, "subject", collect_candidates(&source, &selection))
        .into_iter()
        .take(2)
        .collect::<Vec<_>>();
    assert_eq!(population.len(), 2);
    for (mutant, reason) in population
        .iter_mut()
        .zip(["legacy secondary overlap", "legacy flow-sensitive primary"])
    {
        mutant.outcome = Outcome::CompileError;
        mutant.note = Some(reason.to_owned());
    }
    let mut context = serde_json::to_value(ContextDigest::default()).unwrap();
    context["target"] = "controlled-target".into();
    context["environment"] = "controlled-environment".into();
    context["tool"] = "current-policy".into();
    let current: ContextDigest = serde_json::from_value(context.clone()).unwrap();
    context["tool"] = "previous-policy".into();
    let legacy: ContextDigest = serde_json::from_value(context).unwrap();
    assert_eq!(legacy.differences(&current), [Term::Tool]);
    let record = RunRecord::from_run(&root, &population, &legacy, &[]);
    record.store_completed(base).unwrap();
    let hint = Killer {
        package: "subject".to_owned(),
        target: "lib".to_owned(),
        test: "tests::catches".to_owned(),
    };
    RunRecord::store_probes(base, &HashMap::from_iter([(population[0].id.clone(), hint.clone())]));
    let old = RunRecord::load(base);
    assert_eq!(old.context().term(Term::Tool), legacy.term(Term::Tool));
    assert_eq!(old.iter().filter(|(_, outcome)| *outcome == Outcome::CompileError).count(), 2);
    assert_eq!(old.probes().get(&population[0].id), Some(&hint));
    let mut expected_ids = population.iter().map(|mutant| mutant.id.as_str()).collect::<Vec<_>>();
    expected_ids.sort_unstable();
    assert_eq!(old.ordering(), expected_ids);
    assert!(old.settled(&root, Trust::Free, &Killers::default(), &current).0.is_empty());

    old.store_completed(base).expect("old-policy observations remain publishable");
    let republished = RunRecord::load(base);
    assert_eq!(republished.context().term(Term::Tool), legacy.term(Term::Tool));
    assert_eq!(republished.probes().get(&population[0].id), Some(&hint));
    assert_eq!(republished.ordering(), expected_ids);
    assert!(
        republished.settled(&root, Trust::Free, &Killers::default(), &current).0.is_empty(),
        "republishing an old ledger cannot make its exclusions reusable"
    );

    let fresh = RunRecord::from_run(&root, &population, &current, &[]);
    assert_eq!(
        serde_json::to_value(&old).unwrap()["version"],
        serde_json::to_value(&fresh).unwrap()["version"],
        "policy changes do not change the record format"
    );
    fresh.store_completed(base).unwrap();
    let published = RunRecord::load(base);
    let settled = published.settled(&root, Trust::Free, &Killers::default(), &current).0;
    assert_eq!(settled.len(), 2);
    for mutant in &population {
        assert_eq!(settled.get(&mutant.id), Some(&Outcome::CompileError));
    }
    assert_eq!(published.probes().get(&population[0].id), Some(&hint));
}

#[test]
fn exclusions_survive_disjoint_populations_and_keep_the_first_reason() {
    let directory = workdir("compiler-exclusion-inventory");
    let base = Utf8Path::from_path(directory.path()).unwrap();
    let mut inventory = CompilerExclusions::load(base, &fixture()).unwrap();
    let first = candidate(1);
    assert!(inventory.remember(core::slice::from_ref(&first), &HashSet::from_iter([1]), |_| "E0308".into()));
    assert!(!inventory.remember(core::slice::from_ref(&first), &HashSet::from_iter([1]), |_| {
        "follow-on error".into()
    }));
    inventory.store(base).unwrap();
    let mut inventory = CompilerExclusions::load(base, &fixture()).unwrap();
    let second = candidate(2);
    assert!(inventory.remember(core::slice::from_ref(&second), &HashSet::from_iter([2]), |_| {
        "isolated compiler failure".into()
    }));
    inventory.store(base).unwrap();
    let inventory = CompilerExclusions::load(base, &fixture()).unwrap();
    let saved: serde_json::Value = serde_json::from_str(&fs::read_to_string(base.join(FILE)).unwrap()).unwrap();
    assert_eq!(saved["mutants"].as_object().unwrap().len(), 2);
    assert_eq!(inventory.reason(&first), Some("E0308"));
    assert_eq!(inventory.reason(&second), Some("isolated compiler failure"));
}

#[test]
fn first_load_imports_completed_and_incomplete_failures_but_not_interactions() {
    let directory = workdir("compiler-exclusion-migration");
    let base = Utf8Path::from_path(directory.path()).unwrap();
    let root = fixture();
    let first = Mutant {
        outcome: Outcome::CompileError,
        ..candidate(1)
    };
    let second = Mutant {
        outcome: Outcome::CompileError,
        ..candidate(2)
    };
    let interaction = Mutant {
        outcome: Outcome::NotBuilt,
        ..candidate(3)
    };
    RunRecord::from_run(&root, &[first.clone(), interaction.clone()], &ContextDigest::default(), &[])
        .store_completed(base)
        .unwrap();
    RunRecord::from_run(&root, core::slice::from_ref(&second), &ContextDigest::default(), &[])
        .store_incomplete_convergence(base)
        .unwrap();
    let inventory = CompilerExclusions::load(base, &root).unwrap();
    let first_reason = inventory.reason(&first).unwrap().to_owned();
    let second_reason = inventory.reason(&second).unwrap().to_owned();
    assert!(inventory.reason(&interaction).is_none());
    inventory.store(base).unwrap();
    let path = base.join(FILE);
    let mut saved: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let removed = saved["mutants"].as_object_mut().unwrap().remove(first.id.as_str()).unwrap();
    assert_eq!(removed, serde_json::Value::String(first_reason));
    fs::write(&path, serde_json::to_string(&saved).unwrap()).unwrap();
    let inventory = CompilerExclusions::load(base, &root).unwrap();
    assert!(
        inventory.reason(&first).is_none(),
        "explicit removal must not reimport stale evidence"
    );
    assert_eq!(inventory.reason(&second), Some(second_reason.as_str()));
}

#[test]
fn invalid_or_unreadable_inventory_is_not_treated_as_an_empty_inventory() {
    let directory = workdir("compiler-exclusion-invalid");
    let base = Utf8Path::from_path(directory.path()).unwrap();
    let inventory = CompilerExclusions::load(base, &fixture()).unwrap();
    fs::copy(fixture().join("unsupported-compiler-exclusions.json"), base.join(FILE)).unwrap();
    assert_eq!(
        CompilerExclusions::load(base, &fixture()).unwrap_err().to_string(),
        format!(
            "compiler exclusion inventory `{}` has unsupported version 2; expected 1",
            base.join(FILE)
        )
    );
    fs::copy(fixture().join("legacy-record.json"), base.join(FILE)).unwrap();
    let failure = CompilerExclusions::load(base, &fixture()).unwrap_err();
    assert_eq!(
        failure.message(),
        format!(
            "could not read compiler exclusion inventory `{}`; repair or remove the file",
            base.join(FILE)
        )
    );
    assert_eq!(
        failure.source().unwrap().downcast_ref::<serde_json::Error>().unwrap().classify(),
        serde_json::error::Category::Data
    );
    fs::remove_file(base.join(FILE)).unwrap();
    fs::create_dir(base.join(FILE)).unwrap();
    let failure = CompilerExclusions::load(base, &fixture()).unwrap_err();
    assert_eq!(
        failure.message(),
        format!("could not read compiler exclusion inventory `{}`", base.join(FILE))
    );
    let cause = failure.source().unwrap().downcast_ref::<std::io::Error>().unwrap();
    #[cfg(windows)]
    assert_eq!(cause.kind(), std::io::ErrorKind::PermissionDenied);
    #[cfg(unix)]
    assert_eq!(cause.kind(), std::io::ErrorKind::IsADirectory);
    let failure = inventory.store(base).unwrap_err();
    assert_eq!(failure.message(), format!("could not write `{}`", base.join(FILE)));
    let _cause = failure.source().unwrap().downcast_ref::<std::io::Error>().unwrap();
    assert_eq!(
        fs::read_dir(base)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>(),
        [FILE],
        "failed publication must preserve the directory and clean its staging file"
    );
}
