// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use crate::exec::{SweepCost, Workspace};

#[must_use]
pub fn prepared_native_kind(file_type: fs::FileType) -> (bool, bool, bool, String, bool) {
    (
        PreparedFileType::is_symlink(&file_type),
        PreparedFileType::is_file(&file_type),
        PreparedFileType::is_dir(&file_type),
        format!("{:?}", prepared_file_kind(&file_type)),
        prepared_leaf(&file_type),
    )
}

pub fn prepared_tree_paths(roots: &[Utf8PathBuf], output: &Utf8Path) -> Result<crate::HashSet<Utf8PathBuf>> {
    tree_paths(roots, output)
}

pub fn with_tree_walk_policy<T>(body: impl FnOnce() -> T) -> (T, Vec<(bool, bool)>) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TREE_WALK_POLICY.with_borrow_mut(|slot| *slot = None);
        }
    }
    TREE_WALK_POLICY.with_borrow_mut(|slot| *slot = Some(Vec::new()));
    let _reset = Reset;
    let result = body();
    let policies = TREE_WALK_POLICY.with_borrow_mut(|slot| slot.take().unwrap_or_default());
    (result, policies)
}

pub fn read_artifact_link(path: &Utf8Path) -> Result<Utf8PathBuf> {
    ArtifactMetadata::read_artifact_link(path)
}

pub fn inspect_native_artifact(path: &Utf8Path, follow: bool) -> std::io::Result<String> {
    inspect_artifact(path, follow).map(|(kind, _)| format!("{kind:?}"))
}

pub fn read_artifact_metadata(path: &Utf8Path) -> Result<serde_json::Value> {
    serde_json::to_value(ArtifactMetadata::read(path)?).map_err(|cause| error!("could not serialize artifact metadata").caused_by(cause))
}

pub fn with_artifact_stats<T>(link: fs::Metadata, target: fs::Metadata, body: impl FnOnce() -> T) -> T {
    struct Reset(Option<(fs::Metadata, fs::Metadata)>);
    impl Drop for Reset {
        fn drop(&mut self) {
            ARTIFACT_STATS.with_borrow_mut(|slot| *slot = self.0.take());
        }
    }
    let _reset = Reset(ARTIFACT_STATS.with_borrow_mut(|slot| slot.replace((link, target))));
    body()
}

pub fn replay_testing_journal(snapshot: serde_json::Value, base: &Utf8Path) -> (Result<u64>, serde_json::Value) {
    let mut state: TestingState = serde_json::from_value(snapshot).expect("the integration fixture must be a complete testing snapshot");
    let result = state.replay_journal(base, u64::MAX, &crate::HashMap::default());
    let snapshot = serde_json::to_value(state).expect("a deserialized testing snapshot contains JSON-compatible values");
    (result, snapshot)
}

pub fn capture_native_artifacts(work: Workspace, snapshot: serde_json::Value) -> Result<serde_json::Value> {
    let state: TestingState = serde_json::from_value(snapshot).expect("the integration fixture must be a complete testing snapshot");
    let artifacts = capture_artifacts(&Built {
        work,
        session: state.session,
        oracle: state.oracle,
    })?;
    serde_json::to_value(artifacts).map_err(|cause| error!("could not serialize captured artifacts").caused_by(cause))
}

pub fn read_native_json_with_limit(path: &Utf8Path, limit: u64) -> Result<serde_json::Value> {
    read_json_with_limit(path, limit)
}

pub fn publish_journal_cost(base: &Utf8Path, cost: SweepCost) -> Result<()> {
    TestingJournal::open(base, None)?.publish_cost(cost)
}

pub fn load_checkpoint_cost(base: &Utf8Path) -> Result<(Plan, Option<SweepCost>, u64)> {
    let (state, valid) = TestingState::load(base)?;
    Ok((state.plan, state.session.phases.sweep, valid))
}

pub fn roundtrip_journal(line: &[u8], path: &Utf8Path) -> Result<Vec<u8>> {
    encode_committed(&decode_committed(line, path)?)
}

pub fn accounting_cost(line: &[u8], path: &Utf8Path) -> Result<SweepCost> {
    let JournalEntry::Accounting { sweep, .. } = decode_committed(line, path)? else {
        return Err(error!("expected an accounting checkpoint"));
    };
    Ok(sweep)
}

pub fn checkpoint_resumed_cost(
    root: Utf8PathBuf,
    base: &Utf8Path,
    prior: SweepCost,
    next: &SweepCost,
) -> Result<std::collections::BTreeSet<crate::model::MutantId>> {
    let mut workspace = Workspace::adopt(root, base.join("target"));
    workspace.arm_testing_journal()?;
    workspace.retain_sweep_cost(Some(prior));
    let confirmations = workspace.prior_initial_confirmations();
    workspace.checkpoint_sweep(next)?;
    Ok(confirmations)
}
