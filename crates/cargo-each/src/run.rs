// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the `cargo each` command: resolve the selection,
//! apply filters, build the plan, and run it.

use std::collections::BTreeSet;
use std::process::{Command, ExitCode};

use cargo_metadata::TargetKind;
use ohno::{AppError, IntoAppError};

use crate::cli::EachArgs;
use crate::error::InvalidTargetKindError;
use crate::filter::Predicate;
use crate::plan::{Mode, PackagesExpansion, Plan};
use crate::select::Selection;
use crate::workspace::{Member, Workspace};

pub(crate) fn run(args: &EachArgs) -> Result<ExitCode, AppError> {
    let selection = build_selection(args);
    let workspace = Workspace::load(args.manifest_path.as_deref()).into_app_err("failed to load workspace")?;

    let mut members = selection.resolve(&workspace).into_app_err("failed to resolve package selection")?;
    apply_filters(&mut members, args)?;

    // The `{packages}` pass-through only applies when the resolved set is the
    // untouched whole workspace: no per-package narrowing and no filters.
    let packages =
        if selection.is_whole_workspace() && args.filters.is_empty() && args.filter_any.is_empty() && args.exclude_filters.is_empty() {
            PackagesExpansion::Workspace
        } else {
            PackagesExpansion::Explicit
        };

    let target_kinds = parse_target_kinds(&args.each_targets)?;
    let target_required_features = args.target_required_feature.iter().cloned().collect();
    let mode = if args.once {
        Mode::Once
    } else if target_kinds.is_empty() {
        Mode::PerPackage
    } else {
        Mode::PerTarget
    };
    let plan = Plan::build(
        &members,
        mode,
        args.chdir,
        packages,
        &target_kinds,
        &target_required_features,
        &args.command,
    )
    .into_app_err("failed to build command plan")?;

    if plan.invocations.is_empty() {
        eprintln!("cargo each: selection resolved to no work; nothing to do");
        return Ok(ExitCode::SUCCESS);
    }

    if args.dry_run {
        for inv in &plan.invocations {
            match &inv.work_dir {
                Some(dir) => println!("(cd {}) {}", dir.display(), shell_join(&inv.argv)),
                None => println!("{}", shell_join(&inv.argv)),
            }
        }
        return Ok(ExitCode::SUCCESS);
    }

    execute(&plan, args.keep_going)
}

/// Assemble a [`Selection`] from the parsed flags.
///
/// The selection is entirely flag-driven: a computed selection (e.g. an
/// impact tier) is fed in by the caller via ordinary shell expansion — anvil
/// splats `_anvil-impact-include <tier>` into the `cargo each` invocation —
/// so cargo-each stays agnostic about where the selectors came from.
fn build_selection(args: &EachArgs) -> Selection {
    Selection {
        packages: args.packages.clone(),
        all: args.workspace,
        exclude: args.exclude.clone(),
        none: args.none,
    }
}

/// Narrow `members` by package keep and drop predicates. `--filter`
/// predicates are AND-combined (a member is kept only
/// if it matches *every* one); `--filter-any` predicates form one optional OR
/// group; and `--exclude-filter` predicates are OR-combined. Exclusion wins.
fn apply_filters(members: &mut Vec<&Member>, args: &EachArgs) -> Result<(), AppError> {
    let keep = parse_predicates(&args.filters)?;
    let keep_any = parse_predicates(&args.filter_any)?;
    let drop = parse_predicates(&args.exclude_filters)?;
    members.retain(|m| {
        keep.iter().all(|p| p.matches(m))
            && (keep_any.is_empty() || keep_any.iter().any(|p| p.matches(m)))
            && !drop.iter().any(|p| p.matches(m))
    });
    Ok(())
}

fn parse_predicates(specs: &[String]) -> Result<Vec<Predicate>, AppError> {
    specs
        .iter()
        .map(|s| Predicate::parse(s).into_app_err("invalid filter predicate"))
        .collect()
}

fn parse_target_kinds(kinds: &[String]) -> Result<BTreeSet<TargetKind>, AppError> {
    kinds
        .iter()
        .map(|kind| {
            if let Some(kind) = crate::workspace::parse_target_kind(kind) {
                Ok(kind)
            } else {
                Err(InvalidTargetKindError::new(kind.clone()).into())
            }
        })
        .collect::<Result<_, crate::error::EachError>>()
        .into_app_err("invalid per-target configuration")
}

