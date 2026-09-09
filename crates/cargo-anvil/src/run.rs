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

use crate::anvil::artifacts;
use crate::anvil::artifacts::region::DELTA_REGION_ID;
use crate::backend::{self, Backend};
use crate::catalog::Catalog;
use crate::catalog::artifact::{Artifact, ComposedHost, HostSelector, RegionSpec};
use crate::checksum::{checksum_str, normalize_line_endings};
use crate::cli::Cli;
use crate::decision::{Decision, RemovalDecision, decide_removal};
use crate::emit::{ManagedRegionRequest, plan_managed_region, plan_owned_file, toml_introduction_refusal};
use crate::io::{read_file_if_present, resolve_existing_case_insensitive};
use crate::manifest::{Manifest, RegionKey};
use crate::plan::{Plan, PlanItem, Target};
#[cfg(test)]
use crate::region::upsert_region;
use crate::region::{CommentSyntax, RegionPlacement, find_region, managed_region_ids, remove_region, repair_markers};
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
    if cli.dry_run {
        let exit_code = outcome.plan.dry_run_exit_code();
        if exit_code != 0 {
            std::process::exit(exit_code);
        }
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

    let mut plan = build_plan(&repo_root, &ws, &manifest, &backends, catalog)?;
    let mut next = plan.projected_manifest(&manifest);
    next.tool = Some(catalog.cli().subcommand.clone());
    next.tool_version = Some(catalog.cli().version.clone());
    next.catalog_checksum = Some(catalog.checksum());

    let expected_manifest = next.to_toml();
    let current_manifest = read_file_if_present(&Manifest::path_for(&repo_root))?;
    let manifest_is_current = current_manifest
        .as_deref()
        .is_some_and(|current| normalize_line_endings(current.as_bytes()) == normalize_line_endings(expected_manifest.as_bytes()));
    plan.set_manifest_update_required(!manifest_is_current);

    let applied = !args.dry_run;
    if applied {
        plan.apply_files(&repo_root)?;
        next.save(&repo_root)?;
    }

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
/// Every `(host, region id)` this pass declares, resolved to the casing on
/// disk.
///
/// Computed before anything is planned, because the validity check each region
/// runs has to know which of the regions already in its host are on their way
/// out — and removals are not planned until every region has been visited.
fn live_region_keys(repo_root: &Path, workspace: &Workspace, catalog: &Catalog) -> BTreeSet<(String, String)> {
    catalog
        .artifacts()
        .iter()
        .filter_map(|artifact| match artifact {
            Artifact::Region(spec) => Some(spec),
            Artifact::OwnedFile(_) => None,
        })
        .flat_map(|spec| {
            region_host_paths(workspace, spec)
                .into_iter()
                .map(|host| (resolve_existing_case_insensitive(repo_root, host), spec.id.as_str().to_owned()))
        })
        .collect()
}

fn build_plan(
    repo_root: &Path,
    workspace: &Workspace,
    manifest: &Manifest,
    backends: &[Backend],
    catalog: &Catalog,
) -> Result<Plan, AppError> {
    let mut plan = Plan::default();
    let mut hosts = HostTextCache::default();
    // Hosts already reported as unsafe to compose. Every region targeting one
    // hits the same fault, and four copies of one message is noise.
    let mut composed = ComposedHosts {
        live: live_region_keys(repo_root, workspace, catalog),
        ..ComposedHosts::default()
    };
    // Adoption, validation, and retirement must see the same repaired text.
    // Extra complete pairs retain their bodies as unmanaged settings.
    for key in manifest.regions.keys() {
        let host = resolve_existing_case_insensitive(repo_root, &key.host);
        if composed.live.iter().any(|(live_host, _)| live_host == &host) && !composed.live.contains(&(host.clone(), key.id.clone())) {
            repair_host_markers(repo_root, &mut plan, &mut hosts, &host, &key.id, CommentSyntax::Hash)?;
        }
    }

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
                push_region(repo_root, workspace, manifest, &mut plan, &mut hosts, &mut composed, spec)?;
            }
        }
    }

    plan_removals(repo_root, manifest, &mut plan, &mut hosts, &composed)?;

    Ok(plan)
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
    newlines: HashMap<String, &'static str>,
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
        self.newlines
            .insert(host.to_owned(), crate::region::text_newline(text.as_deref().unwrap_or("")));
        self.texts.insert(host.to_owned(), text.clone());
        Ok(text)
    }

    /// Record the host text that results from splicing a region in or out
    /// in memory, so later regions targeting the same host compose on top
    /// of it. Only operations that change the host file on disk (`Write`,
    /// region `Remove`) update the cache; refusals leave its content untouched.
    fn set(&mut self, host: &str, text: String) {
        self.texts.insert(host.to_owned(), Some(text));
    }
}

/// The host paths one region spec targets in this workspace.
///
/// - [`HostSelector::Path`] — a single literal host.
/// - [`HostSelector::EachMemberManifest`] — one host per workspace member (no
///   hosts in a single-crate repo, which has no workspace members).
/// - [`HostSelector::WorkspaceCargoToml`] / [`HostSelector::SingleCrateCargoToml`]
///   — the root `Cargo.toml`, gated on whether it declares a `[workspace]`
///   table.
///
/// Shared with the live-key set [`build_plan`] computes up front, so the two
/// cannot drift: a region skipped here because the workspace has the other
/// shape must not be counted as live, or the pass would treat the region it is
/// about to retire as one that is staying.
fn region_host_paths<'a>(workspace: &'a Workspace, spec: &'a RegionSpec) -> Vec<&'a str> {
    match &spec.host {
        HostSelector::Path(path) => vec![path.as_str()],
        HostSelector::WorkspaceCargoToml => {
            if workspace.has_workspace_table {
                vec!["Cargo.toml"]
            } else {
                Vec::new()
            }
        }
        HostSelector::SingleCrateCargoToml => {
            if workspace.has_workspace_table {
                Vec::new()
            } else {
                vec!["Cargo.toml"]
            }
        }
        HostSelector::EachMemberManifest => workspace.members.iter().map(|member| member.manifest_relpath.as_str()).collect(),
    }
}

