// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Coordinator access to the engine-owned source parser.

pub(crate) use cargo_gamma_engine::parse::{BOM, exceeds_nesting_limit, strip_bom};
pub use cargo_gamma_engine::parse::{Comment, CommentKind, SourceFile, without_bom};

/// The engine's nesting budget, re-exported for the tests that hold the proc-macro to the same one.
///
/// Named item by item rather than re-exported wholesale: a glob would silently take on whatever the
/// engine adds next, and a re-export is a promise about a specific set of names.
pub mod nesting {
    pub use cargo_gamma_engine::parse::nesting::{CHAIN_FACTOR, NESTING_LIMIT};
}
