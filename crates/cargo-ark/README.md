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

Validates Rust release candidates by testing their packaged archives.

Use [`release_dependencies::Candidates`][__link0] to parse a candidate report and
[`release_dependencies::CargoInvoker`][__link1] to prepare Cargo, then pass both to
[`release_dependencies::check`][__link2].

For command-line usage, see the [CLI guide][__link3].

## Example

Check a candidate in the current workspace:

```rust
use cargo_ark::release_dependencies::{Candidates, CargoInvoker, check};

let candidates = Candidates::from_report(r#"[{"name":"my-crate","new_version":"1.2.0"}]"#)?;
let cargo = CargoInvoker::discover(std::env::current_dir()?, Default::default(), None)?;
check(candidates, cargo)?;
```


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-ark">source code</a>.
</sub>

 [__cargo_doc2readme_dependencies_info]: ggGmYW0CYXZlMC43LjNhdIQblRYhli3L8qob_NSi_WYo69wbWnMVqZw3jJwb3u56HnT6RDphYvRhcoQbypUwoXTH24AbXEqeCYa_AiIbOn8uoponDOUbvM-NbkpIwg9hZIGDaWNhcmdvLWFya2UwLjEuMGljYXJnb19hcms
 [__link0]: https://docs.rs/cargo-ark/0.1.0/cargo_ark/?search=release_dependencies::Candidates
 [__link1]: https://docs.rs/cargo-ark/0.1.0/cargo_ark/?search=release_dependencies::CargoInvoker
 [__link2]: https://docs.rs/cargo-ark/0.1.0/cargo_ark/?search=release_dependencies::check
 [__link3]: https://github.com/microsoft/ox-tools/blob/main/crates/cargo-ark/docs/cli.md
