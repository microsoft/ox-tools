// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Local recipe emission (the `justfiles/ox-check/` tree).
//!
//! Each owned file under `justfiles/ox-check/` is embedded at compile time
//! via [`include_str!`] from the `templates/justfiles/ox-check/` directory.
//! The emitter just forwards the template through the owned-file driver.
//!
//! See [local.md](../../docs/design/local.md) for the recipe surface.

use std::path::Path;

use anyhow::Result;

use crate::manifest::Manifest;
use crate::plan::PlanItem;

use super::owned_file::plan_owned_file;

/// Contents of `justfiles/ox-check/tools.just` baked into the binary.
pub const TOOLS_JUST: &str =
    include_str!("../../templates/justfiles/ox-check/tools.just");

/// Repo-root-relative path of the tools recipe file.
pub const TOOLS_JUST_PATH: &str = "justfiles/ox-check/tools.just";

/// Emit a [`PlanItem`] for `justfiles/ox-check/tools.just`.
///
/// # Errors
///
/// Propagates I/O errors from [`plan_owned_file`].
pub fn plan_tools_just(repo_root: &Path, manifest: &Manifest) -> Result<PlanItem> {
    plan_owned_file(repo_root, manifest, TOOLS_JUST_PATH, TOOLS_JUST)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::decision::Decision;

    #[test]
    fn tools_just_template_is_not_empty() {
        assert!(TOOLS_JUST.contains("ox-check-tools-check"));
        assert!(TOOLS_JUST.contains("_ox-check-require"));
    }

    #[test]
    fn first_render_writes_tools_just() {
        let tmp = TempDir::new().unwrap();
        let item = plan_tools_just(tmp.path(), &Manifest::default()).unwrap();
        assert_eq!(item.decision, Decision::Write);
        assert_eq!(item.rendered.as_deref(), Some(TOOLS_JUST));
    }

    #[test]
    fn matching_file_is_in_sync() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("justfiles/ox-check")).unwrap();
        std::fs::write(tmp.path().join(TOOLS_JUST_PATH), TOOLS_JUST).unwrap();
        let item = plan_tools_just(tmp.path(), &Manifest::default()).unwrap();
        assert_eq!(item.decision, Decision::InSync);
    }
}
