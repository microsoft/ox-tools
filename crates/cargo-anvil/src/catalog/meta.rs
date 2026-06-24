// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CLI identity for a catalog.
//!
//! [`CliMeta`] is the one cosmetic thing a fork customizes besides its
//! artifact set: the cargo subcommand token, the binary name, the `about`
//! text, and the version. It feeds clap only — it is never interpolated
//! into a path, a sentinel, or a recipe name. See
//! [`extensibility.md §2`](../../docs/design/extensibility.md).

/// CLI identity. Cosmetic only — drives clap, never the on-disk format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliMeta {
    /// Cargo subcommand token (the word after `cargo`). Defaults to `anvil`.
    ///
    /// Used solely to strip the leading word cargo injects (`cargo myforge`
    /// → argv `myforge …`), to render `--help`, and as the `tool` identity
    /// recorded in `.anvil.lock` for the single-tool guard.
    pub subcommand: String,
    /// Binary name shown in help. Defaults to `cargo-{subcommand}`.
    pub bin_name: String,
    /// `about` text shown in `--help`.
    pub about: String,
    /// Version string shown in `--version`.
    pub version: String,
}

impl CliMeta {
    /// Construct CLI metadata for a subcommand, deriving `bin_name` as
    /// `cargo-{subcommand}` and leaving `about` empty and `version` `0.0.0`.
    #[must_use]
    pub fn new(subcommand: impl Into<String>) -> Self {
        let subcommand = subcommand.into();
        let bin_name = format!("cargo-{subcommand}");
        Self {
            subcommand,
            bin_name,
            about: String::new(),
            version: "0.0.0".to_owned(),
        }
    }
}
