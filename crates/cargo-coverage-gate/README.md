<div align="center">
 <img src="./logo.png" alt="Cargo-Coverage-Gate Logo" width="96">

# Cargo-Coverage-Gate

[![crate.io](https://img.shields.io/crates/v/cargo-coverage-gate.svg)](https://crates.io/crates/cargo-coverage-gate)
[![docs.rs](https://docs.rs/cargo-coverage-gate/badge.svg)](https://docs.rs/cargo-coverage-gate)
[![MSRV](https://img.shields.io/crates/msrv/cargo-coverage-gate)](https://crates.io/crates/cargo-coverage-gate)
[![CI](https://github.com/microsoft/ox-tools/actions/workflows/main.yml/badge.svg?event=push)](https://github.com/microsoft/ox-tools/actions/workflows/main.yml)
[![Coverage](https://codecov.io/gh/microsoft/ox-tools/graph/badge.svg?token=FCUG0EL5TI)](https://codecov.io/gh/microsoft/ox-tools)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
<a href="../.."><img src="../../logo.svg" alt="This crate was developed as part of the Oxidizer project" width="20"></a>

</div>

## cargo-coverage-gate

A pull-request-time gate that compares per-crate line coverage produced
by [`cargo-llvm-cov`][__link0] against per-crate thresholds carried in
`Cargo.toml`. The accompanying `cargo-coverage-gate` binary reads the
coverage JSON report, resolves each crate’s threshold from a small
three-layer lookup, and emits a verdict table to stdout (and,
optionally, to a Markdown summary file for CI step summaries).

The full design is in [`docs/design/main.md`][__link1] in the source tree;
the implementation plan tracking the build is in
[`docs/implementation-plans/0000.md`][__link2].

### Threshold resolution

For each workspace member, the effective threshold is the first match
among:

1. `[package.metadata.coverage-gate] min-lines = N` in the crate’s
   `Cargo.toml`,
1. `[workspace.metadata.coverage-gate] min-lines = N` in the workspace
   root `Cargo.toml`, or
1. The built-in default of `100.0` — full coverage required.

Setting `min-lines = 0.0` explicitly opts a crate out of gating.

### Binary usage

```text
cargo coverage-gate  [--json <path>] [--crates <name>,<name>,...]
                     [--summary-file <path>] [--quiet]
```

Exit codes: `0` if every gated crate meets its threshold, `1` if any
gated crate falls below its threshold, and `2` for configuration
errors (unparseable JSON, missing data for a gated crate, an unknown
crate name in `--crates`, an out-of-range `min-lines` value, …).

When `--summary-file` is unset, the binary falls back to
`$GITHUB_STEP_SUMMARY` and then `$COVERAGE_GATE_SUMMARY` to decide
where to write the Markdown verdict table.

### Library usage

```rust
use std::io;

let json = std::fs::read_to_string("target/coverage/coverage.json")?;
let report = cargo_coverage_gate::evaluate(&json, None, &[])?;
report.render_text(&mut io::stdout())?;
let code = report.verdict().exit_code();
```

### Public API

The library exposes [`evaluate`][__link3], which returns an
[`EvaluatedReport`][__link4]. The report can be rendered as plain text via
[`EvaluatedReport::render_text`][__link5] or as GitHub-flavored Markdown
via [`EvaluatedReport::render_markdown`][__link6], and reduced to a single
[`Verdict`][__link7] via [`EvaluatedReport::verdict`][__link8]. The accompanying
binary loads the JSON from disk and orchestrates rendering plus
the appropriate exit code.


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-coverage-gate">source code</a>.
</sub>

 [__cargo_doc2readme_dependencies_info]: ggGmYW0CYXZlMC43LjJhdIQbYLuo4OFUWT8bvMCT2d1BCU8bCvLHCBSvMr0bKR38GpAvnJ5hYvRhcoQbP-8cqWEOtXwbMM-s6Ic4_tUbvKIQoxjk1TobshmFp8qNqMZhZIGDc2NhcmdvLWNvdmVyYWdlLWdhdGVlMC4xLjBzY2FyZ29fY292ZXJhZ2VfZ2F0ZQ
 [__link0]: https://github.com/taiki-e/cargo-llvm-cov
 [__link1]: https://github.com/microsoft/ox-tools/blob/main/crates/cargo-coverage-gate/docs/design/main.md
 [__link2]: https://github.com/microsoft/ox-tools/blob/main/crates/cargo-coverage-gate/docs/implementation-plans/0000.md
 [__link3]: https://docs.rs/cargo-coverage-gate/0.1.0/cargo_coverage_gate/fn.evaluate.html
 [__link4]: https://docs.rs/cargo-coverage-gate/0.1.0/cargo_coverage_gate/struct.EvaluatedReport.html
 [__link5]: https://docs.rs/cargo-coverage-gate/0.1.0/cargo_coverage_gate/?search=EvaluatedReport::render_text
 [__link6]: https://docs.rs/cargo-coverage-gate/0.1.0/cargo_coverage_gate/?search=EvaluatedReport::render_markdown
 [__link7]: https://docs.rs/cargo-coverage-gate/0.1.0/cargo_coverage_gate/enum.Verdict.html
 [__link8]: https://docs.rs/cargo-coverage-gate/0.1.0/cargo_coverage_gate/?search=EvaluatedReport::verdict
