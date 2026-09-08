// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Walking a syntax tree and producing the mutants it admits.
//!
//! The traversal tracks the enclosing item path and how many identical sites for a mutator have
//! already been seen, giving each mutant an identity that survives reformatting and code motion.

mod candidate;
mod collector;
mod defaults;
mod definitions;
mod shape;
mod stated;
mod traversal;

#[doc(inline)]
pub use candidate::Candidate;
#[doc(inline)]
pub use defaults::Defaults;
#[doc(inline)]
pub use definitions::into_definitions;
#[doc(inline)]
pub use shape::Shape;
#[doc(inline)]
pub use stated::check as check_stated;
#[doc(inline)]
pub use traversal::{check_stated_and_collect_with, collect, collect_in, collect_with};

#[cfg(test)]
mod tests;
