// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

#[must_use]
pub fn prepared_report_info(args: &RunArgs, root: Utf8PathBuf) -> Option<crate::elements::RunInfo> {
    let survey = crate::discover::acceptance::native_test_util::checked_survey(root);
    RecordPreparation::for_run(args, &survey, crate::discover::ContextDigest::default()).report_info
}

pub fn publication_lock(lock: File) -> crate::Result<Option<File>> {
    super::resumed_work(&(), lock, u64::MAX, true, |(), _, _| {
        Err(error!("publication-only recovery must not reopen a workspace"))
    })
    .map(|(_, lock)| lock)
}

pub fn configure<H: Host>(host: &mut H, args: &mut RunArgs, styler: Styler) -> crate::Result<()> {
    super::configure(host, args, styler)
}

pub fn resumed_publication(
    plan: &Plan,
    base: &Utf8Path,
    probes: &[(crate::model::MutantId, crate::discover::Killer)],
    remember: impl FnOnce(&Utf8Path, &Utf8Path) -> bool,
    retained_session: Option<exec::Session>,
) -> crate::Result<(bool, bool, exec::Session, Utf8PathBuf)> {
    let prepared = RecordPreparation {
        report_info: None,
        name_filtered: false,
        base: base.to_owned(),
        context: crate::discover::ContextDigest::default(),
        inputs: crate::discover::RunRecord::snapshot_with_external(&plan.root, base, &[], false),
        killers: crate::discover::Killers::default(),
    };
    let mut learning = exec::Killers::default();
    for (id, killer) in probes {
        learning.record(id.clone(), killer.clone());
    }
    super::resumed_publication(prepared, plan, Some(&learning), remember, None, retained_session)
}

pub fn resumed_report_sources(
    root: Utf8PathBuf,
    target: Utf8PathBuf,
    session: exec::Session,
    plan: &Plan,
) -> crate::Result<(exec::Session, Utf8PathBuf)> {
    let mut built = exec::Built {
        work: exec::Workspace::adopt(root, target),
        session,
        oracle: exec::Oracle {
            packages: Vec::new(),
            package_local: false,
            whole_workspace: false,
        },
    };
    super::resumed_report_sources(Some(&mut built), None, Utf8Path::new("unused-retained-base"), plan)
}

#[must_use]
pub fn report_source_root(root: Utf8PathBuf, target: Utf8PathBuf) -> Utf8PathBuf {
    exec::Workspace::adopt(root, target).report_source_root()
}

pub fn resumed_report(
    plan: &Plan,
    source_root: &Utf8Path,
    advice: &str,
    tests: Option<usize>,
    dropped: &[String],
) -> crate::Result<(crate::elements::Report, Option<String>)> {
    let info = super::resumed_report_info(None);
    let contents = super::resumed_report_contents(&info, source_root, advice, tests, dropped);
    let report = crate::elements::build_from(
        plan,
        contents.source_root,
        crate::elements::Thresholds::default(),
        Some(report_info(
            &RunArgs::default(),
            contents.retained_info,
            contents.tests,
            contents.dropped,
        )),
    )?;
    Ok((report, contents.advice.map(str::to_owned)))
}

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
