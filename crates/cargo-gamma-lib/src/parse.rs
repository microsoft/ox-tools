// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Coordinator access to the engine-owned source parser.

pub(crate) use cargo_gamma_engine::parse::{BOM, exceeds_nesting_limit, strip_bom};
pub use cargo_gamma_engine::parse::{Comment, CommentKind, SourceFile, without_bom};

pub mod nesting {
    pub use cargo_gamma_engine::parse::nesting::*;
}
