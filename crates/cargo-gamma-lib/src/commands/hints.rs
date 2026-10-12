// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::Write;

use super::cli::HintsArgs;
use super::dispatch::EXIT_OK;
use super::host::Host;
use crate::discover::{Hints, RunRecord, hints_path};
use crate::report::{Styler, quantity};

/// Implements `hints`.
///
/// Promoting rather than copying is the whole point of the command existing. A documented `cp` from
/// the scratch record would put a file in version control with a format nobody owns, no version, no
/// provenance, entries for mutants that were edited away three months ago, and — worst — every
/// verdict the record holds, which the next run would then be reading from somebody else's machine.
/// Everything this does that a copy does not is one of those problems:
///
/// - it admits only the tiers that cannot move a score, and drops the rest on the floor;
/// - it uses the workspace-relative identities persisted by the selected campaign instead of
///   rediscovering and reparsing the current population;
/// - it stamps the format version and the tool that wrote it;
/// - it writes atomically and reads back what it wrote.
///
/// Errors are surfaced rather than absorbed, which is the reverse of how the artifact is read. A
/// run consults the file automatically and must never fail over it, so reading is best-effort.
/// Writing it is something somebody asked for: incremental promotion therefore refuses an existing
/// generation it cannot understand, while `--replace` is the explicit permission to discard one.
/// Publication compares against the exact bytes used for the merge so concurrent writers cannot
/// silently replace one another.
/// Implements `hints` with the configuration generation dispatch already resolved.
#[cfg_attr(coverage_nightly, coverage(off))]
pub(super) fn hints<H: Host>(host: &mut H, args: &HintsArgs, styler: Styler) -> crate::Result<i32> {
    let selected = crate::paths::physical(&args.dir)?;
    let (root, base, _lock) = crate::exec::claim_campaign_state(&selected, args.cache_dir.as_deref())?;
    let record = promotion_record(
        args.replace,
        || crate::exec::partial_testing_record(&base),
        |strict| {
            if strict {
                RunRecord::load_required(&base)
            } else {
                Ok(RunRecord::load(&base))
            }
        },
    )?;
    let (existing, generation) = Hints::load_for_promotion(&root, args.replace)?;
    let (promoted, omitted) = Hints::promoted_record(&record);
    let merged = if args.replace {
        Hints::merged(&existing, promoted, true, &root)?
    } else {
        Hints::merged_record(&existing, promoted, &record, &root)?
    };
    let changes = merged.changes_from(&existing);

    if omitted > 0 {
        writeln!(
            host.error(),
            "{} {} was outside the current campaign population or had no persisted source path and could not be promoted; generalized knowledge was preserved",
            styler.warning(),
            quantity(omitted, "exact hint")
        )?;
    }

    if merged.is_empty() && existing.is_empty() && generation.is_none() && !args.replace {
        writeln!(
            host.error(),
            "{} nothing to promote: no run under `{base}` has recorded a killing test, build-order hint, or generalized test-order hint for the current population",
            styler.verb("Finished")
        )?;

        return Ok(EXIT_OK);
    }

    let path = hints_path(&root);

    if args.dry_run {
        writeln!(
            host.error(),
            "{} `{path}`: {}, {}, {}, and {}",
            styler.verb("Preview"),
            format_args!("{} added", quantity(changes.added, "record")),
            format_args!("{} updated", quantity(changes.updated, "record")),
            format_args!("{} removed", quantity(changes.removed, "record")),
            format_args!("{} preserved", quantity(changes.preserved, "record"))
        )?;

        return Ok(EXIT_OK);
    }

    let promotion = if !args.replace && generation.is_some() && changes.is_empty() {
        merged.counts()
    } else {
        merged.write_from_locked(&path, generation.as_deref())?
    };

    let verb = if promotion.changed { "Wrote" } else { "Unchanged" };

    writeln!(
        host.error(),
        "{} `{path}`: {}, {}, {}, and {}",
        styler.verb(verb),
        format_args!("{} added", quantity(changes.added, "record")),
        format_args!("{} updated", quantity(changes.updated, "record")),
        format_args!("{} removed", quantity(changes.removed, "record")),
        format_args!("{} preserved", quantity(changes.preserved, "record"))
    )?;

    Ok(EXIT_OK)
}

fn promotion_record(
    replace: bool,
    partial: impl FnOnce() -> crate::Result<Option<RunRecord>>,
    completed: impl FnOnce(bool) -> crate::Result<RunRecord>,
) -> crate::Result<RunRecord> {
    if replace {
        return completed(true);
    }
    match partial()? {
        Some(record) => Ok(record),
        None => completed(false),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::error::error;

    #[test]
    fn replacement_requires_completed_evidence_without_reading_partial_state() {
        let failure = promotion_record(
            true,
            || panic!("replacement must never consult the interrupted campaign"),
            |strict| {
                assert!(strict, "replacement must strictly load a completed record");
                Err(error!("missing completed ledger"))
            },
        )
        .unwrap_err();
        assert_eq!(failure.message(), "missing completed ledger");
        let record = promotion_record(
            true,
            || panic!("replacement must never consult the interrupted campaign"),
            |strict| {
                assert!(strict);
                Ok(RunRecord::default())
            },
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(record).unwrap(),
            serde_json::to_value(RunRecord::default()).unwrap()
        );
    }

    #[test]
    fn ordinary_promotion_prefers_partial_evidence_and_keeps_completed_fallback_best_effort() {
        let partial = promotion_record(
            false,
            || Ok(Some(RunRecord::default())),
            |_| panic!("partial evidence must take precedence over older completed evidence"),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(partial).unwrap(),
            serde_json::to_value(RunRecord::default()).unwrap()
        );
        let completed = promotion_record(
            false,
            || Ok(None),
            |strict| {
                assert!(!strict, "ordinary promotion retains best-effort completed-record loading");
                Ok(RunRecord::default())
            },
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(completed).unwrap(),
            serde_json::to_value(RunRecord::default()).unwrap()
        );
        let failure = promotion_record(
            false,
            || Err(error!("corrupt interrupted checkpoint")),
            |_| panic!("a corrupt interrupted checkpoint must not fall back to old evidence"),
        )
        .unwrap_err();
        assert_eq!(failure.message(), "corrupt interrupted checkpoint");
    }
}
