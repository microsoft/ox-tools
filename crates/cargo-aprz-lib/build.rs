// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Declares the crate's custom `cfg` names so `unexpected_cfgs` stays quiet without
//! having to extend the workspace-wide `check-cfg` list.
//!
//! - `all_tables`: compile the full set of crates.io database table readers, rather than
//!   only the subset the tool actually consumes.
//! - `all_fields`: compile every column of those tables, rather than only the columns the
//!   tool actually consumes.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(all_tables)");
    println!("cargo::rustc-check-cfg=cfg(all_fields)");
    println!("cargo::rerun-if-changed=build.rs");
}
