// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Plan accumulation, proposed-file emission, and dry-run summary.
//!
//! The `update` driver builds a [`Plan`] by appending one [`PlanItem`]
//! per file or region it processes. After all decisions are made, the
//! plan either:
//!
//! - **Applies** to disk: writes owned files, splices in region updates,
//!   writes `.ox-check-proposed` siblings for divergent items, and
//!   refreshes the manifest.
//! - **Summarizes** for `--dry-run`: prints counts and outstanding items,
//!   without touching disk. Returns a non-zero exit code if anything is
//!   out of date.
//!
//! See [updates.md §7](../../docs/design/updates.md) for the proposed-file
//! protocol.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use ohno::{AppError, IntoAppError as _};

use crate::decision::Decision;
use crate::manifest::{Manifest, RegionKey};

/// What is being changed by a single plan item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// An owned file at a repo-root-relative forward-slash path.
    File {
        /// Repo-root-relative forward-slash path.
        path: String,
    },
    /// A managed region inside a host file.
    Region {
        /// Repo-root-relative forward-slash path to the host file.
        host: String,
        /// Stable region id.
        id: String,
    },
}

impl Target {
    /// A short human-readable label for summary output.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::File { path } => path.clone(),
            Self::Region { host, id } => format!("{host} [{id}]"),
        }
    }
}

/// One unit of work the driver decided on.
#[derive(Debug, Clone)]
pub struct PlanItem {
    /// What is being changed.
    pub target: Target,
    /// The driver's decision.
    pub decision: Decision,
    /// What the driver wants to write — either to disk (for `Write`) or
    /// to a `.ox-check-proposed` sibling (for `Propose`). `None` for
    /// decisions that don't write.
    pub rendered: Option<String>,
    /// The full host-file body that contains the rendered region after
    /// splice — used for `Region` targets in either `Write` or `Propose`
    /// modes. Per [updates.md §7](../../docs/design/updates.md), proposed
    /// outputs show the *full file* even for regions, not just the
    /// region body. `None` for `File` targets.
    pub spliced_host: Option<String>,
    /// Checksum of [`Self::rendered`], populated when `rendered` is
    /// `Some`. The manifest stores this for `Write` decisions.
    pub rendered_checksum: Option<String>,
}

impl PlanItem {
    /// Construct a plan item for an in-sync, skipped, or leave-alone
    /// decision (no payload).
    #[must_use]
    pub fn noop(target: Target, decision: Decision) -> Self {
        debug_assert!(!decision.writes());
        Self {
            target,
            decision,
            rendered: None,
            spliced_host: None,
            rendered_checksum: None,
        }
    }

    /// Construct a plan item for a `Write` decision on an owned file.
    #[must_use]
    pub fn write_file(path: impl Into<String>, rendered: String, checksum: String) -> Self {
        Self {
            target: Target::File { path: path.into() },
            decision: Decision::Write,
            rendered: Some(rendered),
            spliced_host: None,
            rendered_checksum: Some(checksum),
        }
    }

    /// Construct a plan item for a `Propose` decision on an owned file.
    #[must_use]
    pub fn propose_file(path: impl Into<String>, rendered: String) -> Self {
        Self {
            target: Target::File { path: path.into() },
            decision: Decision::Propose,
            rendered: Some(rendered),
            spliced_host: None,
            rendered_checksum: None,
        }
    }

    /// Construct a plan item for a `Write` decision on a region.
    #[must_use]
    pub fn write_region(
        host: impl Into<String>,
        id: impl Into<String>,
        body: String,
        spliced_host: String,
        body_checksum: String,
    ) -> Self {
        Self {
            target: Target::Region {
                host: host.into(),
                id: id.into(),
            },
            decision: Decision::Write,
            rendered: Some(body),
            spliced_host: Some(spliced_host),
            rendered_checksum: Some(body_checksum),
        }
    }

    /// Construct a plan item for a `Propose` decision on a region. The
    /// proposed payload is the *full host file* that would result from
    /// the splice (so the user can review by diffing the proposed file
    /// against the live host).
    #[must_use]
    pub fn propose_region(
        host: impl Into<String>,
        id: impl Into<String>,
        spliced_host: String,
    ) -> Self {
        Self {
            target: Target::Region {
                host: host.into(),
                id: id.into(),
            },
            decision: Decision::Propose,
            rendered: None,
            spliced_host: Some(spliced_host),
            rendered_checksum: None,
        }
    }
}

/// A collection of plan items ready to be applied or summarized.
#[derive(Debug, Default)]
pub struct Plan {
    items: Vec<PlanItem>,
}

impl Plan {
    /// Append a new item.
    pub fn push(&mut self, item: PlanItem) {
        self.items.push(item);
    }

    /// All plan items in insertion order.
    #[must_use]
    pub fn items(&self) -> &[PlanItem] {
        &self.items
    }

