// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Top-level `update` driver.
//!
//! Orchestrates: workspace discovery, manifest load, backend resolution,
//! emitter invocation, plan accumulation, and final apply/summarize.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use ohno::{AppError, bail};
use tracing::info;

use crate::backend::{self, Backend};
use crate::catalog::Catalog;
use crate::catalog::artifact::{Artifact, HostSelector, RegionSpec};
use crate::checksum::checksum_str;
use crate::cli::Cli;
use crate::decision::{Decision, RemovalDecision, decide_removal};
use crate::emit::{plan_managed_region, plan_owned_file};
use crate::io::{read_file_if_present, resolve_existing_case_insensitive};
use crate::manifest::Manifest;
use crate::plan::{Plan, PlanItem, Target};
use crate::region::{CommentSyntax, find_region, remove_region, upsert_region};
use crate::workspace::{self, Workspace};

/// Outcome of an `update` invocation.
#[derive(Debug)]
pub struct RunOutcome {
    /// The plan that was built.
    pub plan: Plan,
    /// The manifest as it existed before this run (useful for the
    /// categorized summary so `Will create` and `Will update` can be
    /// distinguished, and so stale entries can be enumerated).
    pub previous_manifest: Manifest,
    /// Whether the plan was actually applied to disk.
    pub applied: bool,
    /// The resolved backend set.
    pub backends: Vec<Backend>,
}

/// Run the parsed CLI.
///
/// # Errors
///
/// Returns an error when the underlying update flow fails.
#[cfg_attr(coverage_nightly, coverage(off))]
#[mutants::skip] // Thin process-boundary glue (cwd lookup, stdout print, `std::process::exit`); behavior covered by `run_update` tests which exercise every dispatch path.
pub fn run(catalog: &Catalog, cli: &Cli) -> Result<(), AppError> {
    let outcome = run_update(catalog, cli, &std::env::current_dir()?)?;
    print!("{}", outcome.plan.summary(Some(&outcome.previous_manifest)));
    if cli.dry_run && outcome.plan.has_changes() {
        std::process::exit(1);
    }
    Ok(())
}

/// Run the update flow against the workspace containing `start_dir`.
///
/// Exposed for integration tests that want to drive the algorithm
/// without `std::process::exit`.
///
/// # Errors
///
/// Propagates errors from any subsystem (workspace discovery, manifest
/// I/O, emitter, plan application).
pub fn run_update(catalog: &Catalog, args: &Cli, start_dir: &Path) -> Result<RunOutcome, AppError> {
    let repo_root = workspace::find_workspace_root(start_dir)?;
    let manifest = Manifest::load(&repo_root)?;

    // The single-tool guard: a repository is managed by exactly one
    // anvil-family tool. If the lock records a *different* tool, refuse
    // before doing any other work — content-free, and honored even under
    // --dry-run — unless --force is passed to switch ownership to this tool.
    // A lock with no `tool` field (first run, or a legacy pre-split lock) is
    // never blocked. See updates.md §1 "The single-tool guard".
    //
    // This runs immediately after loading the lock and before
    // `load_workspace`, so a mismatched lock reliably refuses regardless of
    // the workspace shape — the wrong tool never reaches workspace/member
    // parsing, which could otherwise surface unrelated errors first.
    enforce_single_tool_guard(catalog, args, &manifest)?;

    let ws = workspace::load_workspace(&repo_root)?;

    let backends = backend::resolve(&args.backends, args.no_backends, &repo_root)?;
    info!(
        repo_root = %repo_root.display(),
        backends = ?backends.iter().map(|b| b.name()).collect::<Vec<_>>(),
        dry_run = args.dry_run,
        "anvil"
    );

    let plan = build_plan(&repo_root, &ws, &manifest, &backends, catalog)?;

    let applied = if args.dry_run {
        false
    } else {
        let mut next = plan.apply(&repo_root, &manifest)?;
        // Stamp this tool's provenance on every save.
        next.tool = Some(catalog.cli().subcommand.clone());
        next.tool_version = Some(catalog.cli().version.clone());
        next.catalog_checksum = Some(catalog.checksum());
        next.save(&repo_root)?;
        true
    };

    Ok(RunOutcome {
        plan,
        previous_manifest: manifest,
        applied,
        backends,
    })
}

/// Enforce the single-tool guard: refuse if the lock names a different tool
/// and `--force` was not passed.
///
/// # Errors
///
/// Returns a refusal error when `manifest.tool` is `Some` and differs from
/// `catalog.cli().subcommand` and `args.force` is `false`.
fn enforce_single_tool_guard(catalog: &Catalog, args: &Cli, manifest: &Manifest) -> Result<(), AppError> {
    let current = &catalog.cli().subcommand;
    if let Some(owner) = &manifest.tool
        && owner != current
        && !args.force
    {
        bail!(
            "this repository is managed by '{owner}' (per .anvil.lock); refusing to run '{current}'. \
             A repository must be managed by a single anvil-family tool. Run '{owner}' instead, \
             or re-run with --force to switch this repository to '{current}'."
        );
    }
    Ok(())
}

/// Build the full plan by iterating the catalog's artifacts.
///
/// Each artifact dispatches to the generic owned-file / managed-region
/// driver. Owned files carrying a backend `gate` are emitted only when that
/// backend is in the resolved set. Managed-region host selectors are expanded
/// against the discovered workspace (see [`push_region`]). Every path is
/// resolved to its on-disk casing so anvil follows whatever a repo already
/// uses (e.g. `justfile` vs `Justfile`).
fn build_plan(
    repo_root: &Path,
    workspace: &Workspace,
    manifest: &Manifest,
    backends: &[Backend],
    catalog: &Catalog,
) -> Result<Plan, AppError> {
    let mut plan = Plan::default();
    let mut hosts = HostTextCache::default();

    for artifact in catalog.artifacts() {
        match artifact {
            Artifact::OwnedFile(spec) => {
                let selected = spec.gate.is_none_or(|gate| backends.contains(&gate));
                if selected {
                    let path = resolve_existing_case_insensitive(repo_root, spec.path);
                    plan.push(plan_owned_file(repo_root, manifest, &path, &spec.body)?);
                }
            }
            Artifact::Region(spec) => {
                push_region(repo_root, workspace, manifest, &mut plan, &mut hosts, spec)?;
            }
        }
    }

    plan_removals(repo_root, manifest, &mut plan, &mut hosts)?;

    // Region proposals are computed eagerly as each region is visited, so a
    // `Propose` planned before a sibling `Write`/`Remove` on the same host
    // captures a stale host (missing the later update). The accumulator is
    // fully composed now, so re-splice every proposal against it.
    recompose_region_proposals(repo_root, &mut plan, &mut hosts)?;

    Ok(plan)
}

