// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Data collection and aggregation for Rust crates
//!
//! This module is responsible for gathering comprehensive information about Rust crates
//! from multiple external sources. It collects data from crates.io, GitHub repositories,
//! `RustSec` advisory database, code coverage services, and documentation hosting.
//!
//! # Implementation Model
//!
//! The core type is [`CrateFacts`], which aggregates data from various providers:
//! - **Crates.io data**: Version info, downloads, dependencies, metadata
//! - **Repository hosting**: GitHub stats (stars, forks, issues, commits)
//! - **Security advisories**: `RustSec` vulnerability and warning counts
//! - **Code analysis**: Line counts, unsafe usage, CI workflow detection
//! - **Coverage data**: Test coverage percentages from external services
//! - **Documentation**: Docs.rs metrics like doc coverage and broken links
//!
//! Each data source is wrapped in a [`ProviderResult`] which can be `Found`, `NotFound`,
//! or `Error`, allowing the system to gracefully handle partial data availability.
//!
//! The [`Collector`] orchestrates parallel data fetching with caching and rate limiting.
//! It uses a request tracker to deduplicate concurrent requests and maintains both
//! document-based caching (for raw API responses) and lock-based caching (for parsed
//! facts) to minimize redundant work and API calls.

pub mod advisories;
pub mod cache;
mod cache_lock;
pub(crate) mod codebase;
mod collector;
pub mod coverage;
mod crate_facts;
mod crate_ref;
mod crate_spec;
pub mod crates;
pub mod docs;
pub mod endpoints;
pub mod hosting;
mod path_utils;
mod progress;
mod provider_result;
mod repo_spec;
mod request_tracker;
pub(crate) mod resilient_http;
#[cfg(test)]
#[cfg(not(miri))]
pub(crate) mod test_logging;
pub(crate) mod throttler;

pub use collector::Collector;
pub use crate_facts::CrateFacts;
pub use crate_ref::CrateRef;
pub use crate_spec::CrateSpec;
pub use crates::CratesData;
#[cfg(feature = "internals")]
pub use endpoints::DEFAULT_ADVISORY_URL;
pub use endpoints::Endpoints;
pub use hosting::BugLabelMatcher;
pub use progress::Progress;
pub use provider_result::ProviderResult;
pub use repo_spec::RepoSpec;
#[cfg(feature = "internals")]
pub use request_tracker::RequestTracker;
