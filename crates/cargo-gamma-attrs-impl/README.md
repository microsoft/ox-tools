<div align="center">
 <img src="./logo.png" alt="Cargo-Gamma-Attrs-Impl Logo" width="96">

# Cargo-Gamma-Attrs-Impl

[![crates.io](https://img.shields.io/crates/v/cargo-gamma-attrs-impl.svg)](https://crates.io/crates/cargo-gamma-attrs-impl)
[![docs.rs](https://docs.rs/cargo-gamma-attrs-impl/badge.svg)](https://docs.rs/cargo-gamma-attrs-impl)
[![MSRV](https://img.shields.io/crates/msrv/cargo-gamma-attrs-impl)](https://crates.io/crates/cargo-gamma-attrs-impl)
[![CI](https://github.com/microsoft/ox-tools/actions/workflows/anvil-pr.yml/badge.svg?event=pull_request)](https://github.com/microsoft/ox-tools/actions/workflows/anvil-pr.yml)
[![Coverage](https://codecov.io/gh/microsoft/ox-tools/graph/badge.svg?token=FCUG0EL5TI)](https://codecov.io/gh/microsoft/ox-tools)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
<a href="../.."><img src="../../logo.svg" alt="This crate was developed as part of the Oxidizer project" width="20"></a>

</div>

The implementation behind [`cargo-gamma-attrs`][__link0],
which is where the inert `#[gamma::skip]`, `#[gamma::expect_survived]` and
`#[gamma::expect_killed]` attributes are actually exposed.

You almost certainly want that crate instead. This one is a normal library rather than a
proc-macro crate so its logic can be called by ordinary tests, covered, and mutation tested.
What remains in the proc-macro crate is a shim thin enough to read at a glance.

## Why this crate exists

`cargo-gamma-attrs` is a proc-macro crate, and a proc macro’s code runs only inside `rustc`,
while some *other* crate is being compiled. That puts it beyond the reach of both measurements
this project cares about:

* A coverage harness collects counters from test binaries. A proc macro increments its counters
  inside the compiler, which writes no profile the harness sees.
* A mutation run selects one mutant per test process at run time. A proc macro has already
  finished by then, so none of its mutants can be active while a test is watching.

Splitting the logic into an ordinary library makes it reachable by coverage and mutation tests.
The proc-macro crate remains a thin shim.

## What the macros accept

See the [`cargo-gamma-attrs`][__link1] documentation for the
user-facing description. In brief: a comma-separated selector list, optionally followed by
`reason = "..."` and `tag = "..."`, both of which must be string literals.

`#[gamma::value(<expr>)]` instead takes an expression. It is checked by [`value`][__link2], because its
argument is spliced into the user’s crate as a mutant and must be exactly one expression.

## Stability

This crate is an implementation detail of `cargo-gamma-attrs` and carries no stability
guarantee of its own. Depend on `cargo-gamma-attrs`.


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-gamma-attrs-impl">source code</a>.
</sub>

 [__cargo_doc2readme_dependencies_info]: ggGmYW0CYXZlMC43LjNhdIQbFhzZ8rzWNNYbuRaDSGWynFgbH4PMdoT7GNcbVwNPtPjAhvFhYvRhcoQbH5RUmmY8e-sbYyqmHPyeK9obgdLJAJ7T65AbUAUW0Y4uz2thZIGDdmNhcmdvLWdhbW1hLWF0dHJzLWltcGxlMC4yLjB2Y2FyZ29fZ2FtbWFfYXR0cnNfaW1wbA
 [__link0]: https://crates.io/crates/cargo-gamma-attrs
 [__link1]: https://docs.rs/cargo-gamma-attrs
 [__link2]: https://docs.rs/cargo-gamma-attrs-impl/0.2.0/cargo_gamma_attrs_impl/?search=value
