<div align="center">
 <img src="./logo.png" alt="Cargo-Gamma-Engine Logo" width="96">

# Cargo-Gamma-Engine

[![crates.io](https://img.shields.io/crates/v/cargo-gamma-engine.svg)](https://crates.io/crates/cargo-gamma-engine)
[![docs.rs](https://docs.rs/cargo-gamma-engine/badge.svg)](https://docs.rs/cargo-gamma-engine)
[![MSRV](https://img.shields.io/crates/msrv/cargo-gamma-engine)](https://crates.io/crates/cargo-gamma-engine)
[![CI](https://github.com/microsoft/ox-tools/actions/workflows/anvil-pr.yml/badge.svg?event=pull_request)](https://github.com/microsoft/ox-tools/actions/workflows/anvil-pr.yml)
[![Coverage](https://codecov.io/gh/microsoft/ox-tools/graph/badge.svg?token=FCUG0EL5TI)](https://codecov.io/gh/microsoft/ox-tools)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
<a href="../.."><img src="../../logo.svg" alt="This crate was developed as part of the Oxidizer project" width="20"></a>

</div>

This crate is an internal implementation detail of
[`cargo-gamma`][__link0]. It contains the Rust source parsing,
mutation collection, stable identity, and schema instrumentation pipeline.

Do not depend on it directly. Its API may change incompatibly without notice; it is published
only so that `cargo-gamma` can be installed through crates.io.


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-gamma-engine">source code</a>.
</sub>

 [__link0]: https://crates.io/crates/cargo-gamma
