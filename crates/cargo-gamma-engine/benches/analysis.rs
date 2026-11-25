// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use core::hint::black_box;
use std::fmt::Write as _;
use std::time::Instant;

use cargo_gamma_engine::ops::collect::collect;
use cargo_gamma_engine::ops::registry::Selection;
use cargo_gamma_engine::parse::SourceFile;
use cargo_gamma_engine::schema::{AssignedMutant, Ordinal, instrument_with_guards};

const SAMPLES: u32 = 10;

fn main() {
    let selection = Selection::parse("literal.int_to_zero,arith.add_to_sub,relational.gt_to_ge,fn_value.empty_collection")
        .expect("the benchmark uses registered mutator names");
    for modules in [32, 128, 512] {
        let mut text = String::new();
        for index in 0..modules {
            writeln!(text, "mod sample_{index} {{ {} }}", include_str!("analysis/workload.rs.fixture"))
                .expect("formatting into a String cannot fail");
        }
        let file = SourceFile::parse("analysis.rs", text).expect("the benchmark workload is valid Rust");
        let candidates = collect(&file, &selection);
        let assigned = candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                AssignedMutant::from_parts(
                    Ordinal::new(u32::try_from(index + 1).expect("the fixed workload has fewer than u32::MAX mutants")),
                    &candidate.span,
                    &candidate.replacement,
                    candidate.shape,
                )
            })
            .collect::<Vec<_>>();
        let label = format!("modules={modules} bytes={} mutants={}", file.text().len(), candidates.len());
        measure("collect", &label, || collect(black_box(&file), black_box(&selection)));
        measure("instrument_with_guards", &label, || {
            instrument_with_guards(black_box(file.text()), black_box(&assigned)).expect("collected candidates have properly nested spans")
        });
    }
}

fn measure<T>(phase: &str, label: &str, mut run: impl FnMut() -> T) {
    black_box(run());
    let started = Instant::now();
    for _sample in 0..SAMPLES {
        black_box(run());
    }
    println!("{phase}: {label} mean={:?}", started.elapsed() / SAMPLES);
}
