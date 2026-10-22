// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Finding the workspace, its packages, and the source files worth mutating.

mod compile_fail;
mod compiler_exclusions;
mod diff;
mod glob;
mod hints;
mod input;
mod killers;
mod module_identities;
mod modules;
mod order;
mod plan;
mod record;
mod resource_dependencies;
mod shard;
mod survey;
mod target_file;
mod workspace_snapshot;

#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod acceptance {
    #[doc(hidden)]
    pub use super::modules::{Declaration, declarations, excluded_files, identity};
    #[doc(hidden)]
    pub use super::survey::{DeclarationOutcome, Declarations, Scan, parse_declarations_of, parse_declarations_with_parser, scan};
    #[doc(hidden)]
    pub use crate::paths::physical;
}

#[doc(inline)]
pub use compile_fail::{CompileFailTarget, advice as compile_fail_advice};
#[cfg(feature = "internals")]
#[doc(hidden)]
pub use compiler_exclusions::CompilerExclusions;
#[cfg(not(feature = "internals"))]
pub(crate) use compiler_exclusions::CompilerExclusions;
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
#[doc(inline)]
pub use record::{
    BinaryHint, Context as RecordContext, ContextDigest, Entries as RecordEntries, FileBinaryHints, GENERALIZED_HINTS_VERSION,
    GeneralizedHints, ItemHints, Killer, RankedHint, ReachCluster, RunRecord, SiteIdentity, Term, Tier, Trust, context as record_context,
    rustflags, toolchain,
};
pub(crate) use record::{Settled, digest};
#[doc(inline)]
pub use shard::shard_of;
#[cfg(test)]
#[doc(inline)]
pub use survey::plan;
pub(crate) use survey::{LibraryTestTarget, plan_for_build};
#[doc(inline)]
pub use survey::{Scanned, Survey, load_metadata};
#[doc(inline)]
pub use target_file::TargetFile;
pub(crate) use workspace_snapshot::WorkspaceSnapshot;
