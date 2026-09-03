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
coverage lcov tracefile, resolves each package’s base policy from a small
three-layer lookup, applies any matching package target policy, and emits a
verdict table to stdout (and,
optionally, to a Markdown summary file for CI step summaries). A failing
verdict includes actionable details without relying on a later
coverage-service upload. A coverable line is a distinct LCOV `DA:` record.
Numeric failures show exact covered/coverable counts and uncovered ranges;
`expect-no-coverable-lines` failures show the unexpected coverable ranges;
and `NO DATA` explains that no records were attributed. Location lists are
bounded, with an exact count of omitted locations.

### Configuration

#### Numeric thresholds

A workspace can define the default line-coverage threshold:

```toml
# Illustrative workspace policy.
[workspace.metadata.coverage-gate]
min-lines-percent = 80
```

Individual packages can override it:

```toml
# Illustrative package policy, intentionally stricter than the workspace.
[package.metadata.coverage-gate]
min-lines-percent = 95
```

For each workspace member, the base threshold is the first match
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

A package can replace its base policy for a Cargo-style target selector:

```toml
[package.metadata.coverage-gate]
min-lines-percent = 100

[package.metadata.coverage-gate.target.'cfg(not(windows))']
expect-no-coverable-lines = true

[package.metadata.coverage-gate.target.x86_64-unknown-linux-gnu]
min-lines-percent = 100
```

A target-specific no-coverable-lines assertion uses the same nesting:

```toml
[package.metadata.coverage-gate.target.thumbv7em-none-eabihf]
expect-no-coverable-lines = true
```

Target tables are package-scoped; they are invalid in workspace metadata.
Their keys accept exact Rust target triples or quoted `cfg(...)` expressions
using the target-derived subset of Cargo’s target grammar. Target
configuration options such as `windows`, `unix`, `target_os`, and
`target_arch` are supported. Build-context options such as `feature`, `test`,
`debug_assertions`, and `proc_macro` are rejected because a standalone target
query cannot evaluate them. A selected target table sets either
`min-lines-percent` or `expect-no-coverable-lines = true`, completely
replacing the package’s base policy to produce its effective policy. Exact
triples take precedence over matching `cfg(...)` expressions. Multiple
matching cfg policies are a configuration error rather than depending on
declaration order.

A zero target-specific threshold disables gating on the matching target,
but does not disable test execution or instrumentation. Those test binaries
remain instrumented because they may contribute coverage to other packages.
If cargo-llvm-cov reports that an instrumented run produced no coverage
data, automation can supply an empty lcov tracefile: zero-threshold and
`expect-no-coverable-lines` packages pass, while positively gated packages
report `NO DATA`.

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

[`evaluate`][__link1] gates one lcov tracefile for the rustc host target, while
[`evaluate_many`][__link2] merges multiple tracefiles at line level.
[`evaluate_many_for_target`][__link3] evaluates a selected Rust target, which may be
supplied explicitly or omitted to select the rustc host target.
Evaluation returns an [`EvaluatedReport`][__link4], which renders as plain
text via [`EvaluatedReport::render_text`][__link5] or GitHub-flavored Markdown via
[`EvaluatedReport::render_markdown`][__link6] and reduces to a [`Verdict`][__link7] via
[`EvaluatedReport::verdict`][__link8]. The accompanying binary loads tracefiles from
disk and orchestrates rendering plus the appropriate exit code.


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-coverage-gate">source code</a>.
</sub>

 [__cargo_doc2readme_dependencies_info]: ggGmYW0CYXZlMC43LjNhdIQbFhzZ8rzWNNYbuRaDSGWynFgbH4PMdoT7GNcbVwNPtPjAhvFhYvRhcoQb3wjNoVxGaCAbpkmpjr98NCcbw-HRsqJQXfkb8-afvWiSredhZIGDc2NhcmdvLWNvdmVyYWdlLWdhdGVlMC40LjBzY2FyZ29fY292ZXJhZ2VfZ2F0ZQ
 [__link0]: https://github.com/taiki-e/cargo-llvm-cov
 [__link1]: https://docs.rs/cargo-coverage-gate/0.4.0/cargo_coverage_gate/fn.evaluate.html
 [__link2]: https://docs.rs/cargo-coverage-gate/0.4.0/cargo_coverage_gate/fn.evaluate_many.html
 [__link3]: https://docs.rs/cargo-coverage-gate/0.4.0/cargo_coverage_gate/fn.evaluate_many_for_target.html
 [__link4]: https://docs.rs/cargo-coverage-gate/0.4.0/cargo_coverage_gate/struct.EvaluatedReport.html
 [__link5]: https://docs.rs/cargo-coverage-gate/0.4.0/cargo_coverage_gate/?search=EvaluatedReport::render_text
 [__link6]: https://docs.rs/cargo-coverage-gate/0.4.0/cargo_coverage_gate/?search=EvaluatedReport::render_markdown
 [__link7]: https://docs.rs/cargo-coverage-gate/0.4.0/cargo_coverage_gate/enum.Verdict.html
 [__link8]: https://docs.rs/cargo-coverage-gate/0.4.0/cargo_coverage_gate/?search=EvaluatedReport::verdict
