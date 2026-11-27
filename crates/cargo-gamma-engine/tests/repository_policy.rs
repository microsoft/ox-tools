// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::path::Path;
use std::{env, fs};

use cargo_gamma_engine::parse::exceeds_nesting_limit;
use walkdir::WalkDir;

/// Calibrate the production nesting guard against the real workspace sources.
#[test]
fn this_workspace_is_within_the_limit() {
    // Gamma's instrumented scratch sources intentionally contain deeper guard expressions.
    if env::var_os("CARGO_GAMMA").is_some() {
        return;
    }

    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for entry in WalkDir::new(crates).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "rs")
            && let Ok(text) = fs::read_to_string(path)
        {
            assert!(!exceeds_nesting_limit(&text), "{} is refused by its own guard", path.display());
        }
    }
}
