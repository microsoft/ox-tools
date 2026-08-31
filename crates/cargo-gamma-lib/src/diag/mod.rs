// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! What a run cost, said two ways.
//!
//! This is not [`crate::advise`]. Advice is written for someone whose run was slow and who wants
//! to know what to do about it, so it withholds anything they cannot act on. These withhold
//! nothing.
//!
//! [`render`] is the prose dump behind `--diag`: unstable, undocumented, and written for people
//! working on the tool, so that a change to the scheduler, the build sequencing or the mutator
//! catalog can be judged against numbers rather than against how the run felt. It goes to the
//! diagnostic stream, so it composes with piping the results somewhere.
//!
//! [`bundle`] is the same measurements as a versioned document, written for someone else to read.
//! A user reporting that a run was slow has no way to show us why and we have no way to ask for it,
//! so what arrives in an issue is a screenshot or a paraphrase. The bundle is the thing to attach —
//! which is why it carries no source text and hashes the identifiers by default.

mod bundle;
mod render;

pub use bundle::{Bundle, Context, Redaction, bundle, to_json};
pub use render::render;
