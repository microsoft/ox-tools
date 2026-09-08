// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Repairing manifests inside the copied tree.

use std::fs;

use camino::{Utf8Path, Utf8PathBuf};
use toml_edit::{DocumentMut, Item, Table, Value};

use super::workspace::absolute;
use crate::Result;
use crate::error::error;

/// The dependency name the instrumented code refers to.
pub(crate) const RUNTIME_CRATE: &str = "gamma_rt";

/// The package that provides the guard runtime.
pub(super) const RUNTIME_PACKAGE: &str = "cargo-gamma-rt";

/// Dependency tables, all of which can carry a path.
const DEPENDENCY_TABLES: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];

/// A manifest in the copied tree, edited in place.
///
/// Edits go through `toml_edit` rather than a parse-and-reserialise, so a manifest comes back out
/// with its comments, key order and formatting exactly as the user wrote them. Only the values
/// actually changed are touched.
#[derive(Debug)]
pub(super) struct Manifest {
    path: Utf8PathBuf,
    document: DocumentMut,
    changed: bool,

    /// Where this manifest sits relative to the copied root, which is what decides whether a
    /// dependency path leaves the tree.
    within: Utf8PathBuf,
}

impl Manifest {
    /// Reads a manifest.
    pub(super) fn read(path: &Utf8Path) -> Result<Self> {
        let text = fs::read_to_string(path.as_std_path()).map_err(|cause| error!("could not read `{path}`").caused_by(cause))?;

        let document = text
            .parse::<DocumentMut>()
            .map_err(|cause| error!("could not parse `{path}`: {cause}"))?;

        Ok(Self {
            path: path.to_owned(),
            document,
            changed: false,
            within: Utf8PathBuf::new(),
        })
    }

    /// Writes the manifest back if anything changed.
    pub(super) fn save(&self) -> Result<()> {
        if !self.changed {
            return Ok(());
        }

        fs::write(self.path.as_std_path(), self.document.to_string())
            .map_err(|cause| error!("could not update `{}`", self.path).caused_by(cause))
    }

    /// Rewrites every relative path that leaves the copied tree so that it points back into the
    /// original one.
    ///
    /// A path dependency resolves against the manifest holding it. The copy does not sit where the
    /// original did, so a path leaving the tree — `../../shared` from a package one level down —
    /// lands somewhere that does not exist, and the build fails naming a missing crate rather than
    /// the move that lost it.
    ///
    /// What matters is whether the path leaves the *tree*, not whether it leaves the package. A
    /// sibling dependency written `../core` climbs out of its package but stays well inside the
    /// workspace, and the copy brought the sibling along; re-anchoring it to the original would
    /// make cargo see the same package at two different locations and refuse to write a lockfile.
    ///
    /// `original` is the directory this manifest was copied from, and `within` is that same
    /// directory expressed relative to the copied root — empty for the root manifest itself.
    pub(super) fn anchor_paths(&mut self, original: &Utf8Path, within: &Utf8Path) {
        self.within = within.to_owned();

        for name in DEPENDENCY_TABLES {
            self.anchor_table(name, original);
        }

        // `[replace]` names crates by version requirement, and every entry is a source
        // specification of exactly the same shape as a dependency.
        self.anchor_table("replace", original);

        // Every `[patch.<registry>]` is its own table of dependencies, and a workspace commonly
        // patches a crate to a sibling checkout — the exact shape that breaks.
        if let Some(patch) = self.document.get_mut("patch").and_then(Item::as_table_like_mut) {
            let registries: Vec<String> = patch.iter().map(|(name, _entry)| name.to_owned()).collect();

            for registry in registries {
                if let Some(table) = patch.get_mut(&registry).and_then(Item::as_table_like_mut) {
                    anchor_dependencies(table, original, &self.within, &mut self.changed);
                }
            }
        }

        // A target-specific table holds the same dependency tables one level down.
        if let Some(targets) = self.document.get_mut("target").and_then(Item::as_table_like_mut) {
            let platforms: Vec<String> = targets.iter().map(|(name, _entry)| name.to_owned()).collect();

            for platform in platforms {
                let Some(table) = targets.get_mut(&platform).and_then(Item::as_table_like_mut) else {
                    continue;
                };

                for name in DEPENDENCY_TABLES {
                    if let Some(dependencies) = table.get_mut(name).and_then(Item::as_table_like_mut) {
                        anchor_dependencies(dependencies, original, &self.within, &mut self.changed);
                    }
                }
            }
        }

        // A workspace's own dependency table feeds every member that says `workspace = true`.
        if let Some(workspace) = self.document.get_mut("workspace").and_then(Item::as_table_like_mut)
            && let Some(dependencies) = workspace.get_mut("dependencies").and_then(Item::as_table_like_mut)
        {
            anchor_dependencies(dependencies, original, &self.within, &mut self.changed);
        }
    }

    /// Rewrites the paths in one top-level table.
    fn anchor_table(&mut self, name: &str, original: &Utf8Path) {
        if let Some(table) = self.document.get_mut(name).and_then(Item::as_table_like_mut) {
            anchor_dependencies(table, original, &self.within, &mut self.changed);
        }
    }

    /// Makes the package use the one guard runtime vendored for this run.
    ///
    /// An existing dependency on the implementation crate is replaced rather than retained. Every
    /// instrumented package must share one runtime instance: two package identities would carry
    /// independent active-mutant and census state into the same test executable. The replacement
    /// still carries forward any `features` and `default-features` the replaced entry declared,
    /// so a package that opted into a runtime feature keeps that selection.
    ///
    /// The path written is absolute. Cargo resolves a dependency path against the manifest holding
    /// it, and this manifest is the copy rather than the original, so a relative path would be
    /// read from a different directory than the one it was measured from — and point at nothing,
    /// or worse, at something else.
    ///
    /// Adding a dependency means the lockfile in the copied tree has to be written, which is why a
    /// run cannot honour `--locked` or `--frozen` as written: those flags forbid exactly this edit.
    /// The build substitutes `--offline` for them and says so once.
    pub(super) fn link_runtime(&mut self, runtime: &Utf8Path) -> Result<()> {
        self.link_runtime_inheriting(runtime, &WorkspaceRuntimeFeatures::default())
    }

