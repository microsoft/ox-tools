# Migrating to the next cargo-gamma release

The next cargo-gamma release is breaking. Upgrade every developer and CI
installation that shares configuration, hints, or artifacts before promoting
new hints.

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

## Hints artifact

`cargo gamma hints` now writes `gamma-hints.yaml`. The new version can read a
legacy `gamma-hints.json` while no YAML artifact exists, then migrates it on
the next successful promotion and removes the verified JSON input. Older
cargo-gamma versions cannot read the YAML artifact, so mixed-version
installations do not share a compatible promoted-hints format.

## Baseline-failure artifacts

The single `baseline-failure.json` artifact has been replaced by a
`baseline-failures/` tree. Each failed test now has a stable directory beneath
`baseline-failures/<package>/<target>/tests/<test-name>/` containing
`failure.json` and `diags.json`; unnamed binary failures use a categorical
leaf. Update artifact collectors and links that depended on the old filename.
