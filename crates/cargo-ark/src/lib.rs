// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(coverage_nightly, test), feature(coverage_attribute))]

//! Validates Rust release candidates by testing their packaged archives.
//!
//! Use [`release_dependencies::Candidates`] to parse a candidate report and
//! [`release_dependencies::CargoInvoker`] to prepare Cargo, then pass both to
//! [`release_dependencies::check`].
//!
//! For command-line usage, see the [CLI guide].
//!
//! # Example
//!
//! Check a candidate in the current workspace:
//!
//! ```no_run
//! use cargo_ark::release_dependencies::{Candidates, CargoInvoker, check};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let candidates = Candidates::from_report(r#"[{"name":"my-crate","new_version":"1.2.0"}]"#)?;
//! let cargo = CargoInvoker::discover(std::env::current_dir()?, Default::default(), None)?;
//! check(candidates, cargo)?;
//! # Ok(())
//! # }
//! ```
//!
//! [CLI guide]: https://github.com/microsoft/ox-tools/blob/main/crates/cargo-ark/docs/cli.md

pub mod release_dependencies;
