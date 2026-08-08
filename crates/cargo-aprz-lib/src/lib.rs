// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![doc(hidden)]
#![doc(html_logo_url = "https://media.githubusercontent.com/media/microsoft/ox-tools/refs/heads/main/crates/cargo-aprz-lib/logo.png")]
#![doc(html_favicon_url = "https://media.githubusercontent.com/media/microsoft/ox-tools/refs/heads/main/crates/cargo-aprz-lib/favicon.ico")]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![allow(
    clippy::redundant_pub_crate,
    reason = "the crate's modules are private, so this fires on every pub(crate) item; widening them to `pub` is exactly what the private module tree exists to prevent"
)]

//! This is an implementation detail of the cargo-aprz tool. Do not take a dependency on this crate
//! as it may change in incompatible ways without warning.

// Core library for cargo-aprz
//
// This library consolidates all functionality for the cargo-aprz tool, which analyzes
// Rust crates for compliance with user-defined policies.
//
// # Module Organization
//
// - [`commands`]: Command-line interface and orchestration
// - [`facts`]: Data collection and aggregation
// - [`metrics`]: Metric extraction from facts
// - [`expr`]: Expression-based evaluation
// - [`reports`]: Report generation in multiple formats

pub type Result<T, E = ohno::AppError> = core::result::Result<T, E>;
pub(crate) type HashMap<K, V> = rustc_hash::FxHashMap<K, V>;
pub(crate) type HashSet<V> = rustc_hash::FxHashSet<V>;

pub(crate) fn hash_map_with_capacity<K, V>(capacity: usize) -> HashMap<K, V> {
    HashMap::with_capacity_and_hasher(capacity, rustc_hash::FxBuildHasher)
}

pub(crate) fn hash_set_with_capacity<V>(capacity: usize) -> HashSet<V> {
    HashSet::with_capacity_and_hasher(capacity, rustc_hash::FxBuildHasher)
}

mod commands;
mod expr;
mod facts;
mod metrics;
mod reports;

/// Re-exports the crate internals for the crate's own integration tests.
///
/// The modules above are declared privately and unconditionally, which is what keeps dead-code
/// analysis honest: this library's only consumer is the `cargo-aprz` binary, so an item nothing
/// uses is genuinely dead - but a `pub` module makes it look like a deliberate public API that
/// some external crate might call, and the analysis goes quiet.
///
/// Integration tests compile as separate crates and cannot see a private module, so they reach the
/// same items through this facade instead. It declares only inline modules, which is what lets it
/// stay a macro: rustfmt and every `syn`-based tool need to resolve file-bearing `mod` declarations
/// to a path on disk, and there are none here.
///
/// It is gated on a feature rather than on `debug_assertions` because `cargo test --release` turns
/// `debug_assertions` off while still building the integration tests, which then fail to compile.
macro_rules! expose_internals {
    ($($name:ident),+ $(,)?) => {
        #[cfg(feature = "internals")]
        #[doc(hidden)]
        pub mod internals {
            $(
                pub mod $name {
                    pub use crate::$name::*;
                }
            )+
        }
    };
}

expose_internals!(commands, expr, facts, metrics, reports);

pub use crate::commands::{Host, run};
