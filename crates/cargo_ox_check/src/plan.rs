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
    ///
    /// `template_checksum` is the checksum of `rendered`; the apply step
    /// records it in the manifest so the next run sees the user's
    /// divergence as `LeaveAlone` (D ≠ L, L = T) rather than reproposing
    /// the same content. The .ox-check-proposed sibling is the user's
    /// review artifact; the proposal "disappears" from the dry-run
    /// summary on subsequent runs unless the template moves again.
    #[must_use]
    pub fn propose_file(
        path: impl Into<String>,
        rendered: String,
        template_checksum: String,
    ) -> Self {
        Self {
            target: Target::File { path: path.into() },
            decision: Decision::Propose,
            rendered: Some(rendered),
            spliced_host: None,
            rendered_checksum: Some(template_checksum),
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
    /// against the live host). `body_checksum` is the checksum of the
    /// rendered region body — the apply step records it in the
    /// manifest so subsequent runs see the proposal as resolved (see
    /// [`propose_file`](Self::propose_file)).
    #[must_use]
    pub fn propose_region(
        host: impl Into<String>,
        id: impl Into<String>,
        spliced_host: String,
        body_checksum: String,
    ) -> Self {
        Self {
            target: Target::Region {
                host: host.into(),
                id: id.into(),
            },
            decision: Decision::Propose,
            rendered: None,
            spliced_host: Some(spliced_host),
            rendered_checksum: Some(body_checksum),
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
    ///
    /// When `previous_manifest` is provided, `Write` items are split
    /// into "Will create" (no prior manifest entry) and "Will update"
    /// (existing entry getting refreshed) per
    /// [updates.md §9](../../docs/design/updates.md). The stale-entries
    /// section enumerates manifest entries that were present before
    /// this run but are no longer in the plan; these are purged on
    /// non-dry-run application (see [`Plan::apply`]).
    #[must_use]
    pub fn summary(&self, previous_manifest: Option<&Manifest>) -> String {
        let mut creates: Vec<&PlanItem> = Vec::new();
        let mut updates: Vec<&PlanItem> = Vec::new();
        let mut leave_alones: Vec<&PlanItem> = Vec::new();
        let mut proposes: Vec<&PlanItem> = Vec::new();
        let mut in_syncs: Vec<&PlanItem> = Vec::new();

        for item in &self.items {
            match item.decision {
                Decision::Write => {
                    let existed = previous_manifest.is_some_and(|m| match &item.target {
                        Target::File { path } => m.files.contains_key(path),
                        Target::Region { host, id } => m.regions.contains_key(&RegionKey {
                            host: host.clone(),
                            id: id.clone(),
                        }),
                    });
                    if existed {
                        updates.push(item);
                    } else {
                        creates.push(item);
                    }
                }
                Decision::LeaveAlone => leave_alones.push(item),
                Decision::Propose => proposes.push(item),
                Decision::InSync => in_syncs.push(item),
            }
        }

        // Stale entries: in the prior manifest but no longer covered by
        // any plan item. These are purged on application.
        let mut stale_files: Vec<&str> = Vec::new();
        let mut stale_regions: Vec<RegionKey> = Vec::new();
        if let Some(prev) = previous_manifest {
            let live_files: std::collections::BTreeSet<&str> = self
                .items
                .iter()
                .filter_map(|i| match &i.target {
                    Target::File { path } => Some(path.as_str()),
                    Target::Region { .. } => None,
                })
                .collect();
            let live_regions: std::collections::BTreeSet<RegionKey> = self
                .items
                .iter()
                .filter_map(|i| match &i.target {
                    Target::Region { host, id } => Some(RegionKey {
                        host: host.clone(),
                        id: id.clone(),
                    }),
                    Target::File { .. } => None,
                })
                .collect();
            for path in prev.files.keys() {
                if !live_files.contains(path.as_str()) {
                    stale_files.push(path.as_str());
                }
            }
            for key in prev.regions.keys() {
                if !live_regions.contains(key) {
                    stale_regions.push(key.clone());
                }
            }
        }

        let mut out = String::new();
        let _ = writeln!(out, "cargo-ox-check plan: {} item(s)", self.items.len());

        write_section(&mut out, "Will create", &creates);
        write_section(&mut out, "Will update", &updates);
        write_section(&mut out, "Will propose", &proposes);
        write_section(&mut out, "Will leave alone (silent)", &leave_alones);

        if !in_syncs.is_empty() {
            let _ = writeln!(out, "Unchanged: {} item(s)", in_syncs.len());
        }
        if !stale_files.is_empty() || !stale_regions.is_empty() {
            let _ = writeln!(
                out,
                "Stale manifest entries (will be purged): {}",
                stale_files.len() + stale_regions.len()
            );
            for path in &stale_files {
                let _ = writeln!(out, "  - {path}");
            }
            for key in &stale_regions {
                let _ = writeln!(out, "  - {} [{}]", key.host, key.id);
            }
        }

        out
    }

    /// Apply the plan to disk and return an updated manifest.
    ///
    /// - `Write` items write their owned-file content or splice region
    ///   bodies into host files and record their checksums in the new
    ///   manifest.
    /// - `Propose` items write a `.ox-check-proposed` sibling and
    ///   bump the manifest entry to the new template checksum so
    ///   subsequent runs see the divergence as resolved
    ///   (`LeaveAlone`) until the template moves again — see
    ///   [updates.md §5](../../docs/design/updates.md).
    /// - `InSync`, `LeaveAlone` items preserve their existing
    ///   manifest entries from `previous_manifest`.
    /// - Stale entries — items present in `previous_manifest` but not
    ///   in this plan (e.g. a removed workspace member, a backend the
    ///   user disabled) — are purged from the returned manifest.
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
                    if let Some(checksum) = &item.rendered_checksum {
                        // Bump L to the new T so subsequent runs see the
                        // divergence as resolved (LeaveAlone). The user's
                        // .ox-check-proposed sibling stays on disk for
                        // review; deleting or accepting it is the user's
                        // job.
                        next.files.insert(path.clone(), checksum.clone());
                    }
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
                (Target::Region { host, id }, Decision::Propose) => {
                    let spliced = item
                        .spliced_host
                        .as_ref()
                        .expect("region Propose must carry spliced host");
                    let abs = repo_root.join(format!("{host}.ox-check-proposed"));
                    write_file(&abs, spliced)?;
                    if let Some(checksum) = &item.rendered_checksum {
                        // Same rationale as the File/Propose branch: bump
                        // L = T so subsequent runs see LeaveAlone until
                        // the template moves again.
                        next.regions.insert(
                            RegionKey {
                                host: host.clone(),
                                id: id.clone(),
                            },
                            checksum.clone(),
                        );
                    }
                }
                (_, Decision::InSync | Decision::LeaveAlone) => {
                    // No-op; manifest entry already preserved.
                }
            }
        }

        // Purge stale entries: anything in the previous manifest that the
        // current run didn't touch (e.g. a removed workspace member, a
        // disabled backend) is dropped. We use the items the plan covers
        // as the active-key set; entries outside that set fall away.
        let live_files: std::collections::BTreeSet<&str> = self
            .items
            .iter()
            .filter_map(|i| match &i.target {
                Target::File { path } => Some(path.as_str()),
                Target::Region { .. } => None,
            })
            .collect();
        let live_regions: std::collections::BTreeSet<RegionKey> = self
            .items
            .iter()
            .filter_map(|i| match &i.target {
                Target::Region { host, id } => Some(RegionKey {
                    host: host.clone(),
                    id: id.clone(),
                }),
                Target::File { .. } => None,
            })
            .collect();
        next.files.retain(|path, _| live_files.contains(path.as_str()));
        next.regions.retain(|key, _| live_regions.contains(key));

        Ok(next)
    }
}

