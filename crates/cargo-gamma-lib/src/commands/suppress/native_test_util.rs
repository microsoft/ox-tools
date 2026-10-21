// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

pub fn partial_plan(root: &Utf8Path, record: &RunRecord) -> crate::Result<(Plan, Vec<String>)> {
    super::plan_from_record(root, record, &[crate::fix::Eligible::Timeout])
}

pub fn apply_partial_plan<H: Host>(host: &mut H, args: &SuppressArgs, plan: &Plan, record: &RunRecord) -> crate::Result<i32> {
    let _lock = crate::exec::claim_workspace(&args.dir)?;
    super::suppress_plan(host, args, plan, &[crate::fix::Eligible::Timeout], record, Styler::new(false))
}
