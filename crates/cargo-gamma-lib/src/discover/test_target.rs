// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fs;

use camino::{Utf8Path, Utf8PathBuf};
use cargo_metadata::{Metadata, Target, TargetKind};
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::error::error;

/// Portable target identity within a named workspace package.
///
/// Package and target names accompany this identity in binary and test hints.
/// Physical Cargo package IDs remain on executable artifacts, not persisted hints.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TargetIdentity {
    /// Cargo target kinds, including library crate kinds.
    pub kind: Vec<String>,
    /// Target source root relative to the workspace.
    pub source: Utf8PathBuf,
}

/// A declared Cargo target and its test-harness eligibility.
#[derive(Debug, Clone)]
pub(crate) struct TestTarget {
    pub(crate) identity: TargetIdentity,
    pub(crate) package: String,
    pub(crate) name: String,
    pub(crate) library: bool,
    pub(crate) test: bool,
    pub(crate) harness: bool,
}

/// Cargo represents a library using any of its supported library crate kinds.
pub(crate) fn is_library(target: &Target) -> bool {
    target.kind.iter().any(|kind| {
        matches!(
            kind,
            TargetKind::Lib | TargetKind::RLib | TargetKind::DyLib | TargetKind::CDyLib | TargetKind::StaticLib | TargetKind::ProcMacro
        )
    })
}

pub(crate) fn test_inventory(metadata: &Metadata) -> Result<Vec<TestTarget>> {
    let mut targets = Vec::new();
    for package in metadata.workspace_packages() {
        // Cargo metadata does not expose `harness`. Read only this manifest property;
        // target existence, names, kinds and test eligibility remain Cargo's decisions.
        let text = fs::read_to_string(&package.manifest_path)
            .map_err(|cause| error!("could not read `{}` for test harness policy", package.manifest_path).caused_by(cause))?;
        let manifest: toml::Value = toml::from_str(&text)
            .map_err(|cause| error!("could not parse `{}` for test harness policy", package.manifest_path).caused_by(cause))?;
        for target in &package.targets {
            let source = target.src_path.strip_prefix(&metadata.workspace_root).unwrap_or(&target.src_path);
            targets.push(TestTarget {
                identity: TargetIdentity {
                    kind: target.kind.iter().map(ToString::to_string).collect(),
                    source: Utf8PathBuf::from(source.as_str().replace('\\', "/")),
                },
                package: package.name.to_string(),
                name: target.name.clone(),
                library: is_library(target),
                test: target.test,
                harness: uses_libtest(&manifest, target),
            });
        }
    }
    Ok(targets)
}

fn uses_libtest(manifest: &toml::Value, target: &Target) -> bool {
    let declaration = if is_library(target) {
        manifest.get("lib")
    } else {
        target.kind.first().and_then(|kind| {
            manifest.get(kind.to_string())?.as_array()?.iter().find(|entry| {
                entry.get("name").and_then(toml::Value::as_str) == Some(target.name.as_str())
                    || entry
                        .get("path")
                        .and_then(toml::Value::as_str)
                        .is_some_and(|path| target.src_path.ends_with(Utf8Path::new(path)))
            })
        })
    };
    declaration
        .and_then(|entry| entry.get("harness"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(true)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use core::panic::{RefUnwindSafe, UnwindSafe};

    use super::*;

    const _: fn() = assert_traits::<TargetIdentity>;

    fn assert_traits<T: Send + Sync + UnwindSafe + RefUnwindSafe>() {}

    fn target(kind: &str) -> Target {
        serde_json::from_value(serde_json::json!({
            "name": "same", "kind": [kind], "src_path": "src/lib.rs",
        }))
        .unwrap()
    }

    #[test]
    fn library_classification_covers_cargos_library_kinds_not_other_targets() {
        for kind in ["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"] {
            assert!(is_library(&target(kind)), "{kind}");
        }
        for kind in ["bin", "test", "bench", "example", "custom-build", "unknown"] {
            assert!(!is_library(&target(kind)), "{kind}");
        }
    }

    #[test]
    fn custom_harness_selection_distinguishes_same_named_declarations() {
        let manifest: toml::Value = toml::from_str(
            r#"
[lib]
harness = false
[[test]]
name = "same"
harness = true
[[bin]]
path = "src/lib.rs"
harness = false
"#,
        )
        .unwrap();
        assert!(!uses_libtest(&manifest, &target("rlib")));
        assert!(uses_libtest(&manifest, &target("test")));
        assert!(!uses_libtest(&manifest, &target("bin")));
        assert!(uses_libtest(&manifest, &target("example")));
    }
}
