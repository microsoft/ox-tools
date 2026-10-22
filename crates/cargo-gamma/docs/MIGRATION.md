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

Runs use checked exact and inferred tests, then fall back to each complete
reachable test binary. There is no up-front reachability sampling phase.
Remove the obsolete `--optimize-test-execution` CLI flag and
`optimize-test-execution` TOML setting: optimization is now the default, and
strict configuration parsing rejects the removed setting.
A run can establish `uncovered` from complete, sealed, passing whole-binary
observations learned while evaluating another replacement at the same stable
source site, provided reachability is deterministic.

`--whole-test-binaries` explicitly disables case-level selection, including
selection from checked hints, and deterministic reach narrowing. Use it when
tests have nondeterministic reachability and every selected case in a reachable
binary must run for each mutant.
This is a conservative opt-out of the new defaults, not a required replacement
for the removed optimization flag or setting.

## Baseline-failure artifacts

The single `baseline-failure.json` artifact has been replaced by a
`baseline-failures/` tree. Each failed test now has a stable directory beneath
`baseline-failures/<package>/<target>/tests/<test-name>/` containing
`failure.json` and `diags.json`; unnamed binary failures use a categorical
leaf. Update artifact collectors and links that depended on the old filename.
