// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run with `cargo run -p cargo-anvil --example memory_catalog`.
//! Downstream catalogs need no `test-util` feature.

#[path = "support/memory.rs"]
mod memory;

use cargo_anvil::{Artifact, Backend, Catalog, UpdateOptions, update};
use ohno::AppError;

fn main() -> Result<(), AppError> {
    let catalog = Catalog::anvil()
        .into_builder()
        .subcommand("myforge")
        .version("1.0.0")
        .with_artifact(Artifact::owned_file("justfiles/anvil/extra.just", "extra:\n    @echo extra\n"))
        .build()?;
    let mut repository = memory::MemoryRepository::default();
    repository.insert("Cargo.toml", "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n");
    let mut options = UpdateOptions::default();
    options.backends = vec![Backend::GitHub];

    let generated = update(&catalog, &options, &mut repository)?;
    assert!(generated.applied);
    assert!(repository.files.contains_key(".anvil.lock"));
    let repeat = update(&catalog, &options, &mut repository)?;
    assert!(repeat.in_sync);
    println!("{}", generated.summary);
    Ok(())
}