/// Run each invocation, honoring the fail-fast / `--keep-going` policy.
///
/// A spawn failure (the child could not be launched at all) is treated the
/// same as a non-zero child exit: under `--keep-going` it is logged, counted
/// as a failure, and the run continues (final exit `1`); under fail-fast it
/// aborts. This keeps the documented "run them all" contract intact even when
/// one invocation cannot start.
fn execute(plan: &Plan, keep_going: bool) -> Result<ExitCode, AppError> {
    let mut any_failed = false;
    for inv in &plan.invocations {
        if let Some(label) = &inv.label {
            eprintln!("cargo each: {label}");
        }
        let (program, rest) = inv.argv.split_first().expect("Plan::build never emits an empty argv");
        let mut command = Command::new(program);
        command.args(rest);
        if let Some(dir) = &inv.work_dir {
            command.current_dir(dir);
        }
        let status = match command.status() {
            Ok(status) => status,
            // A spawn failure under --keep-going is a failed invocation, not an
            // abort: log it, mark the run failed, and move on so the remaining
            // members still run (contract: exit 1 when any invocation failed).
            Err(err) if keep_going => {
                eprintln!("cargo each: failed to spawn `{program}`: {err}");
                any_failed = true;
                continue;
            }
            // Fail-fast: a spawn failure is a hard error (exit 2 via main.rs).
            other => other.into_app_err(format!("failed to spawn `{program}`"))?,
        };
        if !status.success() {
            if !keep_going {
                // Fail-fast: propagate the failing child's own exit code,
                // reduced to the u8 `ExitCode` can carry (see `exit_byte`).
                return Ok(ExitCode::from(exit_byte(status.code())));
            }
            any_failed = true;
        }
    }
    // Under --keep-going the individual child codes may differ, so we cannot
    // pick a single meaningful one; the documented contract is a flat `1` when
    // any invocation failed.
    Ok(if any_failed { ExitCode::from(1) } else { ExitCode::SUCCESS })
}

/// Render an argv for display (`--dry-run`). Best-effort quoting for
/// readability only — nothing consumes this as input.
fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.contains(char::is_whitespace) {
                format!("\"{a}\"")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Reduce a raw process exit code to the `u8` that [`ExitCode`] can carry.
///
/// `ExitCode` is a `u8`, but process exit codes are wider: `None` means the
/// child was terminated by a signal (Unix) and non-`None` codes are a full
/// `i32` on Windows. We reduce a code to its low byte, which is a closer
/// approximation of the child's code than collapsing everything to `1`. Two
/// cases still map to `1`: a signal-terminated child (no numeric code), and a
/// non-zero code whose low byte is `0` (e.g. `256`) — which would otherwise be
/// indistinguishable from success. This function is only called on the
/// fail-fast path, where the child has already failed, so `1` is always a
/// correct non-zero fallback.
fn exit_byte(raw: Option<i32>) -> u8 {
    let Some(raw) = raw else { return 1 };
    let byte = u8::try_from(raw & 0xFF).expect("`raw & 0xFF` masks to the low byte, always within 0..=255");
    if byte == 0 { 1 } else { byte }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::exit_byte;

    #[test]
    fn signal_terminated_child_maps_to_one() {
        assert_eq!(exit_byte(None), 1);
    }

    #[test]
    fn in_range_codes_pass_through() {
        assert_eq!(exit_byte(Some(1)), 1);
        assert_eq!(exit_byte(Some(2)), 2);
        assert_eq!(exit_byte(Some(255)), 255);
    }

    #[test]
    fn wide_codes_reduce_to_low_byte() {
        // 259 = 0x103 -> low byte 3 (a common Windows code).
        assert_eq!(exit_byte(Some(259)), 3);
        assert_eq!(exit_byte(Some(257)), 1);
    }

    #[test]
    fn nonzero_code_with_zero_low_byte_maps_to_one() {
        // 256 = 0x100 -> low byte 0, which would look like success; map to 1.
        assert_eq!(exit_byte(Some(256)), 1);
        assert_eq!(exit_byte(Some(512)), 1);
    }
}
