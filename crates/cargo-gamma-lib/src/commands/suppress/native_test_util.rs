// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

pub fn suppress<H: Host>(host: &mut H, args: &SuppressArgs, when: When, styler: Styler) -> crate::Result<i32> {
    super::suppress(host, args, when, styler)
}

pub fn physical(path: &Utf8Path) -> crate::Result<Utf8PathBuf> {
    crate::paths::physical(path)
}

#[must_use]
pub fn outcome_files(record: &RunRecord) -> Vec<String> {
    record.outcomes().into_iter().map(|outcome| outcome.file.to_string()).collect()
}

pub fn partial_plan(root: &Utf8Path, record: &RunRecord) -> crate::Result<(Plan, Vec<String>)> {
    super::plan_from_record(root, record, &[crate::fix::Eligible::Timeout])
}

pub fn apply_partial_plan<H: Host>(host: &mut H, args: &SuppressArgs, plan: &Plan, record: &RunRecord) -> crate::Result<i32> {
    let _lock = crate::exec::claim_workspace(&args.dir)?;
    super::suppress_plan(host, args, plan, &[crate::fix::Eligible::Timeout], record, Styler::new(false))
}
