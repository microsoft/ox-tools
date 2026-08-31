// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Coordinator adapter for engine-owned schema instrumentation.

use cargo_gamma_engine::schema::AssignedMutant;
pub use cargo_gamma_engine::schema::{EITHER_PATH, GUARD_PATH, Guard, Position};

use crate::model::Mutant;
use crate::{HashMap, Result};

pub fn instrument(text: &str, mutants: &[&Mutant]) -> Result<String> {
    let sites = sites(mutants);

    cargo_gamma_engine::schema::instrument(text, &sites).map_err(Into::into)
}

pub fn instrument_with_guards(text: &str, mutants: &[&Mutant]) -> Result<(String, HashMap<u32, Guard>)> {
    let sites = sites(mutants);

    cargo_gamma_engine::schema::instrument_with_guards(text, &sites).map_err(Into::into)
}

fn sites<'a>(mutants: &[&'a Mutant]) -> Vec<AssignedMutant<'a>> {
    mutants
        .iter()
        .map(|mutant| AssignedMutant::from_parts(mutant.ordinal, &mutant.span, &mutant.replacement, mutant.shape))
        .collect()
}
