# cargo-gamma-attrs-impl — Design

> Status: **Implemented**.
> Crate name: `cargo-gamma-attrs-impl`.

## Purpose

This ordinary library implements parsing and validation for the inert
attributes exported by `cargo-gamma-attrs`.

## Boundaries

- This crate must remain a normal library, not a proc-macro crate. Keeping the
  logic outside rustc makes it directly testable and mutation-testable.
- It accepts exactly one Rust expression where an attribute promises an
  expression and rejects unsupported keys or malformed selectors.
- It returns the original item unchanged after validation.

## Stability

The crate is published only to support `cargo-gamma-attrs`. Its Rust API is an
implementation detail, so its rustdoc is hidden and its hand-written README
warns downstream users not to depend on it. The diagnostics and accepted
attribute syntax are the user-visible contract.
