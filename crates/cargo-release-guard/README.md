<div align="center">
 <img src="./logo.png" alt="Cargo-Release-Guard Logo" width="96">

# Cargo-Release-Guard

[![crates.io](https://img.shields.io/crates/v/cargo-release-guard.svg)](https://crates.io/crates/cargo-release-guard)
[![docs.rs](https://docs.rs/cargo-release-guard/badge.svg)](https://docs.rs/cargo-release-guard)
[![MSRV](https://img.shields.io/crates/msrv/cargo-release-guard)](https://crates.io/crates/cargo-release-guard)
[![CI](https://github.com/microsoft/ox-tools/actions/workflows/anvil-scheduled.yml/badge.svg)](https://github.com/microsoft/ox-tools/actions/workflows/anvil-scheduled.yml)
[![Coverage](https://codecov.io/gh/microsoft/ox-tools/graph/badge.svg?token=FCUG0EL5TI)](https://codecov.io/gh/microsoft/ox-tools)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
<a href="../.."><img src="../../logo.svg" alt="This crate was developed as part of the Oxidizer project" width="20"></a>

</div>

Rehearse selected release candidates without unpublished workspace dependency shortcuts.

Invoke `cargo release-guard check --base origin/main --output-dir <fresh-directory>`.
The guard never publishes or edits the development checkout. See `--help` for
candidate-report reuse, explicit selections, and feature/test-runner options.


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-release-guard">source code</a>.
</sub>

