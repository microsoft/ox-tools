// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// Cargo-resolved identity evidence for an external resource-analysis contract.
///
/// Populate this from a resolved Cargo graph, not package-name or lockfile guesses.
/// A nested import name such as `serde::serde_derive` identifies an edge reached
/// through the corresponding direct dependency.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceDependency {
    /// Physical source label of the consuming crate root.
    pub crate_root: String,
    /// Actual renamed dependency edge, with nested edges separated by `::`.
    pub import_name: String,
    /// Exact resolved Cargo package ID.
    pub package_id: String,
    /// Resolved package name.
    pub package_name: String,
    /// Exact resolved package version.
    pub version: String,
    /// Cargo source identifier.
    pub source: String,
    /// Features enabled on this resolved dependency node.
    pub features: Vec<String>,
    /// Whether its complete, bounded dependency closure uses official registry sources.
    pub registry_closure: bool,
    /// Physical library root for a uniquely resolved workspace library dependency.
    pub library_root: Option<String>,
}
