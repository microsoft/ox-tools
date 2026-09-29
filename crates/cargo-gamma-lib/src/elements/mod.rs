// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The `mutation-testing-elements` report schema.
//!
//! This is the interchange format the Stryker report viewers consume, and emitting it is what
//! gives cargo-gamma a report UI, an Azure DevOps extension and a GitHub integration without
//! writing any of them. The schema is a published artifact of another project, so the mapping is
//! spelled out here rather than left implicit: drift is silent and shows up as a blank page in
//! someone's browser rather than as a failing build.

mod digest;
mod population;
mod publication;
mod report;

#[doc(inline)]
pub use digest::{Digest, FileDigest, FrameworkDigest, MutantDigest, settled_mutants};
pub(crate) use digest::{gamma_outcome, surviving_mutants};
pub(crate) use population::context_key;
#[doc(inline)]
pub use population::{Observation, Population, PopulationAssertion, PopulationOrigin, Scope, SelectionKind};
pub(crate) use publication::{
    Publication, remove_if_unchanged, remove_if_unchanged_locked, write_if_unchanged, write_if_unchanged_locked, write_streamed,
};
#[cfg(test)]
pub(crate) use publication::{before_next_publication, fail_next_directory_sync, next_scratch_path};
#[doc(inline)]
pub use publication::{publish, write};
pub(crate) use report::{FRAMEWORK_NAME, build_from, supported_schema_version, validate_file_result, validate_schema};
#[doc(inline)]
pub use report::{
    FileResult, Framework, Location, MergeProvenance, MutantResult, Position, Report, RunInfo, ShardInfo, SourceProvenance, Thresholds,
    VerdictProvenance, build, to_json, write_json,
};
