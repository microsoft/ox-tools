// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use crate::exec::{SweepCost, Workspace};

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
