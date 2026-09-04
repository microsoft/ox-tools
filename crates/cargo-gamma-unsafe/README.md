<div align="center">
 <img src="./logo.png" alt="Cargo-Gamma-Unsafe Logo" width="96">

# Cargo-Gamma-Unsafe

[![crates.io](https://img.shields.io/crates/v/cargo-gamma-unsafe.svg)](https://crates.io/crates/cargo-gamma-unsafe)
[![docs.rs](https://docs.rs/cargo-gamma-unsafe/badge.svg)](https://docs.rs/cargo-gamma-unsafe)
[![MSRV](https://img.shields.io/crates/msrv/cargo-gamma-unsafe)](https://crates.io/crates/cargo-gamma-unsafe)
[![CI](https://github.com/microsoft/ox-tools/actions/workflows/main.yml/badge.svg?event=push)](https://github.com/microsoft/ox-tools/actions/workflows/main.yml)
[![Coverage](https://codecov.io/gh/microsoft/ox-tools/graph/badge.svg?token=FCUG0EL5TI)](https://codecov.io/gh/microsoft/ox-tools)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
<a href="../.."><img src="../../logo.svg" alt="This crate was developed as part of the Oxidizer project" width="20"></a>

</div>

Platform calls that [`cargo-gamma`][__link0] cannot make safely
are concentrated behind an interface that is safe to call. This crate is an implementation
detail of the tool; you should never need to depend on it directly.

Two things the tool does have no safe expression in `std`: killing a whole process subtree (a
process group on Unix, a job object on Windows) and bounding what that subtree allocates (a
cgroup leaf on Linux, the same job object on Windows). Neither is a case of reaching for
`unsafe` to go faster — there is no safe version to prefer.

Concentrating those calls here is what lets every other crate in the workspace carry
`#![forbid(unsafe_code)]`, which turns “we reviewed the unsafe code” into a property the
compiler checks on every build. `cargo-gamma-rt` is the one exception, and only because it is
injected into the dependency graph of the crate under test and so can depend on nothing at
all.

Policy does not live here. What a memory ceiling *should* be is arithmetic on a baseline
measurement, and it stays in `cargo-gamma-lib` where it can be tested without a kernel. This
crate answers “what can the platform do, and do it”; its caller answers “what should we ask
for”.


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-gamma-unsafe">source code</a>.
</sub>

 [__link0]: https://crates.io/crates/cargo-gamma
