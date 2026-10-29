// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use camino::{Utf8Path, Utf8PathBuf};

use super::testing_state::TestingState;
#[doc(hidden)]
pub use super::testing_state::native_test_util::{
    accounting_cost, capture_native_artifacts, checkpoint_resumed_cost, inspect_native_artifact, load_checkpoint_cost,
    prepared_native_kind, prepared_tree_paths, publish_journal_cost, read_artifact_link, read_artifact_metadata,
    read_native_json_with_limit, replay_testing_journal, roundtrip_journal, with_artifact_stats, with_tree_walk_policy,
};
#[doc(hidden)]
pub use super::workspace::native_test_util::{
    anchor_native_cargo_config, anchor_native_manifest_paths, anchor_native_manifests, claim_native_redirected_cache,
    claim_native_state_with, prepare_native_workspace, publish_cache_owner, reject_native_linked_cache, require_native_manifest_within,
    retain_cache_locks, retain_prepared_locks, retained_cache_locks, with_cache_metadata, with_workspace_resolver,
};
use crate::Result;
use crate::discover::Plan;

pub struct SavedTestingState {
    pub plan: Plan,
    state: TestingState,
}

impl core::fmt::Debug for SavedTestingState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SavedTestingState")
            .field("plan", &self.plan)
            .finish_non_exhaustive()
    }
}

impl SavedTestingState {
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.state.is_settled()
    }

    pub fn validate_artifacts(&self) -> Result<()> {
        self.state.validate_artifacts()
    }
}

pub fn load_testing_state(base: &Utf8Path) -> Result<(SavedTestingState, u64)> {
    let (state, valid) = TestingState::load(base)?;
    Ok((
        SavedTestingState {
            plan: state.plan.clone(),
            state,
        },
        valid,
    ))
}

pub fn claim_testing_state(root: &Utf8Path, base: Option<&Utf8Path>) -> Result<(Utf8PathBuf, Utf8PathBuf, impl Sized)> {
    super::workspace::claim_testing_state(root, base)
}

#[must_use]
pub fn same_existing_path(left: &Utf8Path, right: &Utf8Path) -> bool {
    super::workspace::same_existing_path(left, right)
}

pub fn create_private_dir(path: &Utf8Path) {
    super::workspace::create_private_dir_all_for_test(path);
}

pub fn mark_cache_owned(root: &Utf8Path, base: &Utf8Path) {
    super::workspace::mark_cache_owned_for_test(root, base);
}

#[must_use]
pub fn empty_execution_killers() -> serde_json::Value {
    serde_json::to_value(super::Killers::default()).expect("empty killer maps serialize without fallible values")
}

pub fn with_cache_home_at<T>(home: Utf8PathBuf, body: impl FnOnce() -> T) -> T {
    crate::testing::with_cache_home_at(home, body)
}

pub fn observe_workspace_preparation<T>(root: &Utf8Path, body: impl FnOnce() -> T) -> (T, Option<usize>) {
    crate::testing::observe_workspace_preparation(root, body)
}

#[must_use]
pub fn physical_workspace(path: &Utf8Path) -> Utf8PathBuf {
    super::workspace::native_test_util::physical_workspace(path)
}

#[must_use]
pub fn native_gamma_base(root: &Utf8Path) -> Utf8PathBuf {
    super::workspace::native_test_util::native_gamma_base(root)
}

#[must_use]
pub fn workspace_digest(root: &Utf8Path) -> String {
    super::workspace::native_test_util::workspace_digest(root)
}

pub fn snapshot_launch(snapshot: serde_json::Value) -> Result<(Option<std::ffi::OsString>, String)> {
    let launch: super::loader::Launch = serde_json::from_value(snapshot)
        .map_err(|cause| crate::error::Error::new("could not deserialize launch snapshot").caused_by(cause))?;
    Ok((launch.loader, launch.stack))
}

use super::{CargoOptions, Workspace};

impl Workspace {
    #[doc(hidden)]
    pub fn test_base(&self) -> &Utf8Path {
        self.base()
    }

    #[doc(hidden)]
    pub fn test_root(&self) -> Utf8PathBuf {
        self.root.clone()
    }

    #[doc(hidden)]
    pub fn test_cargo_mut(&mut self) -> &mut CargoOptions {
        &mut self.cargo
    }

    #[doc(hidden)]
    pub fn test_set_rustc_captures(&mut self, captures: Option<Utf8PathBuf>) {
        self.set_rustc_captures(captures);
    }
}
