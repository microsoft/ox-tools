// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Pins the limit constants the proc-macro hand-copies from this library.
//!
//! `cargo-gamma-attrs-impl` is a proc-macro-support crate that the tool cannot depend on in the
//! direction that would let the two share a constant, so it re-declares three of the library's
//! limits and the crate docs on each side promise, in prose, that they agree. Nothing but this test
//! binds the copies: change `MOST_FACTOR`, `NESTING_LIMIT`, or `CHAIN_FACTOR` on one side alone and
//! the compile-time attribute check and the run-time directive scanner start accepting different
//! values, with the user told about the difference by whichever tool happens to run second and no
//! failing test to catch it. The values are compared symbol-to-symbol so no literal is duplicated
//! here for a later edit to leave stale.

use cargo_gamma_lib::internals::bounds;
use cargo_gamma_lib::internals::parse::nesting;

#[test]
fn the_proc_macro_limits_match_the_library() {
    // `MOST_FACTOR` is a float; comparing the bit patterns is an exact equality that also keeps the
    // pedantic `float_cmp` lint from firing on two constants that are, by construction, identical.
    assert_eq!(
        cargo_gamma_attrs_impl::MOST_FACTOR.to_bits(),
        bounds::MOST_FACTOR.to_bits(),
        "the proc-macro's multiplier ceiling drifted from `bounds::MOST_FACTOR`"
    );
    assert_eq!(
        cargo_gamma_attrs_impl::NESTING_LIMIT,
        nesting::NESTING_LIMIT,
        "the proc-macro's nesting limit drifted from `parse::nesting::NESTING_LIMIT`"
    );
    assert_eq!(
        cargo_gamma_attrs_impl::CHAIN_FACTOR,
        nesting::CHAIN_FACTOR,
        "the proc-macro's chain factor drifted from `parse::nesting::CHAIN_FACTOR`"
    );
}
