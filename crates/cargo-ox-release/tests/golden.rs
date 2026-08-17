// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Golden test: the resolver must produce the canonical plan for a captured
//! real-workspace input.
//!
//! The fixtures under `tests/data/` are a real changed-mode release simulation:
//! 39 releases whose normalized projection is stable across many independent
//! runs. Reproducing it here guards the resolver's determinism and correctness.

use std::path::PathBuf;

use cargo_ox_release::{ChangeType, Facts, Plan, PlanStatus, ReleaseSource, Request, resolve};
use pretty_assertions::assert_eq;

fn data_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("data").join(name)
}

fn load<T: serde::de::DeserializeOwned>(name: &str) -> T {
    let text = std::fs::read_to_string(data_path(name)).unwrap_or_else(|e| panic!("failed to read {name}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("failed to parse {name}: {e}"))
}

/// Sorts the presentation-only orderings so two semantically equal plans
/// compare equal regardless of topological tie-breaking artifacts.
fn normalize(plan: &mut Plan) {
    plan.releases.sort_by(|a, b| a.folder.cmp(&b.folder));
    for release in &mut plan.releases {
        release
            .cascade_reasons
            .sort_by(|a, b| (a.target.as_str(), a.edge_class.as_str()).cmp(&(b.target.as_str(), b.edge_class.as_str())));
    }
    plan.selection_decisions.sort_by(|a, b| a.package.cmp(&b.package));
    for decision in &mut plan.selection_decisions {
        decision
            .regression_evidence
            .sort_by(|a, b| (a.kind.as_str(), a.probe.as_str()).cmp(&(b.kind.as_str(), b.probe.as_str())));
    }
    plan.macro_contracts.sort_by(|a, b| a.package.cmp(&b.package));
}

#[test]
fn resolves_canonical_plan() {
    let facts: Facts = load("facts.json");
    let request: Request = load("request.json");

    let mut got = resolve(&facts, &request).expect("the canonical request resolves");
    let mut want: Plan = load("golden-plan.json");

    normalize(&mut got);
    normalize(&mut want);

    assert_eq!(got.status, PlanStatus::Resolved, "the canonical plan resolves");
    assert_eq!(got.releases.len(), 39, "the canonical plan has 39 releases");
    assert_eq!(got, want, "the resolver produces the canonical plan");
}

#[test]
fn canonical_plan_release_shape() {
    // A focused cross-check of the headline counts, independent of the
    // wholesale comparison above, so a regression points at the dimension that
    // drifted.
    let facts: Facts = load("facts.json");
    let request: Request = load("request.json");
    let plan = resolve(&facts, &request).expect("the canonical request resolves");

    let breaking = plan.releases.iter().filter(|r| r.change_type == ChangeType::Breaking).count();
    let nonbreaking = plan.releases.iter().filter(|r| r.change_type == ChangeType::NonBreaking).count();
    let patch = plan.releases.iter().filter(|r| r.change_type == ChangeType::Patch).count();
    let user = plan.releases.iter().filter(|r| r.source == ReleaseSource::User).count();
    let cascade = plan.releases.iter().filter(|r| r.source == ReleaseSource::Cascade).count();

    assert_eq!((breaking, nonbreaking, patch), (18, 2, 19), "change-type distribution");
    assert_eq!((user, cascade), (13, 26), "source distribution");
}
