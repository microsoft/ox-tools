// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the `cargo each` command: resolve the selection,
//! apply filters, build the plan, and run it.

use std::process::{Command, ExitCode};

use cargo_each::{Mode, Plan, Predicate, Selection, Workspace};
use ohno::{AppError, IntoAppError};

use crate::cli::EachArgs;

pub(crate) fn run(args: &EachArgs) -> Result<ExitCode, AppError> {
    let selection = build_selection(args);
    let workspace = Workspace::load(args.manifest_path.as_deref()).into_app_err("failed to load workspace")?;

    let mut members = selection.resolve(&workspace).into_app_err("failed to resolve package selection")?;
    apply_filters(&mut members, args)?;

    // The `{packages}` pass-through only applies when the resolved set is the
    // untouched whole workspace: no per-package narrowing and no filters.
    let whole_workspace = selection.is_whole_workspace() && args.filters.is_empty() && args.exclude_filters.is_empty();

    let mode = if args.once { Mode::Once } else { Mode::PerPackage };
    let plan = Plan::build(&members, mode, args.chdir, whole_workspace, &args.command).into_app_err("failed to build command plan")?;

    if plan.invocations.is_empty() {
        eprintln!("cargo each: selection resolved to no packages; nothing to do");
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

/// Narrow `members` by the `--filter` (keep) and `--exclude-filter` (drop)
/// predicates. Both are AND-combined; `--exclude-filter` wins on conflict.
fn apply_filters(members: &mut Vec<&cargo_each::Member>, args: &EachArgs) -> Result<(), AppError> {
    let keep = parse_predicates(&args.filters)?;
    let drop = parse_predicates(&args.exclude_filters)?;
    members.retain(|m| keep.iter().all(|p| p.matches(m)) && !drop.iter().any(|p| p.matches(m)));
    Ok(())
}

fn parse_predicates(specs: &[String]) -> Result<Vec<Predicate>, AppError> {
    specs
        .iter()
        .map(|s| Predicate::parse(s).into_app_err("invalid filter predicate"))
        .collect()
}

/// Run each invocation, honoring the fail-fast / `--keep-going` policy.
fn execute(plan: &Plan, keep_going: bool) -> Result<ExitCode, AppError> {
    let mut first_failure: Option<u8> = None;
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
        let status = command.status().into_app_err(format!("failed to spawn `{program}`"))?;
        if !status.success() {
            let code = u8::try_from(status.code().unwrap_or(1)).unwrap_or(1);
            if !keep_going {
                return Ok(ExitCode::from(code));
            }
            first_failure.get_or_insert(code);
        }
    }
    Ok(first_failure.map_or(ExitCode::SUCCESS, ExitCode::from))
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