/// Dispatch one managed-region artifact into the plan, expanding its host
/// selector against the discovered workspace.
fn push_region(
    repo_root: &Path,
    workspace: &Workspace,
    manifest: &Manifest,
    plan: &mut Plan,
    hosts: &mut HostTextCache,
    composed: &mut ComposedHosts,
    spec: &RegionSpec,
) -> Result<(), AppError> {
    for host in region_host_paths(workspace, spec) {
        push_region_at(repo_root, manifest, plan, hosts, composed, host, spec)?;
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
    composed: &mut ComposedHosts,
    host: &str,
    spec: &RegionSpec,
) -> Result<(), AppError> {
    let host = resolve_existing_case_insensitive(repo_root, host);
    repair_host_markers(repo_root, plan, hosts, &host, spec.id.as_str(), spec.syntax)?;
    let composed_host = composed_host_spec(&host);
    if let Some(declared) = composed_host
        && !composed.states.contains_key(&host)
    {
        let state = prepare_composed_host(repo_root, manifest, plan, hosts, declared, &host)?;
        if matches!(state, ComposedHostState::SeedFromScaffold) {
            hosts.set(&host, declared.scaffold.to_owned());
        }
        composed.states.insert(host.clone(), state);
    }
    if let Some(ComposedHostState::Unsafe(reason)) = composed.states.get(&host) {
        if composed.reported.insert(host.clone()) {
            plan.refusal(format!(
                "Refused to manage {host}: {reason}. Only marker cleanup, if needed, was written to it, and other \
                 artifacts were still planned."
            ));
        }
        plan.push(PlanItem::noop(
            Target::Region {
                host,
                id: spec.id.as_str().to_owned(),
            },
            Decision::LeaveAlone,
        ));
        return Ok(());
    }
    let current = hosts.get_or_read(repo_root, &host)?;
    if let Some(declared) = composed_host
        && !declared.order.contains(&spec.id.as_str())
    {
        // End-of-file is the default placement everywhere else, and it is wrong
        // here by construction: it lands below the region that closes the file.
        // A built-in region reaching this is caught by the registry test in
        // `artifacts::mod`, but a downstream catalog can add one at run time,
        // and appending it below `CMD` would be silent and would classify the
        // host as composable on every later run.
        bail!(
            "region '{}' targets the composed host '{host}', whose region order is semantic, \
             but is not declared in that order. Add it to the host's `ComposedHost::order` \
             at the position it must occupy.",
            spec.id.as_str()
        );
    }
    let placement = composed_host.map_or_else(
        || region_placement(spec.id.as_str(), current.as_deref()),
        |declared| composed_placement(declared.order, declared.scaffold, spec.id.as_str(), current.as_deref()),
    );
    let body = match delta_region_body(current.as_deref(), spec) {
        DeltaRegionBody::Managed => spec.body.as_str(),
        DeltaRegionBody::PreserveRepositoryKey => {
            plan.note(
                "The repository's .delta.toml already defines top-level `trip_wire_patterns`; \
                 the managed anvil-delta region was left empty. Remove the repository key to \
                 adopt the managed trip-wire list.",
            );
            ""
        }
        DeltaRegionBody::Malformed(reason) => {
            plan.refusal(format!(
                "Refused to manage .delta.toml [anvil-delta] because the existing host could not \
                 be safely inspected: {reason}. Other artifacts were still planned."
            ));
            plan.push(PlanItem::noop(
                Target::Region {
                    host,
                    id: spec.id.as_str().to_owned(),
                },
                Decision::LeaveAlone,
            ));
            return Ok(());
        }
    };
    let request = ManagedRegionRequest {
        host_relpath: &host,
        region_id: spec.id.as_str(),
        rendered_body: body,
        syntax: spec.syntax,
        placement,
        newline: hosts.newlines.get(&host).copied(),
    };
    let item = match plan_managed_region(manifest, current.as_deref(), request) {
        Ok(item) => item,
        Err(error) => {
            refuse_region(plan, host, spec.id.as_str(), &error.to_string());
            return Ok(());
        }
    };
    let retiring = current
        .as_deref()
        .map(|text| composed.retiring_regions(manifest, &host, text, spec.syntax))
        .unwrap_or_default();
    if item.decision == Decision::Write
        && let Some(reason) = toml_introduction_refusal(current.as_deref(), request, &retiring)
    {
        refuse_region(plan, host, spec.id.as_str(), &reason);
        return Ok(());
    }
    // Fold actual writes into the accumulator so later regions compose.
    if item.decision == Decision::Write
        && let Some(spliced) = &item.spliced_host
    {
        hosts.set(&host, spliced.clone());
    }
    plan.push(item);
    Ok(())
}

/// Repair markers and classify a composed host once, before updating its bodies.
///
/// Split out of `push_region_at` because it answers a different question:
/// whether the file on disk is the shape a composed host must be, independent
/// of which region is being planned. Later regions targeting the same host see
/// text this pass has already spliced, which is partially composed by
/// construction, so the answer is computed once and cached.
fn prepare_composed_host(
    repo_root: &Path,
    manifest: &Manifest,
    plan: &mut Plan,
    hosts: &mut HostTextCache,
    declared: ComposedHost,
    host: &str,
) -> Result<ComposedHostState, AppError> {
    for id in declared.order {
        repair_host_markers(repo_root, plan, hosts, host, id, CommentSyntax::Hash)?;
    }
    let state = match hosts.get_or_read(repo_root, host)? {
        Some(text) => composed_host_state(declared.order, host, &text, manifest),
        // Nothing on disk. The scaffold becomes the base the first region
        // splices into, carrying the parts of the file that cannot live
        // inside a region -- the `# syntax=` parser directive above all. It
        // is written once and never reconciled; everything outside the
        // sentinels is the repository's from then on.
        None => ComposedHostState::SeedFromScaffold,
    };
    // Resolved through a case variant. Case-insensitive resolution is right
    // for an ordinary host, whose consumers open it by whatever name it
    // has, but a composed host is read by something that requires the
    // declared spelling: the container driver refuses any other, because
    // `BuildKit` derives the ignore file's name from the Dockerfile's and
    // there is no flag to point it elsewhere. Keeping the file up to date
    // would leave the two halves disagreeing about one on-disk state, with
    // the generator reporting the tree in sync while every recipe that uses
    // it exits 1. The generator is the component that just wrote the file,
    // so it is the one positioned to say so.
    //
    // A content state that already refuses keeps its own diagnosis: it
    // describes the deeper problem, and its recovery -- move the file
    // aside, restore the regions -- resolves the spelling along the way,
    // whereas renaming first would only surface the same refusal again.
    if host == declared.path || matches!(state, ComposedHostState::Unsafe(_)) {
        Ok(state)
    } else {
        Ok(ComposedHostState::Unsafe(format!(
            "it must be named exactly `{}`, and the recipes that consume it refuse any other \
             spelling, so anvil would be maintaining a file nothing can use. Rename it",
            declared.path
        )))
    }
}

/// Record that one region was refused: a diagnostic naming the host, and a
/// no-op so the plan still accounts for it.
///
/// The refusal is scoped to the region, not the run — every other artifact is
/// still planned, which is what makes refusing an acceptable answer rather than
/// a wall in front of onboarding.
fn refuse_region(plan: &mut Plan, host: String, id: &str, reason: &str) {
    // Some reasons are whole sentences and some are a parser's error text, so
    // the sentence break is supplied only when the reason has not already
    // written one.
    let stop = if reason.trim_end().ends_with('.') { "" } else { "." };
    plan.refusal(format!(
        "Refused to manage {host} [{id}]: {reason}{stop} This region was left unchanged; other regions in the same \
         file and other artifacts may still be updated. Reconcile the hand-written table with the managed \
         one before retrying."
    ));
    plan.push(PlanItem::noop(Target::Region { host, id: id.to_owned() }, Decision::LeaveAlone));
}

/// Persist cheap marker repairs independently of adopting or updating the body.
fn repair_host_markers(
    repo_root: &Path,
    plan: &mut Plan,
    hosts: &mut HostTextCache,
    host: &str,
    id: &str,
    syntax: CommentSyntax,
) -> Result<(), AppError> {
    if let Some(text) = hosts.get_or_read(repo_root, host)? {
        let repaired = repair_markers(&text, id, syntax);
        if repaired != text {
            hosts.set(host, repaired.clone());
            plan.push(PlanItem::repair_region(host, id, repaired));
        }
    }
    Ok(())
}

/// Where a region belongs inside a composed host whose order is semantic.
///
/// An existing region is updated where it is found, so this only decides where
/// an *absent* one lands — which matters whenever anvil adds a region to a
/// release. Appending it at end-of-file, the default for every other host,
/// would put it after regions it must precede: a newly added base-image
/// argument would land below the `FROM` that consumes it. Without this, adding
/// a region would either corrupt or (with the composition check) refuse every
/// file that already exists.
fn composed_placement(order: &[&str], scaffold: &str, id: &str, text: Option<&str>) -> RegionPlacement {
    let Some(text) = text else {
        return RegionPlacement::End;
    };
    if matches!(find_region(text, id, CommentSyntax::Hash), Ok(Some(_))) {
        // Present: `upsert_region` replaces it where it is, and the offset is
        // never consulted.
        return RegionPlacement::End;
    }
    let Some(position) = order.iter().position(|candidate| *candidate == id) else {
        return RegionPlacement::End;
    };
    // The nearest declared predecessor that is actually in the file. Anything
    // after it and before the next present region is the gap this region opens.
    for earlier in order[..position].iter().rev() {
        if let Ok(Some(region)) = find_region(text, earlier, CommentSyntax::Hash) {
            return RegionPlacement::At(region.end_line.end);
        }
    }
    // Nothing precedes it, so it goes to the top -- but below the scaffold's
    // first line, which for a Dockerfile is the `# syntax=` parser directive
    // that BuildKit honors only as the very first line.
    //
    // Matched a line at a time rather than as one prefix. A whole-scaffold
    // comparison fails on any later divergence -- a CRLF working tree, or a
    // scaffold that has since grown a line -- and the fallback is byte 0, which
    // puts the region *above* the very line the branch exists to protect. The
    // first line is the one that must not be preceded, so it is the one matched.
    let opening = scaffold.lines().next().unwrap_or_default();
    // A parser directive is `# key=value`, and the value may legitimately differ
    // from the scaffold's -- a repository on a newer frontend carries
    // `# syntax=docker/dockerfile:1.7`. Match on the key and splice after the
    // whole line the file actually carries: equality would push the region above
    // an upgraded directive, and a bare prefix match would cut the file's own
    // line in half.
    let first_line_end = text.find('\n').unwrap_or(text.len());
    let first_line = text[..first_line_end].trim_end_matches('\r');
    let key = opening.split_once('=').map(|(key, _)| key);
    let carries_directive =
        !opening.is_empty() && (first_line == opening || key.is_some_and(|key| opening.starts_with(key) && first_line.starts_with(key)));
    RegionPlacement::At(if carries_directive { first_line.len() } else { 0 })
}

fn region_placement(region_id: &str, current: Option<&str>) -> RegionPlacement {
    if matches!(region_id, DELTA_REGION_ID | "anvil-spellcheck-root") {
        return RegionPlacement::Start;
    }
    if matches!(region_id, "anvil-spellcheck-hunspell" | "anvil-spellcheck-quirks")
        && let Some(old) = current.and_then(|text| find_region(text, "anvil-spellcheck", CommentSyntax::Hash).ok().flatten())
    {
        // Install the replacement tables before retiring the combined block:
        // its trailing user settings must still follow Hunspell.quirks.
        return RegionPlacement::At(old.start_line.start);
    }
    RegionPlacement::End
}

/// The composed-host declaration for a host, when it has one.
///
/// Most region hosts are files the repository already has (`Cargo.toml`,
/// `deny.toml`) or files whose first region can be appended to nothing, and
/// whose regions are order-independent, such as TOML tables and line sets.
/// Those are not registered, and their regions append at end-of-file.
///
/// The comparison ignores case because the caller has already replaced the
/// canonical path with the host's real on-disk name. A repository that spelled
/// the host differently would otherwise miss this lookup entirely, and with it
/// every guard below: its regions would be appended at end-of-file, under the
/// content they have to precede.
fn composed_host_spec(host_relpath: &str) -> Option<ComposedHost> {
    artifacts::composed_hosts()
        .into_iter()
        .find(|host| host_relpath.eq_ignore_ascii_case(host.path))
}

/// What a composed host's current content allows anvil to do with it.
enum ComposedHostState {
    /// Carries every region anvil owns, in the declared order. Update in place.
    Composable,
    /// Either absent, or a byte-identical render of a version that owned this
    /// path as a whole file. Every byte of it is anvil's, so the scaffold
    /// replaces it and the regions rebuild the file.
    SeedFromScaffold,
    /// Anvil cannot reach a valid file from here without either destroying
    /// repository content or writing something that will not build.
    Unsafe(String),
}

/// Per-host bookkeeping for composed hosts, for the length of one pass.
#[derive(Default)]
struct ComposedHosts {
    /// Classification per host, computed once from its on-disk state. Later
    /// regions targeting the same host see text this pass has already spliced,
    /// which is partially composed by construction — re-classifying that would
    /// refuse the file halfway through writing it.
    states: HashMap<String, ComposedHostState>,
    /// Hosts whose refusal has already been reported, so one fault produces one
    /// diagnostic rather than one per region.
    reported: BTreeSet<String>,
    /// Every `(host, region id)` this pass declares. A managed region found in
    /// a host that is absent from this set is an orphan: the pass may be about
    /// to remove it, in which case the tables it declares must not be held
    /// against the region being written.
    live: BTreeSet<(String, String)>,
}

impl ComposedHosts {
    /// The managed regions of `host_text` this pass will actually remove.
    ///
    /// A region is retiring only if the catalog no longer declares it *and* the
    /// removal decision is to remove it: a customized orphan is kept, stays in
    /// the file, and so still owns the tables it declares. Getting that wrong
    /// in either direction is a real fault — treating a kept orphan as gone
    /// writes a duplicate header, and treating a removed one as staying refuses
    /// a migration that is about to become valid.
    fn retiring_regions(&self, manifest: &Manifest, host_relpath: &str, host_text: &str, syntax: CommentSyntax) -> BTreeSet<String> {
        managed_region_ids(host_text, syntax)
            .into_iter()
            .filter(|id| !self.live.contains(&(host_relpath.to_owned(), id.clone())))
            .filter(|id| {
                let key = RegionKey {
                    host: host_relpath.to_owned(),
                    id: id.clone(),
                };
                let Some(last) = manifest.regions.get(&key) else {
                    // Never recorded, so anvil does not own it and will not
                    // remove it, whatever the sentinels say.
                    return false;
                };
                let region = find_region(host_text, id, syntax).ok().flatten();
                region.is_some_and(|region| region.is_empty() || checksum_str(region.body_str()) == *last)
            })
            .collect()
    }
}

/// Classify a composed host before anything is written to it.
///
/// A Dockerfile is the first managed-region host where the file is only valid
/// in one arrangement, so the states an ordinary region host can ignore all
/// matter here. Each rejected case is one anvil could "handle" by splicing
/// regions in anyway, and each would produce a file that is silently wrong:
/// `upsert_region` appends a missing region at end-of-file, which is the right
/// answer only when the file is already the composed shape.
fn composed_host_state(order: &[&str], host_relpath: &str, text: &str, manifest: &Manifest) -> ComposedHostState {
    // A malformed sentinel is its own diagnosis. Folding it in with "absent"
    // would report a broken region as a missing one, sending the reader looking
    // for content that is in fact right there with a mismatched marker.
    let mut malformed: Option<String> = None;
    let present: Vec<(&str, usize)> = order
        .iter()
        .filter_map(|id| match find_region(text, id, CommentSyntax::Hash) {
            Ok(Some(region)) => Some((*id, region.start_line.start)),
            Ok(None) => None,
            Err(err) => {
                malformed.get_or_insert_with(|| format!("its '{id}' region cannot be read: {err}"));
                None
            }
        })
        .collect();
    if let Some(reason) = malformed {
        return ComposedHostState::Unsafe(reason);
    }

    if present.is_empty() {
        // Nothing of anvil's is in the file. Either it is a whole-file render
        // anvil produced -- safe to replace, because every byte of it came from
        // anvil -- or it is content anvil has never owned.
        //
        // The checksum comparison is the whole safety property: without it, a
        // repository that edited such a file would have that edit silently
        // destroyed. Appending the regions instead is not a kinder answer,
        // because everything already in the file would then sit above `FROM`.
        return match manifest.file_checksum(host_relpath) {
            Some(recorded) if recorded == checksum_str(text) => ComposedHostState::SeedFromScaffold,
            Some(_) => ComposedHostState::Unsafe(
                "it was edited after anvil last wrote it, and anvil composes the file from managed \
                 regions rather than owning it whole. Move the edits you want to keep into the \
                 gaps of a freshly generated file, or delete it and re-run to have one written"
                    .to_owned(),
            ),
            // A composed host is recorded in `regions` and never in `files`, so
            // "no file entry" is not the same as "anvil has never seen this".
            // It is also every composed file that has lost its regions -- a
            // merge resolved the other way, a revert, a checkout of an older
            // commit -- and for those the lock still holds the provenance. The
            // two cases have opposite recoveries, and telling someone to delete
            // a file anvil rendered would throw away the gap content the
            // message elsewhere tells them to preserve.
            None if manifest.has_region_host(host_relpath) => ComposedHostState::Unsafe(
                "anvil composes it from managed regions and the lock still records them, but none \
                 of them is in the file: they were dropped by a merge, a revert or an edit. \
                 Restore the file from version control to recover the regions and your own content \
                 around them together"
                    .to_owned(),
            ),
            None => ComposedHostState::Unsafe(
                "it exists but anvil has never owned it, so there is nowhere to splice the managed \
                 regions that would not put your content above `FROM`. Delete it and re-run to have \
                 a composed file written, then move your instructions into the gaps"
                    .to_owned(),
            ),
        };
    }

    // A file carrying some regions but not all is what every adopter has the
    // first time anvil adds one to the set -- a normal upgrade, not damage.
    // `composed_placement` inserts each missing region at its declared position
    // rather than at end-of-file, so the result stays ordered and the gap
    // content is untouched. Refusing here would make every future region
    // addition mean "delete your file".
    //
    // The same regions, in the order the file carries them. Comparing the two
    // sequences rather than adjacent offsets keeps the check free of an
    // ordering operator whose boundary cannot be exercised: two distinct
    // regions can never share a start offset, so `<` and `<=` over positions
    // would be indistinguishable by any test.
    let mut by_position = present.clone();
    by_position.sort_by_key(|&(_, start)| start);
    match present
        .iter()
        .zip(by_position.iter())
        .find(|(expected, found)| expected.0 != found.0)
    {
        None => ComposedHostState::Composable,
        Some((expected, found)) => ComposedHostState::Unsafe(format!(
            "region '{}' appears before '{}', but must follow it. Restore the documented order and \
             re-run",
            found.0, expected.0
        )),
    }
}

enum DeltaRegionBody {
    Managed,
    PreserveRepositoryKey,
    Malformed(String),
}

fn delta_region_body(host_text: Option<&str>, spec: &RegionSpec) -> DeltaRegionBody {
    if spec.id.as_str() != DELTA_REGION_ID {
        return DeltaRegionBody::Managed;
    }
    let Some(host_text) = host_text else {
        return DeltaRegionBody::Managed;
    };
    let without_region = match remove_region(host_text, spec.id.as_str(), spec.syntax) {
        Ok(without_region) => without_region,
        Err(error) => return DeltaRegionBody::Malformed(format!("managed-region markers are malformed: {error}")),
    };
    let document = match without_region.parse::<toml_edit::DocumentMut>() {
        Ok(document) => document,
        Err(error) => {
            return DeltaRegionBody::Malformed(format!("invalid TOML: {error}"));
        }
    };
    if document.as_table().contains_key("trip_wire_patterns") {
        DeltaRegionBody::PreserveRepositoryKey
    } else {
        DeltaRegionBody::Managed
    }
}

/// Scan the previous manifest for entries that the active plan items
/// don't cover. For each, classify as `Remove` (user untouched since the
/// last render, or the file is already gone — the manifest entry is
/// purged and the disk delete is a no-op when absent). Edited owned files
/// transfer ownership; edited managed regions refuse and retain tracking.
///
/// This is what removes orphaned cloud-workflow artifacts, dropped catalog entries,
/// disabled-backend files, and any other previously-tracked item that
/// is no longer in scope.
fn plan_removals(
    repo_root: &Path,
    previous: &Manifest,
    plan: &mut Plan,
    hosts: &mut HostTextCache,
    composed: &ComposedHosts,
) -> Result<(), AppError> {
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
        // Marker-only writes do not make a retired catalog entry live again.
        .filter(|item| item.decision != Decision::Write || item.rendered_checksum.is_some())
        .filter_map(|i| match &i.target {
            Target::Region { host, id } => Some((host.clone(), id.clone())),
            Target::File { .. } => None,
        })
        .collect();
    let live_region_hosts: BTreeSet<String> = live_regions.iter().map(|(host, _)| host.clone()).collect();

    for (path, last) in &previous.files {
        // The lock carries the casing the path had when the entry was written,
        // while every live plan item carries the casing resolved from disk, so
        // the two are compared through the same resolution. Without it a
        // case-only rename makes anvil's own file look retired and the removal
        // below deletes the artifact this very pass just wrote.
        let resolved = resolve_existing_case_insensitive(repo_root, path);
        if live_files.contains(&resolved) {
            continue;
        }
        // A path that is no longer an owned file but *is* the host of a live
        // managed region has not been retired -- it has changed ownership
        // model. Deleting it here would erase what the region writes planned
        // for the same pass just produced, and the user content around them
        // with it. Drop the stale manifest entry and leave the file alone; the
        // region entries describe what anvil owns inside it.
        //
        // Unless the host was *refused*, in which case anvil declined to touch
        // it and the lock entry is the provenance the next run reclassifies
        // from. Dropping it would make a file anvil rendered look like one it
        // has never owned, flip the diagnostic to the wrong wording, and
        // destroy the clean recovery -- reverting the edit -- that the refusal
        // message tells the reader to use. "Nothing was written to it" has to
        // be true of the lock as well as the file.
        if live_region_hosts.contains(&resolved) {
            if !matches!(composed.states.get(&resolved), Some(ComposedHostState::Unsafe(_))) {
                plan.push(PlanItem::orphaned_kept(Target::File { path: path.clone() }));
            }
            continue;
        }
        // Read under the resolved casing: a lock entry recorded before a
        // case-only rename would otherwise find nothing on disk, classify a
        // file that is still present as `AlreadyGone`, and delete it.
        let disk = read_file_if_present(&repo_root.join(&resolved))?;
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
        // A lock key records the casing its host had when the entry was
        // written; a live key records the casing the write path resolved from
        // disk this pass. A case-only rename of the host therefore makes every
        // one of its regions look orphaned at the very moment the pass has
        // rewritten all of them under the new name -- and for a composed host
        // that is the whole file, so removing them would strip this pass's own
        // writes and leave a Dockerfile with no `FROM`. The regions are still
        // there under a key the manifest already carries, so the honest answer
        // is to transfer ownership to the new key and touch nothing on disk.
        //
        // No `resolved_host != key.host` guard: the `continue` above has
        // already established that the recorded key is not live, so when the
        // resolution changes nothing this lookup repeats it and fails.
        let resolved_host = resolve_existing_case_insensitive(repo_root, &key.host);
        // A refused host was not opened, and "nothing was written to it" has to
        // be true of the lock as well as the file -- the same invariant the
        // owned-file loop above keeps. A lock entry naming a region the catalog
        // does not declare would otherwise reach `remove_region` below, so the
        // run would splice a block out of the very file whose refusal says it
        // was left alone, and purge the provenance the next run reclassifies
        // from. Unreachable while every declared id is live; a region rename or
        // retirement is what exposes it.
        if matches!(composed.states.get(&resolved_host), Some(ComposedHostState::Unsafe(_))) {
            continue;
        }
        if live_regions.contains(&(resolved_host.clone(), key.id.clone())) {
            plan.push(PlanItem::orphaned_kept(Target::Region {
                host: key.host.clone(),
                id: key.id.clone(),
            }));
            continue;
        }
        repair_host_markers(repo_root, plan, hosts, &resolved_host, &key.id, CommentSyntax::Hash)?;
        let Some(host_text) = hosts.get_or_read(repo_root, &resolved_host)? else {
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
        let decision = if region.as_ref().is_some_and(crate::region::Region::is_empty) {
            RemovalDecision::Remove
        } else {
            decide_removal(last, body_checksum.as_deref())
        };
        match decision {
            RemovalDecision::Remove => {
                // Splice against — and update — the accumulated host text
                // so a removal composes with the writes already planned
                // for this host this pass instead of clobbering them
                // (their item is applied earlier; this one, later). The
                // cache is keyed by the resolved spelling, which is what
                // the writes used; reading under the recorded spelling
                // would miss it and splice into the pre-pass text.
                let spliced = remove_region(&host_text, &key.id, syntax)?;
                hosts.set(&resolved_host, spliced.clone());
                plan.push(PlanItem::remove_region(key.host.clone(), key.id.clone(), spliced));
            }
            RemovalDecision::OrphanedKept => {
                refuse_region(
                    plan,
                    key.host.clone(),
                    &key.id,
                    "this retired managed region contains edits. Restore its last generated body, empty it, or remove it to complete retirement",
                );
            }
            RemovalDecision::AlreadyGone => {
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

    /// The refusal diagnostic joins a reason to a fixed remedy, and the two
    /// classes of reason punctuate themselves differently: adoption writes
    /// whole sentences, while the parser backstop appends `toml_edit`'s error
    /// text, which does not end in a full stop. Supplying the break
    /// unconditionally produced `TOML rejects.. This region`, and omitting it
    /// unconditionally would run the parser's message straight into the remedy.
    mod refuse_region {
        use super::*;

        fn refusal_for(reason: &str) -> String {
            let mut plan = Plan::default();
            super::super::refuse_region(&mut plan, "deny.toml".to_owned(), "anvil-deny-advisories", reason);
            plan.refusals().first().expect("a refusal is recorded").clone()
        }

        #[test]
        fn a_reason_that_ends_a_sentence_is_not_given_a_second_full_stop() {
            let refusal = refusal_for("keeping both would repeat the key, which TOML rejects.");

            assert!(
                refusal.contains("which TOML rejects. This region was left unchanged"),
                "one full stop, one space: {refusal}"
            );
            assert!(!refusal.contains(".."), "no doubled full stop: {refusal}");
        }

        #[test]
        fn a_reason_without_a_full_stop_is_given_one() {
            let refusal = refusal_for("splicing the region would leave deny.toml unparsable as TOML: expected `]`");

            assert!(
                refusal.contains("expected `]`. This region was left unchanged"),
                "the reason is closed before the remedy begins: {refusal}"
            );
        }

        /// Trailing whitespace on the reason must not defeat the check: the
        /// break is decided by the last non-space character, and the reason is
        /// still emitted exactly as it arrived.
        #[test]
        fn a_trailing_space_does_not_hide_the_full_stop() {
            let refusal = refusal_for("which TOML rejects. ");

            assert!(!refusal.contains(".."), "no doubled full stop: {refusal}");
            assert!(
                refusal.contains("which TOML rejects.  This region was left unchanged"),
                "the reason's own trailing space is preserved: {refusal}"
            );
        }

        /// The plan still accounts for the region it refused, as a no-op —
        /// otherwise the summary would simply not mention it.
        #[test]
        fn the_refused_region_is_still_planned_as_a_no_op() {
            let mut plan = Plan::default();
            super::super::refuse_region(&mut plan, "deny.toml".to_owned(), "anvil-deny-advisories", "because.");

            let item = plan.items().first().expect("the region is planned");
            assert_eq!(item.decision, Decision::LeaveAlone);
            assert_eq!(
                item.target,
                Target::Region {
                    host: "deny.toml".to_owned(),
                    id: "anvil-deny-advisories".to_owned(),
                }
            );
        }
    }

    /// `composed_placement` decides where an *absent* region lands in a host
    /// whose order is semantic. Every branch matters: getting it wrong puts a
    /// newly added region after ones it must precede, which for a Dockerfile
    /// means `FROM` below the layers that depend on it.
    mod composed_placement {
        use super::*;

        const SCAFFOLD: &str = "# syntax=docker/dockerfile:1\n";
        const ORDER: &[&str] = &["a", "b", "c"];

        fn region(id: &str, body: &str) -> String {
            format!("# >>> anvil-managed: {id}\n{body}# <<< anvil-managed: {id}\n")
        }

        #[test]
        fn a_host_that_does_not_exist_yet_appends() {
            // Nothing to order against; the regions are written in catalog
            // order onto the scaffold.
            assert_eq!(super::super::composed_placement(ORDER, SCAFFOLD, "b", None), RegionPlacement::End);
        }

        #[test]
        fn a_region_already_present_is_replaced_where_it_is() {
            let host = format!("{SCAFFOLD}{}", region("b", "body\n"));
            assert_eq!(
                super::super::composed_placement(ORDER, SCAFFOLD, "b", Some(&host)),
                RegionPlacement::End,
                "an existing region is upserted in place, so the offset is never consulted"
            );
        }

        #[test]
        fn a_region_outside_the_declared_order_appends() {
            let host = format!("{SCAFFOLD}{}", region("a", "body\n"));
            assert_eq!(
                super::super::composed_placement(ORDER, SCAFFOLD, "unknown", Some(&host)),
                RegionPlacement::End
            );
        }

        #[test]
        fn a_missing_region_lands_after_its_nearest_present_predecessor() {
            // `c` is absent and both `a` and `b` are present, so it must follow
            // `b` -- the nearest, not merely the first.
            let host = format!("{SCAFFOLD}{}{}", region("a", "first\n"), region("b", "second\n"));
            let RegionPlacement::At(offset) = super::super::composed_placement(ORDER, SCAFFOLD, "c", Some(&host)) else {
                panic!("a missing region with a present predecessor must be placed by offset");
            };
            assert_eq!(offset, host.len(), "it belongs after the close of `b`");
        }

        #[test]
        fn a_missing_region_skips_predecessors_that_are_absent_too() {
            // `c` is missing and so is its nearest predecessor `b`, so the
            // search has to walk past `b` and anchor on `a`. A fixture where
            // the first candidate matches would never exercise the skip.
            let host = format!("{SCAFFOLD}{}", region("a", "first\n"));
            let RegionPlacement::At(offset) = super::super::composed_placement(ORDER, SCAFFOLD, "c", Some(&host)) else {
                panic!("expected an offset placement");
            };
            assert_eq!(offset, host.len(), "it belongs after the close of `a`, the only one present");
        }

        #[test]
        fn the_first_region_lands_below_the_scaffold_never_above_it() {
            // The scaffold is the parser directive, which BuildKit honors only
            // as line 1. Placing the first region at byte 0 would push it down.
            let host = format!("{SCAFFOLD}{}", region("b", "second\n"));
            let RegionPlacement::At(offset) = super::super::composed_placement(ORDER, SCAFFOLD, "a", Some(&host)) else {
                panic!("expected an offset placement");
            };
            assert_eq!(offset, SCAFFOLD.trim_end_matches('\n').len());
            assert!(offset > 0, "the directive must keep line 1");
        }

        #[test]
        fn a_scaffold_that_grew_still_places_below_its_first_line() {
            // A scaffold is written once and never reconciled, so a file created
            // by an earlier release carries only what that release seeded. If a
            // later scaffold gains a line, a whole-prefix comparison stops
            // matching every such file at once and drops the region to byte 0,
            // above the directive. Only the first line is load-bearing.
            const GROWN: &str = "# syntax=docker/dockerfile:1\n# added later\n";
            let host = format!("{SCAFFOLD}{}", region("b", "second\n"));
            let RegionPlacement::At(offset) = super::super::composed_placement(ORDER, GROWN, "a", Some(&host)) else {
                panic!("expected an offset placement");
            };
            assert!(offset > 0, "the directive must keep line 1");
            assert_eq!(offset, "# syntax=docker/dockerfile:1".len());
        }

        #[test]
        fn an_upgraded_parser_directive_keeps_line_one() {
            // `# syntax=docker/dockerfile:1.7` is an equally valid directive and
            // a repository may well have upgraded to it. Equality with the
            // scaffold fails, so the region must still land after the whole
            // line the file carries: at byte 0 it would sit above the directive
            // and BuildKit would ignore the frontend pin, and at the scaffold's
            // own length it would cut the directive in half.
            const UPGRADED: &str = "# syntax=docker/dockerfile:1.7";
            let host = format!("{UPGRADED}\n{}", region("b", "second\n"));
            assert_eq!(
                super::super::composed_placement(ORDER, SCAFFOLD, "a", Some(&host)),
                RegionPlacement::At(UPGRADED.len()),
                "the region belongs after the directive the file actually carries"
            );
        }

        #[test]
        fn a_first_line_that_is_not_a_parser_directive_places_at_the_top() {
            // Only a directive earns the reserved first line. Anything else is
            // ordinary content the region goes above.
            let host = format!("FROM scratch\n{}", region("b", "second\n"));
            assert_eq!(
                super::super::composed_placement(ORDER, SCAFFOLD, "a", Some(&host)),
                RegionPlacement::At(0)
            );
        }
        #[test]
        fn a_host_without_the_scaffold_places_the_first_region_at_the_top() {
            let host = region("b", "second\n");
            assert_eq!(
                super::super::composed_placement(ORDER, SCAFFOLD, "a", Some(&host)),
                RegionPlacement::At(0)
            );
        }

        #[test]
        fn a_crlf_host_still_keeps_the_scaffold_on_line_one() {
            // `text` is read verbatim, while the scaffold is an LF `include_str!`,
            // so a CRLF working tree must not look like a host that never carried
            // the scaffold -- which would place the first region at byte 0, above
            // the parser directive `BuildKit` honors nowhere else.
            let host = format!("{SCAFFOLD}{}", region("b", "second\n")).replace('\n', "\r\n");
            let RegionPlacement::At(offset) = super::super::composed_placement(ORDER, SCAFFOLD, "a", Some(&host)) else {
                panic!("expected an offset placement");
            };
            assert!(offset > 0, "the directive must keep line 1 on a CRLF checkout");
            assert!(
                host[..offset].starts_with(SCAFFOLD.trim_end_matches('\n')),
                "the offset must fall after the directive, not inside it"
            );
        }

        #[test]
        fn the_test_scaffold_is_the_one_production_uses() {
            // Every case above is argued against a real Dockerfile. A stub that
            // drifted from the emitted scaffold would let them all pass while
            // production placed a region above the directive.
            assert_eq!(
                SCAFFOLD,
                crate::anvil::artifacts::container::composed_host().scaffold,
                "the placement tests must exercise the scaffold anvil actually seeds"
            );
        }
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
    fn a_fork_region_absent_from_the_composed_order_is_refused() {
        // A downstream catalog can add a region to a composed host at run time,
        // where the registry test in `artifacts::mod` cannot see it. Placement
        // defaults to end-of-file, which for this host is below the region that
        // closes the file, and the host would still classify as composable on
        // every later run -- so planning stops instead of appending below `CMD`.
        let tmp = empty_workspace();
        let catalog = Catalog::anvil()
            .into_builder()
            .with_artifact(Artifact::region(crate::catalog::RegionSpec {
                host: crate::catalog::HostSelector::Path(".anvil/container/Dockerfile".to_owned()),
                id: crate::catalog::RegionId::new("fork-invented"),
                body: "RUN echo fork\n".to_owned(),
                syntax: CommentSyntax::Hash,
            }))
            .build()
            .unwrap();

        let err = run_update(&catalog, &local_only(), tmp.path()).unwrap_err();

        let message = format!("{err}");
        assert!(
            message.contains("fork-invented") && message.contains("order"),
            "the refusal must name the region and the contract it breaks, got: {message}"
        );
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
            "justfiles/anvil/helpers.just",
            "justfiles/anvil/checks/fmt.just",
            "justfiles/anvil/checks/miri.just",
            "justfiles/anvil/groups/pr-fast.just",
            "justfiles/anvil/groups/scheduled-exhaustive.just",
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

    #[mutants::skip]
    fn assert_dry_run_detects_manifest_drift(mutate: impl FnOnce(&mut Manifest)) {
        let tmp = empty_workspace();
        let catalog = Catalog::anvil();
        let _ = run_update(&catalog, &local_only(), tmp.path()).unwrap();

        let mut manifest = Manifest::load(tmp.path()).unwrap();
        mutate(&mut manifest);
        manifest.save(tmp.path()).unwrap();
        let stale_lock = fs::read_to_string(Manifest::path_for(tmp.path())).unwrap();

        let mut args = local_only();
        args.dry_run = true;
        let outcome = run_update(&catalog, &args, tmp.path()).unwrap();

        assert!(!outcome.applied);
        assert_eq!(outcome.plan.dry_run_exit_code(), 1);
        assert!(outcome.plan.summary(Some(&manifest)).contains(".anvil.lock"));
        assert_eq!(fs::read_to_string(Manifest::path_for(tmp.path())).unwrap(), stale_lock);
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn dry_run_detects_stale_manifest_checksum() {
        assert_dry_run_detects_manifest_drift(|manifest| {
            let path = manifest.files.keys().next().unwrap().clone();
            manifest.files.insert(path, "sha256:stale".to_owned());
        });
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn dry_run_detects_missing_manifest_region() {
        assert_dry_run_detects_manifest_drift(|manifest| {
            let key = manifest.regions.keys().next().unwrap().clone();
            manifest.regions.remove(&key);
        });
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn dry_run_detects_stale_catalog_checksum() {
        assert_dry_run_detects_manifest_drift(|manifest| {
            manifest.catalog_checksum = Some("sha256:stale".to_owned());
        });
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn dry_run_detects_noncanonical_manifest_text() {
        let tmp = empty_workspace();
        let catalog = Catalog::anvil();
        let _ = run_update(&catalog, &local_only(), tmp.path()).unwrap();
        let lock_path = Manifest::path_for(tmp.path());
        let canonical = fs::read_to_string(&lock_path).unwrap();
        fs::write(&lock_path, format!("{canonical}\n")).unwrap();

        let mut args = local_only();
        args.dry_run = true;
        let outcome = run_update(&catalog, &args, tmp.path()).unwrap();

        assert_eq!(outcome.plan.dry_run_exit_code(), 1);
        assert_eq!(fs::read_to_string(lock_path).unwrap(), format!("{canonical}\n"));
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn dry_run_accepts_crlf_manifest_text() {
        let tmp = empty_workspace();
        let catalog = Catalog::anvil();
        let _ = run_update(&catalog, &local_only(), tmp.path()).unwrap();
        let lock_path = Manifest::path_for(tmp.path());
        let crlf = fs::read_to_string(&lock_path).unwrap().replace('\n', "\r\n");
        fs::write(&lock_path, &crlf).unwrap();

        let mut args = local_only();
        args.dry_run = true;
        let outcome = run_update(&catalog, &args, tmp.path()).unwrap();

        assert_eq!(outcome.plan.dry_run_exit_code(), 0);
        assert_eq!(fs::read_to_string(lock_path).unwrap(), crlf);
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

    #[cfg_attr(miri, ignore = "uses filesystem")]
    #[test]
    fn marker_recovery_preserves_content_and_settles_for_both_syntaxes() {
        use crate::catalog::{CliMeta, RegionId};
        for syntax in [CommentSyntax::Hash, CommentSyntax::SlashSlash] {
            let prefix = if syntax == CommentSyntax::Hash { "#" } else { "//" };
            let open = format!("{prefix} >>> anvil-managed: repair\r\n");
            let close = format!("{prefix} <<< anvil-managed: repair\r\n");
            let catalog = Catalog::builder(CliMeta::new("anvil"))
                .with_artifact(Artifact::region(RegionSpec {
                    host: HostSelector::Path("host.txt".to_owned()),
                    id: RegionId::new("repair"),
                    body: "generated\n".to_owned(),
                    syntax,
                }))
                .build()
                .unwrap();
            for input in [
                format!("{open}user"),
                format!("{close}user\r\n{open}"),
                format!("{open}{open}generated\r\n{close}user\r\n"),
                format!("{open}generated\r\n{close}user\r\n{close}"),
            ] {
                let tmp = empty_workspace();
                let path = tmp.path().join("host.txt");
                write(&path, &input);
                let outcome = run_update(&catalog, &local_only(), tmp.path()).unwrap();
                assert!(outcome.plan.refusals().is_empty());
                let output = fs::read_to_string(&path).unwrap();
                let region = find_region(&output, "repair", syntax).unwrap().unwrap();
                assert_eq!(region.body_str(), "generated\r\n");
                assert!(remove_region(&output, "repair", syntax).unwrap().contains("user"));
                assert_eq!(output.matches(&open).count(), 1);
                assert_eq!(output.matches(&close).count(), 1);
                assert!(!output.replace("\r\n", "").contains('\n'));
                assert!(!run_update(&catalog, &local_only(), tmp.path()).unwrap().plan.has_changes());
                assert_eq!(fs::read_to_string(&path).unwrap(), output);
            }
        }
    }

    #[cfg_attr(miri, ignore = "uses filesystem")]
    #[test]
    fn marker_cleanup_does_not_accept_body_edits_or_unmanaged_conflicts() {
        let tmp = empty_workspace();
        let path = tmp.path().join("deny.toml");
        let catalog = one_region_catalog("deny.toml", "repair", "[advisories]\nyanked = \"deny\"\n");
        run_update(&catalog, &local_only(), tmp.path()).unwrap();
        let original = Manifest::load(tmp.path()).unwrap().regions;
        let open = "# >>> anvil-managed: repair\n";
        let close = "# <<< anvil-managed: repair\n";
        for input in [
            format!("{open}[advisories]\nyanked = \"warn\"\n{open}{close}"),
            format!("{open}[advisories]\nyanked = \"warn\"\n"),
        ] {
            write(&path, &input);
            let outcome = run_update(&catalog, &local_only(), tmp.path()).unwrap();
            assert_eq!(outcome.plan.refusals().len(), 1);
            assert_eq!(Manifest::load(tmp.path()).unwrap().regions, original);
            let output = fs::read_to_string(&path).unwrap();
            assert_eq!(
                output.parse::<toml_edit::DocumentMut>().unwrap()["advisories"]["yanked"].as_str(),
                Some("warn")
            );
            assert!(!tmp.path().join("deny.toml.anvil-proposed").exists());
            assert_eq!(run_update(&catalog, &local_only(), tmp.path()).unwrap().plan.refusals().len(), 1);
        }
    }

    #[cfg_attr(miri, ignore = "uses filesystem")]
    #[test]
    fn edited_retirement_repeats_until_restored_emptied_or_removed() {
        for recovery in ["[old]\na = 1\n", " \t\n", ""] {
            let tmp = empty_workspace();
            let path = tmp.path().join("shared.toml");
            let old = one_region_catalog("shared.toml", "old", "[old]\na = 1\n");
            run_update(&old, &local_only(), tmp.path()).unwrap();
            let previous = Manifest::load(tmp.path()).unwrap().regions;
            let next = one_region_catalog("shared.toml", "new", "[new]\nb = 2\n");
            write(&path, &upsert_region("", "old", "[old]\na = 9\n", CommentSyntax::Hash).unwrap());
            for _ in 0..2 {
                let outcome = run_update(&next, &local_only(), tmp.path()).unwrap();
                assert_eq!(outcome.plan.refusals().len(), 1);
                let manifest = Manifest::load(tmp.path()).unwrap();
                for (key, checksum) in &previous {
                    assert_eq!(manifest.regions.get(key), Some(checksum));
                }
                let output = fs::read_to_string(&path).unwrap();
                let parsed = output.parse::<toml_edit::DocumentMut>().unwrap();
                assert_eq!(parsed["old"]["a"].as_integer(), Some(9));
                assert_eq!(parsed["new"]["b"].as_integer(), Some(2));
            }
            let current = fs::read_to_string(&path).unwrap();
            let reconciled = if recovery.is_empty() {
                remove_region(&current, "old", CommentSyntax::Hash).unwrap()
            } else {
                upsert_region(&current, "old", recovery, CommentSyntax::Hash).unwrap()
            };
            write(&path, &reconciled);
            assert!(run_update(&next, &local_only(), tmp.path()).unwrap().plan.refusals().is_empty());
            assert!(!Manifest::load(tmp.path()).unwrap().regions.keys().any(|key| key.id == "old"));
            assert!(!run_update(&next, &local_only(), tmp.path()).unwrap().plan.has_changes());
        }
    }

    #[cfg_attr(miri, ignore = "uses filesystem")]
    #[test]
    fn shipped_spellcheck_split_migrates_and_adopts_matching_root_settings() {
        let body = [
            include_str!("../templates/regions/spellcheck.toml"),
            include_str!("../templates/regions/spellcheck-hunspell.toml"),
            include_str!("../templates/regions/spellcheck-quirks.toml"),
        ]
        .join("\n");
        for (managed, newline) in [(true, "\n"), (true, "\r\n"), (false, "\n")] {
            let tmp = empty_workspace();
            if managed {
                let old = one_region_catalog("spellcheck.toml", "anvil-spellcheck", &body);
                run_update(&old, &local_only(), tmp.path()).unwrap();
            } else {
                write(&tmp.path().join("spellcheck.toml"), &body);
            }
            let path = tmp.path().join("spellcheck.toml");
            let input = format!("{}\n# repository quirks\nallow_dashes = true\n", fs::read_to_string(&path).unwrap())
                .replace("\r\n", "\n")
                .replace('\n', newline);
            write(&path, &input);
            let before = input.parse::<toml_edit::DocumentMut>().unwrap();
            assert_eq!(before["Hunspell"]["quirks"]["allow_dashes"].as_bool(), Some(true));
            let catalog = Catalog::anvil();
            let outcome = run_update(&catalog, &local_only(), tmp.path()).unwrap();
            assert!(outcome.plan.refusals().is_empty(), "{:?}", outcome.plan.refusals());
            let output = fs::read_to_string(tmp.path().join("spellcheck.toml")).unwrap();
            assert!(find_region(&output, "anvil-spellcheck", CommentSyntax::Hash).unwrap().is_none());
            let parsed = output.parse::<toml_edit::DocumentMut>().unwrap();
            assert_eq!(parsed["dev_comments"].as_bool(), Some(false));
            assert_eq!(parsed["skip_readme"].as_bool(), Some(false));
            assert!(parsed["Hunspell"]["quirks"].is_table());
            assert_eq!(
                parsed["Hunspell"]["quirks"].get("allow_dashes").and_then(toml_edit::Item::as_bool),
                before["Hunspell"]["quirks"]["allow_dashes"].as_bool()
            );
            assert!(!parsed.contains_key("allow_dashes"));
            assert!(output.contains("# repository quirks"));
            if newline == "\r\n" {
                assert!(!output.replace("\r\n", "").contains('\n'));
            }
            assert!(!run_update(&catalog, &local_only(), tmp.path()).unwrap().plan.has_changes());
        }
    }

    #[cfg_attr(miri, ignore = "uses filesystem")]
    #[test]
    fn retired_duplicate_pairs_expose_user_settings_before_adoption() {
        for value in [2, 9] {
            let tmp = empty_workspace();
            let path = tmp.path().join("shared.toml");
            let old = one_region_catalog("shared.toml", "old", "[old]\na = 1\n");
            run_update(&old, &local_only(), tmp.path()).unwrap();
            let input = format!(
                "{}\n{}",
                fs::read_to_string(&path).unwrap(),
                upsert_region("", "old", &format!("[new]\nb = {value}\n"), CommentSyntax::Hash).unwrap()
            );
            write(&path, &input);
            let before = input.parse::<toml_edit::DocumentMut>().unwrap();
            assert_eq!(before["new"]["b"].as_integer(), Some(value));
            let next = one_region_catalog("shared.toml", "new", "[new]\nb = 2\n");
            for _ in 0..2 {
                let outcome = run_update(&next, &local_only(), tmp.path()).unwrap();
                assert_eq!(outcome.plan.refusals().len(), usize::from(value != 2));
                let output = fs::read_to_string(&path).unwrap();
                let after = output.parse::<toml_edit::DocumentMut>().unwrap();
                assert_eq!(after["new"]["b"].as_integer(), before["new"]["b"].as_integer());
                assert!(!after.contains_key("old"));
                let tracked = Manifest::load(tmp.path()).unwrap().regions;
                assert!(!tracked.keys().any(|key| key.id == "old"));
                assert_eq!(tracked.len(), usize::from(value == 2));
            }
        }
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
    fn emptied_region_is_regenerated_on_second_run() {
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
        assert_eq!(rustfmt_item.decision, crate::decision::Decision::Write);
        assert_eq!(fs::read_to_string(&path).unwrap(), host);
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn user_edit_inside_region_is_refused_when_template_unchanged() {
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
        assert_eq!(outcome.plan.refusals().len(), 1);
        let final_text = fs::read_to_string(&path).unwrap();
        assert!(final_text.contains("edition = \"2021\""));
    }

    /// Refusals repeat without advancing the last-rendered checksum.
    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn edited_region_refusal_repeats_until_reconciled() {
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
        // the next run sees D ≠ L ≠ T.
        let manifest_path = Manifest::path_for(tmp.path());
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        let key = RegionKey {
            host: "rustfmt.toml".to_owned(),
            id: region::RUSTFMT_REGION_ID.to_owned(),
        };
        manifest.regions.insert(key.clone(), checksum_str("synthetic old template"));
        manifest.save(tmp.path()).unwrap();
        let _ = manifest_path; // sanity

        // User diverged and the template moved: refuse without proposing.
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
        assert_eq!(item.decision, crate::decision::Decision::LeaveAlone);
        assert_eq!(second.plan.refusals().len(), 1);
        assert!(
            !tmp.path().join("rustfmt.toml.anvil-proposed").exists(),
            "managed regions never propose"
        );

        // Nothing changed, so the refusal must remain visible.
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
            "the edited region is never rewritten"
        );
        assert_eq!(third.plan.refusals().len(), 1);
        assert_eq!(Manifest::load(tmp.path()).unwrap().regions[&key], manifest.regions[&key]);
        fs::write(&path, host).unwrap();
        let reconciled = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        assert!(reconciled.plan.refusals().is_empty());
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
            ".github/actions/anvil-setup/just-problem-matcher.json",
            ".github/actions/anvil-run-group/action.yml",
            ".github/actions/anvil-report-status/action.yml",
            ".github/actions/anvil-impact/action.yml",
            ".github/workflows/anvil-pr-impl.yml",
            ".github/workflows/anvil-scheduled-impl.yml",
            ".github/workflows/anvil-pr.yml",
            ".github/workflows/anvil-scheduled.yml",
            ".github/skills/code-review/SKILL.md",
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
            ".pipelines/anvil/steps/pr-msrv.yml",
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

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn existing_delta_trip_wires_are_preserved_without_duplicate_key() {
        let tmp = empty_workspace();
        fs::write(
            tmp.path().join(".delta.toml"),
            "trip_wire_patterns = [\"custom/**\"]\n\n[git]\nremote_branch = \"origin/release\"\n",
        )
        .unwrap();
        let args = Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        };

        let outcome = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();

        let content = fs::read_to_string(tmp.path().join(".delta.toml")).unwrap();
        let document: toml_edit::DocumentMut = content.parse().expect("migrated delta config must remain valid TOML");
        assert_eq!(
            document["trip_wire_patterns"].as_array().unwrap().get(0).unwrap().as_str(),
            Some("custom/**")
        );
        assert_eq!(content.matches("trip_wire_patterns").count(), 1);
        let region = find_region(&content, DELTA_REGION_ID, CommentSyntax::Hash).unwrap().unwrap();
        assert!(region.is_empty(), "existing repository policy should opt out of managed defaults");
        assert!(
            outcome
                .plan
                .notes()
                .iter()
                .any(|note| note.contains("Remove the repository key to adopt")),
            "the persistent opt-out must be visible in the plan summary"
        );
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn legacy_delta_region_moves_to_start_and_settles() {
        let tmp = empty_workspace();
        let old_body = "[delta]\nroot-files = [\"Cargo.lock\", \"Cargo.toml\", \"rust-toolchain.toml\"]\n";
        let old_host = format!(
            "[git]\nremote_branch = \"origin/main\"\n\n# >>> anvil-managed: {DELTA_REGION_ID}\n\
             {old_body}# <<< anvil-managed: {DELTA_REGION_ID}\n"
        );
        fs::write(tmp.path().join(".delta.toml"), old_host).unwrap();
        let mut manifest = Manifest::default();
        manifest.set_region(".delta.toml", DELTA_REGION_ID, checksum_str(old_body));
        manifest.save(tmp.path()).unwrap();

        let first = run_update(&Catalog::anvil(), &local_only(), tmp.path()).unwrap();

        let content = fs::read_to_string(tmp.path().join(".delta.toml")).unwrap();
        assert!(
            content.starts_with(&format!("# >>> anvil-managed: {DELTA_REGION_ID}\n")),
            "the upgraded root-level key must precede every TOML table:\n{content}"
        );
        let document: toml_edit::DocumentMut = content.parse().expect("upgraded delta config must be valid TOML");
        assert!(document["trip_wire_patterns"].is_array());
        assert_eq!(content.matches("trip_wire_patterns").count(), 1);
        assert_eq!(first.plan.notes().len(), 0);

        let saved = Manifest::load(tmp.path()).unwrap();
        let key = crate::manifest::RegionKey {
            host: ".delta.toml".to_owned(),
            id: DELTA_REGION_ID.to_owned(),
        };
        let expected_checksum = checksum_str(include_str!("../templates/regions/delta.toml"));
        assert_eq!(saved.regions.get(&key).map(String::as_str), Some(expected_checksum.as_str()));

        let second = run_update(&Catalog::anvil(), &local_only(), tmp.path()).unwrap();
        assert!(
            !second.plan.has_changes(),
            "the relocated Delta region should be idempotent:\n{}",
            second.plan.summary(Some(&second.previous_manifest))
        );
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn malformed_delta_host_is_left_untouched_while_other_artifacts_update() {
        {
            let malformed = "[git\nremote_branch = \"origin/main\"\n".to_owned();
            let tmp = empty_workspace();
            fs::write(tmp.path().join(".delta.toml"), &malformed).unwrap();
            let previous_checksum = checksum_str("previous managed body\n");
            let mut manifest = Manifest::default();
            manifest.set_region(".delta.toml", DELTA_REGION_ID, previous_checksum.clone());
            manifest.save(tmp.path()).unwrap();

            let outcome = run_update(&Catalog::anvil(), &local_only(), tmp.path()).unwrap();

            assert_eq!(fs::read_to_string(tmp.path().join(".delta.toml")).unwrap(), malformed);
            assert!(tmp.path().join("Justfile").is_file(), "unrelated artifacts should still be emitted");
            assert!(
                outcome
                    .plan
                    .refusals()
                    .iter()
                    .any(|refusal| refusal.contains("Refused to manage .delta.toml")),
                "the scoped refusal must be visible to the user"
            );
            assert_eq!(
                outcome.plan.dry_run_exit_code(),
                1,
                "a scoped refusal must fail a dry-run drift gate"
            );
            let delta_item = outcome
                .plan
                .items()
                .iter()
                .find(|item| {
                    matches!(
                        &item.target,
                        Target::Region { host, id } if host == ".delta.toml" && id == DELTA_REGION_ID
                    )
                })
                .expect("Delta region should remain represented in the plan");
            assert_eq!(delta_item.decision, Decision::LeaveAlone);
            assert!(!tmp.path().join(".delta.toml.anvil-proposed").exists());
            let saved = Manifest::load(tmp.path()).unwrap();
            let key = crate::manifest::RegionKey {
                host: ".delta.toml".to_owned(),
                id: DELTA_REGION_ID.to_owned(),
            };
            assert_eq!(
                saved.regions.get(&key),
                Some(&previous_checksum),
                "a scoped refusal must preserve the previous region ownership record"
            );
        }
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
        plan_removals(
            tmp.path(),
            &previous,
            &mut plan,
            &mut HostTextCache::default(),
            &ComposedHosts::default(),
        )
        .unwrap();
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
    /// must refuse, preserving the user's edits and tracking checksum.
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
        plan_removals(
            tmp.path(),
            &previous,
            &mut plan,
            &mut HostTextCache::default(),
            &ComposedHosts::default(),
        )
        .unwrap();
        let orphans: Vec<(&str, &str)> = plan
            .items()
            .iter()
            .filter(|i| i.decision == Decision::LeaveAlone)
            .filter_map(|i| match &i.target {
                Target::Region { host, id } => Some((host.as_str(), id.as_str())),
                Target::File { .. } => None,
            })
            .collect();
        assert_eq!(orphans, vec![("Justfile", "anvil-r")]);
        assert_eq!(plan.refusals().len(), 1);
        assert_eq!(plan.projected_manifest(&previous).regions, previous.regions);
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
    /// One region on one host, for staging the state a later catalog grows out
    /// of.
    fn one_region_catalog(host: &str, id: &str, body: &str) -> Catalog {
        use crate::catalog::CliMeta;
        use crate::catalog::artifact::RegionId;

        let id: &'static str = Box::leak(id.to_owned().into_boxed_str());
        Catalog::builder(CliMeta::new("anvil"))
            .with_artifact(Artifact::region(RegionSpec {
                host: HostSelector::Path(host.to_owned()),
                id: RegionId::new(id),
                body: body.to_owned(),
                syntax: CommentSyntax::Hash,
            }))
            .build()
            .unwrap()
    }

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

    /// Invalid synthetic catalog composition still hits the generic parse backstop.
    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn two_regions_claiming_one_table_refuse_the_second() {
        let tmp = empty_workspace();
        let catalog = two_region_catalog(
            "shared.toml",
            "anvil-sec-a",
            "[licenses]\nallow = [\"MIT\"]\n",
            "anvil-sec-b",
            "[licenses]\nconfidence-threshold = 0.9\n",
        );

        let outcome = run_update(&catalog, &local_only(), tmp.path()).unwrap();

        let shared = fs::read_to_string(tmp.path().join("shared.toml")).unwrap();
        shared
            .parse::<toml_edit::DocumentMut>()
            .unwrap_or_else(|error| panic!("the host must stay readable: {error}\n---\n{shared}\n---"));
        assert_eq!(shared.matches("[licenses]").count(), 1, "no duplicate header:\n{shared}");
        assert!(shared.contains("anvil-sec-a"), "the first claim is honored:\n{shared}");
        assert!(!shared.contains("anvil-sec-b"), "the colliding sibling is not written:\n{shared}");

        let refusal = outcome
            .plan
            .refusals()
            .iter()
            .find(|reason| reason.contains("anvil-sec-b"))
            .unwrap_or_else(|| panic!("the collision is reported; got {:#?}", outcome.plan.refusals()));
        assert!(refusal.contains("[licenses]"), "it names the table: {refusal}");
        assert!(
            refusal.contains("unparsable as TOML"),
            "the generic parser backstop reports the fault: {refusal}"
        );
        assert_eq!(
            outcome.plan.dry_run_exit_code(),
            1,
            "and a catalog defect fails the drift gate rather than passing quietly"
        );
    }

    /// A sibling that is *already on disk* is the harder half of the same
    /// fault, and the claim registry got it exactly backwards. `anvil-sec-b`
    /// exists with `[licenses]`; a later catalog adds `anvil-sec-a` ahead of it
    /// declaring the same table. Whichever region claimed first won, so A wrote
    /// and B was refused — but refusing B leaves B's existing region **in the
    /// file**, and the host ends up carrying two `[licenses]` headers, which is
    /// the very outcome the backstop exists to prevent.
    ///
    /// Judging the host as this pass will leave it inverts that: B is staying,
    /// so A is the one refused, and the file on disk stays readable.
    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn a_region_added_ahead_of_an_existing_sibling_does_not_break_the_host() {
        let tmp = empty_workspace();
        let threshold = "[licenses]\nconfidence-threshold = 0.9\n";
        let existing = one_region_catalog("shared.toml", "anvil-sec-b", threshold);
        run_update(&existing, &local_only(), tmp.path()).unwrap();

        let grown = two_region_catalog(
            "shared.toml",
            "anvil-sec-a",
            "[licenses]\nallow = [\"MIT\"]\n",
            "anvil-sec-b",
            threshold,
        );
        let outcome = run_update(&grown, &local_only(), tmp.path()).unwrap();

        let shared = fs::read_to_string(tmp.path().join("shared.toml")).unwrap();
        shared
            .parse::<toml_edit::DocumentMut>()
            .unwrap_or_else(|error| panic!("the host must stay readable: {error}\n---\n{shared}\n---"));
        assert_eq!(shared.matches("[licenses]").count(), 1, "no duplicate header:\n{shared}");
        assert!(shared.contains("anvil-sec-b"), "the region already on disk is kept:\n{shared}");
        assert!(
            !shared.contains("anvil-sec-a"),
            "the region that would collide is not written:\n{shared}"
        );

        let refusal = outcome
            .plan
            .refusals()
            .iter()
            .find(|reason| reason.contains("anvil-sec-a"))
            .unwrap_or_else(|| panic!("the collision is reported; got {:#?}", outcome.plan.refusals()));
        assert!(refusal.contains("unparsable as TOML"), "the parser reports the fault: {refusal}");
    }

    /// A dotted assignment declares its table exactly as a header does, so a
    /// region writing `lints.rust.* = ...` and a sibling writing `[lints]`
    /// compose into a file TOML rejects. The claim registry enumerated headers
    /// only, so the dotted side claimed nothing and both regions passed. The
    /// parser has no such blind spot.
    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn a_dotted_region_and_a_headed_sibling_do_not_compose_into_a_broken_host() {
        let tmp = empty_workspace();
        let catalog = two_region_catalog(
            "shared.toml",
            "anvil-sec-a",
            "lints.rust.unsafe_code = \"deny\"\n",
            "anvil-sec-b",
            "[lints]\nworkspace = true\n",
        );

        let outcome = run_update(&catalog, &local_only(), tmp.path()).unwrap();

        let shared = fs::read_to_string(tmp.path().join("shared.toml")).unwrap();
        shared
            .parse::<toml_edit::DocumentMut>()
            .unwrap_or_else(|error| panic!("the host must stay readable: {error}\n---\n{shared}\n---"));
        assert!(
            outcome.plan.refusals().iter().any(|reason| reason.contains("anvil-sec-b")),
            "the collision is reported; got {:#?}",
            outcome.plan.refusals()
        );
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

    /// Refusing an edited region does not prevent a clean sibling's update.
    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn edited_region_is_refused_while_sibling_updates() {
        let tmp = empty_workspace();
        let host = tmp.path().join("shared.toml");

        // Run 1: seed both regions (anvil-sec-a is ordered before anvil-sec-b).
        let v1 = two_region_catalog("shared.toml", "anvil-sec-a", "a = \"v1\"\n", "anvil-sec-b", "b = \"v1\"\n");
        assert!(run_update(&v1, &local_only(), tmp.path()).unwrap().applied);

        let cur = fs::read_to_string(&host).unwrap();
        let customized = upsert_region(&cur, "anvil-sec-a", "a = \"USER\"\n", CommentSyntax::Hash).unwrap();
        fs::write(&host, &customized).unwrap();

        let original = Manifest::load(tmp.path()).unwrap();
        let v2 = two_region_catalog("shared.toml", "anvil-sec-a", "a = \"v2\"\n", "anvil-sec-b", "b = \"v2\"\n");
        let outcome = run_update(&v2, &local_only(), tmp.path()).unwrap();
        assert_eq!(outcome.plan.refusals().len(), 1);

        // Live host: a keeps the user's content; b is updated to v2.
        let live = fs::read_to_string(&host).unwrap();
        assert!(live.contains("a = \"USER\""), "user's region a preserved live:\n{live}");
        assert!(live.contains("b = \"v2\""), "sibling region b written live:\n{live}");

        live.parse::<toml_edit::DocumentMut>().unwrap();
        assert!(!tmp.path().join("shared.toml.anvil-proposed").exists());
        let key = RegionKey {
            host: "shared.toml".into(),
            id: "anvil-sec-a".into(),
        };
        assert_eq!(Manifest::load(tmp.path()).unwrap().regions[&key], original.regions[&key]);
        let repeated = run_update(&v2, &local_only(), tmp.path()).unwrap();
        assert_eq!(repeated.plan.refusals().len(), 1);
        assert_eq!(repeated.plan.dry_run_exit_code(), 1);
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn unmatched_opener_is_removed_without_claiming_its_content() {
        let tmp = empty_workspace();
        fs::write(
            tmp.path().join("rustfmt.toml"),
            format!("# >>> anvil-managed: {}\nmax_width = 120\n", region::RUSTFMT_REGION_ID),
        )
        .unwrap();

        let outcome = run_update(&Catalog::anvil(), &local_only(), tmp.path()).unwrap();
        assert!(outcome.plan.refusals().iter().any(|reason| reason.contains("rustfmt.toml")));
        assert_eq!(fs::read_to_string(tmp.path().join("rustfmt.toml")).unwrap(), "max_width = 120\n");
    }
}