    /// Whether the plan would change anything on disk if applied.
    #[must_use]
    pub fn has_changes(&self) -> bool {
        self.items.iter().any(|i| i.decision.writes())
    }

    /// Exit code for `--dry-run`: 0 if everything is in sync, 1 otherwise.
    #[must_use]
    pub fn dry_run_exit_code(&self) -> i32 {
        i32::from(self.has_changes())
    }

    /// Render a stable, line-oriented summary suitable for stdout.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
        for item in &self.items {
            *counts.entry(decision_label(item.decision)).or_default() += 1;
        }

        let mut out = String::new();
        let _ = writeln!(out, "cargo-ox-check plan: {} item(s)", self.items.len());
        for (k, v) in &counts {
            let _ = writeln!(out, "  {k}: {v}");
        }
        for item in &self.items {
            if item.decision.writes() {
                let _ = writeln!(
                    out,
                    "  [{}] {}",
                    decision_label(item.decision),
                    item.target.label()
                );
            }
        }
        out
    }

    /// Apply the plan to disk and return an updated manifest.
    ///
    /// - `Write` items write their owned-file content or splice region
    ///   bodies into host files and record their checksums in the new
    ///   manifest.
    /// - `Propose` items write a `.ox-check-proposed` sibling. For region
    ///   targets, the sibling is on the *host* file:
    ///   `<host>.ox-check-proposed`. The manifest is NOT updated for
    ///   proposals — see [updates.md §7](../../docs/design/updates.md).
    /// - `InSync`, `Skipped`, `LeaveAlone` items preserve their existing
    ///   manifest entries from `previous_manifest`.
    ///
    /// # Errors
    ///
    /// Returns an error if any filesystem operation fails.
    ///
    /// # Panics
    ///
    /// Panics if a plan item's invariants are violated — `Write` and
    /// `Propose` items for files must carry rendered content; region
    /// items must carry a spliced host. These invariants are enforced
    /// by the `PlanItem::*` constructors, so violations only happen if
    /// callers build `PlanItem` directly with inconsistent fields.
    pub fn apply(&self, repo_root: &Path, previous_manifest: &Manifest) -> Result<Manifest, AppError> {
        let mut next = Manifest {
            rendered_by: Some(format!(
                "{} {}",
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION")
            )),
            files: previous_manifest.files.clone(),
            regions: previous_manifest.regions.clone(),
        };

        for item in &self.items {
            match (&item.target, item.decision) {
                (Target::File { path }, Decision::Write) => {
                    let content = item
                        .rendered
                        .as_ref()
                        .expect("Write decision must carry rendered content");
                    let abs = repo_root.join(path);
                    write_file(&abs, content)?;
                    if let Some(checksum) = &item.rendered_checksum {
                        next.files.insert(path.clone(), checksum.clone());
                    }
                }
                (Target::File { path }, Decision::Propose) => {
                    let content = item
                        .rendered
                        .as_ref()
                        .expect("Propose decision must carry rendered content");
                    let abs = repo_root.join(format!("{path}.ox-check-proposed"));
                    write_file(&abs, content)?;
                }
                (Target::Region { host, id }, Decision::Write) => {
                    let spliced = item
                        .spliced_host
                        .as_ref()
                        .expect("region Write must carry spliced host");
                    let abs = repo_root.join(host);
                    write_file(&abs, spliced)?;
                    if let Some(checksum) = &item.rendered_checksum {
                        next.regions.insert(
                            RegionKey {
                                host: host.clone(),
                                id: id.clone(),
                            },
                            checksum.clone(),
                        );
                    }
                }
                (Target::Region { host, .. }, Decision::Propose) => {
                    let spliced = item
                        .spliced_host
                        .as_ref()
                        .expect("region Propose must carry spliced host");
                    let abs = repo_root.join(format!("{host}.ox-check-proposed"));
                    write_file(&abs, spliced)?;
                }
                (_, Decision::InSync | Decision::Skipped | Decision::LeaveAlone) => {
                    // No-op; manifest entry already preserved.
                }
            }
        }

        Ok(next)
    }
}

const fn decision_label(d: Decision) -> &'static str {
    match d {
        Decision::InSync => "in-sync",
        Decision::Skipped => "skipped",
        Decision::Write => "write",
        Decision::Propose => "propose",
        Decision::LeaveAlone => "leave-alone",
    }
}

