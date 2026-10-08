# Migrating to the next cargo-gamma release

The next cargo-gamma release is breaking. Upgrade every developer and CI
installation that shares configuration, hints, or artifacts before promoting
new hints.

## List commands

The value previously passed to the flat `list [WHAT]` command is now a
subcommand. Bare `cargo gamma list` remains shorthand for `cargo gamma list
mutants`, but options must follow the selected subcommand:

| Previous invocation | New invocation |
| --- | --- |
| `cargo gamma list [OPTIONS]` | `cargo gamma list mutants [OPTIONS]` |
| `cargo gamma list [OPTIONS] mutants` | `cargo gamma list mutants [OPTIONS]` |
| `cargo gamma list [OPTIONS] files` | `cargo gamma list files [OPTIONS]` |
| `cargo gamma list [OPTIONS] mutators` | `cargo gamma list mutators [OPTIONS]` |
| `cargo gamma list [OPTIONS] presets` | `cargo gamma list presets [OPTIONS]` |

Each subcommand now accepts only its relevant options. In particular,
population and file selection options belong to `mutants` or `files`, while
registry selection options belong to `mutators` or `presets`.

## Default mutator selection

Ordinary runs now skip uncertain zero decrements, uncertain expression
increments or decrements, and filter removals whose receiver is not known to be
an iterator. Mutations in clear signed, numeric, or iterator contexts remain
enabled, and `literal.int_increment` still changes `0` to `1`.

A non-default selector that includes `literal.int_decrement`,
`expr.increment`, `expr.decrement`, or `iter.remove_filter` also includes the uncertain locations.
This may be the mutator name, its family, another preset containing it, or
`all`. Scripts that require the broadest possible candidate population should
name the relevant selector instead of relying on the default population.

## Survivor selection

`--only-survivors` is now a flag that reads the current artifact directory. Scripts that supplied
a report path as its value must place that report at `<ARTIFACT_DIR>/gamma-report.json` and pass
`--artifact-dir <ARTIFACT_DIR> --only-survivors`.

## Suppression commands

`cargo gamma suppress` and `cargo gamma unsuppress` now preview their edits by
default. Add `--apply` to write the displayed changes. Existing scripts that
relied on the old write-by-default behavior must add that flag.

## Estimate option

`cargo gamma run --estimate` has been removed. It combined an up-front
projection with live execution even though the projection could not account
for compiler withdrawal, learned hints, test-resource contention, or changing
selection costs. Use ordinary count-based progress or `--dashboard` while a
campaign runs.

## Test selection and uncovered verdicts

The reachability census is now opt-in. A default run still uses checked exact
and generalized hints, but otherwise falls back to running each reachable test
binary as a whole. Consequently, the up-front case selection and immediate
`uncovered` classifications supplied by the census no longer occur by default.
A default run can still establish `uncovered` later from a complete, sealed
whole-binary observation learned while evaluating another mutant at the same
site.

Pass `--optimize-test-execution`, or set
`optimize-test-execution = true` in `gamma.toml`, to restore census behavior.
The census remains subject to its economic gate and may be declined when its
projected launch cost cannot repay the work it could save.

`--whole-test-binaries` previously opted out of the census. It now explicitly
disables all case-level selection, including selection from checked hints. It
conflicts with `--optimize-test-execution`; use it when tests have
non-deterministic reachability and every selected case in a reachable binary
must run for each mutant.

## Baseline-failure artifacts

The single `baseline-failure.json` artifact has been replaced by a
`baseline-failures/` tree. Each failed test now has a stable directory beneath
`baseline-failures/<package>/<target>/tests/<test-name>/` containing
`failure.json` and `diags.json`; unnamed binary failures use a categorical
leaf. Update artifact collectors and links that depended on the old filename.
