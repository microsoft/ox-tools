// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Plan accumulation, proposed-file emission, and dry-run summary.
//!
//! The `update` driver builds a [`Plan`] by appending one [`PlanItem`]
//! per file or region it processes. After all decisions are made, the
//! plan either:
//!
//! - **Applies** to disk: writes owned files, splices in region updates,
//!   writes `.anvil-proposed` siblings for divergent items, and
//!   refreshes the manifest.
//! - **Summarizes** for `--dry-run`: prints counts and outstanding items,
//!   without touching disk. Returns a non-zero exit code if anything is
//!   out of date.
//!
//! See [`updates.md §7`](../../docs/design/updates.md) for the proposed-file
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
    /// to a `.anvil-proposed` sibling (for `Propose`). `None` for
    /// decisions that don't write.
    pub rendered: Option<String>,
    /// The full host-file body that contains the rendered region after
    /// splice — used for `Region` targets in either `Write` or `Propose`
    /// modes. Per [`updates.md §7`](../../docs/design/updates.md), proposed
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

    /// Construct a plan item for an `InSync` decision that *also*
    /// carries the current template checksum. The apply step uses it
    /// to opportunistically refresh the manifest's `L` value — so any
    /// stale `L` from older binary versions (e.g. before line-ending
    /// normalization landed) gets self-healed once the file is observed
    /// in sync with the current template. Without this, a subsequent
    /// template change would mis-classify as `Propose` instead of
    /// `Write` because the algorithm would see `F ≠ L` and assume user
    /// customization.
    #[must_use]
    pub fn insync(target: Target, template_checksum: String) -> Self {
        Self {
            target,
            decision: Decision::InSync,
            rendered: None,
            spliced_host: None,
            rendered_checksum: Some(template_checksum),
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
    /// divergence as `LeaveAlone` (`D ≠ L, L = T`) rather than reproposing
    /// the same content. The .anvil-proposed sibling is the user's
    /// review artifact; the proposal "disappears" from the dry-run
    /// summary on subsequent runs unless the template moves again.
    #[must_use]
    pub fn propose_file(path: impl Into<String>, rendered: String, template_checksum: String) -> Self {
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
    pub fn write_region(host: impl Into<String>, id: impl Into<String>, body: String, spliced_host: String, body_checksum: String) -> Self {
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
    pub fn propose_region(host: impl Into<String>, id: impl Into<String>, spliced_host: String, body_checksum: String) -> Self {
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

    /// Construct a plan item for a `Remove` decision on an owned file —
    /// the file is no longer in the catalog and the user hasn't
    /// customized it.
    #[must_use]
    pub fn remove_file(path: impl Into<String>) -> Self {
        Self {
            target: Target::File { path: path.into() },
            decision: Decision::Remove,
            rendered: None,
            spliced_host: None,
            rendered_checksum: None,
        }
    }

    /// Construct a plan item for a `Remove` decision on a managed region.
    /// `spliced_host` is the host-file content with the region (markers
    /// + body) excised.
    #[must_use]
    pub fn remove_region(host: impl Into<String>, id: impl Into<String>, spliced_host: String) -> Self {
        Self {
            target: Target::Region {
                host: host.into(),
                id: id.into(),
            },
            decision: Decision::Remove,
            rendered: None,
            spliced_host: Some(spliced_host),
            rendered_checksum: None,
        }
    }

    /// Construct a plan item for an `OrphanedKept` decision. The
    /// file/region is no longer in the catalog, but the user has
    /// customized it — leave the disk state alone and drop the
    /// manifest entry to transfer ownership.
    #[must_use]
    pub fn orphaned_kept(target: Target) -> Self {
        Self {
            target,
            decision: Decision::OrphanedKept,
            rendered: None,
            spliced_host: None,
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
    ///
    /// When `previous_manifest` is provided, `Write` items are split
    /// into "Will create" (no prior manifest entry) and "Will update"
    /// (existing entry getting refreshed) per
    /// [`updates.md §9`](../../docs/design/updates.md). The stale-entries
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
        let mut removes: Vec<&PlanItem> = Vec::new();
        let mut orphans_kept: Vec<&PlanItem> = Vec::new();

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
                Decision::Remove => removes.push(item),
                Decision::OrphanedKept => orphans_kept.push(item),
            }
        }

        let mut out = String::new();
        let _ = writeln!(out, "cargo-anvil plan: {} item(s)", self.items.len());

        write_section(&mut out, "Will create", &creates);
        write_section(&mut out, "Will update", &updates);
        write_section(&mut out, "Will propose", &proposes);
        write_section(&mut out, "Will remove", &removes);
        write_section(&mut out, "Orphaned (customized; transferring ownership)", &orphans_kept);
        write_section(&mut out, "Will leave alone (silent)", &leave_alones);

        if !in_syncs.is_empty() {
            let _ = writeln!(out, "Unchanged: {} item(s)", in_syncs.len());
        }

        out
    }

    /// Apply the plan to disk and return an updated manifest.
    ///
    /// - `Write` items write their owned-file content or splice region
    ///   bodies into host files and record their checksums in the new
    ///   manifest.
    /// - `Propose` items write a `.anvil-proposed` sibling and
    ///   bump the manifest entry to the new template checksum so
    ///   subsequent runs see the divergence as resolved
    ///   (`LeaveAlone`) until the template moves again — see
    ///   [`updates.md §5`](../../docs/design/updates.md).
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
    #[expect(
        clippy::too_many_lines,
        clippy::expect_used,
        reason = "single dispatch site covering every (target × decision) pair; the expects encode constructor-enforced invariants"
    )]
    pub fn apply(&self, repo_root: &Path, previous_manifest: &Manifest) -> Result<Manifest, AppError> {
        let mut next = Manifest {
            rendered_by: Some(format!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))),
            files: previous_manifest.files.clone(),
            regions: previous_manifest.regions.clone(),
        };

        for item in &self.items {
            match (&item.target, item.decision) {
                (Target::File { path }, Decision::Write) => {
                    let content = item.rendered.as_ref().expect("Write decision must carry rendered content");
                    let abs = repo_root.join(path);
                    write_file(&abs, content)?;
                    if let Some(checksum) = &item.rendered_checksum {
                        next.files.insert(path.clone(), checksum.clone());
                    }
                }
                (Target::File { path }, Decision::Propose) => {
                    let content = item.rendered.as_ref().expect("Propose decision must carry rendered content");
                    let abs = repo_root.join(format!("{path}.anvil-proposed"));
                    write_file(&abs, content)?;
                    if let Some(checksum) = &item.rendered_checksum {
                        // Bump L to the new T so subsequent runs see the
                        // divergence as resolved (LeaveAlone). The user's
                        // .anvil-proposed sibling stays on disk for
                        // review; deleting or accepting it is the user's
                        // job.
                        next.files.insert(path.clone(), checksum.clone());
                    }
                }
                (Target::Region { host, id }, Decision::Write) => {
                    let spliced = item.spliced_host.as_ref().expect("region Write must carry spliced host");
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
                    let spliced = item.spliced_host.as_ref().expect("region Propose must carry spliced host");
                    let abs = repo_root.join(format!("{host}.anvil-proposed"));
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
                (Target::File { path }, Decision::Remove) => {
                    // Untouched orphan file: delete and drop the
                    // manifest entry. If the file is already missing
                    // (race / external delete), absorb the error so
                    // the result is idempotent.
                    let abs = repo_root.join(path);
                    if let Err(e) = std::fs::remove_file(&abs)
                        && e.kind() != std::io::ErrorKind::NotFound
                    {
                        return Err::<Manifest, _>(e).into_app_err_with(|| format!("failed to remove {}", abs.display()));
                    }
                    next.files.remove(path);
                }
                (Target::Region { host, id }, Decision::Remove) => {
                    // Untouched orphan region: splice the markers + body
                    // out of the host file and drop the manifest entry.
                    let spliced = item.spliced_host.as_ref().expect("region Remove must carry spliced host");
                    let abs = repo_root.join(host);
                    write_file(&abs, spliced)?;
                    next.regions.remove(&RegionKey {
                        host: host.clone(),
                        id: id.clone(),
                    });
                }
                (Target::File { path }, Decision::OrphanedKept) => {
                    // Customized orphan: leave the file in place,
                    // transfer ownership by dropping the manifest entry.
                    next.files.remove(path);
                }
                (Target::Region { host, id }, Decision::OrphanedKept) => {
                    // Customized orphan region: leave the host file
                    // and the in-region content in place, transfer
                    // ownership by dropping the manifest entry.
                    next.regions.remove(&RegionKey {
                        host: host.clone(),
                        id: id.clone(),
                    });
                }
                (_, Decision::InSync) => {
                    // Disk content matches the current template. No
                    // file write needed. We DO refresh the manifest L
                    // to the current template checksum if the plan
                    // item carries one — this self-heals stale-L
                    // values left over from older binary versions
                    // whose hash function differed (e.g. before
                    // line-ending normalization).
                    if let Some(checksum) = &item.rendered_checksum {
                        match &item.target {
                            Target::File { path } => {
                                next.files.insert(path.clone(), checksum.clone());
                            }
                            Target::Region { host, id } => {
                                next.regions.insert(
                                    RegionKey {
                                        host: host.clone(),
                                        id: id.clone(),
                                    },
                                    checksum.clone(),
                                );
                            }
                        }
                    }
                }
                (_, Decision::LeaveAlone) => {
                    // No-op; manifest entry already preserved.
                }
            }
        }

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
        std::fs::create_dir_all(parent).into_app_err_with(|| format!("failed to create parent directory {}", parent.display()))?;
    }
    let tmp = make_temp_path(path);
    std::fs::write(&tmp, content).into_app_err_with(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).into_app_err_with(|| format!("failed to rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn make_temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(std::ffi::OsString::from).unwrap_or_default();
    name.push(".anvil-tmp");
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
        plan.push(PlanItem::noop(Target::File { path: "a.txt".into() }, Decision::InSync));
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
        plan.push(PlanItem::noop(Target::File { path: "b.txt".into() }, Decision::InSync));
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
    fn summary_lists_removal_and_orphan_sections() {
        // Plan items with the new Remove / OrphanedKept decisions surface
        // in dedicated summary sections — they replaced the older
        // implicit "Stale manifest entries" footer because removal is
        // now an explicit plan action rather than a side effect of
        // apply().
        let mut plan = Plan::default();
        plan.push(PlanItem::remove_file("dropped.txt"));
        plan.push(PlanItem::orphaned_kept(Target::Region {
            host: "Justfile".into(),
            id: "anvil-old".into(),
        }));
        let s = plan.summary(None);
        assert!(s.contains("Will remove: 1 item(s)"));
        assert!(s.contains("- dropped.txt"));
        assert!(s.contains("Orphaned (customized; transferring ownership): 1 item(s)"));
        assert!(s.contains("- Justfile [anvil-old]"));
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn apply_writes_owned_file() {
        let tmp = TempDir::new().unwrap();
        let mut plan = Plan::default();
        plan.push(PlanItem::write_file("subdir/a.txt", "hello\n".into(), "sha256:abcd".into()));
        let m = plan.apply(tmp.path(), &Manifest::default()).unwrap();
        let written = std::fs::read_to_string(tmp.path().join("subdir/a.txt")).unwrap();
        assert_eq!(written, "hello\n");
        assert_eq!(m.files.get("subdir/a.txt").map(String::as_str), Some("sha256:abcd"));
        assert!(m.rendered_by.is_some());
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn apply_writes_proposed_file_sibling_and_bumps_manifest() {
        let tmp = TempDir::new().unwrap();
        let mut plan = Plan::default();
        plan.push(PlanItem::propose_file("a.txt", "new content\n".into(), "sha256:newt".into()));
        let m = plan.apply(tmp.path(), &Manifest::default()).unwrap();
        assert!(tmp.path().join("a.txt.anvil-proposed").is_file());
        assert!(!tmp.path().join("a.txt").exists());
        // The manifest L is bumped to the new template checksum so the
        // next run sees the divergence as resolved (LeaveAlone), not as
        // a fresh proposal.
        assert_eq!(m.files.get("a.txt").map(String::as_str), Some("sha256:newt"));
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn apply_region_write_splices_host() {
        let tmp = TempDir::new().unwrap();
        let mut plan = Plan::default();
        plan.push(PlanItem::write_region(
            "Justfile",
            "anvil-imports",
            "import 'foo'\n".into(),
            "user content\n# >>> anvil-managed: anvil-imports\nimport 'foo'\n# <<< anvil-managed: anvil-imports\n".into(),
            "sha256:body".into(),
        ));
        let m = plan.apply(tmp.path(), &Manifest::default()).unwrap();
        let written = std::fs::read_to_string(tmp.path().join("Justfile")).unwrap();
        assert!(written.contains("# >>> anvil-managed: anvil-imports"));
        let key = RegionKey {
            host: "Justfile".into(),
            id: "anvil-imports".into(),
        };
        assert_eq!(m.regions.get(&key).map(String::as_str), Some("sha256:body"));
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn apply_region_propose_writes_sibling_and_bumps_manifest() {
        let tmp = TempDir::new().unwrap();
        let mut plan = Plan::default();
        plan.push(PlanItem::propose_region(
            "Justfile",
            "anvil-imports",
            "spliced host content\n".into(),
            "sha256:newt".into(),
        ));
        let m = plan.apply(tmp.path(), &Manifest::default()).unwrap();
        assert!(tmp.path().join("Justfile.anvil-proposed").is_file());
        // Region L is bumped to the new template checksum on propose;
        // subsequent runs see LeaveAlone until the template changes.
        let key = RegionKey {
            host: "Justfile".into(),
            id: "anvil-imports".into(),
        };
        assert_eq!(m.regions.get(&key).map(String::as_str), Some("sha256:newt"));
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn apply_insync_refreshes_stale_manifest_l() {
        // Regression test: when an older binary recorded an L using
        // a different hash function (e.g. pre-line-ending-normalization),
        // and the current binary observes F == T (InSync), the manifest
        // L gets self-healed to the current T. Without this refresh,
        // the next template change would mis-classify as Propose
        // because the algorithm would see F ≠ L and assume user
        // customization.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "current content\n").unwrap();
        let mut prev = Manifest::default();
        prev.set_file("a.txt", "sha256:stale-from-older-binary");
        let mut plan = Plan::default();
        plan.push(PlanItem::insync(
            Target::File { path: "a.txt".into() },
            "sha256:current-template".into(),
        ));
        let next = plan.apply(tmp.path(), &prev).unwrap();
        assert_eq!(
            next.files.get("a.txt").map(String::as_str),
            Some("sha256:current-template"),
            "InSync must refresh the manifest L to the current template checksum",
        );
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn apply_leave_alone_does_not_refresh_manifest_l() {
        // Dual of the above: LeaveAlone explicitly preserves L. The
        // user has diverged from the template; bumping L = T would
        // make the next run see F ≠ L (true), L == T (true) → Write,
        // which would silently overwrite the user's customization.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "user edited\n").unwrap();
        let mut prev = Manifest::default();
        prev.set_file("a.txt", "sha256:original");
        let mut plan = Plan::default();
        plan.push(PlanItem::noop(Target::File { path: "a.txt".into() }, Decision::LeaveAlone));
        let next = plan.apply(tmp.path(), &prev).unwrap();
        assert_eq!(
            next.files.get("a.txt").map(String::as_str),
            Some("sha256:original"),
            "LeaveAlone must preserve the manifest L unchanged",
        );
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn apply_remove_deletes_file_and_purges_manifest() {
        // A Remove plan item deletes the file from disk and drops
        // the manifest entry. This is the path the new plan_removals
        // hook takes for an untouched orphan (replacing the old
        // safety-net purge that only touched the manifest).
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("dropped.txt"), "old content\n").unwrap();
        let mut prev = Manifest::default();
        prev.set_file("dropped.txt", "sha256:old");
        let mut plan = Plan::default();
        plan.push(PlanItem::remove_file("dropped.txt"));
        let next = plan.apply(tmp.path(), &prev).unwrap();
        assert!(!tmp.path().join("dropped.txt").exists(), "Remove must delete the file from disk");
        assert!(next.files.is_empty(), "Remove must drop the manifest entry: {:?}", next.files);
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn apply_remove_region_splices_out_markers_and_body() {
        // A Region Remove plan item replaces the host file with
        // its spliced-out content (markers + body excised), and
        // drops the manifest entry. The spliced_host payload is
        // computed by the plan builder via region::remove_region.
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Justfile"),
            "before\n\n# >>> anvil-managed: r\nbody\n# <<< anvil-managed: r\nafter\n",
        )
        .unwrap();
        let mut prev = Manifest::default();
        prev.set_region("Justfile", "r", "sha256:body");
        let mut plan = Plan::default();
        plan.push(PlanItem::remove_region("Justfile", "r", "before\nafter\n".to_string()));
        let next = plan.apply(tmp.path(), &prev).unwrap();
        let host = std::fs::read_to_string(tmp.path().join("Justfile")).unwrap();
        assert_eq!(host, "before\nafter\n");
        assert!(next.regions.is_empty());
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn apply_orphaned_kept_preserves_disk_and_drops_manifest() {
        // An OrphanedKept plan item leaves the file/region alone and
        // drops the manifest entry — transferring ownership to the
        // user.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("custom.txt"), "user edited\n").unwrap();
        let mut prev = Manifest::default();
        prev.set_file("custom.txt", "sha256:original");
        let mut plan = Plan::default();
        plan.push(PlanItem::orphaned_kept(Target::File { path: "custom.txt".into() }));
        let next = plan.apply(tmp.path(), &prev).unwrap();
        let live = std::fs::read_to_string(tmp.path().join("custom.txt")).unwrap();
        assert_eq!(live, "user edited\n");
        assert!(next.files.is_empty());
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn apply_remove_missing_file_is_idempotent() {
        // Race: the file is already gone (someone deleted it
        // externally between the plan build and apply). Remove must
        // still complete cleanly and purge the manifest.
        let tmp = TempDir::new().unwrap();
        let mut prev = Manifest::default();
        prev.set_file("absent.txt", "sha256:old");
        let mut plan = Plan::default();
        plan.push(PlanItem::remove_file("absent.txt"));
        let next = plan.apply(tmp.path(), &prev).unwrap();
        assert!(next.files.is_empty());
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn apply_preserves_in_plan_manifest_entries() {
        // The dual of the purging test: a manifest entry whose target IS
        // in the plan (even as InSync/LeaveAlone) survives.
        let tmp = TempDir::new().unwrap();
        let mut prev = Manifest::default();
        prev.set_file("kept.txt", "sha256:k");
        let mut plan = Plan::default();
        plan.push(PlanItem::noop(Target::File { path: "kept.txt".into() }, Decision::LeaveAlone));
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