fn write_file(path: &Path, content: &str) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).into_app_err_with(|| {
            format!("failed to create parent directory {}", parent.display())
        })?;
    }
    let tmp = make_temp_path(path);
    std::fs::write(&tmp, content)
        .into_app_err_with(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .into_app_err_with(|| format!("failed to rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn make_temp_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsString::from)
        .unwrap_or_default();
    name.push(".ox-check-tmp");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn empty_plan_is_in_sync() {
        let plan = Plan::default();
        assert!(!plan.has_changes());
        assert_eq!(plan.dry_run_exit_code(), 0);
    }

    #[test]
    fn plan_with_write_is_out_of_sync() {
        let mut plan = Plan::default();
        plan.push(PlanItem::write_file("a.txt", "x".into(), "sha256:1".into()));
        assert!(plan.has_changes());
        assert_eq!(plan.dry_run_exit_code(), 1);
    }

    #[test]
    fn plan_with_only_in_sync_items_is_in_sync() {
        let mut plan = Plan::default();
        plan.push(PlanItem::noop(
            Target::File { path: "a.txt".into() },
            Decision::InSync,
        ));
        plan.push(PlanItem::noop(
            Target::Region {
                host: "Justfile".into(),
                id: "x".into(),
            },
            Decision::Skipped,
        ));
        assert!(!plan.has_changes());
    }

    #[test]
    fn summary_contains_counts_and_changed_items() {
        let mut plan = Plan::default();
        plan.push(PlanItem::write_file("a.txt", "x".into(), "sha256:1".into()));
        plan.push(PlanItem::noop(
            Target::File { path: "b.txt".into() },
            Decision::InSync,
        ));
        let s = plan.summary();
        assert!(s.contains("write: 1"));
        assert!(s.contains("in-sync: 1"));
        assert!(s.contains("a.txt"));
        // b.txt is in-sync, listed only in counts not in the changed list.
        assert!(!s.contains("[in-sync] b.txt"));
    }

    #[test]
    fn apply_writes_owned_file() {
        let tmp = TempDir::new().unwrap();
        let mut plan = Plan::default();
        plan.push(PlanItem::write_file(
            "subdir/a.txt",
            "hello\n".into(),
            "sha256:abcd".into(),
        ));
        let m = plan.apply(tmp.path(), &Manifest::default()).unwrap();
        let written = std::fs::read_to_string(tmp.path().join("subdir/a.txt")).unwrap();
        assert_eq!(written, "hello\n");
        assert_eq!(m.files.get("subdir/a.txt").map(String::as_str), Some("sha256:abcd"));
        assert!(m.rendered_by.is_some());
    }

    #[test]
    fn apply_writes_proposed_file_sibling() {
        let tmp = TempDir::new().unwrap();
        let mut plan = Plan::default();
        plan.push(PlanItem::propose_file("a.txt", "new content\n".into()));
        let _ = plan.apply(tmp.path(), &Manifest::default()).unwrap();
        assert!(tmp.path().join("a.txt.ox-check-proposed").is_file());
        assert!(!tmp.path().join("a.txt").exists());
    }

    #[test]
    fn apply_region_write_splices_host() {
        let tmp = TempDir::new().unwrap();
        let mut plan = Plan::default();
        plan.push(PlanItem::write_region(
            "Justfile",
            "ox-check-imports",
            "import 'foo'\n".into(),
            "user content\n# >>> ox-check-managed: ox-check-imports\nimport 'foo'\n# <<< ox-check-managed: ox-check-imports\n".into(),
            "sha256:body".into(),
        ));
        let m = plan.apply(tmp.path(), &Manifest::default()).unwrap();
        let written = std::fs::read_to_string(tmp.path().join("Justfile")).unwrap();
        assert!(written.contains("# >>> ox-check-managed: ox-check-imports"));
        let key = RegionKey {
            host: "Justfile".into(),
            id: "ox-check-imports".into(),
        };
        assert_eq!(m.regions.get(&key).map(String::as_str), Some("sha256:body"));
    }

    #[test]
    fn apply_region_propose_writes_proposed_host_sibling() {
        let tmp = TempDir::new().unwrap();
        let mut plan = Plan::default();
        plan.push(PlanItem::propose_region(
            "Justfile",
            "ox-check-imports",
            "spliced host content\n".into(),
        ));
        let m = plan.apply(tmp.path(), &Manifest::default()).unwrap();
        assert!(tmp.path().join("Justfile.ox-check-proposed").is_file());
        // Manifest region entries unchanged.
        assert!(m.regions.is_empty());
    }

    #[test]
    fn apply_preserves_unrelated_manifest_entries() {
        let tmp = TempDir::new().unwrap();
        let mut prev = Manifest::default();
        prev.set_file("unrelated.txt", "sha256:old");
        let plan = Plan::default();
        let next = plan.apply(tmp.path(), &prev).unwrap();
        assert_eq!(
            next.files.get("unrelated.txt").map(String::as_str),
            Some("sha256:old")
        );
    }

    #[test]
    fn target_label_for_files() {
        let t = Target::File { path: "a.txt".into() };
        assert_eq!(t.label(), "a.txt");
    }

    #[test]
    fn target_label_for_regions() {
        let t = Target::Region {
            host: "Justfile".into(),
            id: "x".into(),
        };
        assert_eq!(t.label(), "Justfile [x]");
    }
}
