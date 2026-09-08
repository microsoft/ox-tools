// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Writing suppressions back into the source, the way `cargo clippy --fix` does.
//!
//! A run knows exactly which mutants caused trouble and exactly where they live, so it can write the
//! directive rather than describing it. What makes that safe is a single rule and a single check.
//!
//! **The rule: a surviving mutant is never eligible.** Not by default, not behind a flag, not with a
//! force switch. A survivor is a real gap in the test suite, and a tool that offers to delete gaps
//! from its own denominator is a tool for manufacturing a mutation score. The moment this can hide a
//! survivor, every number the tool reports becomes unfalsifiable — so the refusal is structural: a
//! surviving verdict has no spelling that reaches this module.
//!
//! **The check: verify, do not assert.** A directive placed one line off, or attached to a
//! multi-line expression, can silently suppress a dozen unrelated mutants — including survivors,
//! which is the rule above being violated by accident rather than by design. So after writing,
//! discovery runs again and the suppressed set is compared: every intended mutant must now be
//! suppressed and nothing else may have become suppressed. If either half fails, the whole edit is
//! reverted.

mod diff;
mod edit;
mod eligible;
mod plan;
mod removal;
mod verification;
mod verify;

#[doc(inline)]
pub use diff::diff;
#[doc(inline)]
pub use edit::Edit;
#[doc(inline)]
pub use eligible::Eligible;
#[doc(inline)]
pub use plan::{apply, plan, today};
#[doc(inline)]
pub use removal::{removable, remove};
#[doc(inline)]
pub use verification::Verification;
#[doc(inline)]
pub use verify::verify;
