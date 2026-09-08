// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Diagnosis of a mutation run: where the time went, and what can be done about it.
//!
//! Mutation testing is the kind of tool that gets adopted enthusiastically, runs for four hours,
//! and is then quietly deleted from the CI configuration. The run itself does not explain why it
//! was slow, so the only remedies available to a frustrated user are the blunt ones — fewer
//! operators, fewer files, or nothing at all — chosen without knowing what they cost in signal.
//!
//! This module turns a completed run into a list of findings. Each is a measured symptom, a named
//! cause, a remedy, and the signal cost of taking that remedy. The last part is the one that
//! matters: every mitigation here trades information for time, and a recommendation that hides the
//! trade is worse than no recommendation, because it will be taken.
mod analysis;
mod finding;
mod render;
mod text;
mod timing;
mod yield_;

#[doc(inline)]
pub use analysis::{analyze, analyze_run, yields};
#[doc(inline)]
pub use finding::Finding;
#[doc(inline)]
pub use render::{Layout, render_markdown};
#[doc(inline)]
pub use text::human;
#[doc(inline)]
pub use timing::Timing;
#[doc(inline)]
pub use yield_::Yield;
