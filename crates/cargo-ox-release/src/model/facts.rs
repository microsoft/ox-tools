// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The facts contract: the deterministic workspace snapshot produced by
//! fact-gathering and consumed by the resolver.
//!
//! Facts are emitted by our own tooling and are always well-structured, so
//! they are fully typed here. Vector fields tolerate an explicit JSON `null`
//! (the fact emitter writes `null` rather than `[]` for some empty lists) via
//! [`flexible_vec`].

use serde::Deserialize;

use crate::model::serde_helpers::{flexible_vec, null_string};

/// The facts schema version this resolver understands.
pub(crate) const SCHEMA_VERSION: u32 = 5;

/// The top-level facts document.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Facts {
    /// Schema version; the resolver requires exactly `5`.
    #[serde(default)]
    pub schema_version: u32,
    /// Absolute repository root the facts were gathered from.
    #[serde(default, deserialize_with = "null_string")]
    pub repo_root: String,
    /// The baseline git ref the modification state was computed against.
    #[serde(default, deserialize_with = "null_string")]
    pub base_ref: String,
    /// Every workspace package.
    #[serde(default)]
    pub packages: Vec<PackageFact>,
}

/// A single workspace package's release-relevant facts.
///
/// Field names use camelCase to match the JSON contract, via `rename_all`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[expect(
    clippy::struct_excessive_bools,
    reason = "these are independent boolean flags in the JSON facts contract; modeling each as an enum would distort the wire contract"
)]
pub struct PackageFact {
    /// Directory name under `crates/` — the canonical identifier.
    pub folder: String,
    /// Cargo `[package].name` (may differ from the folder).
    pub name: String,
    /// Current version in the manifest.
    pub version: String,
    /// Whether the package is publishable (not `publish = false`).
    #[serde(default)]
    pub published: bool,
    /// Whether the package is a proc-macro-only crate.
    #[serde(default)]
    pub proc_macro_only: bool,
    /// Whether the package has a library target.
    #[serde(default)]
    pub has_library_target: bool,
    /// Normalized names of every workspace dependency.
    #[serde(default, deserialize_with = "flexible_vec")]
    pub deps: Vec<String>,
    /// Normalized names of workspace dependencies whose types this package
    /// exposes in its public API.
    #[serde(default, deserialize_with = "flexible_vec")]
    pub exposed_deps: Vec<String>,
    /// Normalized names of proc-macro dependencies whose macros this package
    /// re-exports publicly.
    #[serde(default, deserialize_with = "flexible_vec")]
    pub macro_public_deps: Vec<String>,
    /// For a proc macro, the normalized names of its implementation-closure
    /// members.
    #[serde(default, deserialize_with = "flexible_vec")]
    pub macro_implementation_closure: Vec<String>,
    /// For a proc macro, the normalized names of its runtime partner crates.
    #[serde(default, deserialize_with = "flexible_vec")]
    pub macro_runtime_partners: Vec<String>,
    /// Whether the exposed-dependency analysis was inconclusive for this
    /// package (forces conservative type-exposure cascades).
    #[serde(default)]
    pub exposure_unknown: bool,
    /// The commit the package was last released from.
    #[serde(default, deserialize_with = "null_string")]
    pub baseline_sha: String,
    /// Whether a release baseline was found.
    #[serde(default)]
    pub has_baseline: bool,
    /// Whether the package has ever been released.
    #[serde(default)]
    pub ever_released: bool,
    /// Whether the package's own tree changed since the baseline.
    #[serde(default)]
    pub modified: bool,
    /// The changed files attributed to this package (may be `null` when none).
    #[serde(default, deserialize_with = "flexible_vec")]
    pub modified_files: Vec<String>,
    /// Count of changed files.
    #[serde(default)]
    pub modified_file_count: i64,
    /// Which manifest dependency scopes changed (`normal`, `build`, `dev`,
    /// `features`).
    #[serde(default, deserialize_with = "flexible_vec")]
    pub manifest_dependency_scopes: Vec<String>,
    /// Whether a non-dependency manifest field changed.
    #[serde(default)]
    pub manifest_other_changed: bool,
    /// Whether packaged Rust implementation source changed (excludes doc
    /// comments).
    #[serde(default)]
    pub rust_implementation_changed: bool,
    /// Whether a rustdoc-visible doc comment changed.
    #[serde(default)]
    pub doc_comment_changed: bool,
    /// Whether anything in the package's workspace tree changed (broader than
    /// [`Self::modified`]).
    #[serde(default)]
    pub workspace_modified: bool,
    /// External (crates.io) dependency requirement changes.
    #[serde(default, deserialize_with = "flexible_vec")]
    pub external_dep_changes: Vec<ExternalDepChange>,
    /// Names of external dependencies whose types this package exposes
    /// publicly.
    #[serde(default, deserialize_with = "flexible_vec")]
    pub external_exposed_deps: Vec<String>,
    /// Compile fixtures whose result may have changed, forming a macro's
    /// evidence obligations.
    #[serde(default, deserialize_with = "flexible_vec")]
    pub macro_compile_fixture_changes: Vec<MacroCompileFixtureChange>,
}

/// A change to an external dependency's version requirement.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalDepChange {
    /// Dependency crate name.
    pub name: String,
    /// The requirement at the release baseline.
    #[serde(default, deserialize_with = "null_string")]
    pub baseline_req: String,
    /// The requirement at the current revision.
    #[serde(default, deserialize_with = "null_string")]
    pub current_req: String,
    /// The dependency kinds affected (`normal`, `build`, `dev`).
    #[serde(default, deserialize_with = "flexible_vec")]
    pub kinds: Vec<String>,
    /// Whether the requirement moved to a different Cargo compatibility line.
    #[serde(default)]
    pub breaking: bool,
    /// The commit the baseline requirement was read from.
    #[serde(default, deserialize_with = "null_string")]
    pub baseline_rev: String,
}

/// A compile fixture the facts saw change, which a macro contract must
/// evidence.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MacroCompileFixtureChange {
    /// The package that owns the fixture (the consumer program).
    pub owner_package: String,
    /// Whether the owner is a published crate with its own release identity.
    #[serde(default)]
    pub owner_published: bool,
    /// Path to the fixture (may be a `.rs`, `.stderr`, or `.stdout` file).
    pub path: String,
    /// The fixture kind (for example `uiExpectation`).
    #[serde(default, deserialize_with = "null_string")]
    pub kind: String,
    /// The change status (for example `modified`).
    #[serde(default, deserialize_with = "null_string")]
    pub status: String,
    /// The recorded expected result, when known.
    #[serde(default, deserialize_with = "null_string")]
    pub expected_result: String,
    /// The commit the baseline fixture was read from.
    #[serde(default, deserialize_with = "null_string")]
    pub baseline_rev: String,
    /// The fixture's role relative to the macro under review
    /// (`implementationClosure`, `runtimePartner`, `self`).
    #[serde(default, deserialize_with = "null_string")]
    pub scope_role: String,
}

impl PackageFact {
    /// The package name normalized for graph comparisons.
    pub(crate) fn normalized_name(&self) -> String {
        super::normalize_ident(&self.name)
    }
}
