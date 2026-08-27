<div align="center">
 <img src="./logo.png" alt="Cargo-Coverage-Gate Logo" width="96">

# Cargo-Coverage-Gate

[![crates.io](https://img.shields.io/crates/v/cargo-coverage-gate.svg)](https://crates.io/crates/cargo-coverage-gate)
[![docs.rs](https://docs.rs/cargo-coverage-gate/badge.svg)](https://docs.rs/cargo-coverage-gate)
[![MSRV](https://img.shields.io/crates/msrv/cargo-coverage-gate)](https://crates.io/crates/cargo-coverage-gate)
[![CI](https://github.com/microsoft/ox-tools/actions/workflows/main.yml/badge.svg?event=push)](https://github.com/microsoft/ox-tools/actions/workflows/main.yml)
[![Coverage](https://codecov.io/gh/microsoft/ox-tools/graph/badge.svg?token=FCUG0EL5TI)](https://codecov.io/gh/microsoft/ox-tools)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
<a href="../.."><img src="../../logo.svg" alt="This crate was developed as part of the Oxidizer project" width="20"></a>

</div>

## cargo-coverage-gate

A pull-request-time gate that compares per-package line coverage produced
by [`cargo-llvm-cov`][__link0] against per-package thresholds carried in
`Cargo.toml`. The accompanying `cargo-coverage-gate` binary reads the
coverage lcov tracefile, resolves each package’s threshold from a small
three-layer lookup, and emits a verdict table to stdout (and,
optionally, to a Markdown summary file for CI step summaries).

### Configuration

#### Numeric thresholds

A workspace can define the default line-coverage threshold:

```toml
[workspace.metadata.coverage-gate]
min-lines-percent = 80
```

Individual packages can override it:

```toml
[package.metadata.coverage-gate]
min-lines-percent = 95
```

For each workspace member, the effective threshold is the first match
among:

1. `[package.metadata.coverage-gate] min-lines-percent = N` in the package’s
   `Cargo.toml`,
1. `[workspace.metadata.coverage-gate] min-lines-percent = N` in the workspace
   root `Cargo.toml`, or
1. The built-in default of `100.0` — full coverage required.

Setting `min-lines-percent = 0.0` explicitly opts a package out of
gating: it always passes, regardless of attributed data. Thresholds must
be in the inclusive range `0.0..=100.0`.

#### Packages with no coverable lines

A package that legitimately contains no coverable lines (pure re-exports,
type definitions, or a thin binary shim) can make that invariant explicit:

```toml
[package.metadata.coverage-gate]
expect-no-coverable-lines = true
```

The gate passes only while the package has no attributed coverable lines
and fails as a regression if coverable code later appears. This differs
from `min-lines-percent = 0`, which keeps passing if the package grows
coverable code. The two keys are mutually exclusive, and
`expect-no-coverable-lines` is package-scoped only.

#### Target-specific policies

A package can replace that policy for a Cargo-style target selector:

```toml
[package.metadata.coverage-gate]
min-lines-percent = 100

[package.metadata.coverage-gate.target.'cfg(not(windows))']
min-lines-percent = 0

[package.metadata.coverage-gate.target.x86_64-pc-windows-msvc]
min-lines-percent = 100
```

A target-specific no-coverable-lines assertion uses the same nesting:

```toml
[package.metadata.coverage-gate.target.thumbv7em-none-eabihf]
expect-no-coverable-lines = true
```

Target keys accept exact Rust target triples or quoted `cfg(...)`
expressions using Cargo’s target grammar. A target table sets either
`min-lines-percent` or `expect-no-coverable-lines = true`, replacing the
package’s base policy for that target. Exact triples take precedence over
matching `cfg(...)` expressions. Multiple matching cfg policies are a
configuration error rather than depending on declaration order.

A zero target-specific threshold disables gating on the matching target,
but does not itself control test execution or instrumentation. Coverage
automation can call `cargo coverage-gate --print-test-only-packages --target <triple>` or [`test_only_packages`][__link1] to
identify packages that should run through a non-instrumented test path.
The command prints one bare package name per line (without `@version`) and
exits successfully without reading lcov.

### Why lcov, not the JSON?

`cargo-llvm-cov` exports the same instrumentation run in several
formats (JSON, lcov, cobertura, codecov-custom-JSON). The gate
consumes lcov because that is what every other coverage report fed by
the same data sees: Codecov ingests lcov uploads directly, ADO
consumes cobertura that cargo-llvm-cov derives from lcov, and the
lcov line semantics (“a line is covered if any region on it was
hit”) match the human reading of “did we hit this line”. The JSON
export uses a stricter “every region on the line must be hit”
interpretation that systematically reports a couple of
percentage-points lower, which makes calibrating thresholds against
Codecov / ADO numbers confusing.

### Binary usage

```text
cargo coverage-gate  [--lcov <path>]... [-p|--package <spec>]...
                     [--target <triple>]
                     [--print-test-only-packages]
                     [--summary-file <path>] [--quiet]
```

`--lcov` may be repeated; the tracefiles are merged at the line level
(per-line counts summed) so multiple feature-config exports
(`--all-features`, `--no-default-features`) can be gated together
without a separate, platform-specific merge step.

Exit codes: `0` if every gated package meets its threshold, `1` if any
gated package falls below its threshold, and `2` for configuration
errors (unparseable lcov, missing data for a gated package, a `--package`
selector that matches no member, an out-of-range `min-lines-percent`
value, …).

When `--summary-file` is unset, the binary falls back to
`$GITHUB_STEP_SUMMARY` and then `$COVERAGE_GATE_SUMMARY` to decide
where to write the Markdown verdict table.

### Library usage

```rust
use std::io;

let lcov = std::fs::read_to_string("target/coverage/lcov.info")?;
let report = cargo_coverage_gate::evaluate(&lcov, None, &[])?;
report.render_text(&mut io::stdout())?;
let code = report.verdict().as_exit_code();
```

### Public API

[`evaluate`][__link2] gates one lcov tracefile for the host target, while
[`evaluate_many`][__link3] merges multiple tracefiles at line level.
[`evaluate_many_for_target`][__link4] evaluates an explicit target triple, and
[`test_only_packages`][__link5] performs the metadata-only package query described
above. Evaluation returns an [`EvaluatedReport`][__link6], which renders as plain
text via [`EvaluatedReport::render_text`][__link7] or GitHub-flavored Markdown via
[`EvaluatedReport::render_markdown`][__link8] and reduces to a [`Verdict`][__link9] via
[`EvaluatedReport::verdict`][__link10]. The accompanying binary loads tracefiles from
disk and orchestrates rendering plus the appropriate exit code.


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-coverage-gate">source code</a>.
</sub>

 [__cargo_doc2readme_dependencies_info]: ggGmYW0CYXZlMC43LjJhdIQbFhzZ8rzWNNYbuRaDSGWynFgbH4PMdoT7GNcbVwNPtPjAhvFhYvRhcoQbdKLP7PcbAPsbeHSPlXhUImobKq_kwe1zsH4bcq-jhiXZ7SNhZIGDc2NhcmdvLWNvdmVyYWdlLWdhdGVlMC40LjBzY2FyZ29fY292ZXJhZ2VfZ2F0ZQ
 [__link0]: https://github.com/taiki-e/cargo-llvm-cov
 [__link1]: https://docs.rs/cargo-coverage-gate/0.4.0/cargo_coverage_gate/fn.test_only_packages.html
 [__link10]: https://docs.rs/cargo-coverage-gate/0.4.0/cargo_coverage_gate/?search=EvaluatedReport::verdict
 [__link2]: https://docs.rs/cargo-coverage-gate/0.4.0/cargo_coverage_gate/fn.evaluate.html
 [__link3]: https://docs.rs/cargo-coverage-gate/0.4.0/cargo_coverage_gate/fn.evaluate_many.html
 [__link4]: https://docs.rs/cargo-coverage-gate/0.4.0/cargo_coverage_gate/fn.evaluate_many_for_target.html
 [__link5]: https://docs.rs/cargo-coverage-gate/0.4.0/cargo_coverage_gate/fn.test_only_packages.html
 [__link6]: https://docs.rs/cargo-coverage-gate/0.4.0/cargo_coverage_gate/struct.EvaluatedReport.html
 [__link7]: https://docs.rs/cargo-coverage-gate/0.4.0/cargo_coverage_gate/?search=EvaluatedReport::render_text
 [__link8]: https://docs.rs/cargo-coverage-gate/0.4.0/cargo_coverage_gate/?search=EvaluatedReport::render_markdown
 [__link9]: https://docs.rs/cargo-coverage-gate/0.4.0/cargo_coverage_gate/enum.Verdict.html
