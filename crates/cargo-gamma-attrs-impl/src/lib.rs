// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(all(test, miri)))]
#![doc(hidden)]
#![forbid(
    unsafe_code,
    reason = "every raw platform call in this workspace lives in `cargo-gamma-unsafe`, behind a safe interface"
)]

//! The implementation behind [`cargo-gamma-attrs`](https://crates.io/crates/cargo-gamma-attrs),
//! which is where the inert `#[gamma::skip]`, `#[gamma::expect_survived]` and
//! `#[gamma::expect_killed]` attributes are actually exposed.
//!
//! You almost certainly want that crate instead. This one is a normal library rather than a
//! proc-macro crate so its logic can be called by ordinary tests, covered, and mutation tested.
//! What remains in the proc-macro crate is a shim thin enough to read at a glance.
//!
//! # Why this crate exists
//!
//! `cargo-gamma-attrs` is a proc-macro crate, and a proc macro's code runs only inside `rustc`,
//! while some *other* crate is being compiled. That puts it beyond the reach of both measurements
//! this project cares about:
//!
//! - A coverage harness collects counters from test binaries. A proc macro increments its counters
//!   inside the compiler, which writes no profile the harness sees.
//! - A mutation run selects one mutant per test process at run time. A proc macro has already
//!   finished by then, so none of its mutants can be active while a test is watching.
//!
//! Splitting the logic into an ordinary library makes it reachable by coverage and mutation tests.
//! The proc-macro crate remains a thin shim.
//!
//! # What the macros accept
//!
//! See the [`cargo-gamma-attrs`](https://docs.rs/cargo-gamma-attrs) documentation for the
//! user-facing description. In brief: a comma-separated selector list, optionally followed by
//! `reason = "..."` and `tag = "..."`, both of which must be string literals.
//!
//! `#[gamma::value(<expr>)]` instead takes an expression. It is checked by [`value`], because its
//! argument is spliced into the user's crate as a mutant and must be exactly one expression.
//!
//! # Stability
//!
//! This crate is an implementation detail of `cargo-gamma-attrs` and carries no stability
//! guarantee of its own. Depend on `cargo-gamma-attrs`.

mod implementation;

pub use implementation::{CHAIN_FACTOR, MOST_FACTOR, NESTING_LIMIT, inert, inert_timeout, value};
