// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use camino::{Utf8Path, Utf8PathBuf};

use super::testing_state::TestingState;
#[doc(hidden)]
pub use super::testing_state::native_test_util::{
    accounting_cost, checkpoint_resumed_cost, load_checkpoint_cost, publish_journal_cost, roundtrip_journal,
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
