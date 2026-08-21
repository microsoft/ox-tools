// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Benchmarks for assembling the built-in catalog.
//!
//! `Catalog::anvil()` builds the whole artifact set — every embedded
//! template, every per-group fan-out — and `checksum()` renders and hashes
//! all of it. Both run on every `cargo anvil` invocation before anything is
//! written, so their cost is paid by every adopter on every update, and both
//! grow with the catalog: this crate's own history is a steady accretion of
//! checks, groups and backend files.
//!
//! They are also the shape a trend watch handles well — pure, deterministic,
//! no I/O, no network — so a move here is a change in the code rather than in
//! the environment.

use cargo_anvil::Catalog;
use criterion::{Criterion, criterion_group, criterion_main};

fn catalog(c: &mut Criterion) {
    let mut group = c.benchmark_group("catalog");

    // Assembly alone: the embedded templates and the per-group expansions.
    group.bench_function("anvil", |b| b.iter(Catalog::anvil));

    // Assembly plus rendering and hashing every artifact body, which is what
    // the update path pays to decide whether anything changed.
    group.bench_function("checksum", |b| {
        let catalog = Catalog::anvil();
        b.iter(|| catalog.checksum());
    });

    group.finish();
}

criterion_group!(benches, catalog);
criterion_main!(benches);