/// Re-splice every region `Propose` item's `.anvil-proposed` payload against
/// the *final* composed host text — the in-memory host after all `Write` and
/// region `Remove` operations for that host have been folded into the
/// accumulator.
///
/// Proposals are planned eagerly as each region is visited (see
/// [`push_region_at`]), so a proposal computed before a sibling `Write` or
/// region `Remove` on the same host would otherwise capture a stale host
/// (missing the later update). Applying such a proposal via
/// `mv <host>.anvil-proposed <host>` would silently revert those sibling
/// updates. Re-splicing here guarantees the proposed sibling is composed on
/// top of every applied region update in the run, honoring `updates.md`'s
/// "ready-to-use" proposal guarantee.
///
/// When several regions on one host propose, each proposal's new body is
/// spliced on top of the others' too, so every proposal for the host (which
/// all share the single `<host>.anvil-proposed` path) converges on the same
/// fully-updated content instead of the last write clobbering the rest.
fn recompose_region_proposals(repo_root: &Path, plan: &mut Plan, hosts: &mut HostTextCache) -> Result<(), AppError> {
    // First pass: collect each region `Propose`'s (index, id, rendered body),
    // grouped by host and preserving first-seen host order so the
    // recomposition is deterministic.
    let mut hosts_in_order: Vec<String> = Vec::new();
    let mut grouped: HashMap<String, Vec<(usize, String, String)>> = HashMap::new();
    for (idx, item) in plan.items().iter().enumerate() {
        let Target::Region { host, id } = &item.target else {
            continue;
        };
        if item.decision != Decision::Propose {
            continue;
        }
        let body = item.rendered.clone().expect("region Propose carries its rendered body");
        if !grouped.contains_key(host) {
            hosts_in_order.push(host.clone());
        }
        grouped.entry(host.clone()).or_default().push((idx, id.clone(), body));
    }

    for host in &hosts_in_order {
        let entries = &grouped[host];
        let Some(mut composed) = hosts.get_or_read(repo_root, host)? else {
            // Host file vanished (external race during the run); keep the
            // eagerly-computed proposals rather than dropping them.
            continue;
        };
        // Build the fully-updated host = final live host with every proposed
        // region's new body spliced in. CommentSyntax is currently always
        // Hash for managed regions (mirrors plan_managed_region /
        // plan_removals); revisit when the manifest records per-region syntax.
        for (_, id, body) in entries {
            composed = upsert_region(&composed, id, body, CommentSyntax::Hash)?;
        }
        // Stamp the composed host onto every proposal for this host.
        for (idx, _, _) in entries {
            plan.items_mut()[*idx].spliced_host = Some(composed.clone());
        }
    }
    Ok(())
}

/// In-memory accumulator of host-file text, shared across every region
/// (and region removal) targeting the same host file within one planning
/// pass.
///
/// Several managed regions can target a single host file — for example the
/// `[advisories]`, `[licenses]`, `[bans]`, and `[sources]` sections of
/// `deny.toml`. Planning each region against the original on-disk text and
/// then writing each region's full spliced host back would make the last
/// write overwrite the others (and, for a brand-new file, lose every region
/// but the last). Instead, the first region to touch a host seeds the
/// cache from disk; every subsequent region splices against — and, when it
/// writes, updates — the accumulated in-memory text, so the composed
/// result preserves every region. See `updates.md §4`.
#[derive(Default)]
struct HostTextCache {
    texts: HashMap<String, Option<String>>,
}

impl HostTextCache {
    /// The current in-memory text for `host`, reading from disk on first
    /// access. `None` means the host file does not (yet) exist on disk and
    /// no in-memory write has created it.
    fn get_or_read(&mut self, repo_root: &Path, host: &str) -> Result<Option<String>, AppError> {
        if let Some(text) = self.texts.get(host) {
            return Ok(text.clone());
        }
        let text = read_file_if_present(&repo_root.join(host))?;
        self.texts.insert(host.to_owned(), text.clone());
        Ok(text)
    }

    /// Record the host text that results from splicing a region in or out
    /// in memory, so later regions targeting the same host compose on top
    /// of it. Only operations that change the host file on disk (`Write`,
    /// region `Remove`) update the cache; proposals leave the live host
    /// untouched and so must not.
    fn set(&mut self, host: &str, text: String) {
        self.texts.insert(host.to_owned(), Some(text));
    }
}

/// Dispatch one managed-region artifact into the plan, expanding its host
/// selector against the discovered workspace.
///
/// - [`HostSelector::Path`] — a single literal host.
/// - [`HostSelector::EachMemberManifest`] — one host per workspace member (no
///   hosts in a single-crate repo, which has no workspace members).
/// - [`HostSelector::WorkspaceCargoToml`] / [`HostSelector::SingleCrateCargoToml`]
///   — the root `Cargo.toml`, gated on whether it declares a `[workspace]`
///   table.
fn push_region(
    repo_root: &Path,
    workspace: &Workspace,
    manifest: &Manifest,
    plan: &mut Plan,
    hosts: &mut HostTextCache,
    spec: &RegionSpec,
) -> Result<(), AppError> {
    match &spec.host {
        HostSelector::Path(path) => {
            push_region_at(repo_root, manifest, plan, hosts, path, spec)?;
        }
        HostSelector::WorkspaceCargoToml => {
            if workspace.has_workspace_table {
                push_region_at(repo_root, manifest, plan, hosts, "Cargo.toml", spec)?;
            }
        }
        HostSelector::SingleCrateCargoToml => {
            if !workspace.has_workspace_table {
                push_region_at(repo_root, manifest, plan, hosts, "Cargo.toml", spec)?;
            }
        }
        HostSelector::EachMemberManifest => {
            for member in &workspace.members {
                push_region_at(repo_root, manifest, plan, hosts, &member.manifest_relpath, spec)?;
            }
        }
    }
    Ok(())
}