    /// Same as [`link_runtime`](Self::link_runtime), but also merges in feature settings the
    /// caller already resolved from the workspace's own `[workspace.dependencies]` declaration for
    /// the runtime crate.
    ///
    /// A member entry that says `workspace = true` carries none of the `features` or
    /// `default-features` its inherited declaration set — those live in the workspace's manifest,
    /// a different file this method never opens — so the caller resolves them once, before any
    /// manifest in the tree is edited, and threads the result through here. A declaration that
    /// does *not* say `workspace = true` — a direct path, version, or git dependency — never
    /// inherits from `workspace_features`, whatever it carries: that table only ever describes
    /// what a `workspace = true` entry would otherwise be missing, not a default every member
    /// picks up regardless of its own declaration.
    pub(super) fn link_runtime_inheriting(&mut self, runtime: &Utf8Path, workspace_features: &WorkspaceRuntimeFeatures) -> Result<()> {
        let runtime = absolute(runtime);

        let conflicting_target = self
            .document
            .get("target")
            .and_then(Item::as_table_like)
            .into_iter()
            .flat_map(toml_edit::TableLike::iter)
            .filter_map(|(_platform, target)| target.as_table_like()?.get("dependencies")?.as_table_like())
            .any(|dependencies| {
                dependencies.contains_key(RUNTIME_CRATE) && !dependency_points_to(dependencies.get(RUNTIME_CRATE), &runtime)
            });

        if conflicting_target {
            return Err(Self::runtime_name_reserved(&self.path));
        }

        // Every target-specific table can name the runtime under either key, and each is
        // discarded once the single canonical entry below takes its place — so whatever
        // `features` and `default-features` it declared are read and merged in before that
        // removal, rather than lost with it. Each key only inherits the workspace's matching key,
        // and only when that key's own entry actually says `workspace = true`.
        let mut target_features = FeatureSettings::default();

        if let Some(targets) = self.document.get_mut("target").and_then(Item::as_table_like_mut) {
            for (_platform, target) in targets.iter_mut() {
                let Some(dependencies) = target
                    .as_table_like_mut()
                    .and_then(|table| table.get_mut("dependencies"))
                    .and_then(Item::as_table_like_mut)
                else {
                    continue;
                };

                target_features = target_features
                    .merge(workspace_features.resolve_aliased(dependencies.get(RUNTIME_CRATE)))
                    .merge(workspace_features.resolve_canonical(dependencies.get(RUNTIME_PACKAGE)));

                self.changed |= dependencies.remove(RUNTIME_CRATE).is_some();
                self.changed |= dependencies.remove(RUNTIME_PACKAGE).is_some();
            }
        }

        let dependencies = self.document.entry("dependencies").or_insert_with(|| Item::Table(Table::new()));

        let Some(table) = dependencies.as_table_like_mut() else {
            return Ok(());
        };

        if table.contains_key(RUNTIME_CRATE) && !dependency_points_to(table.get(RUNTIME_CRATE), &runtime) {
            return Err(Self::runtime_name_reserved(&self.path));
        }

        // Preserve whatever feature selection every declaration that already named this
        // dependency carried before each is discarded: the member's own top-level entry (whether
        // a direct declaration or a `workspace = true` override, which wins a genuine
        // `default-features` conflict as the most specific and the one in effect today), the
        // workspace's own matching-key declaration that entry inherits from when — and only when
        // — it says `workspace = true`, and any target-specific declaration. `features` from
        // every source are unioned rather than one replacing another, since Cargo would have
        // unified them across the same crate instance anyway.
        let own_features = workspace_features
            .resolve_aliased(table.get(RUNTIME_CRATE))
            .merge(workspace_features.resolve_canonical(table.get(RUNTIME_PACKAGE)));
        let feature_settings = own_features.merge(target_features);

        let _existing_runtime = table.remove("cargo-gamma-rt");
        let mut entry = toml_edit::InlineTable::new();
        let _package = entry.insert("package", Value::from(RUNTIME_PACKAGE));
        let _replaced = entry.insert("path", Value::from(portable_path(&runtime)));
        feature_settings.apply(&mut entry);
        let _added = table.insert(RUNTIME_CRATE, Item::Value(Value::InlineTable(entry)));

        self.changed = true;

        Ok(())
    }

    /// Redirects a runtime dependency already present in this package to the campaign's copy.
    ///
    /// `workspace_features` carries feature settings the caller already resolved from the
    /// workspace's own `[workspace.dependencies]` declaration for the runtime crate — settings a
    /// member entry that says `workspace = true` cannot see for itself, since that declaration
    /// lives in a different manifest than the one this method edits. A caller with nothing to
    /// contribute passes [`WorkspaceRuntimeFeatures::default`].
    pub(super) fn redirect_runtime(&mut self, runtime: &Utf8Path, workspace_features: &WorkspaceRuntimeFeatures) -> Result<()> {
        let runtime = absolute(runtime);
        self.redirect_workspace_runtime(&runtime)?;

        let top_level = self
            .document
            .get("dependencies")
            .and_then(Item::as_table_like)
            .is_some_and(|dependencies| dependencies.contains_key(RUNTIME_CRATE) || dependencies.contains_key("cargo-gamma-rt"));
        let targeted = self
            .document
            .get("target")
            .and_then(Item::as_table_like)
            .into_iter()
            .flat_map(toml_edit::TableLike::iter)
            .filter_map(|(_platform, target)| target.as_table_like()?.get("dependencies")?.as_table_like())
            .any(|dependencies| dependencies.contains_key(RUNTIME_CRATE) || dependencies.contains_key("cargo-gamma-rt"));

        if top_level || targeted {
            self.link_runtime_inheriting(&runtime, workspace_features)?;
        }

        Ok(())
    }

    /// Normalizes a workspace dependency before members inherit it by name.
    fn redirect_workspace_runtime(&mut self, runtime: &Utf8Path) -> Result<()> {
        let Some(dependencies) = self
            .document
            .get_mut("workspace")
            .and_then(Item::as_table_like_mut)
            .and_then(|workspace| workspace.get_mut("dependencies"))
            .and_then(Item::as_table_like_mut)
        else {
            return Ok(());
        };

        let aliased = dependencies.get(RUNTIME_CRATE);
        if aliased.is_some() && !dependency_points_to(aliased, runtime) {
            return Err(Self::runtime_name_reserved(&self.path));
        }

        if aliased.is_some() || dependencies.contains_key(RUNTIME_PACKAGE) {
            // Each key is its own workspace dependency declaration, so its feature settings are
            // carried onto its own replacement rather than merged across the two keys.
            let aliased_features = FeatureSettings::extract(dependencies.get(RUNTIME_CRATE));
            let canonical_features = FeatureSettings::extract(dependencies.get(RUNTIME_PACKAGE));

            let _aliased = dependencies.remove(RUNTIME_CRATE);
            let _canonical = dependencies.remove(RUNTIME_PACKAGE);
            let mut aliased_entry = toml_edit::InlineTable::new();
            let _package = aliased_entry.insert("package", Value::from(RUNTIME_PACKAGE));
            let _path = aliased_entry.insert("path", Value::from(portable_path(runtime)));
            aliased_features.apply(&mut aliased_entry);
            let _runtime = dependencies.insert(RUNTIME_CRATE, Item::Value(Value::InlineTable(aliased_entry)));

            // Keep the canonical workspace key resolvable for dev- and build-dependencies, which
            // are not rewritten to the guard's reserved crate name.
            let mut canonical_entry = toml_edit::InlineTable::new();
            let _path = canonical_entry.insert("path", Value::from(portable_path(runtime)));
            canonical_features.apply(&mut canonical_entry);
            let _runtime = dependencies.insert(RUNTIME_PACKAGE, Item::Value(Value::InlineTable(canonical_entry)));
            self.changed = true;
        }

        Ok(())
    }

    fn runtime_name_reserved(path: &Utf8Path) -> crate::error::Error {
        error!(
            "`gamma_rt` is already a dependency in `{path}` but cargo-gamma reserves that crate name for its guard runtime.\n\
             Rename that dependency so cargo-gamma can instrument this package."
        )
        .usage()
    }
}

/// The `features` and `default-features` carried by an existing dependency specification.
///
/// Redirecting a dependency to the vendored runtime replaces its whole specification with a
/// fresh path dependency, which would otherwise silently drop any feature selection the manifest
/// already made — including a member's own override of a `workspace = true` dependency, which
/// Cargo allows to add `features` alongside the inherited entry, and any target-specific
/// declaration.
#[derive(Default, Clone)]
pub(super) struct FeatureSettings {
    features: Option<Value>,
    default_features: Option<Value>,
}

impl FeatureSettings {
    /// Reads the feature settings off an existing dependency specification, if any.
    fn extract(item: Option<&Item>) -> Self {
        let specification = item.and_then(Item::as_table_like);

        Self {
            features: specification
                .and_then(|table| table.get("features"))
                .and_then(Item::as_value)
                .cloned(),
            default_features: specification
                .and_then(|table| table.get("default-features"))
                .and_then(Item::as_value)
                .cloned(),
        }
    }

    /// Merges this side's settings with `other`'s: `features` are unioned rather than one side's
    /// array replacing the other's, since Cargo would have unified them across the same crate
    /// instance anyway, while `default-features` prefers this side's explicit choice and falls
    /// back to `other`'s only where this side left a gap.
    fn merge(self, other: Self) -> Self {
        Self {
            features: merge_feature_arrays(self.features, other.features),
            default_features: self.default_features.or(other.default_features),
        }
    }

