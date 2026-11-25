# Cargo Ark

## Release dependency validation

`cargo ark release-dependencies [--temp-dir <DIR>]` reads a JSON array from
standard input. Each entry supplies `name` and `new_version`; other fields are
ignored. Run it from the candidate workspace. An empty array succeeds without
running Cargo.

Ark packages exactly the listed versions together with Cargo's normal package
verification and registry selection. It then runs the tests from each `.crate`
archive separately with `--all-features` and `--no-default-features`. Extracted
candidates are available to tests through a crates.io patch; Cargo resolves
remaining dependencies using the workspace's Cargo configuration, including
any existing source overrides.

Packaging and archive tests use a temporary directory outside the source
workspace. The optional `--temp-dir` selects its parent. Ark reports errors
and exits nonzero if it cannot complete packaging or testing. It does not
discover candidates or publish crates. Cargo may update the workspace lockfile
during packaging and the extracted lockfiles during testing.
