// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(all(test, miri)))]
#![doc(hidden)]
#![forbid(
    unsafe_code,
    reason = "raw platform calls stay in `cargo-gamma-unsafe`; the source engine has no reason to use them"
)]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![allow(
    clippy::redundant_pub_crate,
    reason = "this non-published crate exposes implementation types only so workspace crates can compose the engine"
)]

//! This crate is an internal implementation detail of
//! [`cargo-gamma`](https://crates.io/crates/cargo-gamma). It contains the Rust source parsing,
//! mutation collection, stable identity, and schema instrumentation pipeline.
//!
//! Do not depend on it directly. Its API may change incompatibly without notice; it is published
//! only so that `cargo-gamma` can be installed through crates.io.

use rustc_hash::{FxHashMap, FxHashSet};

pub(crate) type HashMap<K, V> = FxHashMap<K, V>;
pub(crate) type HashSet<V> = FxHashSet<V>;

/// The engine's error result.
pub type Result<T, E = Error> = core::result::Result<T, E>;

pub mod cfg;
mod error;
pub mod model;
pub mod ops;
pub mod parse;
pub mod schema;
pub mod text;

#[doc(inline)]
pub use error::{Error, Parts};

#[cfg(test)]
mod unwind_contracts {
    use core::panic::{RefUnwindSafe, UnwindSafe};

    use crate::model::{MutantId, SiteIndex};
    use crate::schema::{AssignedMutant, Guard, Ordinal, Position};

    fn assert_unwind_safe<T: UnwindSafe + RefUnwindSafe>() {}

    #[test]
    fn public_value_types_are_unwind_safe() {
        assert_unwind_safe::<MutantId>();
        assert_unwind_safe::<SiteIndex>();
        assert_unwind_safe::<Position>();
        assert_unwind_safe::<Guard>();
        assert_unwind_safe::<Ordinal>();
        assert_unwind_safe::<AssignedMutant<'static>>();
    }
}
