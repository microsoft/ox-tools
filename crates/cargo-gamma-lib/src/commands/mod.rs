// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The command-line surface and the orchestration behind it.

mod clean;
mod cli;
mod completions;
mod console_events;
mod dispatch;
mod explain;
mod hints;
mod host;
mod list;
mod merge;
mod run;
mod suppress;
mod unsuppress;
mod verdict_log;
mod when;

#[doc(inline)]
pub use cli::{
    BuildLimitArgs, CleanArgs, Cli, Command, CompletionsArgs, ConfigArgs, ExplainArgs, FeatureArgs, HintsArgs, ListArgs, ListKind,
    MeasureArgs, MergeArgs, RunArgs, SelectArgs, SuppressArgs, UnsuppressArgs,
};
#[doc(inline)]
pub use dispatch::{EXIT_CANNOT_PROCEED, EXIT_GATE_FAILED, EXIT_INTERNAL, EXIT_OK, EXIT_USAGE, run};
#[doc(inline)]
pub use host::Host;
#[doc(inline)]
pub use when::When;
