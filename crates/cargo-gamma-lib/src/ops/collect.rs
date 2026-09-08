// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::Arc;

use cargo_gamma_engine::ops::collect::into_definitions;
pub use cargo_gamma_engine::ops::collect::{
    Candidate, Defaults, Shape, check_stated, check_stated_and_collect_with, collect, collect_in, collect_with,
};

use crate::model::Mutant;
use crate::parse::SourceFile;

/// Attaches Cargo package and neutral run state to source-level mutant definitions.
#[must_use]
pub fn into_mutants(file: &SourceFile, package: &str, candidates: Vec<Candidate>) -> Vec<Mutant> {
    let package = Arc::from(package);

    into_definitions(file, candidates)
        .into_iter()
        .map(|definition| Mutant::from_definition(definition, Arc::clone(&package)))
        .collect()
}

/// Attaches Cargo package and neutral run state while retaining short-lived trait selection data.
pub(crate) fn into_mutants_with_traits(file: &SourceFile, package: &str, candidates: Vec<Candidate>) -> Vec<(Mutant, Option<Arc<str>>)> {
    let package = Arc::from(package);

    into_definitions(file, candidates)
        .into_iter()
        .map(|mut definition| {
            let trait_impl = definition.trait_impl.take();
            let mutant = Mutant::from_definition(definition, Arc::clone(&package));

            (mutant, trait_impl)
        })
        .collect()
}
