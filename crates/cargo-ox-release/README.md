<div align="center">
 <img src="./logo.png" alt="Cargo-Ox-Release Logo" width="96">

# Cargo-Ox-Release

[![crates.io](https://img.shields.io/crates/v/cargo-ox-release.svg)](https://crates.io/crates/cargo-ox-release)
[![docs.rs](https://docs.rs/cargo-ox-release/badge.svg)](https://docs.rs/cargo-ox-release)
[![MSRV](https://img.shields.io/crates/msrv/cargo-ox-release)](https://crates.io/crates/cargo-ox-release)
[![CI](https://github.com/microsoft/ox-tools/actions/workflows/main.yml/badge.svg?event=push)](https://github.com/microsoft/ox-tools/actions/workflows/main.yml)
[![Coverage](https://codecov.io/gh/microsoft/ox-tools/graph/badge.svg?token=FCUG0EL5TI)](https://codecov.io/gh/microsoft/ox-tools)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
<a href="../.."><img src="../../logo.svg" alt="This crate was developed as part of the Oxidizer project" width="20"></a>

</div>

`cargo-ox-release`: a deterministic release planner for Oxidizer-style Cargo
workspaces.

Releasing a workspace of many interdependent crates happens in three phases:

1. **facts** — gather a deterministic workspace snapshot: the dependency
   graph, public type exposure, macro publication, and modification state.
1. **resolve** — [`resolve`][__link0] turns that snapshot plus the caller’s classified
   decisions into an exact release plan: token parsing, version arithmetic,
   type- and macro-contract-aware cascades, pin reconciliation, ambiguity
   reporting, and topological ordering. This crate implements this phase.
1. **apply** — write the new versions, changelogs, and READMEs atomically.

The facts snapshot is consumed as JSON (see [`Facts`][__link1]); gathering it and
applying a plan are separate concerns outside this crate’s current scope.

The resolver performs only mechanical work — classifying source diffs and
reviewing proc-macro behavior are the caller’s responsibility, supplied
through the [`Request`][__link2]. Given the same facts and request it always produces
the same plan.


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-ox-release">source code</a>.
</sub>

 [__cargo_doc2readme_dependencies_info]: ggGmYW0CYXZlMC43LjJhdIQbFhzZ8rzWNNYbuRaDSGWynFgbH4PMdoT7GNcbVwNPtPjAhvFhYvRhcoQb8w9NQEWi4rkbmKA_2VELlTcb2iSwJe4Cp-8bXqBYeYEdOt5hZIGDcGNhcmdvLW94LXJlbGVhc2VlMC4xLjBwY2FyZ29fb3hfcmVsZWFzZQ
 [__link0]: https://docs.rs/cargo-ox-release/0.1.0/cargo_ox_release/?search=resolve
 [__link1]: https://docs.rs/cargo-ox-release/0.1.0/cargo_ox_release/?search=model::Facts
 [__link2]: https://docs.rs/cargo-ox-release/0.1.0/cargo_ox_release/?search=model::Request
