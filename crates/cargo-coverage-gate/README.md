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

### Threshold resolution

For each workspace member, the effective threshold is the first match
among:

1. `[package.metadata.coverage-gate] min-lines-percent = N` in the package’s
   `Cargo.toml`,
1. `[workspace.metadata.coverage-gate] min-lines-percent = N` in the workspace
   root `Cargo.toml`, or
1. The built-in default of `100.0` — full coverage required.

Setting `min-lines-percent = 0.0` explicitly opts a package out of
gating (it always passes, regardless of attributed data). A package
that legitimately contains no coverable lines (pure re-exports, type
definitions, a thin binary shim) instead declares
`expect-no-coverable-lines = true`: the gate passes only while that
holds and fails — as a regression — if coverable lines later appear.
The two keys are mutually exclusive, and `expect-no-coverable-lines`
is package-scoped only.

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

The library exposes [`evaluate`][__link1], which returns an
[`EvaluatedReport`][__link2]. The report can be rendered as plain text via
[`EvaluatedReport::render_text`][__link3] or as GitHub-flavored Markdown
via [`EvaluatedReport::render_markdown`][__link4], and reduced to a single
[`Verdict`][__link5] via [`EvaluatedReport::verdict`][__link6]. The accompanying
binary loads the lcov tracefile from disk and orchestrates rendering
plus the appropriate exit code.


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-coverage-gate">source code</a>.
</sub>

 [__cargo_doc2readme_dependencies_info]: ggGkYW0CYXSEGxYc2fK81jTWG7kWg0hlspxYGx-DzHaE-xjXG1cDT7T4wIbxYXKEGw80cH9KnXVkG0Ik84btPmxNG1_q7rZL3w7mGyE4F77TF4kVYWSBg3NjYXJnby1jb3ZlcmFnZS1nYXRlZTAuMy4wc2NhcmdvX2NvdmVyYWdlX2dhdGU
 [__link0]: https://github.com/taiki-e/cargo-llvm-cov
 [__link1]: https://docs.rs/cargo-coverage-gate/0.3.0/cargo_coverage_gate/fn.evaluate.html
 [__link2]: https://docs.rs/cargo-coverage-gate/0.3.0/cargo_coverage_gate/struct.EvaluatedReport.html
 [__link3]: https://docs.rs/cargo-coverage-gate/0.3.0/cargo_coverage_gate/?search=EvaluatedReport::render_text
 [__link4]: https://docs.rs/cargo-coverage-gate/0.3.0/cargo_coverage_gate/?search=EvaluatedReport::render_markdown
 [__link5]: https://docs.rs/cargo-coverage-gate/0.3.0/cargo_coverage_gate/enum.Verdict.html
 [__link6]: https://docs.rs/cargo-coverage-gate/0.3.0/cargo_coverage_gate/?search=EvaluatedReport::verdict
