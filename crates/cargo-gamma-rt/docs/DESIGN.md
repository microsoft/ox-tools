# cargo-gamma-rt — Design

> Status: **Implemented**.
> Crate name: `cargo-gamma-rt`.

## Purpose

This crate is the guard runtime injected into crates under mutation test. It
selects one compiled mutation at process start while keeping the inactive path
equivalent to the original program.

## Hard constraints

- Zero ordinary dependencies.
- No features and no build script.
- `no_std` compatibility.
- The library target remains named `gamma_rt`.

These constraints prevent injection from perturbing dependency resolution,
feature unification, offline builds, or the target crate's standard-library
requirements.

## Selection and census protocol

The runtime captures `GAMMA_ACTIVE` and `GAMMA_CENSUS` during process startup,
then guards use only the captured atomic selection. Linux reads the immutable
environment image retained by `exec`, avoiding races with later environment
mutation. `OVERFLOW` and `SEAL` are public wire-format constants shared with
the census reader so the protocol has one source of truth.

The vendored standalone crate inherits the workspace edition and minimum Rust
version from the same manifest values used to build this crate.
