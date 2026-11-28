// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

#[path = "../examples/support/memory.rs"]
mod memory;

use cargo_anvil::{Backend, Catalog, CliMeta, UpdateOptions, artifacts, update};
use memory::MemoryRepository;

#[test]
fn this_repository_keeps_receiving_generated_action_updates() {
    let policy = include_str!("../../../.github/dependabot.yml");
    assert!(!policy.contains("exclude-paths:"));
    let mut repo = MemoryRepository::default();
    repo.insert("Cargo.toml", include_str!("fixtures/single-crate/Cargo.toml.in"));
    repo.insert(".github/dependabot.yml", policy);

    let catalog = Catalog::builder(CliMeta::new("anvil"))
        .with_artifact(artifacts::github::setup_action())
        .build()
        .unwrap();
    let mut options = UpdateOptions::default();
    options.backends.push(Backend::GitHub);
    let result = update(&catalog, &options, &mut repo).unwrap();
    assert!(result.refusals.is_empty(), "{}", result.summary);
    assert_eq!(repo.files[".github/dependabot.yml"], policy);
}
