// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Finding the workspace, its packages, and the source files worth mutating.

mod compile_fail;
mod diff;
mod glob;
mod hints;
mod input;
mod killers;
mod modules;
mod order;
mod plan;
mod record;
mod shard;
mod survey;
mod target_file;
mod workspace_snapshot;

#[doc(inline)]
pub use compile_fail::{CompileFailTarget, advice as compile_fail_advice};
#[doc(inline)]
pub use diff::Diff;
pub(crate) use glob::Glob;
#[doc(inline)]
pub use glob::matches_glob;
#[doc(inline)]
pub use hints::{Hints, Promotion, path as hints_path};
#[doc(inline)]
pub use killers::Killers;
pub(crate) use order::stages;
#[doc(inline)]
pub use plan::Plan;
pub(crate) use record::digest;
#[doc(inline)]
pub use record::{
    Context as RecordContext, ContextDigest, Entries as RecordEntries, Killer, RunRecord, Term, Tier, Trust, context as record_context,
    rustflags, toolchain,
};
#[doc(inline)]
pub use shard::shard_of;
#[cfg(test)]
#[doc(inline)]
pub use survey::plan;
pub(crate) use survey::plan_for_build;
#[doc(inline)]
pub use survey::{Scanned, Survey, load_metadata};
#[doc(inline)]
pub use target_file::TargetFile;
pub(crate) use workspace_snapshot::WorkspaceSnapshot;
