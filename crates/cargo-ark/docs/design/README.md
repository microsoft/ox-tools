# Cargo Ark

Anvil Release Kit (ARK) provides tools for Rust release workflows through `cargo ark`.

## Release dependency validation

`cargo ark release-dependencies [--temp-dir <DIR>]` reads a JSON array from
standard input. Each entry supplies `name` and `new_version`; other fields are
ignored. Run it from the candidate workspace. An empty array succeeds without
running Cargo.

Repeated entries for the same name and version count as one candidate. Cargo
requires workspace members to have distinct package names, even when their
versions differ, so a report from one workspace snapshot cannot contain
conflicting versions. If a hand-written or combined report lists different
versions for the same name, Ark rejects it and names both versions in the
error.

Ark packages exactly the listed versions together with Cargo's normal package
verification and registry selection. It then runs the tests from each `.crate`
archive separately with `--all-features`, with `--no-default-features`, and with
no feature flags so Cargo enables the package's `default` feature if one exists.
Extracted candidates are available to tests through a crates.io patch; Cargo
resolves remaining dependencies using the workspace's Cargo configuration,
including any existing source overrides. A relative `CARGO_HOME` is resolved
from the directory where Ark was invoked.

Ark extracts archives and writes build outputs to a temporary directory outside
the source workspace. This keeps validation artifacts out of the checkout and
helps catch tests that rely on files present only in the checkout. The optional
`--temp-dir` selects the parent directory; Ark rejects a parent that resolves
to the workspace or a directory inside it. If the workspace `Cargo.lock` is
out of date, `cargo package` may update it in your checkout.

The command reports errors and exits nonzero if the input is invalid or if
packaging or testing fails. It does not discover candidates or publish crates.

The `cargo_ark` library implements release dependency validation in `check`.
The CLI validates the report with `Candidates::from_report` and, if there
are candidates, uses `CargoInvoker::discover` to locate the workspace and
prepare Cargo. The CLI passes both inputs to `check` and reports the result.
Discovery resolves relative `CARGO_HOME` values against the invocation
directory, whether inherited from the environment or supplied as an override.
An explicit override takes precedence, including an empty value, which lets
Cargo use its default.
The invoker owns the scratch directory and is consumed by `check`, tying that
directory to one run. Callers with a known workspace and toolchain can use
`CargoInvoker::new` to skip discovery. Integration tests use this path: `check`
still runs Cargo to package and test the archives without launching the Ark binary.