    /// Writes the carried settings onto a freshly built replacement entry.
    fn apply(self, entry: &mut toml_edit::InlineTable) {
        if let Some(features) = self.features {
            let _features = entry.insert("features", features);
        }

        if let Some(default_features) = self.default_features {
            let _default_features = entry.insert("default-features", default_features);
        }
    }
}

/// The feature settings a workspace's own `[workspace.dependencies]` table declares for the
/// runtime crate, kept separate per key.
///
/// The aliased and canonical keys are two independent declarations that just happen to name the
/// same crate: a member that inherits one with `workspace = true` must not acquire settings the
/// *other* key declares, so the two are never merged with each other here — only [`resolve_aliased`](Self::resolve_aliased)
/// or [`resolve_canonical`](Self::resolve_canonical) combine one of them with a specific entry,
/// and only when that entry actually inherits from the workspace.
#[derive(Default, Clone)]
pub(super) struct WorkspaceRuntimeFeatures {
    aliased: FeatureSettings,
    canonical: FeatureSettings,
}

impl WorkspaceRuntimeFeatures {
    /// Reads the settings a workspace's own `[workspace.dependencies]` table declares for the
    /// runtime crate, under both its aliased and canonical keys, before anything redirects them.
    ///
    /// A member that inherits the runtime with `workspace = true` names it by key alone, carrying
    /// none of these settings itself — they have to be read from the workspace's own manifest
    /// before that member's entry is replaced, since that replacement is the last point at which
    /// the workspace-level declaration is still reachable by name.
    pub(super) fn from_workspace(root: &Utf8Path) -> Result<Self> {
        let manifest = Manifest::read(root)?;

        let dependencies = manifest
            .document
            .get("workspace")
            .and_then(Item::as_table_like)
            .and_then(|workspace| workspace.get("dependencies"))
            .and_then(Item::as_table_like);

        Ok(Self {
            aliased: dependencies.map_or_else(FeatureSettings::default, |table| FeatureSettings::extract(table.get(RUNTIME_CRATE))),
            canonical: dependencies.map_or_else(FeatureSettings::default, |table| {
                FeatureSettings::extract(table.get(RUNTIME_PACKAGE))
            }),
        })
    }

    /// Resolves the effective feature settings for a dependency-table entry named by the aliased
    /// key (`gamma_rt`): its own settings, merged with the workspace's aliased-key declaration
    /// only when `item` itself says `workspace = true`. An `item` that inherits nothing of its
    /// own — no entry at all, or one that does not say `workspace = true` — never acquires the
    /// workspace's settings, whatever they are.
    fn resolve_aliased(&self, item: Option<&Item>) -> FeatureSettings {
        Self::resolve(item, &self.aliased)
    }

    /// Same as [`resolve_aliased`](Self::resolve_aliased), but for the canonical key
    /// (`cargo-gamma-rt`) and the workspace's canonical-key declaration.
    fn resolve_canonical(&self, item: Option<&Item>) -> FeatureSettings {
        Self::resolve(item, &self.canonical)
    }

    fn resolve(item: Option<&Item>, inherited: &FeatureSettings) -> FeatureSettings {
        let own = FeatureSettings::extract(item);

        if inherits_workspace_true(item) {
            own.merge(inherited.clone())
        } else {
            own
        }
    }
}

/// Whether a dependency specification declares `workspace = true`, meaning it inherits from the
/// matching `[workspace.dependencies]` key rather than declaring its own path, version, or git
/// source.
///
/// A direct declaration that merely sits alongside a workspace with its own `[workspace.dependencies]`
/// entry never reads that entry — only a `workspace = true` declaration names it at all.
fn inherits_workspace_true(item: Option<&Item>) -> bool {
    item.and_then(Item::as_table_like)
        .and_then(|table| table.get("workspace"))
        .and_then(Item::as_value)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Unions two `features` array values rather than letting one replace the other.
///
/// A malformed `features` value that is not an array is left as whichever side already has one,
/// rather than guessing how to combine it with something that is not a list.
fn merge_feature_arrays(left: Option<Value>, right: Option<Value>) -> Option<Value> {
    match (left, right) {
        (Some(Value::Array(mut merged)), Some(Value::Array(additional))) => {
            for feature in additional {
                let already_present = merged.iter().any(|existing| existing.as_str() == feature.as_str());

                if !already_present {
                    merged.push_formatted(feature);
                }
            }

            Some(Value::Array(merged))
        }
        (Some(only), None) | (None, Some(only)) => Some(only),
        (Some(preferred), Some(_not_an_array)) => Some(preferred),
        (None, None) => None,
    }
}

/// Whether an existing dependency already names this run's vendored runtime.
fn dependency_points_to(item: Option<&Item>, runtime: &Utf8Path) -> bool {
    let Some(specification) = item.and_then(Item::as_table_like) else {
        return false;
    };

    specification.get("workspace").and_then(Item::as_bool) == Some(true)
        || specification.get("package").and_then(Item::as_str) == Some(RUNTIME_PACKAGE)
        || specification
            .get("path")
            .and_then(Item::as_str)
            .is_some_and(|path| absolute(Utf8Path::new(path)) == runtime)
}

/// Rewrites every escaping path in one dependency table.
fn anchor_dependencies(table: &mut dyn toml_edit::TableLike, original: &Utf8Path, within: &Utf8Path, changed: &mut bool) {
    let names: Vec<String> = table.iter().map(|(name, _entry)| name.to_owned()).collect();

    for name in names {
        let Some(entry) = table.get_mut(&name) else {
            continue;
        };

        // A dependency written as a bare version string has no path to fix.
        let Some(specification) = entry.as_table_like_mut() else {
            continue;
        };

        let Some(path) = specification.get("path").and_then(|item| item.as_str()) else {
            continue;
        };

        let Some(anchored) = anchor(path, original, within) else {
            continue;
        };

        let _replaced = specification.insert("path", Item::Value(Value::from(portable_path(&anchored))));

        *changed = true;
    }
}

/// Returns the absolute form of a path that would not survive the move, or `None` if it would.
///
/// A path is left alone when it is already absolute, and when it resolves to somewhere still
/// inside the copied tree — those still resolve in the copy, and rewriting them would tie a tree
/// meant to be self-contained back to the original for no reason.
///
/// `within` is the manifest's directory relative to the copied root, so `within.join(path)` is
/// where the dependency lands relative to that root. Only a path that climbs above it has left.
fn anchor(path: &str, original: &Utf8Path, within: &Utf8Path) -> Option<Utf8PathBuf> {
    let candidate = Utf8Path::new(path);

    if candidate.is_absolute() || !escapes(&within.join(candidate)) {
        return None;
    }

    Some(normalize(&original.join(candidate)))
}

/// Returns whether a relative path ever climbs above the directory it is written in.
fn escapes(path: &Utf8Path) -> bool {
    let mut depth = 0_i32;

    for component in path.components() {
        match component.as_str() {
            "." => {}
            ".." => {
                depth -= 1;

                if depth < 0 {
                    return true;
                }
            }
            _named => depth += 1,
        }
    }

    false
}

/// Resolves `.` and `..` textually.
///
/// The target of a path dependency need not exist yet — a workspace can be assembled in any order
/// — so this cannot go through the filesystem the way canonicalization would. Symlinks are
/// therefore not resolved, which is also what cargo itself does with these paths.
fn normalize(path: &Utf8Path) -> Utf8PathBuf {
    let mut resolved = Utf8PathBuf::new();

    for component in path.components() {
        match component.as_str() {
            "." => {}
            ".." => {
                if !resolved.pop() {
                    resolved.push("..");
                }
            }
            named => resolved.push(named),
        }
    }

    resolved
}

/// Cargo paths are portable when written with `/`, including on Windows.
fn portable_path(path: &Utf8Path) -> String {
    path.as_str().replace('\\', "/")
}

/// Rewrites the `paths` overrides in a `.cargo/config.toml`, if there is one.
///
/// These are relative to the directory holding `.cargo`, and break in exactly the way a path
/// dependency does. A missing file is not an error because cargo tolerates its absence. An
/// existing file that cannot be parsed is an error: Cargo cannot use it either, and pretending it
/// was absent would build a different tree.
pub(super) fn anchor_cargo_config(root: &Utf8Path, original: &Utf8Path) -> Result<()> {
    for name in ["config.toml", "config"] {
        let path = root.join(".cargo").join(name);

        let _destination = crate::paths::require_within(&path, root, "a scratch Cargo configuration")?;

        if !path.as_std_path().is_file() {
            continue;
        }

        let mut manifest = Manifest::read(&path)?;

        if let Some(paths) = manifest.document.get_mut("paths").and_then(Item::as_array_mut) {
            for entry in paths.iter_mut() {
                let Some(anchored) = entry.as_str().and_then(|path| anchor(path, original, Utf8Path::new(""))) else {
                    continue;
                };

                *entry = Value::from(portable_path(&anchored));
                manifest.changed = true;
            }
        }

        manifest.save()?;
    }

    Ok(())
}

/// The flag the instrumented tree is built with, so that the user's lint levels do not judge it.
pub(super) const CAP_LINTS: &str = "--cap-lints=allow";

/// Adds [`CAP_LINTS`] to whatever rustflags the copied tree already configures.
///
/// Setting `RUSTFLAGS` in the environment would be simpler, but the environment variable *replaces*
/// the configured flags rather than adding to them: a workspace whose `.cargo/config.toml` sets
/// `target.<triple>.rustflags` would build with none of them, which can change what its code
/// compiles to and therefore what its tests prove.
///
/// The flag is appended to every rustflags key already present, because cargo picks exactly one of
/// them — `target.<triple>` over `target.<cfg>` over `build` — and which one is not knowable here
/// without resolving the target triple. Appending to all of them means the winner carries the flag
/// whichever it turns out to be. If none is configured, `build.rustflags` is created.
pub(super) fn cap_lints(root: &Utf8Path) -> Result<()> {
    let path = root.join(".cargo").join("config.toml");
    let legacy = root.join(".cargo").join("config");
    let path = if !path.as_std_path().is_file() && legacy.as_std_path().is_file() {
        legacy
    } else {
        path
    };
    let _destination = crate::paths::require_within(&path, root, "a scratch Cargo configuration")?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent.as_std_path()).map_err(|cause| error!("could not create `{parent}`").caused_by(cause))?;
    }

    if !path.as_std_path().is_file() {
        fs::write(path.as_std_path(), format!("[build]\nrustflags = [\"{CAP_LINTS}\"]\n"))
            .map_err(|cause| error!("could not write `{path}`").caused_by(cause))?;

        return Ok(());
    }

    let mut manifest = Manifest::read(&path)?;
    let mut found = false;

    if let Some(build) = manifest.document.get_mut("build") {
        let Some(build) = build.as_table_like_mut() else {
            return Err(error!(
                "Cargo configuration `{path}` has a non-table `build` setting; use a `[build]` table so cargo-gamma can add `{CAP_LINTS}`"
            ));
        };

        if let Some(flags) = build.get_mut("rustflags") {
            found |= append_flag(flags);
        }
    }

    if let Some(targets) = manifest.document.get_mut("target").and_then(Item::as_table_like_mut) {
        for (_name, entry) in targets.iter_mut() {
            let Some(table) = entry.as_table_like_mut() else {
                continue;
            };

            if let Some(flags) = table.get_mut("rustflags") {
                found |= append_flag(flags);
            }
        }
    }

    if !found {
        let build = manifest
            .document
            .entry("build")
            .or_insert(Item::Table(Table::new()))
            .as_table_like_mut()
            .ok_or_else(|| error!("Cargo configuration `{path}` has a non-table `build` setting"))?;

        let _previous = build.insert("rustflags", Item::Value(Value::Array(core::iter::once(CAP_LINTS).collect())));
    }

    manifest.changed = true;
    manifest.save()
}

