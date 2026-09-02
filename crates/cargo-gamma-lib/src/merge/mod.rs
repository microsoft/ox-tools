// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Combining per-shard reports into one answer.
//!
//! A single shard cannot answer "what is our mutation score?", and scoring a shard on its own is
//! actively misleading: on a three-hundred-mutant shard one survivor moves the score by a third of a
//! point, so a threshold set on a shard fires on noise. The merged view across a rotation is the real
//! deliverable, and the per-shard report is an intermediate.
//!
//! Merging is a union by stable mutant ID, most recent verdict winning. That works only because IDs
//! are content-addressed: a mutant keeps its ID as the code around it changes, so last night's
//! verdict still refers to the same construct, and a construct that *was* edited gets a different ID
//! and correctly shows up as never tested rather than inheriting a verdict it never earned.
//!
//! A union alone is not enough for the second half of that promise. The edited construct's *new* ID
//! does appear as never tested, but nothing removes the *old* one, so a survivor that has since been
//! fixed goes on depressing the score and a caught verdict goes on crediting code that has changed.
//! Whichever unsharded input is newest states the complete population of every file it contains, so
//! an ID absent from it has been withdrawn, and [`Merged::withdrawn`] counts what that dropped. A
//! sharded input describes only its own slice of the population and can never withdraw anything.
//!
//! Three numbers matter as much as the score, and all three are invisible without merging:
//!
//! - **Never tested.** Code added since the rotation last touched its shard. Reported separately and
//!   never counted as killed, because counting untested code as passing is how a mutation score
//!   becomes a decoration.
//! - **Stale.** A verdict older than the freshness window. Still reported, but not claimed as
//!   current.
//! - **Rotation health.** Shards seen against shards expected. A rotation that is not keeping up is
//!   the actual problem behind a score that will not move.

mod incoming;
mod merged;
mod read;
mod status;
mod union;
mod verdict;

pub(crate) use merged::MAX_SHARDS;
#[doc(inline)]
pub use merged::Merged;
pub(crate) use read::read_limited;
#[doc(inline)]
pub use union::merge;
