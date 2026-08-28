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

pub use cli::{
    CleanArgs, Cli, Command, CompletionsArgs, ConfigArgs, ExplainArgs, FeatureArgs, HintsArgs, ListArgs, ListKind, MergeArgs, RunArgs,
    SelectArgs, SuppressArgs, UnsuppressArgs,
};
pub use dispatch::{EXIT_CANNOT_PROCEED, EXIT_GATE_FAILED, EXIT_INTERNAL, EXIT_OK, EXIT_USAGE, run};
pub use host::Host;
pub use when::When;
