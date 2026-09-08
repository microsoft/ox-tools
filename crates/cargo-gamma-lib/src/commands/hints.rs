// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::Write;

use super::cli::HintsArgs;
use super::dispatch::EXIT_OK;
use super::host::Host;
use crate::discover::{Hints, RunRecord, hints_path};
use crate::exec::{CargoOptions, gamma_base};
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
/// - it joins against the population as it stands now, so the file does not grow forever;
/// - it stamps the format version and the tool that wrote it;
/// - it writes atomically and reads back what it wrote.
///
/// Errors are surfaced rather than absorbed, which is the reverse of how the artifact is read. A
/// run consults the file automatically and must never fail over it, so reading is best-effort to
/// the point of treating a corrupt file as an empty one. Writing it is something somebody asked
/// for, and a promotion that quietly did nothing would be discovered as a slow CI run weeks later.
/// Implements `hints` with the configuration generation dispatch already resolved.
pub(super) fn hints_with_cargo<H: Host>(host: &mut H, args: &HintsArgs, styler: Styler, cargo: &CargoOptions) -> crate::Result<i32> {
    let selection = args.select.selection()?;

    // Deliberately unsharded. A shard sees a fraction of the population, so promoting from one
    // would drop every hint outside it — the file would be correct and almost empty, and each shard
    // in a matrix would fight the others over it.
    let plan = crate::discover::plan_for_build(&args.select, &selection, None, cargo, &mut |_| {})?;

    let base = gamma_base(&plan.root, args.cache_dir.as_deref());
    let record = RunRecord::load(&base);
    let promoted = Hints::promoted(&record, &plan.mutants);

    if promoted.is_empty() {
        writeln!(
            host.error(),
            "{} nothing to promote: no run under `{base}` has recorded a killing test or an unviable mutant for the current population",
            styler.verb("Finished")
        )?;

        return Ok(EXIT_OK);
    }

    let path = hints_path(&plan.root);

    if args.dry_run {
        let counts = promoted.counts();

        writeln!(
            host.error(),
            "{} `{path}` would carry {} and {}, for {}",
            styler.verb("Preview"),
            quantity(counts.probes, "killing test"),
            quantity(counts.ordering, "build-order hint"),
            quantity(counts.mutants, "mutant")
        )?;

        return Ok(EXIT_OK);
    }

    let promotion = promoted.write(&path)?;

    let verb = if promotion.changed { "Wrote" } else { "Unchanged" };

    writeln!(
        host.error(),
        "{} `{path}`: {} and {}, for {}",
        styler.verb(verb),
        quantity(promotion.probes, "killing test"),
        quantity(promotion.ordering, "build-order hint"),
        quantity(promotion.mutants, "mutant")
    )?;

    Ok(EXIT_OK)
}
