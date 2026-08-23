// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Finding the workspace, its packages, and the source files worth mutating.

mod compile_fail;
mod diff;
mod glob;
mod hints;
mod killers;
mod modules;
mod order;
mod plan;
mod record;
mod shard;
mod survey;
mod target_file;
mod workspace_snapshot;

pub use compile_fail::{CompileFailTarget, advice as compile_fail_advice};
pub use diff::Diff;
pub(crate) use glob::Glob;
pub use glob::matches_glob;
pub use hints::{Hints, Promotion, path as hints_path};
pub use killers::Killers;
pub(crate) use order::stages;
pub use plan::Plan;
pub(crate) use record::digest;
pub use record::{
    Context as RecordContext, ContextDigest, Killer, RunRecord, Term, Tier, Trust, context as record_context, rustflags, toolchain,
};
pub use shard::shard_of;
#[cfg(test)]
pub use survey::plan;
pub(crate) use survey::plan_for_build;
pub use survey::{Scanned, Survey, load_metadata};
pub use target_file::TargetFile;
pub(crate) use workspace_snapshot::WorkspaceSnapshot;