/// Appends the cap to one rustflags entry, which cargo accepts as an array or as one string.
fn append_flag(flags: &mut Item) -> bool {
    if let Some(array) = flags.as_array_mut() {
        array.push(CAP_LINTS);

        return true;
    }

    if let Some(text) = flags.as_str() {
        *flags = Item::Value(Value::from(format!("{text} {CAP_LINTS}")));

        return true;
    }

    false
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;

    fn fixed(text: &str, original: &str) -> String {
        within(text, original, "")
    }

    fn within(text: &str, original: &str, within: &str) -> String {
        let temporary = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temporary.path().join("Cargo.toml")).unwrap();

        fs::write(path.as_std_path(), text).unwrap();

        let mut manifest = Manifest::read(&path).unwrap();

        manifest.anchor_paths(Utf8Path::new(original), Utf8Path::new(within));

        manifest.document.to_string()
    }

    #[test]
    fn a_dependency_written_as_a_bare_version_is_left_alone() {
        // `serde = "1"` has no path to anchor, and treating the string as a specification table
        // would either panic or silently rewrite the version requirement.
        let text = "[dependencies]\nserde = \"1\"\ncore = { path = \"../core\" }\n";
        let fixed = fixed(text, "/src/app");

        assert!(fixed.contains("serde = \"1\""), "{fixed}");
        assert!(fixed.contains("/src/core"), "{fixed}");
    }

    #[test]
    fn a_sibling_inside_the_copied_tree_is_left_alone() {
        // `app/../core` is `core`, which the copy brought along. Anchoring it back to the original
        // would put the same package at two locations and cargo would refuse to write a lockfile.
        let fixed = within("[dependencies]\ncore = { path = \"../core\" }\n", "/src/app", "app");

        assert!(fixed.contains("path = \"../core\""), "{fixed}");
    }

    #[test]
    fn a_path_leaving_the_copied_tree_is_anchored_even_from_a_nested_package() {
        let fixed = within("[dependencies]\nshared = { path = \"../../shared\" }\n", "/src/work/app", "app");

        assert!(fixed.contains("path = \"/src/shared\""), "{fixed}");
    }

    #[test]
    fn a_path_leaving_the_package_is_anchored() {
        let fixed = fixed("[dependencies]\nshared = { path = \"../shared\" }\n", "/src/app");

        assert!(fixed.contains("path = \"/src/shared\""), "{fixed}");
    }

    #[test]
    fn a_path_staying_inside_the_package_is_left_alone() {
        // It still resolves in the copy, and rewriting it would tie the tree back to the original.
        let fixed = fixed("[dependencies]\ninner = { path = \"crates/inner\" }\n", "/src/app");

        assert!(fixed.contains("path = \"crates/inner\""), "{fixed}");
    }

    #[test]
    fn an_absolute_path_is_left_alone() {
        let fixed = fixed("[dependencies]\nshared = { path = \"/elsewhere/shared\" }\n", "/src/app");

        assert!(fixed.contains("path = \"/elsewhere/shared\""), "{fixed}");
    }

    #[test]
    fn a_path_that_descends_before_climbing_is_judged_on_the_whole_journey() {
        // `crates/../../shared` leaves the package even though it starts by entering it.
        let fixed = fixed("[dependencies]\nshared = { path = \"crates/../../shared\" }\n", "/src/app");

        assert!(fixed.contains("path = \"/src/shared\""), "{fixed}");
    }

    #[test]
    fn every_kind_of_dependency_table_is_covered() {
        let text = "[dependencies]\na = { path = \"../a\" }\n\
                    [dev-dependencies]\nb = { path = \"../b\" }\n\
                    [build-dependencies]\nc = { path = \"../c\" }\n\
                    [target.'cfg(unix)'.dependencies]\nd = { path = \"../d\" }\n\
                    [patch.crates-io]\ne = { path = \"../e\" }\n\
                    [workspace.dependencies]\nf = { path = \"../f\" }\n";

        let fixed = fixed(text, "/src/app");

        for crate_name in ["a", "b", "c", "d", "e", "f"] {
            assert!(fixed.contains(&format!("path = \"/src/{crate_name}\"")), "{crate_name} in {fixed}");
        }
    }

    #[test]
    fn a_patch_entry_that_is_not_a_table_does_not_stop_the_next_registry_from_being_anchored() {
        // A malformed `[patch]` entry — a bare string rather than a table of dependencies — must
        // not abort repairing the registries that come after it in the same document.
        let text = "[patch]\nbroken = \"not a table\"\n\n\
                    [patch.crates-io]\nshared = { path = \"../shared\" }\n";

        let fixed = fixed(text, "/src/app");

        assert!(fixed.contains("broken = \"not a table\""), "{fixed}");
        assert!(fixed.contains("path = \"/src/shared\""), "{fixed}");
    }

    #[test]
    fn comments_and_formatting_survive() {
        // The whole reason for editing rather than reserialising.
        let text = "# keep me\n[dependencies]\n# and me\nshared = { path = \"../shared\" }  # trailing\n";
        let fixed = fixed(text, "/src/app");

        assert!(fixed.contains("# keep me"), "{fixed}");
        assert!(fixed.contains("# and me"), "{fixed}");
        assert!(fixed.contains("# trailing"), "{fixed}");
    }

    #[test]
    fn a_version_only_dependency_is_untouched() {
        let fixed = fixed("[dependencies]\nserde = \"1\"\n", "/src/app");

        assert!(fixed.contains("serde = \"1\""), "{fixed}");
    }

    #[test]
    fn target_entries_that_are_not_tables_are_skipped() {
        let fixed = fixed(
            "[target]\nnot_a_table = \"ignored\"\n[dependencies]\na = { path = \"../a\" }\n",
            "/src/app",
        );

        // Some manifests put metadata under target-like tables; non-tables must not stop ordinary
        // dependencies later in the document from being repaired.
        assert!(fixed.contains("not_a_table = \"ignored\""), "{fixed}");
        assert!(fixed.contains("path = \"/src/a\""), "{fixed}");
    }

    #[test]
    fn dependencies_without_a_path_are_skipped() {
        let fixed = fixed(
            "[dependencies]\nserde = { version = \"1\" }\nlocal = { path = \"../local\" }\n",
            "/src/app",
        );

        // Versioned inline-table dependencies are pathless and should survive byte-for-byte while
        // path dependencies next to them are still anchored.
        assert!(fixed.contains("serde = { version = \"1\" }"), "{fixed}");
        assert!(fixed.contains("path = \"/src/local\""), "{fixed}");
    }

    fn linked(text: &str) -> String {
        let temporary = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temporary.path().join("Cargo.toml")).unwrap();

        fs::write(path.as_std_path(), text).unwrap();

        let mut manifest = Manifest::read(&path).unwrap();

        manifest.link_runtime(Utf8Path::new("/scratch/rt")).unwrap();

        manifest.document.to_string()
    }

    /// Same as `linked`, but folds in feature settings resolved separately from a workspace's own
    /// `[workspace.dependencies]` declaration, the way `anchor_manifests` does for a real tree.
    fn linked_inheriting(text: &str, workspace_features: &WorkspaceRuntimeFeatures) -> String {
        let temporary = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temporary.path().join("Cargo.toml")).unwrap();

        fs::write(path.as_std_path(), text).unwrap();

        let mut manifest = Manifest::read(&path).unwrap();

        manifest
            .link_runtime_inheriting(Utf8Path::new("/scratch/rt"), workspace_features)
            .unwrap();

        manifest.document.to_string()
    }

    /// Reads the `features`/`default-features` a workspace manifest's text declares for the
    /// runtime crate, the way `anchor_manifests` resolves them before editing any member.
    fn workspace_features(text: &str) -> WorkspaceRuntimeFeatures {
        let temporary = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temporary.path().join("Cargo.toml")).unwrap();

        fs::write(path.as_std_path(), text).unwrap();

        WorkspaceRuntimeFeatures::from_workspace(&path).unwrap()
    }

    #[test]
    fn the_runtime_is_added_to_a_package_without_it() {
        let text = linked("[package]\nname = \"x\"\n");
        let runtime = absolute(Utf8Path::new("/scratch/rt"));

        assert!(text.contains("gamma_rt"), "{text}");
        assert!(text.contains(&portable_path(&runtime)), "{text}");
    }

    /// Cargo reads a dependency path relative to the manifest holding it, and this manifest lives
    /// in the copied tree rather than where the run was started, so a relative runtime path would
    /// be looked for under the copy.
    #[test]
    fn a_relative_runtime_path_is_written_absolute() {
        let temporary = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temporary.path().join("Cargo.toml")).unwrap();

        fs::write(path.as_std_path(), "[package]\nname = \"x\"\n").unwrap();

        let mut manifest = Manifest::read(&path).unwrap();

        manifest.link_runtime(Utf8Path::new("scratch/gamma/rt")).unwrap();

        let text = manifest.document.to_string();
        let expected = absolute(Utf8Path::new("scratch/gamma/rt"));

        assert!(expected.is_absolute(), "{expected}");
        assert!(text.contains(&portable_path(&expected)), "{text}");
    }

    #[test]
    fn the_runtime_is_added_to_an_existing_dependency_table() {
        let text = linked("[package]\nname = \"x\"\n\n[dependencies]\nserde = \"1\"\n");

        assert!(text.contains("gamma_rt"), "{text}");
        assert!(text.contains("serde = \"1\""), "{text}");
    }

    #[test]
    fn a_malformed_dependency_table_cannot_receive_the_runtime() {
        let text = linked("dependencies = \"not a table\"\n[package]\nname = \"x\"\n");

        // If the user wrote a non-table where dependencies belong, this pass leaves it to cargo's
        // manifest parser rather than inventing a structure and hiding the original problem.
        assert!(!text.contains("gamma_rt"), "{text}");
        assert!(text.contains("dependencies = \"not a table\""), "{text}");
    }

    #[test]
    fn an_existing_runtime_dependency_is_replaced_by_the_vendored_one() {
        let runtime = absolute(Utf8Path::new("/scratch/rt"));

        for text in [
            "[dependencies]\ncargo-gamma-rt = { workspace = true }\n",
            "[dependencies]\ngamma_rt = { path = \"/scratch/rt\" }\n",
            "[dependencies]\ngamma_rt = { package = \"cargo-gamma-rt\", version = \"0.1\" }\n",
            "[dependencies]\ngamma_rt = { workspace = true }\n",
        ] {
            let linked = linked(text);

            assert_eq!(linked.matches("gamma_rt").count(), 1, "{linked}");
            assert!(linked.contains("package = \"cargo-gamma-rt\""), "{linked}");
            assert!(linked.contains(&portable_path(&runtime)), "{linked}");
        }
    }

    #[test]
    fn the_features_of_a_direct_dependency_survive_being_linked_to_the_vendored_runtime() {
        let linked = linked("[dependencies]\ngamma_rt = { package = \"cargo-gamma-rt\", version = \"0.1\", features = [\"embedding\"] }\n");

        assert!(linked.contains("features = [\"embedding\"]"), "{linked}");
        assert!(linked.contains("package = \"cargo-gamma-rt\""), "{linked}");
    }

    #[test]
    fn a_member_level_feature_override_survives_being_linked_to_the_vendored_runtime() {
        let linked = linked("[dependencies]\ngamma_rt = { workspace = true, features = [\"embedding\"] }\n");

        assert!(linked.contains("features = [\"embedding\"]"), "{linked}");
        assert!(linked.contains("package = \"cargo-gamma-rt\""), "{linked}");
    }

    #[test]
    fn default_features_false_survives_being_linked_to_the_vendored_runtime() {
        let linked = linked("[dependencies]\ngamma_rt = { package = \"cargo-gamma-rt\", version = \"0.1\", default-features = false }\n");

        assert!(linked.contains("default-features = false"), "{linked}");
    }

    #[test]
    fn a_dependency_without_feature_settings_produces_the_existing_output() {
        let linked = linked("[dependencies]\ngamma_rt = { package = \"cargo-gamma-rt\", version = \"0.1\" }\n");

        assert!(!linked.contains("features"), "{linked}");
        assert!(!linked.contains("default-features"), "{linked}");
    }

    /// A member overriding `workspace = true` with its own `features` does not replace whatever
    /// the workspace's own `[workspace.dependencies]` declaration already listed — Cargo unions
    /// both onto the one shared crate instance, so losing either half would compile a runtime
    /// missing a feature some guard actually needs.
    #[test]
    fn combined_workspace_and_member_features_survive_being_linked_to_the_vendored_runtime() {
        let workspace = workspace_features(
            "[workspace]\nmembers = []\n\n\
             [workspace.dependencies]\ngamma_rt = { package = \"cargo-gamma-rt\", version = \"0.1\", features = [\"base\"] }\n",
        );

        let linked = linked_inheriting(
            "[dependencies]\ngamma_rt = { workspace = true, features = [\"embedding\"] }\n",
            &workspace,
        );

        assert!(linked.contains("package = \"cargo-gamma-rt\""), "{linked}");
        assert!(linked.contains("\"embedding\""), "{linked}");
        assert!(linked.contains("\"base\""), "{linked}");
        assert_eq!(linked.matches("features").count(), 1, "{linked}");
    }

    /// The workspace's own `default-features` setting has to reach a member that inherits the
    /// dependency bare — `link_runtime` only ever sees that member's own entry, which carries no
    /// override to fall back on.
    #[test]
    fn a_workspace_declared_default_features_false_survives_a_bare_workspace_true_inheritance() {
        let workspace = workspace_features(
            "[workspace]\nmembers = []\n\n\
             [workspace.dependencies]\ngamma_rt = { package = \"cargo-gamma-rt\", version = \"0.1\", default-features = false }\n",
        );

        let linked = linked_inheriting("[dependencies]\ngamma_rt = { workspace = true }\n", &workspace);

        assert!(linked.contains("default-features = false"), "{linked}");
    }

    /// A target-specific declaration is discarded once folded into the single canonical entry;
    /// its features have to be read out first or they vanish along with the table that named them.
    #[test]
    fn target_specific_features_survive_being_linked_to_the_vendored_runtime() {
        let linked = linked("[target.'cfg(unix)'.dependencies]\ngamma_rt = { workspace = true, features = [\"only-target\"] }\n");

        assert!(linked.contains("package = \"cargo-gamma-rt\""), "{linked}");
        assert!(linked.contains("features = [\"only-target\"]"), "{linked}");
        // The target-specific entry itself is gone, folded into the one canonical entry below —
        // even though the (now empty) table header that held it is left in place, the same way
        // removing an ordinary dependency does.
        assert_eq!(linked.matches("gamma_rt").count(), 1, "{linked}");
    }

    /// Guards against the target-specific and workspace-inheritance merging above inventing an
    /// empty `features`/`default-features` key when nothing anywhere actually declared one.
    #[test]
    fn no_feature_settings_from_any_source_produce_the_existing_output() {
        let linked = linked_inheriting(
            "[target.'cfg(unix)'.dependencies]\ngamma_rt = { workspace = true }\n\n\
             [dependencies]\ngamma_rt = { workspace = true }\n",
            &WorkspaceRuntimeFeatures::default(),
        );

        assert!(!linked.contains("features"), "{linked}");
        assert!(!linked.contains("default-features"), "{linked}");
    }

    /// A direct dependency — one that names its own path or version rather than saying
    /// `workspace = true` — must not acquire `features` the workspace's own
    /// `[workspace.dependencies]` entry happens to declare for the same key: that entry is
    /// irrelevant to a member that never named it, and inheriting from it anyway would silently
    /// add a feature the member's own declaration never asked for.
    #[test]
    fn a_direct_dependency_does_not_acquire_unrelated_workspace_features() {
        let workspace = workspace_features(
            "[workspace]\nmembers = []\n\n\
             [workspace.dependencies]\ngamma_rt = { package = \"cargo-gamma-rt\", version = \"0.1\", features = [\"unrelated\"] }\n",
        );

        let linked = linked_inheriting(
            "[dependencies]\ngamma_rt = { package = \"cargo-gamma-rt\", path = \"../rt\", features = [\"direct\"] }\n",
            &workspace,
        );

        assert!(linked.contains("\"direct\""), "{linked}");
        assert!(!linked.contains("unrelated"), "{linked}");
    }

    /// A target-specific entry that says `workspace = true` under the canonical key must inherit
    /// only the workspace's canonical-key declaration, not whatever the aliased key happens to
    /// declare — the two keys are independent declarations even though both ultimately name the
    /// same crate.
    #[test]
    fn target_specific_workspace_true_inherits_only_its_own_key() {
        let workspace = workspace_features(
            "[workspace]\nmembers = []\n\n\
             [workspace.dependencies]\n\
             gamma_rt = { package = \"cargo-gamma-rt\", version = \"0.1\", features = [\"aliased-only\"] }\n\
             cargo-gamma-rt = { version = \"0.1\", features = [\"canonical-only\"] }\n",
        );

        let linked = linked_inheriting(
            "[target.'cfg(unix)'.dependencies]\ncargo-gamma-rt = { workspace = true }\n",
            &workspace,
        );

        assert!(linked.contains("\"canonical-only\""), "{linked}");
        assert!(!linked.contains("aliased-only"), "{linked}");
    }

    #[test]
    fn an_existing_runtime_is_redirected_before_its_package_is_instrumented() {
        let temporary = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temporary.path().join("Cargo.toml")).unwrap();
        let runtime = absolute(Utf8Path::new("/scratch/rt"));

        fs::write(
            path.as_std_path(),
            "[package]\nname = \"x\"\n\n[dependencies]\ncargo-gamma-rt = { workspace = true }\n",
        )
        .unwrap();

        let mut manifest = Manifest::read(&path).unwrap();
        manifest.redirect_runtime(&runtime, &WorkspaceRuntimeFeatures::default()).unwrap();
        let text = manifest.document.to_string();

        assert!(text.contains("package = \"cargo-gamma-rt\""), "{text}");
        assert!(text.contains(&portable_path(&runtime)), "{text}");
    }

    #[test]
    fn a_workspace_runtime_alias_is_redirected_before_members_inherit_it() {
        let temporary = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temporary.path().join("Cargo.toml")).unwrap();
        let runtime = absolute(Utf8Path::new("/scratch/rt"));

        fs::write(
            path.as_std_path(),
            "[workspace]\nmembers = []\n\n\
             [workspace.dependencies]\ngamma_rt = { package = \"cargo-gamma-rt\", version = \"0.1\" }\n",
        )
        .unwrap();

        let mut manifest = Manifest::read(&path).unwrap();
        manifest.redirect_runtime(&runtime, &WorkspaceRuntimeFeatures::default()).unwrap();
        let text = manifest.document.to_string();

        assert_eq!(text.matches("\ngamma_rt =").count(), 1, "{text}");
        assert!(text.contains("gamma_rt = { package = \"cargo-gamma-rt\""), "{text}");
        assert!(text.contains("cargo-gamma-rt = { path"), "{text}");
        assert!(text.contains(&portable_path(&runtime)), "{text}");
    }

    #[test]
    fn workspace_dependency_features_survive_being_redirected() {
        let temporary = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temporary.path().join("Cargo.toml")).unwrap();
        let runtime = absolute(Utf8Path::new("/scratch/rt"));

        fs::write(
            path.as_std_path(),
            "[workspace]\nmembers = []\n\n\
             [workspace.dependencies]\ngamma_rt = { package = \"cargo-gamma-rt\", version = \"0.1\", features = [\"embedding\"] }\n",
        )
        .unwrap();

        let mut manifest = Manifest::read(&path).unwrap();
        manifest.redirect_runtime(&runtime, &WorkspaceRuntimeFeatures::default()).unwrap();
        let text = manifest.document.to_string();

        assert!(text.contains("gamma_rt = { package = \"cargo-gamma-rt\""), "{text}");
        assert!(text.contains("features = [\"embedding\"]"), "{text}");
    }

    #[test]
    fn a_canonical_workspace_runtime_remains_available_to_inheriting_dependencies() {
        let temporary = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temporary.path().join("Cargo.toml")).unwrap();
        let runtime = absolute(Utf8Path::new("/scratch/rt"));

        fs::write(
            path.as_std_path(),
            "[workspace]\nmembers = []\n\n\
             [workspace.dependencies]\ncargo-gamma-rt = \"0.1\"\n",
        )
        .unwrap();

        let mut manifest = Manifest::read(&path).unwrap();
        manifest.redirect_runtime(&runtime, &WorkspaceRuntimeFeatures::default()).unwrap();
        let text = manifest.document.to_string();

        assert!(text.contains("gamma_rt = { package = \"cargo-gamma-rt\", path"), "{text}");
        assert!(text.contains("cargo-gamma-rt = { path"), "{text}");
        assert_eq!(text.matches(&portable_path(&runtime)).count(), 2, "{text}");
    }

    #[test]
    fn an_unrelated_workspace_dependency_cannot_occupy_the_runtime_name() {
        let temporary = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temporary.path().join("Cargo.toml")).unwrap();
        let runtime = absolute(Utf8Path::new("/scratch/rt"));

        fs::write(
            path.as_std_path(),
            "[workspace]\nmembers = []\n\n\
             [workspace.dependencies]\ngamma_rt = { package = \"some-other-package\", version = \"1\" }\n",
        )
        .unwrap();

        let mut manifest = Manifest::read(&path).unwrap();
        let failure = manifest
            .redirect_runtime(&runtime, &WorkspaceRuntimeFeatures::default())
            .expect_err("the generated guard's crate name must be reserved workspace-wide");

        assert!(failure.is_usage(), "{failure}");
        assert!(failure.to_string().contains("reserves that crate name"), "{failure}");
    }

    #[test]
    fn an_unrelated_dependency_cannot_occupy_the_runtime_name() {
        for dependency in [
            "gamma_rt = \"1\"",
            "gamma_rt = { package = \"some-other-package\", version = \"1\" }",
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let path = Utf8PathBuf::from_path_buf(temporary.path().join("Cargo.toml")).unwrap();

            fs::write(
                path.as_std_path(),
                format!("[package]\nname = \"x\"\n\n[dependencies]\n{dependency}\n"),
            )
            .unwrap();

            let mut manifest = Manifest::read(&path).unwrap();
            let failure = manifest
                .link_runtime(Utf8Path::new("/scratch/rt"))
                .expect_err("the generated guard's crate name must be reserved");

            assert!(failure.is_usage(), "{failure}");
            assert!(failure.to_string().contains("reserves that crate name"), "{failure}");
        }
    }

    #[test]
    fn a_dependency_the_library_target_cannot_see_does_not_count() {
        // Guards live in library code, where a dev- or build-dependency is not in scope.
        let runtime = absolute(Utf8Path::new("/scratch/rt"));

        for text in ["[dev-dependencies]\ngamma_rt = \"1\"\n", "[build-dependencies]\ngamma_rt = \"1\"\n"] {
            assert!(linked(text).contains(&portable_path(&runtime)), "{text}");
        }
    }

    #[test]
    fn a_cargo_config_path_override_is_anchored() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();

        fs::create_dir_all(root.join(".cargo").as_std_path()).unwrap();
        fs::write(
            root.join(".cargo").join("config.toml").as_std_path(),
            "paths = [\"../vendored\", \"inside\"]\n",
        )
        .unwrap();

        anchor_cargo_config(&root, Utf8Path::new("/src/app")).unwrap();

        let text = fs::read_to_string(root.join(".cargo").join("config.toml").as_std_path()).unwrap();

        assert!(text.contains("/src/vendored"), "{text}");
        assert!(text.contains("\"inside\""), "{text}");
    }

    #[test]
    fn a_cargo_config_with_no_paths_override_is_left_alone() {
        // A `.cargo/config.toml` need not configure path overrides at all; the absence of the key
        // must not be treated as an error or invent one out of nothing.
        let temporary = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).expect("utf8");

        fs::create_dir_all(root.join(".cargo").as_std_path()).expect(".cargo");
        fs::write(root.join(".cargo").join("config.toml").as_std_path(), "[net]\nretry = 3\n").expect("config");

        anchor_cargo_config(&root, Utf8Path::new("/src/app")).expect("anchor");

        let text = fs::read_to_string(root.join(".cargo").join("config.toml").as_std_path()).expect("read back");

        assert!(text.contains("retry"), "{text}");
        assert!(!text.contains("paths"), "{text}");
    }

    #[test]
    fn the_lint_cap_is_added_to_configured_rustflags_rather_than_replacing_them() {
        // Setting `RUSTFLAGS` would drop these, which can change what the tree compiles to and
        // therefore what its tests prove.
        let temporary = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();
        let config = root.join(".cargo").join("config.toml");

        fs::create_dir_all(config.parent().unwrap().as_std_path()).unwrap();
        fs::write(
            config.as_std_path(),
            "[build]\nrustflags = [\"--cfg\", \"loom\"]\n\n[target.x86_64-unknown-linux-gnu]\nrustflags = \"-C target-cpu=native\"\n",
        )
        .unwrap();

        cap_lints(&root).unwrap();

        let text = fs::read_to_string(config.as_std_path()).unwrap();

        assert!(text.contains("loom"), "{text}");
        assert!(text.contains("target-cpu=native"), "{text}");
        // Both keys carry it, because which one cargo picks depends on the target triple.
        assert_eq!(text.matches(CAP_LINTS).count(), 2, "{text}");
    }

    #[test]
    fn a_tree_with_no_cargo_config_gets_one() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();

        cap_lints(&root).unwrap();

        let text = fs::read_to_string(root.join(".cargo").join("config.toml").as_std_path()).unwrap();

        assert!(text.contains(CAP_LINTS), "{text}");
    }

    #[test]
    fn a_cargo_config_with_no_rustflags_gains_them() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();
        let config = root.join(".cargo").join("config.toml");

        fs::create_dir_all(config.parent().unwrap().as_std_path()).unwrap();
        fs::write(config.as_std_path(), "[net]\nretry = 3\n").unwrap();

        cap_lints(&root).unwrap();

        let text = fs::read_to_string(config.as_std_path()).unwrap();

        assert!(text.contains(CAP_LINTS), "{text}");
        assert!(text.contains("retry"), "{text}");
    }

    #[test]
    fn a_scalar_build_configuration_is_reported_without_panicking() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).expect("utf8");
        let config = root.join(".cargo").join("config.toml");

        fs::create_dir_all(config.parent().expect("parent").as_std_path()).expect("mkdir");
        fs::write(config.as_std_path(), "build = \"not a table\"\n").expect("config");

        let failure = cap_lints(&root).expect_err("a scalar build key cannot receive rustflags");

        assert!(failure.to_string().contains("non-table `build`"), "{failure}");
        assert!(failure.to_string().contains(config.as_str()), "{failure}");
    }

    /// A target entry that is not itself a table — some manifests store other metadata alongside
    /// real target platforms — must be skipped rather than aborting the whole pass, so that a
    /// well-formed target sharing the document still gets the cap.
    #[test]
    fn a_target_entry_that_is_not_a_table_does_not_stop_the_cap_from_landing_elsewhere() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).expect("utf8");
        let config = root.join(".cargo").join("config.toml");

        fs::create_dir_all(config.parent().expect("parent").as_std_path()).expect("mkdir");
        fs::write(
            config.as_std_path(),
            "[target]\nnot_a_table = \"ignored\"\n\n\
             [target.'cfg(unix)']\nlinker = \"lld\"\n\n\
             [target.x86_64-unknown-linux-gnu]\nrustflags = [\"--cfg\", \"loom\"]\n",
        )
        .expect("write config");

        cap_lints(&root).expect("cap_lints");

        let text = fs::read_to_string(config.as_std_path()).expect("read back");

        assert!(text.contains("not_a_table = \"ignored\""), "{text}");
        // The target with no rustflags key at all keeps its other settings untouched.
        assert!(text.contains("linker = \"lld\""), "{text}");
        // Only the one target that actually carries rustflags gets the cap appended.
        assert_eq!(text.matches(CAP_LINTS).count(), 1, "{text}");
    }

    /// A rustflags value that is neither an array nor a string — malformed, or written some other
    /// way entirely — cannot be appended to; `append_flag` reports that it made no addition, and
    /// the fallback then treats the tree as though it configured no rustflags at all, replacing
    /// the unusable value with a fresh array carrying just the cap.
    #[test]
    fn a_rustflags_value_that_is_neither_an_array_nor_a_string_is_replaced() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).expect("utf8");
        let config = root.join(".cargo").join("config.toml");

        fs::create_dir_all(config.parent().expect("parent").as_std_path()).expect("mkdir");
        fs::write(config.as_std_path(), "[build]\nrustflags = 5\n").expect("write config");

        cap_lints(&root).expect("cap_lints");

        let text = fs::read_to_string(config.as_std_path()).expect("read back");

        assert!(!text.contains("rustflags = 5"), "{text}");
        assert!(text.contains(CAP_LINTS), "{text}");
    }

    #[test]
    fn the_legacy_cargo_config_name_gains_the_cap_too() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();
        let config = root.join(".cargo").join("config");

        fs::create_dir_all(config.parent().unwrap().as_std_path()).unwrap();
        fs::write(config.as_std_path(), "[build]\nrustflags = [\"--cfg\", \"loom\"]\n").unwrap();

        cap_lints(&root).unwrap();

        let text = fs::read_to_string(config.as_std_path()).unwrap();

        assert!(text.contains(CAP_LINTS), "{text}");
        assert!(!root.join(".cargo").join("config.toml").as_std_path().exists());
    }

    #[test]
    fn a_legacy_cargo_config_name_is_anchored() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();

        fs::create_dir_all(root.join(".cargo").as_std_path()).unwrap();
        fs::write(root.join(".cargo").join("config").as_std_path(), "paths = [\"../vendored\"]\n").unwrap();

        anchor_cargo_config(&root, Utf8Path::new("/src/app")).unwrap();

        let text = fs::read_to_string(root.join(".cargo").join("config").as_std_path()).unwrap();

        // Cargo still accepts `.cargo/config`; it needs the same repair as the TOML-suffixed name.
        assert!(text.contains("/src/vendored"), "{text}");
    }

    #[test]
    fn normalization_keeps_leading_parent_components() {
        // Anchoring from a filesystem root keeps the parent component textually rather than
        // resolving it through the host filesystem.
        assert_eq!(normalize(Utf8Path::new("/../shared")), Utf8PathBuf::from("/../shared"));
    }

    #[test]
    fn a_missing_cargo_config_is_not_an_error() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();

        anchor_cargo_config(&root, Utf8Path::new("/src/app")).unwrap();
    }

    /// A path written with an explicit current-directory component — `./sibling` rather than plain
    /// `sibling` — must be judged exactly the same as if the component were absent: a user who adds
    /// or a tool that emits the redundant `./` should not thereby escape the check for whether the
    /// path leaves the tree, nor have it resolved to something subtly different.
    #[test]
    fn a_leading_current_directory_component_does_not_change_whether_a_path_escapes() {
        assert!(!escapes(Utf8Path::new("./sibling/deeper")));
        assert!(escapes(Utf8Path::new("./..")));
    }

    /// The same redundant `./` component must vanish during normalization rather than being kept
    /// verbatim, or two dependency paths that name the same file — one written plainly, one with a
    /// stray `./` — would come out looking different once anchored.
    #[test]
    fn normalization_drops_current_directory_components() {
        assert_eq!(normalize(Utf8Path::new("a/./b")), Utf8PathBuf::from("a/b"));
    }

    /// A `.cargo/config.toml` that exists but is not valid TOML is a file the user's own build is
    /// already failing on, not something this repair step can silently paper over: the read has to
    /// fail loudly, naming the file, rather than being treated the same as a config with no `paths`
    /// key at all.
    #[test]
    fn a_cargo_config_that_cannot_be_parsed_is_reported() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).expect("utf8");

        fs::create_dir_all(root.join(".cargo").as_std_path()).expect(".cargo");
        fs::write(root.join(".cargo").join("config.toml").as_std_path(), "not [ valid toml").expect("config");

        let error = anchor_cargo_config(&root, Utf8Path::new("/src/app")).expect_err("the file does not parse");

        assert!(error.to_string().contains("could not parse"), "{error}");
    }

    /// Once a `paths` override has actually been rewritten, the anchored config has to be written
    /// back; if the file cannot be saved — permissions revoked between the read and the write, say
    /// — that failure has to surface rather than leaving the tree instrumented with the original,
    /// unanchored path that would fail to resolve.
    #[cfg(unix)]
    #[test]
    fn a_cargo_config_that_cannot_be_saved_after_anchoring_is_reported() {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).expect("utf8");
        let config = root.join(".cargo").join("config.toml");

        fs::create_dir_all(config.parent().expect("parent").as_std_path()).expect(".cargo");
        fs::write(config.as_std_path(), "paths = [\"../vendored\"]\n").expect("config");
        fs::set_permissions(config.as_std_path(), fs::Permissions::from_mode(0o400)).expect("chmod");

        let error = anchor_cargo_config(&root, Utf8Path::new("/src/app")).expect_err("the file cannot be written");

        fs::set_permissions(config.as_std_path(), fs::Permissions::from_mode(0o644)).expect("chmod back");

        assert!(error.to_string().contains("could not update"), "{error}");
    }

    /// A fresh `.cargo/config.toml` is created when a tree has none, and if the directory it would
    /// live in refuses the write — a permission denied between the directory being created and the
    /// file inside it — the caller has to be told the tree could not be prepared rather than
    /// silently building it without the lint cap.
    #[cfg(unix)]
    #[test]
    fn a_cargo_config_that_cannot_be_created_reports_the_write_failure() {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).expect("utf8");
        let cargo_dir = root.join(".cargo");

        fs::create_dir_all(cargo_dir.as_std_path()).expect(".cargo");
        fs::set_permissions(cargo_dir.as_std_path(), fs::Permissions::from_mode(0o500)).expect("chmod");

        let error = cap_lints(&root).expect_err("the directory cannot be written into");

        fs::set_permissions(cargo_dir.as_std_path(), fs::Permissions::from_mode(0o755)).expect("chmod back");

        assert!(error.to_string().contains("could not write"), "{error}");
    }

    /// `cap_lints` reads whichever config already exists before adding the flag, and a config that
    /// does not parse must stop it the same way any other unreadable manifest does, rather than
    /// treating the tree as though it had no configuration to preserve.
    #[test]
    fn cap_lints_reports_an_existing_config_that_cannot_be_parsed() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).expect("utf8");
        let config = root.join(".cargo").join("config.toml");

        fs::create_dir_all(config.parent().expect("parent").as_std_path()).expect(".cargo");
        fs::write(config.as_std_path(), "not [ valid toml").expect("config");

        let error = cap_lints(&root).expect_err("the file does not parse");

        assert!(error.to_string().contains("could not parse"), "{error}");
    }
}
