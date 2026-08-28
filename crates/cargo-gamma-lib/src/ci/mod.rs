// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Surfacing a run inside a continuous integration system.
//!
//! A mutation report that lives in an artifact zip is a report nobody reads. The findings have to
//! arrive where the reviewer already is — on the diff, in the job summary, in the security tab —
//! or the tool gets adopted, run nightly, and ignored.
//!
//! Three renderings share one rule: **every undetected mutant is a finding.** A mutant killed by a
//! failing assertion is the tool working; survivors, uncovered sites, timeouts, and memory
//! exhaustion all need attention.

mod annotations;
mod finding;
mod level;
pub(crate) mod sarif;
mod summary;
mod truncation;

pub use annotations::{Annotations, annotations, wanted};
pub use level::Level;
pub use sarif::sarif;
pub(crate) use summary::append;
pub use summary::summary;
pub use truncation::Truncation;
