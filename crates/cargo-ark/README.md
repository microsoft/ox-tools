<div align="center">
 <img src="./logo.png" alt="Cargo-Ark Logo" width="96">

# Cargo-Ark

[![crates.io](https://img.shields.io/crates/v/cargo-ark.svg)](https://crates.io/crates/cargo-ark)
[![docs.rs](https://docs.rs/cargo-ark/badge.svg)](https://docs.rs/cargo-ark)
[![MSRV](https://img.shields.io/crates/msrv/cargo-ark)](https://crates.io/crates/cargo-ark)
[![CI](https://github.com/microsoft/ox-tools/actions/workflows/anvil-scheduled.yml/badge.svg)](https://github.com/microsoft/ox-tools/actions/workflows/anvil-scheduled.yml)
[![Coverage](https://codecov.io/gh/microsoft/ox-tools/graph/badge.svg?token=FCUG0EL5TI)](https://codecov.io/gh/microsoft/ox-tools)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
<a href="../.."><img src="../../logo.svg" alt="This crate was developed as part of the Oxidizer project" width="20"></a>

</div>

Anvil Release Kit (ARK) provides tools for Rust release workflows through `cargo ark`.

## Release dependency validation

`cargo ark release-dependencies` checks whether the selected packages build and
pass their shipped tests from their `.crate` archives. Run it from the workspace
and pass the candidates as a JSON array on standard input:

```sh
echo '[{"name":"my-crate","new_version":"1.2.0"}]' | cargo ark release-dependencies
```

The command runs each package’s tests with all features, with no default
features, and without feature flags (Cargo’s default selection).
It does not discover candidates or publish packages.


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-ark">source code</a>.
</sub>

