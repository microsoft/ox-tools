// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Projections of a run into things a person or a program can read.
//!
//! The console rendering follows `cargo build`: a right-aligned twelve-column bold-green verb, the
//! subject after it, and a single progress line that is rewritten in place. Matching cargo is not
//! decoration. A developer already reads cargo output fluently, and a tool that puts its status in
//! the same shape is one they do not have to learn.

mod progress;
mod styler;
mod summary;
mod text;

pub use progress::Progress;
pub use styler::Styler;
pub use summary::{Listings, session_notes, skipped, summarize};
pub(crate) use text::{bytes, score, unstyled};
pub use text::{continuation, quantity};
