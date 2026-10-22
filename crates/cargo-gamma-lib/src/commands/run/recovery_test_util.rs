// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

#[derive(Debug)]
pub struct Recovered {
    pub plan: Option<Plan>,
}

pub fn resume<H: Host>(host: &mut H, args: &RunArgs, when: When, styler: Styler) -> crate::Result<Recovered> {
    super::resumed_testing(host, args, when, styler).map(|executed| Recovered { plan: executed.plan })
}

pub fn resume_with_support<H: Host, E: std::error::Error + Send + Sync + 'static>(
    host: &mut H,
    args: &RunArgs,
    when: When,
    styler: Styler,
    support: impl FnOnce() -> Result<(), E>,
) -> crate::Result<Recovered> {
    super::resumed_testing_with_support(host, args, when, styler, support, |_| {}).map(|executed| Recovered { plan: executed.plan })
}

pub fn measure<H: Host>(host: &mut H, args: &RunArgs, when: When, styler: Styler) -> crate::Result<Recovered> {
    super::measured(host, args, when, styler).map(|executed| Recovered { plan: executed.plan })
}

pub fn campaign_preflight(args: &RunArgs, resolve: impl FnOnce() -> crate::Result<Utf8PathBuf>) -> crate::Result<()> {
    super::campaign_preflight(args, resolve)
}

#[must_use]
pub fn snapshot_inputs(root: &Utf8Path, base: &Utf8Path) -> serde_json::Value {
    serde_json::to_value(crate::discover::RunRecord::snapshot_with_external(root, base, &[], false))
        .expect("workspace snapshots contain serializable paths and digests")
}

pub fn load_record(base: &Utf8Path) -> crate::Result<crate::discover::RunRecord> {
    crate::discover::RunRecord::load_required(base)
}

pub fn settled_record_against(
    record: &crate::discover::RunRecord,
    root: &Utf8Path,
    context: &crate::discover::ContextDigest,
    inputs: &serde_json::Value,
) -> crate::Result<crate::HashMap<crate::model::MutantId, Outcome>> {
    let inputs = serde_json::from_value(inputs.clone()).map_err(|cause| error!("invalid test snapshot").caused_by(cause))?;
    Ok(record
        .settled_against(
            root,
            crate::discover::Trust::Free,
            &crate::discover::Killers::default(),
            context,
            &inputs,
        )
        .0
        .into_iter()
        .map(|(id, settled)| (id, settled.outcome))
        .collect())
}

pub fn resume_before_execution<H: Host, E: std::error::Error + Send + Sync + 'static>(
    host: &mut H,
    args: &RunArgs,
    when: When,
    styler: Styler,
    support: impl FnOnce() -> Result<(), E>,
    configure: impl FnOnce(),
) -> crate::Result<Recovered> {
    super::resumed_testing_with_support(host, args, when, styler, support, |_| configure())
        .map(|executed| Recovered { plan: executed.plan })
}