fn write_section(out: &mut String, header: &str, items: &[&PlanItem]) {
    if items.is_empty() {
        return;
    }
    let _ = writeln!(out, "{header}: {} item(s)", items.len());
    for item in items {
        let _ = writeln!(out, "  - {}", item.target.label());
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
            Decision::LeaveAlone,
        ));
        assert!(!plan.has_changes());
    }

    #[test]
    fn summary_categorizes_items() {
        let mut plan = Plan::default();
        // A fresh write (no prior manifest entry).
        plan.push(PlanItem::write_file("a.txt", "x".into(), "sha256:1".into()));
        // An item that was previously rendered and is now in sync.
        plan.push(PlanItem::noop(
            Target::File { path: "b.txt".into() },
            Decision::InSync,
        ));
        // No previous manifest → no "Will update" distinction, but the
        // categories still render.
        let s = plan.summary(Some(&Manifest::default()));
        assert!(s.contains("Will create: 1 item(s)"), "summary:\n{s}");
        assert!(s.contains("- a.txt"));
        assert!(s.contains("Unchanged: 1 item(s)"));
        // b.txt is in-sync — listed in the unchanged count, not enumerated by path.
        assert!(!s.contains("- b.txt"));
    }

    #[test]
    fn summary_distinguishes_create_from_update() {
        let mut plan = Plan::default();
        plan.push(PlanItem::write_file("new.txt", "x".into(), "sha256:1".into()));
        plan.push(PlanItem::write_file("existing.txt", "y".into(), "sha256:2".into()));
        let mut prev = Manifest::default();
        prev.set_file("existing.txt", "sha256:old");
        let s = plan.summary(Some(&prev));
        assert!(s.contains("Will create: 1 item(s)"), "summary:\n{s}");
        assert!(s.contains("- new.txt"));
        assert!(s.contains("Will update: 1 item(s)"));
        assert!(s.contains("- existing.txt"));
    }

    #[test]
    fn summary_lists_stale_entries() {
        let plan = Plan::default();
        let mut prev = Manifest::default();
        prev.set_file("stale.txt", "sha256:old");
        prev.set_region("stale-host.toml", "stale-region", "sha256:r");
        let s = plan.summary(Some(&prev));
        assert!(s.contains("Stale manifest entries (will be purged): 2"));
        assert!(s.contains("- stale.txt"));
        assert!(s.contains("- stale-host.toml [stale-region]"));
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
    fn apply_writes_proposed_file_sibling_and_bumps_manifest() {
        let tmp = TempDir::new().unwrap();
        let mut plan = Plan::default();
        plan.push(PlanItem::propose_file(
            "a.txt",
            "new content\n".into(),
            "sha256:newt".into(),
        ));
        let m = plan.apply(tmp.path(), &Manifest::default()).unwrap();
        assert!(tmp.path().join("a.txt.ox-check-proposed").is_file());
        assert!(!tmp.path().join("a.txt").exists());
        // The manifest L is bumped to the new template checksum so the
        // next run sees the divergence as resolved (LeaveAlone), not as
        // a fresh proposal.
        assert_eq!(m.files.get("a.txt").map(String::as_str), Some("sha256:newt"));
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
    fn apply_region_propose_writes_sibling_and_bumps_manifest() {
        let tmp = TempDir::new().unwrap();
        let mut plan = Plan::default();
        plan.push(PlanItem::propose_region(
            "Justfile",
            "ox-check-imports",
            "spliced host content\n".into(),
            "sha256:newt".into(),
        ));
        let m = plan.apply(tmp.path(), &Manifest::default()).unwrap();
        assert!(tmp.path().join("Justfile.ox-check-proposed").is_file());
        // Region L is bumped to the new template checksum on propose;
        // subsequent runs see LeaveAlone until the template changes.
        let key = RegionKey {
            host: "Justfile".into(),
            id: "ox-check-imports".into(),
        };
        assert_eq!(m.regions.get(&key).map(String::as_str), Some("sha256:newt"));
    }

    #[test]
    fn apply_purges_stale_manifest_entries() {
        // Items present in the previous manifest but not in the current
        // plan (e.g. a removed workspace member, a disabled backend) are
        // dropped from the returned manifest.
        let tmp = TempDir::new().unwrap();
        let mut prev = Manifest::default();
        prev.set_file("stale-owned.txt", "sha256:old");
        prev.set_region("stale-host.toml", "stale-region", "sha256:r");
        let plan = Plan::default();
        let next = plan.apply(tmp.path(), &prev).unwrap();
        assert!(
            next.files.is_empty(),
            "stale file entry should be purged when not in plan: {:?}",
            next.files
        );
        assert!(
            next.regions.is_empty(),
            "stale region entry should be purged when not in plan: {:?}",
            next.regions
        );
    }

    #[test]
    fn apply_preserves_in_plan_manifest_entries() {
        // The dual of the purging test: a manifest entry whose target IS
        // in the plan (even as InSync/LeaveAlone) survives.
        let tmp = TempDir::new().unwrap();
        let mut prev = Manifest::default();
        prev.set_file("kept.txt", "sha256:k");
        let mut plan = Plan::default();
        plan.push(PlanItem::noop(
            Target::File { path: "kept.txt".into() },
            Decision::LeaveAlone,
        ));
        let next = plan.apply(tmp.path(), &prev).unwrap();
        assert_eq!(next.files.get("kept.txt").map(String::as_str), Some("sha256:k"));
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