/// Plan one managed region at a single host, resolving the host's on-disk
/// casing first.
///
/// The region is planned against the host's *accumulated* in-memory text
/// (see [`HostTextCache`]), so a region composes on top of any earlier
/// region already spliced into the same host this pass. When the decision
/// writes, the spliced result is fed back into the cache so the next
/// region targeting this host builds on it instead of re-reading the
/// original disk state.
fn push_region_at(
    repo_root: &Path,
    manifest: &Manifest,
    plan: &mut Plan,
    hosts: &mut HostTextCache,
    host: &str,
    spec: &RegionSpec,
) -> Result<(), AppError> {
    let host = resolve_existing_case_insensitive(repo_root, host);
    let current = hosts.get_or_read(repo_root, &host)?;
    let item = plan_managed_region(manifest, &host, current.as_deref(), spec.id.as_str(), &spec.body, spec.syntax)?;
    // Only a `Write` mutates the live host on disk; fold its spliced
    // output back into the accumulator so sibling regions compose. A
    // `Propose` writes a sibling, not the host, so it must not advance the
    // live host text.
    if item.decision == Decision::Write
        && let Some(spliced) = &item.spliced_host
    {
        hosts.set(&host, spliced.clone());
    }
    plan.push(item);
    Ok(())
}

/// Scan the previous manifest for entries that the active plan items
/// don't cover. For each, classify as `Remove` (user untouched since the
/// last render, or the file is already gone — the manifest entry is
/// purged and the disk delete is a no-op when absent) or `OrphanedKept`
/// (user customized — preserve and transfer ownership).
///
/// This is what removes orphaned cloud-workflow artifacts, dropped catalog entries,
/// disabled-backend files, and any other previously-tracked item that
/// is no longer in scope.
fn plan_removals(repo_root: &Path, previous: &Manifest, plan: &mut Plan, hosts: &mut HostTextCache) -> Result<(), AppError> {
    let live_files: BTreeSet<String> = plan
        .items()
        .iter()
        .filter_map(|i| match &i.target {
            Target::File { path } => Some(path.clone()),
            Target::Region { .. } => None,
        })
        .collect();
    let live_regions: BTreeSet<(String, String)> = plan
        .items()
        .iter()
        .filter_map(|i| match &i.target {
            Target::Region { host, id } => Some((host.clone(), id.clone())),
            Target::File { .. } => None,
        })
        .collect();

    for (path, last) in &previous.files {
        if live_files.contains(path) {
            continue;
        }
        let disk = read_file_if_present(&repo_root.join(path))?;
        let disk_checksum = disk.as_deref().map(checksum_str);
        match decide_removal(last, disk_checksum.as_deref()) {
            // A file still matching its last render is safe to delete; an
            // `AlreadyGone` file is removed too -- there is nothing on disk,
            // so `remove_file`'s NotFound-idempotent apply is a no-op, while
            // the summary accurately reports a removal (purging the stale
            // manifest entry) instead of a misleading "customized orphan"
            // transfer.
            RemovalDecision::Remove | RemovalDecision::AlreadyGone => plan.push(PlanItem::remove_file(path.clone())),
            // User customized the file since the last render: leave it in
            // place and drop the manifest entry to transfer ownership.
            RemovalDecision::OrphanedKept => {
                plan.push(PlanItem::orphaned_kept(Target::File { path: path.clone() }));
            }
        }
    }

    for (key, last) in &previous.regions {
        if live_regions.contains(&(key.host.clone(), key.id.clone())) {
            continue;
        }
        let Some(host_text) = hosts.get_or_read(repo_root, &key.host)? else {
            // Host file is gone entirely; just drop the manifest
            // entry. Emit OrphanedKept (no-op apply) so the plan
            // can record the transfer of ownership consistently.
            plan.push(PlanItem::orphaned_kept(Target::Region {
                host: key.host.clone(),
                id: key.id.clone(),
            }));
            continue;
        };

        // CommentSyntax is currently always Hash for managed regions.
        // When that assumption changes, the manifest will need to
        // record the syntax used.
        let syntax = CommentSyntax::Hash;
        let region = find_region(&host_text, &key.id, syntax)?;
        let body_checksum = region.as_ref().map(|r| checksum_str(r.body_str()));
        match decide_removal(last, body_checksum.as_deref()) {
            RemovalDecision::Remove => {
                // Splice against — and update — the accumulated host text
                // so a removal composes with the writes already planned
                // for this host this pass instead of clobbering them
                // (their item is applied earlier; this one, later).
                let spliced = remove_region(&host_text, &key.id, syntax)?;
                hosts.set(&key.host, spliced.clone());
                plan.push(PlanItem::remove_region(key.host.clone(), key.id.clone(), spliced));
            }
            RemovalDecision::OrphanedKept | RemovalDecision::AlreadyGone => {
                plan.push(PlanItem::orphaned_kept(Target::Region {
                    host: key.host.clone(),
                    id: key.id.clone(),
                }));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::anvil::artifacts::region;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn empty_workspace() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("Cargo.toml"),
            "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n",
        );
        write(
            &root.join("crates/alpha/Cargo.toml"),
            "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        );
        write(&root.join("crates/alpha/src/lib.rs"), "");
        tmp
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn existing_lowercase_justfile_is_reused_not_duplicated() {
        // Proposal: anvil follows whatever casing the repo already uses. A
        // pre-existing lowercase `justfile` must be spliced into, not shadowed
        // by a new capital `Justfile`.
        let tmp = empty_workspace();
        std::fs::write(tmp.path().join("justfile"), "# user recipes\n").unwrap();
        let args = local_only();

        let outcome = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        assert!(outcome.applied);

        let lower = std::fs::read_to_string(tmp.path().join("justfile")).unwrap();
        assert!(lower.contains("# user recipes"), "user content preserved");
        assert!(
            lower.contains("anvil-managed: anvil-imports"),
            "region spliced into the lowercase file"
        );

        // The manifest tracks the on-disk (lowercase) host, so a second run is
        // a no-op rather than re-proposing or orphaning the region.
        let second = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        assert!(!second.plan.has_changes(), "second run should be idempotent");
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn first_run_writes_everything_local_only() {
        let tmp = empty_workspace();
        let args = Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        };
        let outcome = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        assert!(outcome.applied);
        assert!(outcome.backends.is_empty());
        assert!(outcome.plan.has_changes());

        for expected in [
            "Justfile",
            "justfiles/anvil/mod.just",
            "justfiles/anvil/checks.just",
            "justfiles/anvil/groups.just",
            "justfiles/anvil/tiers.just",
            "justfiles/anvil/tools.just",
            "justfiles/anvil/versions.just",
            "deny.toml",
            "rustfmt.toml",
            ".delta.toml",
            "spellcheck.toml",
            "clippy.toml",
            ".anvil.lock",
        ] {
            assert!(tmp.path().join(expected).is_file(), "expected '{expected}' after update");
        }

        let root_manifest = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(root_manifest.contains("# >>> anvil-managed: anvil-workspace-lints"));
        let member_manifest = fs::read_to_string(tmp.path().join("crates/alpha/Cargo.toml")).unwrap();
        assert!(member_manifest.contains("# >>> anvil-managed: anvil-lints"));
        assert!(member_manifest.contains("workspace = true"));
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn second_run_is_idempotent_and_in_sync() {
        let tmp = empty_workspace();
        let args = Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        };
        let _ = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        let second = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        assert!(!second.plan.has_changes(), "second run should be a no-op");
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn dry_run_does_not_write() {
        let tmp = empty_workspace();
        let args = Cli {
            backends: vec![],
            no_backends: true,
            dry_run: true,
            force: false,
        };
        let outcome = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        assert!(!outcome.applied);
        assert!(outcome.plan.has_changes());
        assert!(!tmp.path().join("justfiles/anvil/tools.just").exists());
        assert!(!tmp.path().join(".anvil.lock").exists());
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn run_stamps_tool_and_catalog_checksum_into_lock() {
        let tmp = empty_workspace();
        let args = Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        };
        let catalog = Catalog::anvil();
        let _ = run_update(&catalog, &args, tmp.path()).unwrap();

        let saved = Manifest::load(tmp.path()).unwrap();
        assert_eq!(saved.tool.as_deref(), Some("anvil"));
        assert_eq!(saved.tool_version, Some(catalog.cli().version.clone()));
        assert_eq!(saved.catalog_checksum, Some(catalog.checksum()));
    }

    fn local_only() -> Cli {
        Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        }
    }

    fn seed_lock_owner(root: &Path, tool: &str) {
        let m = Manifest {
            tool: Some(tool.to_owned()),
            ..Manifest::default()
        };
        m.save(root).unwrap();
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn guard_allows_matching_tool() {
        let tmp = empty_workspace();
        seed_lock_owner(tmp.path(), "anvil");
        let outcome = run_update(&Catalog::anvil(), &local_only(), tmp.path()).unwrap();
        assert!(outcome.applied);
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn guard_refuses_mismatched_tool_and_writes_nothing() {
        let tmp = empty_workspace();
        seed_lock_owner(tmp.path(), "forge2");
        let err = run_update(&Catalog::anvil(), &local_only(), tmp.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("managed by 'forge2'"), "got: {msg}");
        assert!(msg.contains("--force"), "refusal should suggest --force; got: {msg}");
        assert!(!tmp.path().join("justfiles/anvil/tools.just").exists(), "guard must write nothing");
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn guard_refuses_before_workspace_parsing() {
        // A root that `find_workspace_root` accepts (it declares
        // `[workspace]`) but that `load_workspace` would reject (the explicit
        // member `crates/missing` does not exist). With a lock naming a
        // different tool, the guard must fire first: the refusal — not a
        // workspace-parse error — is what surfaces. This pins the ordering
        // (guard runs immediately after loading the lock, before
        // `load_workspace`).
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("Cargo.toml"),
            "[workspace]\nresolver = \"2\"\nmembers = [\"crates/missing\"]\n",
        );
        seed_lock_owner(root, "forge2");

        let err = run_update(&Catalog::anvil(), &local_only(), root).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("managed by 'forge2'"),
            "guard must refuse before workspace parsing; got: {msg}"
        );
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn guard_refuses_mismatched_tool_under_dry_run() {
        let tmp = empty_workspace();
        seed_lock_owner(tmp.path(), "forge2");
        let args = Cli {
            dry_run: true,
            ..local_only()
        };
        let err = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap_err();
        assert!(err.to_string().contains("managed by 'forge2'"));
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn force_switches_ownership_and_rewrites_provenance() {
        let tmp = empty_workspace();
        seed_lock_owner(tmp.path(), "forge2");
        let args = Cli {
            force: true,
            ..local_only()
        };
        let catalog = Catalog::anvil();
        let outcome = run_update(&catalog, &args, tmp.path()).unwrap();
        assert!(outcome.applied, "force should proceed as a normal update");
        assert!(tmp.path().join("justfiles/anvil/tools.just").is_file());
        let saved = Manifest::load(tmp.path()).unwrap();
        assert_eq!(saved.tool.as_deref(), Some("anvil"), "force rewrites the lock owner");
        assert_eq!(saved.catalog_checksum, Some(catalog.checksum()));
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn legacy_lock_without_tool_is_not_blocked() {
        let tmp = empty_workspace();
        // A pre-split lock: rendered_by present, no `tool` field.
        std::fs::write(tmp.path().join(".anvil.lock"), "version = 1\nrendered_by = \"cargo-anvil 0.0.1\"\n").unwrap();
        let outcome = run_update(&Catalog::anvil(), &local_only(), tmp.path()).unwrap();
        assert!(outcome.applied, "a legacy lock with no tool must not trigger the guard");
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn opted_out_region_is_skipped_on_second_run() {
        let tmp = empty_workspace();
        let args = Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        };
        let _ = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();

        let path = tmp.path().join("rustfmt.toml");
        let host = fs::read_to_string(&path).unwrap();
        let updated = crate::region::upsert_region(&host, region::RUSTFMT_REGION_ID, "", crate::region::CommentSyntax::Hash).unwrap();
        fs::write(&path, updated).unwrap();

        let outcome = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        let rustfmt_item = outcome
            .plan
            .items()
            .iter()
            .find(|i| {
                matches!(&i.target, crate::plan::Target::Region { host, id }
                    if host == "rustfmt.toml" && id == region::RUSTFMT_REGION_ID)
            })
            .expect("rustfmt region item missing from plan");
        assert_eq!(rustfmt_item.decision, crate::decision::Decision::LeaveAlone);
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn user_edit_inside_region_left_alone_when_template_unchanged() {
        let tmp = empty_workspace();
        let args = Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        };
        let _ = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();

        let path = tmp.path().join("rustfmt.toml");
        let host = fs::read_to_string(&path).unwrap();
        let updated = crate::region::upsert_region(
            &host,
            region::RUSTFMT_REGION_ID,
            "edition = \"2021\"\n",
            crate::region::CommentSyntax::Hash,
        )
        .unwrap();
        fs::write(&path, updated).unwrap();

        let outcome = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        let rustfmt_item = outcome
            .plan
            .items()
            .iter()
            .find(|i| {
                matches!(&i.target, crate::plan::Target::Region { host, id }
                    if host == "rustfmt.toml" && id == region::RUSTFMT_REGION_ID)
            })
            .unwrap();
        assert_eq!(rustfmt_item.decision, crate::decision::Decision::LeaveAlone);
        let final_text = fs::read_to_string(&path).unwrap();
        assert!(final_text.contains("edition = \"2021\""));
    }

    /// Verifies B6: after a Propose decision, the next run sees the
    /// divergence as `LeaveAlone` (no re-proposal) until the template
    /// itself moves again.
    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn propose_burns_through_after_one_run() {
        use crate::checksum::checksum_str;
        use crate::manifest::{Manifest, RegionKey};

        let tmp = empty_workspace();
        let args = Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        };

        // First update: write everything.
        let _ = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();

        // User edits the rustfmt region.
        let path = tmp.path().join("rustfmt.toml");
        let host = fs::read_to_string(&path).unwrap();
        let edited = crate::region::upsert_region(
            &host,
            region::RUSTFMT_REGION_ID,
            "edition = \"2021\"\n",
            crate::region::CommentSyntax::Hash,
        )
        .unwrap();
        fs::write(&path, edited).unwrap();

        // Simulate the template moving on by hand-editing the manifest's
        // recorded checksum for the region to a value other than what
        // the user has and other than the current template. That way
        // the next run sees D ≠ L ≠ T → Propose.
        let manifest_path = Manifest::path_for(tmp.path());
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        let key = RegionKey {
            host: "rustfmt.toml".to_owned(),
            id: region::RUSTFMT_REGION_ID.to_owned(),
        };
        manifest.regions.insert(key, checksum_str("synthetic old template"));
        manifest.save(tmp.path()).unwrap();
        let _ = manifest_path; // sanity

        // Second update: should Propose (user diverged + template moved).
        let second = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        let item = second
            .plan
            .items()
            .iter()
            .find(|i| {
                matches!(&i.target, crate::plan::Target::Region { host, id }
                    if host == "rustfmt.toml" && id == region::RUSTFMT_REGION_ID)
            })
            .unwrap();
        assert_eq!(item.decision, crate::decision::Decision::Propose);
        assert!(
            tmp.path().join("rustfmt.toml.anvil-proposed").is_file(),
            "expected a proposed sibling after the Propose run"
        );

        // Third update: nothing has changed since the second run; the
        // proposal should have been "burned through" and the next run
        // should see LeaveAlone (D ≠ L, L = T) — not Propose.
        let third = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        let item = third
            .plan
            .items()
            .iter()
            .find(|i| {
                matches!(&i.target, crate::plan::Target::Region { host, id }
                    if host == "rustfmt.toml" && id == region::RUSTFMT_REGION_ID)
            })
            .unwrap();
        assert_eq!(
            item.decision,
            crate::decision::Decision::LeaveAlone,
            "Propose should bump L = T so subsequent runs see LeaveAlone"
        );
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn github_backend_writes_full_dotgithub_tree() {
        let tmp = empty_workspace();
        let args = Cli {
            backends: vec!["github".to_owned()],
            no_backends: false,
            dry_run: false,
            force: false,
        };
        let outcome = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        assert!(outcome.applied);
        assert_eq!(outcome.backends, vec![Backend::GitHub]);
        for expected in [
            ".github/actions/anvil-setup/action.yml",
            ".github/actions/anvil-impact/action.yml",
            ".github/actions/anvil-pr-fast/action.yml",
            ".github/actions/anvil-pr-test/action.yml",
            ".github/actions/anvil-pr-runtime-analysis/action.yml",
            ".github/actions/anvil-pr-mutants/action.yml",
            ".github/actions/anvil-scheduled-test/action.yml",
            ".github/actions/anvil-scheduled-advisories/action.yml",
            ".github/actions/anvil-scheduled-runtime-analysis/action.yml",
            ".github/actions/anvil-scheduled-exhaustive/action.yml",
            ".github/workflows/anvil-pr-impl.yml",
            ".github/workflows/anvil-scheduled-impl.yml",
            ".github/workflows/anvil-pr.yml",
            ".github/workflows/anvil-scheduled.yml",
        ] {
            assert!(tmp.path().join(expected).is_file(), "expected '{expected}' after github update");
        }
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn github_backend_idempotent() {
        let tmp = empty_workspace();
        let args = Cli {
            backends: vec!["github".to_owned()],
            no_backends: false,
            dry_run: false,
            force: false,
        };
        let _ = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        let second = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        assert!(
            !second.plan.has_changes(),
            "second github run should be a no-op:\n{}",
            second.plan.summary(Some(&second.previous_manifest))
        );
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn ado_backend_writes_full_pipelines_tree() {
        let tmp = empty_workspace();
        let args = Cli {
            backends: vec!["ado".to_owned()],
            no_backends: false,
            dry_run: false,
            force: false,
        };
        let outcome = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        assert!(outcome.applied);
        assert_eq!(outcome.backends, vec![Backend::Ado]);
        for expected in [
            ".pipelines/anvil/steps/setup.yml",
            ".pipelines/anvil/steps/impact.yml",
            ".pipelines/anvil/steps/advisory-comments.yml",
            ".pipelines/anvil/steps/pr-fast.yml",
            ".pipelines/anvil/steps/pr-test.yml",
            ".pipelines/anvil/steps/pr-runtime-analysis.yml",
            ".pipelines/anvil/steps/pr-mutants.yml",
            ".pipelines/anvil/steps/scheduled-test.yml",
            ".pipelines/anvil/steps/scheduled-advisories.yml",
            ".pipelines/anvil/steps/scheduled-runtime-analysis.yml",
            ".pipelines/anvil/steps/scheduled-exhaustive.yml",
            ".pipelines/anvil/pr.yml",
            ".pipelines/anvil/scheduled.yml",
            ".pipelines/anvil-pr.yml",
            ".pipelines/anvil-scheduled.yml",
        ] {
            assert!(tmp.path().join(expected).is_file(), "expected '{expected}' after ado update");
        }
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn both_backends_idempotent() {
        let tmp = empty_workspace();
        let args = Cli {
            backends: vec!["github".to_owned(), "ado".to_owned()],
            no_backends: false,
            dry_run: false,
            force: false,
        };
        let _ = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        let second = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        assert!(!second.plan.has_changes());
    }

    /// Files that were previously rendered but are no longer in scope
    /// (e.g., a backend was disabled) must surface as `Remove` plan items
    /// when the on-disk content still matches what we last wrote. This
    /// exercises the `plan_removals` path end-to-end.
    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn disabling_a_backend_removes_its_orphaned_files() {
        use crate::decision::Decision;

        let tmp = empty_workspace();

        // First: write everything including the github backend.
        let with_gh = Cli {
            backends: vec!["github".to_owned()],
            no_backends: false,
            dry_run: false,
            force: false,
        };
        let first = run_update(&Catalog::anvil(), &with_gh, tmp.path()).unwrap();
        assert!(first.applied);
        let github_workflow = tmp.path().join(".github/workflows/anvil-pr.yml");
        assert!(github_workflow.is_file());

        // Second: disable backends. The previously rendered github files
        // should now be queued for removal (file unchanged on disk since
        // last render → Decision::Remove).
        let no_be = Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        };
        let second = run_update(&Catalog::anvil(), &no_be, tmp.path()).unwrap();

        let removed: Vec<&str> = second
            .plan
            .items()
            .iter()
            .filter(|i| i.decision == Decision::Remove)
            .filter_map(|i| match &i.target {
                crate::plan::Target::File { path } => Some(path.as_str()),
                crate::plan::Target::Region { .. } => None,
            })
            .collect();
        assert!(
            removed.contains(&".github/workflows/anvil-pr.yml"),
            "expected anvil-pr.yml to be queued for removal; got: {removed:?}"
        );
        assert!(
            !github_workflow.exists(),
            "expected the orphaned github workflow file to actually be removed from disk"
        );
    }

    /// User-customized orphans (file no longer in scope, but the on-disk
    /// contents diverge from what we last wrote) must be left alone via
    /// `OrphanedKept`, not deleted.
    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn customized_orphans_are_kept_not_removed() {
        use crate::decision::Decision;

        let tmp = empty_workspace();
        let with_gh = Cli {
            backends: vec!["github".to_owned()],
            no_backends: false,
            dry_run: false,
            force: false,
        };
        let _ = run_update(&Catalog::anvil(), &with_gh, tmp.path()).unwrap();

        let github_workflow = tmp.path().join(".github/workflows/anvil-pr.yml");
        fs::write(&github_workflow, "# user edited this\n").unwrap();

        let no_be = Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        };
        let second = run_update(&Catalog::anvil(), &no_be, tmp.path()).unwrap();

        let kept: Vec<&str> = second
            .plan
            .items()
            .iter()
            .filter(|i| i.decision == Decision::OrphanedKept)
            .filter_map(|i| match &i.target {
                crate::plan::Target::File { path } => Some(path.as_str()),
                crate::plan::Target::Region { .. } => None,
            })
            .collect();
        assert!(
            kept.contains(&".github/workflows/anvil-pr.yml"),
            "expected customized orphan to surface as OrphanedKept; got: {kept:?}"
        );
        assert!(github_workflow.is_file(), "customized orphan must not be deleted from disk");
        assert_eq!(
            fs::read_to_string(&github_workflow).unwrap(),
            "# user edited this\n",
            "customized orphan contents must be preserved"
        );
    }

    /// A previously-tracked owned file that is already missing on disk must
    /// surface as `Remove` (purging the stale manifest entry), not as a
    /// "customized orphan" transfer -- there is no content to keep. The
    /// apply stays a disk no-op because `remove_file` absorbs `NotFound`.
    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn already_gone_orphan_file_surfaces_as_remove() {
        use crate::decision::Decision;

        let tmp = empty_workspace();
        let with_gh = Cli {
            backends: vec!["github".to_owned()],
            no_backends: false,
            dry_run: false,
            force: false,
        };
        let _ = run_update(&Catalog::anvil(), &with_gh, tmp.path()).unwrap();

        // Delete a tracked owned file so it is "already gone" on the next
        // run while still present in the manifest.
        let github_workflow = tmp.path().join(".github/workflows/anvil-pr.yml");
        fs::remove_file(&github_workflow).unwrap();

        let no_be = Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        };
        let second = run_update(&Catalog::anvil(), &no_be, tmp.path()).unwrap();

        let classify = |decision: Decision| -> Vec<String> {
            second
                .plan
                .items()
                .iter()
                .filter(|i| i.decision == decision)
                .filter_map(|i| match &i.target {
                    crate::plan::Target::File { path } => Some(path.clone()),
                    crate::plan::Target::Region { .. } => None,
                })
                .collect()
        };
        let removed = classify(Decision::Remove);
        let kept = classify(Decision::OrphanedKept);
        assert!(
            removed.iter().any(|p| p == ".github/workflows/anvil-pr.yml"),
            "already-gone orphan must surface as Remove; got removed={removed:?}"
        );
        assert!(
            !kept.iter().any(|p| p == ".github/workflows/anvil-pr.yml"),
            "already-gone orphan must NOT be reported as a customized-orphan transfer; got kept={kept:?}"
        );
    }

    /// Direct unit test of `plan_removals` for a region orphan whose host
    /// file no longer exists: it must surface as `OrphanedKept` (drop the
    /// manifest entry, no disk action).
    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn plan_removals_region_orphan_with_missing_host() {
        use crate::decision::Decision;

        let tmp = TempDir::new().unwrap();
        let mut previous = Manifest::default();
        previous.set_region("Justfile", "anvil-r", "sha256:body");
        let mut plan = Plan::default();
        plan_removals(tmp.path(), &previous, &mut plan, &mut HostTextCache::default()).unwrap();
        let orphans: Vec<(&str, &str)> = plan
            .items()
            .iter()
            .filter(|i| i.decision == Decision::OrphanedKept)
            .filter_map(|i| match &i.target {
                Target::Region { host, id } => Some((host.as_str(), id.as_str())),
                Target::File { .. } => None,
            })
            .collect();
        assert_eq!(orphans, vec![("Justfile", "anvil-r")]);
    }

    /// Direct unit test of `plan_removals` for a region orphan whose host
    /// file exists with a customized (checksum-diverged) region body: it
    /// must surface as `OrphanedKept`, preserving the user's edits.
    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn plan_removals_region_orphan_customized_is_kept() {
        use crate::decision::Decision;

        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("Justfile"),
            "# >>> anvil-managed: anvil-r\nuser edited body\n# <<< anvil-managed: anvil-r\n",
        );
        let mut previous = Manifest::default();
        // Stored checksum deliberately differs from the on-disk body, so
        // decide_removal classifies the region as a customized orphan.
        previous.set_region("Justfile", "anvil-r", "sha256:stored-different");
        let mut plan = Plan::default();
        plan_removals(tmp.path(), &previous, &mut plan, &mut HostTextCache::default()).unwrap();
        let orphans: Vec<(&str, &str)> = plan
            .items()
            .iter()
            .filter(|i| i.decision == Decision::OrphanedKept)
            .filter_map(|i| match &i.target {
                Target::Region { host, id } => Some((host.as_str(), id.as_str())),
                Target::File { .. } => None,
            })
            .collect();
        assert_eq!(orphans, vec![("Justfile", "anvil-r")]);
        // Host file untouched.
        assert!(
            fs::read_to_string(tmp.path().join("Justfile"))
                .unwrap()
                .contains("user edited body"),
            "customized region body must be preserved",
        );
    }

    /// A catalog with two managed regions targeting the same host file —
    /// the shape that `deny.toml`'s per-section split uses. Built on the
    /// `anvil` identity so the single-tool guard stays satisfied.
    fn two_region_catalog(host: &str, id_a: &str, body_a: &str, id_b: &str, body_b: &str) -> Catalog {
        use crate::catalog::CliMeta;
        use crate::catalog::artifact::RegionId;

        let region = |id: &'static str, body: &str| {
            Artifact::region(RegionSpec {
                host: HostSelector::Path(host.to_owned()),
                id: RegionId::new(id),
                body: body.to_owned(),
                syntax: CommentSyntax::Hash,
            })
        };
        // Leak the ids so RegionId (which holds &'static str) can borrow
        // them; this is test-only setup.
        let id_a: &'static str = Box::leak(id_a.to_owned().into_boxed_str());
        let id_b: &'static str = Box::leak(id_b.to_owned().into_boxed_str());
        Catalog::builder(CliMeta::new("anvil"))
            .with_artifact(region(id_a, body_a))
            .with_artifact(region(id_b, body_b))
            .build()
            .unwrap()
    }

    /// Two managed regions targeting one not-yet-existing host file must
    /// both land in the composed result — the second write must not
    /// overwrite the first. This is the core bug the in-memory host-text
    /// accumulator fixes.
    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn two_regions_in_one_host_compose_on_fresh_file() {
        let tmp = empty_workspace();
        let catalog = two_region_catalog("shared.toml", "anvil-sec-a", "a = 1\n", "anvil-sec-b", "b = 2\n");

        let outcome = run_update(&catalog, &local_only(), tmp.path()).unwrap();
        assert!(outcome.applied);

        let shared = fs::read_to_string(tmp.path().join("shared.toml")).unwrap();
        assert!(
            shared.contains("# >>> anvil-managed: anvil-sec-a"),
            "first region present:\n{shared}"
        );
        assert!(shared.contains("a = 1"), "first body present:\n{shared}");
        assert!(
            shared.contains("# >>> anvil-managed: anvil-sec-b"),
            "second region present:\n{shared}"
        );
        assert!(shared.contains("b = 2"), "second body present:\n{shared}");

        // Steady state: both regions are now tracked, so a re-run is a no-op.
        let second = run_update(&catalog, &local_only(), tmp.path()).unwrap();
        assert!(!second.plan.has_changes(), "second run should be idempotent");
    }

    /// Splitting one region into several on the same host (the `deny.toml`
    /// migration): the old combined region is removed while the new
    /// per-section regions are written, all in one host. The removal must
    /// compose with the writes — not overwrite them.
    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn splitting_a_region_removes_old_and_keeps_new_in_one_host() {
        use crate::catalog::CliMeta;
        use crate::catalog::artifact::RegionId;

        let tmp = empty_workspace();

        // First run: a single combined region on shared.toml.
        let combined = Catalog::builder(CliMeta::new("anvil"))
            .with_artifact(Artifact::region(RegionSpec {
                host: HostSelector::Path("shared.toml".to_owned()),
                id: RegionId::new("anvil-combined"),
                body: "a = 1\nb = 2\n".to_owned(),
                syntax: CommentSyntax::Hash,
            }))
            .build()
            .unwrap();
        assert!(run_update(&combined, &local_only(), tmp.path()).unwrap().applied);

        // Second run: the combined region is gone from the catalog,
        // replaced by two per-section regions on the same host.
        let split = two_region_catalog("shared.toml", "anvil-sec-a", "a = 1\n", "anvil-sec-b", "b = 2\n");
        let outcome = run_update(&split, &local_only(), tmp.path()).unwrap();
        assert!(outcome.applied);

        let shared = fs::read_to_string(tmp.path().join("shared.toml")).unwrap();
        assert!(
            !shared.contains("anvil-managed: anvil-combined"),
            "old combined region must be spliced out:\n{shared}"
        );
        assert!(shared.contains("anvil-managed: anvil-sec-a"), "new region a kept:\n{shared}");
        assert!(shared.contains("anvil-managed: anvil-sec-b"), "new region b kept:\n{shared}");
        assert!(
            shared.contains("a = 1") && shared.contains("b = 2"),
            "both new bodies kept:\n{shared}"
        );

        // And the migration settles: a re-run with the split catalog is a no-op.
        let third = run_update(&split, &local_only(), tmp.path()).unwrap();
        assert!(!third.plan.has_changes(), "post-split run should be idempotent");
    }

    /// A region `Propose` (the user customized that region) planned *before*
    /// a sibling `Write` on the same host must still produce a
    /// `.anvil-proposed` sibling composed on top of that later write.
    /// Otherwise the eagerly-spliced proposal captures a stale host and
    /// `mv shared.toml.anvil-proposed shared.toml` would silently revert the
    /// sibling region's update.
    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn region_proposal_is_recomposed_against_sibling_writes_on_same_host() {
        let tmp = empty_workspace();
        let host = tmp.path().join("shared.toml");

        // Run 1: seed both regions (anvil-sec-a is ordered before anvil-sec-b).
        let v1 = two_region_catalog("shared.toml", "anvil-sec-a", "a = \"v1\"\n", "anvil-sec-b", "b = \"v1\"\n");
        assert!(run_update(&v1, &local_only(), tmp.path()).unwrap().applied);

        // User customizes region a on disk, so next run classifies a as Propose.
        let cur = fs::read_to_string(&host).unwrap();
        let customized = upsert_region(&cur, "anvil-sec-a", "a = \"USER\"\n", CommentSyntax::Hash).unwrap();
        fs::write(&host, &customized).unwrap();

        // Run 2: both region templates move. a -> Propose (user-edited),
        // b -> Write (untouched). a is planned first, so without the
        // recomposition pass its proposal is computed before b's write folds
        // into the host.
        let v2 = two_region_catalog("shared.toml", "anvil-sec-a", "a = \"v2\"\n", "anvil-sec-b", "b = \"v2\"\n");
        assert!(run_update(&v2, &local_only(), tmp.path()).unwrap().applied);

        // Live host: a keeps the user's content; b is updated to v2.
        let live = fs::read_to_string(&host).unwrap();
        assert!(live.contains("a = \"USER\""), "user's region a preserved live:\n{live}");
        assert!(live.contains("b = \"v2\""), "sibling region b written live:\n{live}");

        // Proposed sibling: must carry BOTH a's proposed v2 AND b's new v2
        // (not the stale v1), so applying it doesn't revert b.
        let proposed = fs::read_to_string(tmp.path().join("shared.toml.anvil-proposed")).unwrap();
        assert!(proposed.contains("a = \"v2\""), "proposal applies a's update:\n{proposed}");
        assert!(
            proposed.contains("b = \"v2\""),
            "proposal composed on top of b's write:\n{proposed}"
        );
        assert!(!proposed.contains("b = \"v1\""), "stale sibling content must be gone:\n{proposed}");
    }

    /// When several regions on one host propose, every proposal converges on
    /// the same fully-updated `<host>.anvil-proposed` content (each proposed
    /// body composed on top of the others), rather than last-writer-wins.
    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn recompose_converges_multiple_proposals_on_one_host() {
        let tmp = TempDir::new().unwrap();
        let host = "deny.toml";
        // Final live host carries both regions at the user's content.
        let final_live = "# >>> anvil-managed: a\nUSER a\n# <<< anvil-managed: a\n\
             # >>> anvil-managed: b\nUSER b\n# <<< anvil-managed: b\n"
            .to_owned();
        let mut hosts = HostTextCache::default();
        hosts.set(host, final_live);

        let mut plan = Plan::default();
        // Two proposals on the same host with deliberately stale payloads.
        plan.push(PlanItem::propose_region(host, "a", "NEW a\n".into(), "stale-a".into(), "sa".into()));
        plan.push(PlanItem::propose_region(host, "b", "NEW b\n".into(), "stale-b".into(), "sb".into()));

        recompose_region_proposals(tmp.path(), &mut plan, &mut hosts).unwrap();

        let p0 = plan.items()[0].spliced_host.as_deref().unwrap();
        let p1 = plan.items()[1].spliced_host.as_deref().unwrap();
        assert_eq!(p0, p1, "both proposals must converge on the same fully-updated host");
        assert!(p0.contains("NEW a") && p0.contains("NEW b"), "every proposed body present:\n{p0}");
    }

    /// If a Propose item's host file vanished mid-run (external race), the
    /// recomposition pass leaves the eagerly-computed proposal untouched
    /// rather than dropping it.
    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn recompose_skips_when_host_file_is_absent() {
        let tmp = TempDir::new().unwrap();
        let mut plan = Plan::default();
        let stale = "eagerly computed proposal\n".to_owned();
        plan.push(PlanItem::propose_region(
            "gone.toml",
            "anvil-x",
            "body\n".into(),
            stale.clone(),
            "sum".into(),
        ));

        // Empty cache + no file on disk -> get_or_read returns None.
        let mut hosts = HostTextCache::default();
        recompose_region_proposals(tmp.path(), &mut plan, &mut hosts).unwrap();

        assert_eq!(
            plan.items()[0].spliced_host.as_deref(),
            Some(stale.as_str()),
            "vanished host leaves the proposal as-is"
        );
    }
}
