// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Working out which Cargo features are on, without asking Cargo to resolve the whole graph.
//!
//! `#[cfg(feature = "...")]` is the most common gate in a real workspace, and `rustc` cannot answer
//! it: features are Cargo's concept, not the compiler's. The obvious source is `cargo metadata`
//! with its dependency resolution, but that requires a resolvable graph and can reach the network,
//! which would make discovery fail on a tree that builds perfectly well offline.
//!
//! So the answer is computed here from the metadata this tool already loads, which lists every
//! workspace member's own feature table and its declared dependencies. Conditional or unselected
//! dependency contributions are kept uncertain when that closure is used for cfg evaluation.
//!
//! # What is resolved
//!
//! Starting from what the command line selected, three things propagate to a fixed point:
//!
//! 1. A feature's own entries, so `default = ["std"]` turns `default` on into `std` on.
//! 2. A `member/feature` entry, so one member can turn on another member's feature.
//! 3. A dependency declaration on another member, which contributes its `features` list and, unless
//!    it opted out, that member's `default`.
//!
//! Development and build dependencies count too, because the schema is built with `cargo test`,
//! which compiles all of them.
//!
//! # Erring toward keeping a mutant
//!
//! An optional dependency contributes features only after its activation is established. Uncertain
//! activation must remain unknown in cfg evaluation: guessing "on" would strip live code under
//! `not(feature = "...")` just as guessing "off" would strip its positive branch.

use cargo_metadata::{DependencyKind, Metadata, Package};

use crate::commands::FeatureArgs;
use crate::{HashMap, HashSet};

/// Returns the features enabled for each workspace member.
///
/// Only workspace members appear: a registry dependency is never mutated, so its features are of no
/// interest, and guessing at them would cost a full resolve.
///
/// ```rust,no_run
/// # #[cfg(feature = "internals")]
/// # fn main() {
/// use cargo_gamma_lib::internals::cfg::features::enabled;
/// use cargo_gamma_lib::internals::commands::FeatureArgs;
/// # let metadata: &cargo_metadata::Metadata = todo!();
/// let features = enabled(metadata, &FeatureArgs::default());
///
/// // Every member is present, even one with no features at all.
/// assert!(features.contains_key("my-crate"));
/// # }
/// # #[cfg(not(feature = "internals"))]
/// # fn main() {}
/// ```
#[must_use]
pub fn enabled(metadata: &Metadata, args: &FeatureArgs) -> HashMap<String, Vec<String>> {
    let members: Vec<&Package> = metadata.workspace_packages();
    let named = requested(args);
    let renames = renames(&members);
    let mut on: HashMap<String, HashSet<String>> = HashMap::default();

    for package in &members {
        let _old = on.insert(package.name.as_str().to_owned(), seed(package, args, &named));
    }

    propagate_worklist(&members, &renames, &mut on);

    on.into_iter()
        .map(|(package, features)| {
            let mut sorted: Vec<String> = features.into_iter().collect();

            sorted.sort();

            (package, sorted)
        })
        .collect()
}

/// Splits the `--features` values into the flat list of names they denote.
///
/// Cargo accepts both `--features a,b` and `--features a --features b`, and a `package/feature`
/// entry names a feature of another package.
fn requested(args: &FeatureArgs) -> Vec<(Option<String>, String)> {
    args.features
        .iter()
        // #[gamma::skip(collection.reverse_array, reason = "split treats the character array as an unordered set of delimiters")]
        .flat_map(|entry| entry.split([',', ' ']))
        .filter(|entry| !entry.is_empty())
        .map(|entry| match entry.split_once('/') {
            Some((package, feature)) => (Some(package.to_owned()), feature.to_owned()),
            None => (None, entry.to_owned()),
        })
        .collect()
}

/// Returns the features a package starts with, before anything propagates.
fn seed(package: &Package, args: &FeatureArgs, named: &[(Option<String>, String)]) -> HashSet<String> {
    let mut on: HashSet<String> = HashSet::default();

    if args.all_features {
        on.extend(package.features.keys().cloned());

        return on;
    }

    if !args.no_default_features && package.features.contains_key("default") {
        let _added = on.insert("default".to_owned());
    }

    for (owner, feature) in named {
        // An unqualified name applies to whichever selected packages declare it, which is what
        // Cargo does for a workspace build. A qualified one names its package outright.
        let mine = owner.as_ref().is_none_or(|owner| owner == package.name.as_str());

        if mine && package.features.contains_key(feature) {
            let _added = on.insert(feature.clone());
        }
    }

    on
}

/// Runs worklist-driven propagation to a fixed point.
///
/// A changed package is queued again so newly activated optional dependencies and previously weak
/// forwarding entries are reconsidered together.
fn propagate_worklist(members: &[&Package], renames: &Renames, on: &mut HashMap<String, HashSet<String>>) {
    use std::collections::VecDeque;

    let packages: HashMap<&str, &Package> = members.iter().map(|package| (package.name.as_str(), *package)).collect();
    let mut queue: VecDeque<String> = members.iter().map(|package| package.name.to_string()).collect();
    let mut queued: HashSet<String> = queue.iter().cloned().collect();
    while let Some(package_name) = queue.pop_front() {
        let _removed = queued.remove(&package_name);
        let package = packages
            .get(package_name.as_str())
            .copied()
            .expect("the worklist only receives workspace package names");
        let mine = on.get(&package_name).cloned().unwrap_or_default();
        let mut changed = Vec::new();
        for feature in &mine {
            let Some(entries) = package.features.get(feature) else { continue };
            for entry in entries {
                if !forwarding_is_active(package, entry, &mine) {
                    continue;
                }
                if let Some((pkg, _feature)) = apply_worklist(entry, &package_name, renames, on) {
                    changed.push(pkg);
                }
            }
        }
        for dependency in &package.dependencies {
            if matches!(dependency.kind, DependencyKind::Unknown) || !dependency_is_active(package, dependency, &mine) {
                continue;
            }
            let target = dependency.name.as_str();
            for feature in dependency
                .features
                .iter()
                .map(String::as_str)
                .chain(dependency.uses_default_features.then_some("default"))
            {
                if turn_on(target, feature, on) {
                    changed.push(target.to_owned());
                }
            }
        }
        for package in changed {
            if
            // #[gamma::skip(cond.always_true, reason = "queued.insert guards only duplicate worklist admission. changed is emitted only for genuinely new feature insertions into finite HashSets. Always queueing still terminates after finitely many such insertions (possibly duplicate package visits); monotone fixed-point feature sets are identical. No callback/output exposes queue order/count.", tag = "equivalent")]
            queued.insert(package.clone()) {
                queue.push_back(package);
            }
        }
    }
}

fn dependency_is_active(package: &Package, dependency: &cargo_metadata::Dependency, mine: &HashSet<String>) -> bool {
    let alias = dependency.rename.as_deref().unwrap_or(dependency.name.as_str());
    !dependency.optional
        || mine.contains(alias)
        || mine
            .iter()
            .filter_map(|feature| package.features.get(feature))
            .flatten()
            .any(|entry| entry.strip_prefix("dep:") == Some(alias) || entry.split_once('/').is_some_and(|(token, _feature)| token == alias))
}

fn forwarding_is_active(package: &Package, entry: &str, mine: &HashSet<String>) -> bool {
    let Some((token, _feature)) = entry.split_once('/') else {
        return true;
    };
    let Some(alias) = token.strip_suffix('?') else { return true };
    package.dependencies.iter().any(|dependency| {
        dependency.rename.as_deref().unwrap_or(dependency.name.as_str()) == alias && dependency_is_active(package, dependency, mine)
    })
}

/// Finds packages whose dependency features remain uncertain.
///
/// Conditional, unknown-kind, and unselected workspace edges seed uncertainty.
/// It propagates through outgoing workspace dependencies.
pub(crate) fn uncertain_packages(metadata: &Metadata, selected: &HashSet<String>) -> HashSet<String> {
    let members = metadata.workspace_packages();
    let mut uncertain: HashSet<&str> = members
        .iter()
        .flat_map(|package| package.dependencies.iter().map(move |dependency| (*package, dependency)))
        .filter(|(package, dependency)| {
            dependency.target.is_some() || matches!(dependency.kind, DependencyKind::Unknown) || !selected.contains(package.name.as_str())
        })
        .map(|(_package, dependency)| dependency.name.as_str())
        .collect();
    let mut pending = uncertain.iter().copied().collect::<Vec<_>>();
    while let Some(name) = pending.pop() {
        for dependency in members
            .iter()
            .filter(|package| package.name.as_str() == name)
            .flat_map(|package| &package.dependencies)
        {
            // #[gamma::skip(cond.always_true, cond.negate, reason = "only new names may be queued or dependency cycles repeat forever")]
            if uncertain.insert(dependency.name.as_str()) {
                pending.push(dependency.name.as_str());
            }
        }
    }
    uncertain.into_iter().map(str::to_owned).collect()
}

/// Applies one feature-table entry, returning the (package, feature) pair if something new was
/// turned on (for enqueueing).
fn apply_worklist(entry: &str, owner: &str, renames: &Renames, on: &mut HashMap<String, HashSet<String>>) -> Option<(String, String)> {
    if entry.starts_with("dep:") {
        return None;
    }

    match entry.split_once('/') {
        Some((token, feature)) => {
            let token = token.trim_end_matches('?');
            let package = renames
                .get(owner)
                .and_then(|by_alias| by_alias.get(token))
                .map_or(token, String::as_str);

            turn_on(package, feature, on).then(|| (package.to_owned(), feature.to_owned()))
        }

        None => turn_on(owner, entry, on).then(|| (owner.to_owned(), entry.to_owned())),
    }
}

/// Runs one propagation pass, returning whether anything changed.
///
/// Retained for the test that exercises cycle termination; the worklist above is what `enabled`
/// actually calls.
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
fn propagate(members: &[&Package], renames: &Renames, on: &mut HashMap<String, HashSet<String>>) -> bool {
    let mut changed = false;

    for package in members {
        let name = package.name.as_str();
        let mine = on.get(name).cloned().unwrap_or_default();

        for feature in &mine {
            let Some(entries) = package.features.get(feature) else {
                continue;
            };

            for entry in entries {
                if forwarding_is_active(package, entry, &mine) {
                    changed |= apply(entry, name, renames, on);
                }
            }
        }

        for dependency in &package.dependencies {
            let target = dependency.name.as_str();

            if !on.contains_key(target) {
                continue;
            }

            if matches!(dependency.kind, DependencyKind::Unknown) || !dependency_is_active(package, dependency, &mine) {
                continue;
            }

            for feature in &dependency.features {
                changed |= turn_on(target, feature, on);
            }

            if dependency.uses_default_features {
                changed |= turn_on(target, "default", on);
            }
        }
    }

    changed
}

/// For each member, the manifest-local dependency names that stand for another package.
type Renames = HashMap<String, HashMap<String, String>>;

/// Collects the dependency aliases each member's manifest declares.
///
/// `bruce = { package = "beta" }` lets the rest of that manifest — including its feature table —
/// call `beta` by the name `bruce`, and nothing outside the manifest knows the alias. The enabled
/// map is keyed by real package names, so a `bruce/y` entry has to be translated before it is
/// looked up or the feature it forwards lands nowhere and `beta`'s `#[cfg(feature = "y")]` code is
/// read as absent.
///
/// Only renamed dependencies are recorded, since every other token already is the package name.
fn renames(members: &[&Package]) -> Renames {
    members
        .iter()
        .filter_map(|package| {
            let mapped: HashMap<String, String> = package
                .dependencies
                .iter()
                .filter_map(|dependency| {
                    dependency
                        .rename
                        .as_ref()
                        .map(|alias| (alias.clone(), dependency.name.as_str().to_owned()))
                })
                .collect();

            (!mapped.is_empty()).then(|| (package.name.as_str().to_owned(), mapped))
        })
        .collect()
}

/// Applies one entry from a feature table, which may name this package's feature or another's.
///
/// The spellings are `plain`, `dep:some-crate`, `some-crate/feature` and `some-crate?/feature`.
/// Dependency activation is checked separately before applying these feature edges.
///
/// The token before the slash is whatever `owner`'s manifest calls the dependency, so it is
/// resolved through that manifest's renames before anything is looked up.
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
fn apply(entry: &str, owner: &str, renames: &Renames, on: &mut HashMap<String, HashSet<String>>) -> bool {
    if entry.starts_with("dep:") {
        return false;
    }

    match entry.split_once('/') {
        Some((token, feature)) => {
            let token = token.trim_end_matches('?');
            let package = renames
                .get(owner)
                .and_then(|by_alias| by_alias.get(token))
                .map_or(token, String::as_str);

            turn_on(package, feature, on)
        }

        None => turn_on(owner, entry, on),
    }
}

/// Turns a feature on for a package, returning whether that was news.
///
/// A package that is not a workspace member is ignored: its code is never mutated, so what it
/// compiles is not this tool's business.
// #[gamma::skip(fn_value.bool_true, tag = "timeout", reason = "reporting every feature as newly enabled prevents the feature propagation fixed point from terminating")]
fn turn_on(package: &str, feature: &str, on: &mut HashMap<String, HashSet<String>>) -> bool {
    on.get_mut(package).is_some_and(|features| features.insert(feature.to_owned()))
}

/// The feature selection `args` asks for, updated by selectors in the passthrough arguments.
///
/// `-C --all-features`, and a `cargo_args` entry in `gamma.toml` naming `--features`, reach the
/// cargo the run really invokes, so the build genuinely enables what they name. A closure computed
/// from the typed arguments alone has never heard of them: every `#[cfg(feature = "…")]` item they
/// turn on resolves absent and yields no mutants, and a `[[bin]]` whose `required-features` they
/// satisfy is judged unbuildable and leaves the population whole — a score raised by exactly the
/// code nobody measured.
///
/// The mining mirrors [`Build::resolve`](crate::cfg::Build::resolve)'s reading of `--target` and
/// `--profile` from the same vector, and covers the spellings cargo accepts: `--features x`,
/// `--features=x`, `-F x` and `-Fx`, plus the two flags that override the selection outright.
/// The result mirrors both enabling selectors and `--no-default-features`, which narrows the
/// selection.
#[must_use]
pub fn from_extra(args: &FeatureArgs, extra: &[String]) -> FeatureArgs {
    let mut widened = args.clone();
    let mut expecting = false;

    for argument in extra {
        if expecting {
            widened.features.push(argument.clone());
            expecting =
            // #[gamma::skip(assign_value.default, reason = "The assigned false is exactly the inferred primitive type's Default value.", tag = "equivalent")]
            false;

            continue;
        }

        match argument.as_str() {
            "--all-features" => widened.all_features = true,
            "--no-default-features" => widened.no_default_features = true,
            "--features" | "-F" => expecting = true,

            // The prefix has to be matched exactly before the `=`, or `--features-of-interest`
            // would be read as a selection; `-F` is the one spelling cargo also accepts attached.
            other => {
                if let Some(value) = other
                    .strip_prefix("--features=")
                    .or_else(|| other.strip_prefix("-F="))
                    .or_else(|| other.strip_prefix("-F"))
                {
                    widened.features.push(value.to_owned());
                }
            }
        }
    }

    widened
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::fs;

    use camino::Utf8PathBuf;
    use serde_json::{Value, from_str, from_value, to_value};
    use tempfile::TempDir;

    use super::*;
    use crate::discover::load_metadata;

    fn optional_metadata() -> Metadata {
        from_str(include_str!("../../tests/fixtures/correctness/optional-features.json")).unwrap()
    }

    #[test]
    fn recovered_missing_feature_entries_do_not_stop_other_selected_feature_edges() {
        let names: Vec<_> = (0..8).map(|index| format!("selected{index}")).collect();
        for missing in &names {
            let mut metadata = optional_metadata();
            for package in &mut metadata.packages {
                package.features.clear();
                package.dependencies.clear();
            }
            for name in names.iter().filter(|name| *name != missing) {
                metadata.packages[0].features.insert(name.clone(), vec![format!("beta/{name}")]);
            }
            let members: Vec<_> = metadata.packages.iter().collect();
            let mut on = HashMap::from_iter([
                ("alpha".to_owned(), names.iter().cloned().collect()),
                ("beta".to_owned(), HashSet::default()),
            ]);
            propagate_worklist(&members, &Renames::default(), &mut on);
            assert_eq!(
                on["beta"],
                names.iter().filter(|name| *name != missing).cloned().collect(),
                "missing entry {missing}"
            );
        }
    }

    #[test]
    fn recovered_uncertainty_worklist_traverses_beyond_the_first_new_dependency() {
        let mut metadata = optional_metadata();
        let mut gamma = metadata.packages[1].clone();
        gamma.name = from_str("\"gamma\"").unwrap();
        gamma.id.repr.push_str("-gamma");
        let mut delta = gamma.clone();
        delta.name = from_str("\"delta\"").unwrap();
        delta.id.repr.push_str("-delta");
        let mut edge = metadata.packages[0].dependencies[0].clone();
        edge.target = None;
        edge.kind = DependencyKind::Normal;
        edge.name = "delta".into();
        gamma.dependencies = vec![edge.clone()];
        delta.dependencies.clear();
        edge.name = "gamma".into();
        metadata.packages[1].dependencies = vec![edge];
        metadata.packages[0].dependencies[0].target = Some("cfg(custom)".parse().unwrap());
        metadata.workspace_members.extend([gamma.id.clone(), delta.id.clone()]);
        metadata.packages.extend([gamma, delta]);
        let selected = ["alpha", "beta", "gamma", "delta"].map(str::to_owned).into_iter().collect();
        assert_eq!(
            uncertain_packages(&metadata, &selected),
            ["beta", "gamma", "delta"].map(str::to_owned).into_iter().collect()
        );
    }

    #[test]
    fn recovered_optional_dependency_activation_accepts_only_the_selected_alias() {
        let mut metadata = optional_metadata();
        let package = &mut metadata.packages[0];
        package.dependencies[0].rename = Some("renamed".into());
        package.dependencies[0].optional = true;
        package.features.clear();
        package.features.insert("selected".into(), vec!["renamed/extra".into()]);
        package.features.insert("unrelated".into(), vec!["other/extra".into()]);
        let dependency = &package.dependencies[0];
        for (feature, expected) in [("selected", true), ("unrelated", false), ("absent", false)] {
            assert_eq!(
                dependency_is_active(package, dependency, &HashSet::from_iter([feature.to_owned()])),
                expected,
                "{feature}"
            );
        }
    }

    #[test]
    fn recovered_weak_forwarding_checks_one_matching_active_dependency() {
        let mut metadata = optional_metadata();
        let package = &mut metadata.packages[0];
        package.dependencies[0].rename = Some("chosen".into());
        package.dependencies[0].optional = true;
        let mut unrelated = package.dependencies[0].clone();
        unrelated.name = "other".into();
        unrelated.rename = None;
        unrelated.optional = false;
        package.dependencies.push(unrelated);
        for active in [false, true] {
            let mine = if active {
                HashSet::from_iter(["chosen".to_owned()])
            } else {
                HashSet::default()
            };
            assert_eq!(forwarding_is_active(package, "chosen?/extra", &mine), active);
            assert!(!forwarding_is_active(package, "missing?/extra", &mine));
        }
    }

    #[test]
    fn recovered_dependency_features_requeue_already_visited_packages() {
        let mut metadata = optional_metadata();
        let mut gamma = metadata.packages[1].clone();
        gamma.name = from_str("\"gamma\"").unwrap();
        gamma.dependencies.clear();
        gamma.features.clear();
        gamma.features.insert("deep".into(), Vec::new());
        metadata.packages[0].features.clear();
        metadata.packages[0].dependencies[0].optional = false;
        metadata.packages[0].dependencies[0].features = vec!["relay".into()];
        metadata.packages[0].dependencies[0].uses_default_features = false;
        let mut edge = metadata.packages[0].dependencies[0].clone();
        edge.name = "gamma".into();
        edge.rename = None;
        edge.features.clear();
        metadata.packages[1].dependencies = vec![edge];
        metadata.packages[1].features.clear();
        metadata.packages[1].features.insert("relay".into(), vec!["gamma/deep".into()]);
        let members = [&gamma, &metadata.packages[1], &metadata.packages[0]];
        let mut on = HashMap::from_iter(["alpha", "beta", "gamma"].map(|name| (name.to_owned(), HashSet::default())));
        propagate_worklist(&members, &renames(&members), &mut on);
        assert_eq!(on["alpha"], HashSet::default());
        assert_eq!(on["beta"], HashSet::from_iter(["relay".to_owned()]));
        assert_eq!(on["gamma"], HashSet::from_iter(["deep".to_owned()]));
    }

    #[test]
    fn inactive_optional_edges_do_not_enable_features_or_strip_negated_nested_cfg() {
        let metadata = optional_metadata();
        for active in [false, true] {
            let args = FeatureArgs {
                no_default_features: true,
                features: if active { vec!["alpha/activate".into()] } else { Vec::new() },
                ..FeatureArgs::default()
            };
            let enabled = enabled(&metadata, &args);
            assert_eq!(enabled["beta"].contains(&"extra".into()), active);
            assert_eq!(enabled["beta"].contains(&"default".into()), active);
            assert_eq!(enabled["beta"].contains(&"from_default".into()), active);
            assert_eq!(enabled, enabled_by_rescan(&metadata, &args));
            let cfgs = crate::cfg::Cfgs::new(&crate::cfg::CfgSet::parse("unix\n"), &enabled);
            let source = crate::parse::SourceFile::parse(
                "beta.rs",
                include_str!("../../tests/fixtures/correctness/optional-features.rs").to_owned(),
            )
            .unwrap();
            let candidates = crate::ops::collect::collect_in(
                &source,
                &crate::ops::Selection::parse("literal.int_to_zero").unwrap(),
                cfgs.for_package("beta"),
            );
            let originals: Vec<_> = candidates.iter().map(|candidate| source.slice(&candidate.span)).collect();
            assert_eq!(originals, if active { vec!["22", "44"] } else { vec!["11", "33"] });
        }
    }

    #[test]
    fn uncertain_feature_activation_preserves_both_negations_and_nested_cfg_controls() {
        let mut metadata = optional_metadata();
        metadata.packages[0].dependencies[0].target = Some("cfg(unknown_target)".parse().unwrap());
        let selected = HashSet::from_iter(["alpha".to_owned(), "beta".to_owned()]);
        let enabled = enabled(&metadata, &FeatureArgs::default());
        let cfgs = crate::cfg::Cfgs::new(&crate::cfg::CfgSet::parse("unix\n"), &enabled)
            .feature_uncertainty(&uncertain_packages(&metadata, &selected));
        let beta = cfgs.for_package("beta");
        for expression in [
            "feature = \"extra\"",
            "not(feature = \"extra\")",
            "all(unix, not(feature = \"extra\"))",
            "not(all(unix, feature = \"extra\"))",
        ] {
            assert!(beta.holds_str(expression), "{expression}");
        }
        assert!(!beta.holds_str("all(windows, feature = \"extra\")"));
        assert!(!beta.holds_str("not(any(unix, feature = \"extra\"))"));
    }

    #[test]
    fn uncertainty_worklist_visits_cyclic_and_same_named_workspace_dependencies() {
        let mut metadata = optional_metadata();
        let mut dependency = metadata.packages[0].dependencies[0].clone();
        dependency.name = "alpha".into();
        dependency.rename = None;
        metadata.packages[1].dependencies.push(dependency.clone());

        let mut duplicate = metadata.packages[1].clone();
        duplicate.id.repr.push_str("-duplicate");
        dependency.name = "leaf".into();
        duplicate.dependencies = vec![dependency];
        metadata.workspace_members.push(duplicate.id.clone());
        metadata.packages.push(duplicate);

        let selected = HashSet::from_iter(["alpha".to_owned(), "beta".to_owned()]);
        assert!(uncertain_packages(&metadata, &selected).is_empty());
        let expected = HashSet::from_iter(["alpha".to_owned(), "beta".to_owned(), "leaf".to_owned()]);
        for cause in 0..3 {
            let mut metadata = metadata.clone();
            let mut selected = selected.clone();
            match cause {
                0 => metadata.packages[0].dependencies[0].target = Some("cfg(unknown_target)".parse().unwrap()),
                1 => metadata.packages[0].dependencies[0].kind = DependencyKind::Unknown,
                _ => {
                    selected.remove("alpha");
                }
            }
            assert_eq!(uncertain_packages(&metadata, &selected), expected, "uncertainty cause {cause}");
        }
    }

    #[test]
    fn survivor_new_cross_package_features_return_the_work_item_that_drives_transitive_propagation() {
        let mut on: HashMap<String, HashSet<String>> = [("alpha".to_owned(), HashSet::default()), ("beta".to_owned(), HashSet::default())]
            .into_iter()
            .collect();
        let renames = Renames::default();
        assert_eq!(
            apply_worklist("beta/leaf", "alpha", &renames, &mut on),
            Some(("beta".to_owned(), "leaf".to_owned()))
        );
        assert!(on["beta"].contains("leaf"));
        assert!(
            apply_worklist("beta/leaf", "alpha", &renames, &mut on).is_none(),
            "an already-enabled feature must not be enqueued again"
        );
        assert_eq!(
            apply_worklist("ordinary", "beta", &renames, &mut on),
            Some(("beta".to_owned(), "ordinary".to_owned()))
        );
        assert!(on["beta"].contains("ordinary"));
    }

    /// Writes a workspace of manifests and gives back the metadata cargo reads from it.
    ///
    /// A `src/lib.rs` is written beside every manifest that declares a package, because a package
    /// with no target at all is not something cargo will describe.
    fn metadata_for(files: &[(&str, &str)]) -> (TempDir, Metadata) {
        let directory = TempDir::new().expect("a temporary directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is not UTF-8");

        crate::testing::write_project(root.as_std_path(), files);
        for (relative, contents) in files {
            let path = root.join(relative);

            if contents.contains("[package]") {
                let source = path.parent().expect("a manifest has a directory").join("src");

                fs::create_dir_all(source.as_std_path()).expect("a source directory");
                fs::write(source.join("lib.rs").as_std_path(), "pub fn f() {}\n").expect("a library root");
            }
        }

        let metadata = load_metadata(&root, &FeatureArgs::default()).expect("the fixture workspace has metadata");

        (directory, metadata)
    }

    /// The features enabled for `package` under `args`, as a sorted list.
    fn features_of(files: &[(&str, &str)], args: &FeatureArgs, package: &str) -> Vec<String> {
        let (_directory, metadata) = metadata_for(files);

        enabled(&metadata, args).remove(package).unwrap_or_default()
    }

    fn enabled_by_rescan(metadata: &Metadata, args: &FeatureArgs) -> HashMap<String, Vec<String>> {
        let members: Vec<&Package> = metadata.workspace_packages();
        let named = requested(args);
        let renames = renames(&members);
        let mut on: HashMap<String, HashSet<String>> = members
            .iter()
            .map(|package| (package.name.as_str().to_owned(), seed(package, args, &named)))
            .collect();

        while propagate(&members, &renames, &mut on) {}

        on.into_iter()
            .map(|(package, features)| {
                let mut features: Vec<_> = features.into_iter().collect();
                features.sort();
                (package, features)
            })
            .collect()
    }

    /// A single-package workspace whose library declares the given feature table.
    fn alone(table: &str) -> String {
        format!("[package]\nname = \"alpha\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[features]\n{table}\n\n[workspace]\n")
    }

    #[test]
    fn default_is_on_and_expands() {
        let manifest = alone("default = [\"std\"]\nstd = []\nstats = []\n");
        let found = features_of(&[("Cargo.toml", &manifest)], &FeatureArgs::default(), "alpha");

        assert_eq!(found, vec!["default".to_owned(), "std".to_owned()]);
    }

    #[test]
    fn worklist_matches_fixed_point_rescans() {
        let files = [
            ("Cargo.toml", "[workspace]\nmembers = [\"alpha\", \"beta\"]\nresolver = \"3\"\n"),
            (
                "alpha/Cargo.toml",
                "[package]\nname = \"alpha\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
                 [features]\ndefault = [\"cycle\", \"bruce/y\"]\ncycle = [\"default\"]\nextra = []\n\n\
                 [dependencies]\nbruce = { package = \"beta\", path = \"../beta\", features = [\"z\"] }\n",
            ),
            (
                "beta/Cargo.toml",
                "[package]\nname = \"beta\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
                 [features]\ndefault = [\"z\"]\ny = [\"z\"]\nz = []\n",
            ),
        ];
        let (_directory, metadata) = metadata_for(&files);

        for args in [
            FeatureArgs::default(),
            FeatureArgs {
                all_features: true,
                ..FeatureArgs::default()
            },
        ] {
            assert_eq!(enabled(&metadata, &args), enabled_by_rescan(&metadata, &args));
        }
    }

    #[test]
    fn feature_arguments_preserve_space_delimiters_qualified_names_and_option_values() {
        let args = FeatureArgs {
            features: vec!["one two,alpha/three".to_owned()],
            ..FeatureArgs::default()
        };
        assert_eq!(
            requested(&args),
            [
                (None, "one".to_owned()),
                (None, "two".to_owned()),
                (Some("alpha".to_owned()), "three".to_owned()),
            ]
        );
        for raw in [
            vec!["--features", "alpha/one", "--features", "two", "--all-features"],
            vec!["-F", "alpha/one", "-F=two", "--all-features"],
            vec!["--features=alpha/one", "-Ftwo", "--all-features"],
        ] {
            let extra: Vec<_> = raw.into_iter().map(str::to_owned).collect();
            let found = from_extra(&FeatureArgs::default(), &extra);
            assert_eq!(found.features, ["alpha/one", "two"]);
            assert!(found.all_features);
            assert!(!found.no_default_features);
        }
    }

    #[test]
    fn dependency_default_and_explicit_features_enqueue_transitive_entries() {
        let files = pair(
            "[features]\ndefault = []\n\n[dependencies]\nbeta = { path = \"../beta\", features = [\"explicit\"] }\n",
            "default = [\"from_default\"]\nfrom_default = [\"deep_default\"]\ndeep_default = []\n\
             explicit = [\"from_explicit\"]\nfrom_explicit = []\nforbidden = []\n",
        );
        let (_directory, mut metadata) = metadata_for(&borrowed(&files));
        let alpha = metadata
            .packages
            .iter_mut()
            .find(|package| package.name.as_str() == "alpha")
            .expect("the fixture declares alpha");
        let mut unknown = alpha.dependencies[0].clone();
        unknown.kind = DependencyKind::Unknown;
        unknown.features = vec!["forbidden".to_owned()];
        unknown.uses_default_features = false;
        alpha.dependencies.insert(0, unknown);
        let args = FeatureArgs {
            no_default_features: true,
            ..FeatureArgs::default()
        };
        for uses_default in [true, false] {
            metadata
                .packages
                .iter_mut()
                .find(|package| package.name.as_str() == "alpha")
                .expect("the fixture declares alpha")
                .dependencies
                .last_mut()
                .expect("the fixture has one known dependency after the unknown dependency")
                .uses_default_features = uses_default;
            let expected: &[&str] = if uses_default {
                &["deep_default", "default", "explicit", "from_default", "from_explicit"]
            } else {
                &["explicit", "from_explicit"]
            };
            assert_eq!(enabled(&metadata, &args)["beta"], expected, "uses_default = {uses_default}");
        }
    }

    #[test]
    fn no_default_features_leaves_nothing_on() {
        let manifest = alone("default = [\"std\"]\nstd = []\n");
        let args = FeatureArgs {
            no_default_features: true,
            ..FeatureArgs::default()
        };

        assert!(features_of(&[("Cargo.toml", &manifest)], &args, "alpha").is_empty());
    }

    #[test]
    fn all_features_turns_on_everything_declared() {
        let manifest = alone("default = [\"std\"]\nstd = []\nstats = []\n");
        let args = FeatureArgs {
            all_features: true,
            ..FeatureArgs::default()
        };
        let found = features_of(&[("Cargo.toml", &manifest)], &args, "alpha");

        assert_eq!(found, vec!["default".to_owned(), "stats".to_owned(), "std".to_owned()]);
    }

    #[test]
    fn a_named_feature_is_turned_on() {
        let manifest = alone("default = [\"std\"]\nstd = []\nstats = []\n");
        let args = FeatureArgs {
            features: vec!["stats".to_owned()],
            ..FeatureArgs::default()
        };

        assert!(features_of(&[("Cargo.toml", &manifest)], &args, "alpha").contains(&"stats".to_owned()));
    }

    #[test]
    fn several_named_features_may_share_one_argument() {
        // Cargo accepts `--features a,b`, so a value that was never split would name no feature at
        // all and quietly leave both off.
        let manifest = alone("a = []\nb = []\nc = []\n");
        let args = FeatureArgs {
            features: vec!["a,b".to_owned()],
            ..FeatureArgs::default()
        };
        let found = features_of(&[("Cargo.toml", &manifest)], &args, "alpha");

        assert_eq!(found, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn a_feature_that_is_not_declared_is_not_invented() {
        let manifest = alone("a = []\n");
        let args = FeatureArgs {
            features: vec!["nope".to_owned()],
            ..FeatureArgs::default()
        };

        assert!(features_of(&[("Cargo.toml", &manifest)], &args, "alpha").is_empty());
    }

    #[test]
    fn a_qualified_name_only_reaches_its_own_package() {
        let manifest = alone("a = []\n");
        let args = FeatureArgs {
            features: vec!["beta/a".to_owned()],
            ..FeatureArgs::default()
        };

        assert!(features_of(&[("Cargo.toml", &manifest)], &args, "alpha").is_empty());
    }

    #[test]
    fn features_chain_through_their_own_entries() {
        let manifest = alone("default = [\"a\"]\na = [\"b\"]\nb = [\"c\"]\nc = []\n");
        let found = features_of(&[("Cargo.toml", &manifest)], &FeatureArgs::default(), "alpha");

        assert_eq!(found, vec!["a".to_owned(), "b".to_owned(), "c".to_owned(), "default".to_owned()]);
    }

    #[test]
    fn a_cycle_between_features_terminates() {
        // A malformed manifest must not hang discovery.
        let manifest = alone("default = [\"a\"]\na = [\"b\"]\nb = [\"a\"]\n");
        let found = features_of(&[("Cargo.toml", &manifest)], &FeatureArgs::default(), "alpha");

        assert!(found.contains(&"a".to_owned()), "{found:?}");
        assert!(found.contains(&"b".to_owned()), "{found:?}");
    }

    #[test]
    fn a_dep_entry_names_no_feature() {
        // `dep:beta` switches on an optional dependency. Reading it as a feature named `dep:beta`
        // would put a name in the set that no `#[cfg]` can ever spell.
        let files = pair(
            "[features]\ndefault = [\"dep:beta\"]\n\n[dependencies]\nbeta = { path = \"../beta\", optional = true, default-features = false }\n",
            "loud = []\n",
        );

        let found = features_of(&borrowed(&files), &FeatureArgs::default(), "alpha");

        assert_eq!(found, vec!["default".to_owned()], "`dep:` activates a crate, not a feature");
        assert!(features_of(&borrowed(&files), &FeatureArgs::default(), "beta").is_empty());
    }

    /// A two-member workspace, where `alpha` relates to `beta` however the caller says.
    fn pair(alpha_extra: &str, beta_features: &str) -> Vec<(String, String)> {
        vec![
            (
                "Cargo.toml".to_owned(),
                "[workspace]\nmembers = [\"alpha\", \"beta\"]\nresolver = \"3\"\n".to_owned(),
            ),
            (
                "alpha/Cargo.toml".to_owned(),
                format!("[package]\nname = \"alpha\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n{alpha_extra}"),
            ),
            (
                "beta/Cargo.toml".to_owned(),
                format!("[package]\nname = \"beta\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[features]\n{beta_features}"),
            ),
        ]
    }

    /// Borrows an owned fixture into the shape the loader wants.
    fn borrowed(files: &[(String, String)]) -> Vec<(&str, &str)> {
        files.iter().map(|(path, text)| (path.as_str(), text.as_str())).collect()
    }

    #[test]
    fn one_member_can_turn_on_anothers_feature() {
        let files = pair(
            "[features]\ndefault = [\"beta/loud\"]\n\n[dependencies]\nbeta = { path = \"../beta\", default-features = false }\n",
            "loud = []\nquiet = []\n",
        );

        let found = features_of(&borrowed(&files), &FeatureArgs::default(), "beta");

        assert_eq!(found, vec!["loud".to_owned()]);
    }

    #[test]
    fn a_dependency_declaration_contributes_its_features() {
        let files = pair(
            "[dependencies]\nbeta = { path = \"../beta\", features = [\"loud\"] }\n",
            "default = [\"quiet\"]\nloud = []\nquiet = []\n",
        );

        let found = features_of(&borrowed(&files), &FeatureArgs::default(), "beta");

        assert!(found.contains(&"loud".to_owned()), "{found:?}");
        assert!(found.contains(&"quiet".to_owned()), "the default came through too: {found:?}");
    }

    /// A manifest may call a dependency whatever it likes — `bruce = { package = "beta" }` — and
    /// everything in that manifest, its feature table included, then speaks of `bruce`. The map
    /// being resolved is keyed by real package names, so a `bruce/y` entry that was not translated
    /// landed nowhere and `beta`'s `#[cfg(feature = "y")]` code was read as absent, taking its live
    /// mutants out of the population.
    #[test]
    fn a_renamed_dependency_forwards_its_features_to_the_package_it_names() {
        let files = pair(
            "[features]\ndefault = [\"bruce/y\"]\n\n\
             [dependencies]\nbruce = { package = \"beta\", path = \"../beta\", default-features = false }\n",
            "y = []\nz = []\n",
        );

        let found = features_of(&borrowed(&files), &FeatureArgs::default(), "beta");

        assert_eq!(found, vec!["y".to_owned()], "{found:?}");
    }

    #[test]
    fn a_weak_reference_through_an_alias_requires_dependency_activation() {
        let mut metadata = optional_metadata();
        metadata.packages[0].features.insert("default".into(), vec!["bruce?/y".into()]);
        metadata.packages[0].dependencies[0].uses_default_features = false;
        metadata.packages[0].dependencies[0].features.clear();
        metadata.packages[1].features.clear();
        metadata.packages[1].features.insert("y".into(), Vec::new());
        assert!(enabled(&metadata, &FeatureArgs::default())["beta"].is_empty());
        metadata.packages[0].features.get_mut("default").unwrap().push("activate".into());
        assert_eq!(enabled(&metadata, &FeatureArgs::default())["beta"], ["y"]);
    }

    /// An alias is local to the manifest that declares it, so a name another member happens to use
    /// for something else must not be translated by it — that would turn a feature on for a
    /// package the entry never named.
    #[test]
    fn an_alias_does_not_reach_beyond_the_manifest_that_declares_it() {
        let files = [
            (
                "Cargo.toml".to_owned(),
                "[workspace]\nmembers = [\"alpha\", \"beta\", \"bruce\"]\nresolver = \"3\"\n".to_owned(),
            ),
            (
                "alpha/Cargo.toml".to_owned(),
                "[package]\nname = \"alpha\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
                 [features]\ndefault = [\"bruce/y\"]\n\n\
                 [dependencies]\nbruce = { path = \"../bruce\", default-features = false }\n"
                    .to_owned(),
            ),
            (
                "beta/Cargo.toml".to_owned(),
                "[package]\nname = \"beta\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[features]\ny = []\n".to_owned(),
            ),
            (
                "bruce/Cargo.toml".to_owned(),
                "[package]\nname = \"bruce\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[features]\ny = []\n".to_owned(),
            ),
        ];

        let found = features_of(&borrowed(&files), &FeatureArgs::default(), "beta");

        assert!(found.is_empty(), "the entry named the crate called bruce, not beta: {found:?}");
        assert_eq!(
            features_of(&borrowed(&files), &FeatureArgs::default(), "bruce"),
            vec!["y".to_owned()]
        );
    }

    #[test]
    fn a_dev_dependency_counts_because_the_schema_is_built_with_cargo_test() {
        let files = pair(
            "[dev-dependencies]\nbeta = { path = \"../beta\", features = [\"loud\"], default-features = false }\n",
            "loud = []\nquiet = []\n",
        );

        let found = features_of(&borrowed(&files), &FeatureArgs::default(), "beta");

        assert_eq!(found, vec!["loud".to_owned()]);
    }

    #[test]
    fn a_feature_of_a_package_outside_the_workspace_is_ignored() {
        // Only members are mutated, so what a dependency outside the workspace compiles is not
        // this tool's business, and naming one must not invent an entry for it.
        let manifest = alone("default = [\"a\"]\na = []\n");
        let (_directory, metadata) = metadata_for(&[("Cargo.toml", &manifest)]);
        let found = enabled(&metadata, &FeatureArgs::default());

        assert_eq!(found.len(), 1, "only the one member is described: {found:?}");
    }

    /// A `some-crate/feature` reference is turned on for the named crate the moment it is written,
    /// before anything checks whether that crate actually declares the feature. Propagation must
    /// still tolerate the mismatch afterwards rather than panicking, or crediting the package with
    /// a feature nothing in its own manifest ever named.
    #[test]
    fn a_member_feature_reference_to_an_undeclared_feature_does_not_invent_downstream_entries() {
        let files = pair(
            "[features]\ndefault = [\"beta/ghost\"]\n\n[dependencies]\nbeta = { path = \"../beta\", default-features = false }\n",
            "loud = []\n",
        );

        let found = features_of(&borrowed(&files), &FeatureArgs::default(), "beta");

        // The reference switched the name on for `beta`, but nothing there declares `ghost`, so no
        // further features chain off it and the set stays exactly what was named.
        assert_eq!(found, vec!["ghost".to_owned()], "{found:?}");
    }

    /// A dependency on a crate outside the workspace has no entry in the feature map at all, since
    /// only members are tracked; the propagation pass has to recognise that and move on rather
    /// than fabricate an entry or panic reaching for one that was never created.
    #[test]
    fn a_dependency_outside_the_workspace_contributes_nothing_to_propagate() {
        let files = [
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"alpha\"]\nexclude = [\"external\"]\nresolver = \"3\"\n",
            ),
            (
                "alpha/Cargo.toml",
                "[package]\nname = \"alpha\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
                 [features]\ndefault = []\n\n\
                 [dependencies]\nexternal = { path = \"../external\", features = [\"derive\"] }\n",
            ),
            (
                "external/Cargo.toml",
                // Excluded from the workspace above and carrying its own `[workspace]` table, this
                // is outside the tool's business entirely, exactly like a registry dependency
                // would be — and outside the feature map, which only ever tracks members.
                "[package]\nname = \"external\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
                 [features]\nderive = []\n\n[workspace]\n",
            ),
        ];

        let found = features_of(&files, &FeatureArgs::default(), "alpha");

        assert_eq!(found, vec!["default".to_owned()], "{found:?}");
    }

    /// Cargo never actually emits `DependencyKind::Unknown` — it is a `#[serde(other)]` catch-all
    /// for a variant this build of `cargo_metadata` does not recognise, kept forward-compatible
    /// against a future cargo. A dependency of that kind must still be inert here rather than
    /// having its features silently propagate, which is the whole reason the check exists at all.
    #[test]
    fn a_dependency_of_an_unrecognised_kind_does_not_propagate_its_features() {
        let files = pair(
            "[dependencies]\nbeta = { path = \"../beta\", features = [\"loud\"] }\n",
            "loud = []\n",
        );

        let (_directory, metadata) = metadata_for(&borrowed(&files));

        // Round-trip the real metadata through JSON, forging the one field that real cargo never
        // produces, so the dependency in `alpha` is now of a kind this module cannot recognise.
        let mut value = to_value(&metadata).expect("metadata serialises");
        let packages = value.get_mut("packages").and_then(Value::as_array_mut).expect("packages array");
        let alpha = packages
            .iter_mut()
            .find(|package| package["name"] == "alpha")
            .expect("the fixture always declares an alpha package");
        let dependency = alpha["dependencies"]
            .as_array_mut()
            .expect("alpha has dependencies")
            .first_mut()
            .expect("alpha declares exactly one dependency");
        dependency["kind"] = Value::String("build-tool".to_owned());

        let forged: Metadata = from_value(value).expect("the forged metadata still deserialises");
        let found = enabled(&forged, &FeatureArgs::default()).remove("beta").unwrap_or_default();

        assert!(!found.contains(&"loud".to_owned()), "{found:?}");
    }

    #[test]
    fn every_member_appears_even_with_no_features() {
        let manifest = alone("");
        let (_directory, metadata) = metadata_for(&[("Cargo.toml", &manifest)]);
        let found = enabled(&metadata, &FeatureArgs::default());

        assert!(found.contains_key("alpha"), "a missing package would be left unconditional");
        assert!(found["alpha"].is_empty());
    }

    /// Every spelling cargo itself accepts, because the vector is passed to cargo verbatim: a
    /// spelling this misses is a feature the build has and the closure does not.
    #[test]
    fn a_passthrough_selector_is_read_however_it_is_written() {
        let extra = |arguments: &[&str]| {
            let owned: Vec<String> = arguments.iter().map(|argument| (*argument).to_owned()).collect();

            from_extra(&FeatureArgs::default(), &owned)
        };

        assert_eq!(extra(&["--features", "cli"]).features, vec!["cli".to_owned()]);
        assert_eq!(extra(&["--features=cli,extra"]).features, vec!["cli,extra".to_owned()]);
        assert_eq!(extra(&["-F", "cli"]).features, vec!["cli".to_owned()]);
        assert_eq!(extra(&["-Fcli"]).features, vec!["cli".to_owned()]);
        assert!(extra(&["--all-features"]).all_features);
        assert!(extra(&["--no-default-features"]).no_default_features);
    }

    /// The prefix is matched exactly before the `=`, or an unrelated flag that merely starts the
    /// same way would name a feature nobody asked for and turn on code the build does not compile.
    #[test]
    fn an_argument_that_merely_begins_like_a_selector_names_no_feature() {
        let extra = vec!["--features-of-interest=cli".to_owned(), "--target".to_owned(), "wasm32".to_owned()];
        let widened = from_extra(&FeatureArgs::default(), &extra);

        assert!(widened.features.is_empty(), "{:?}", widened.features);
        assert!(!widened.all_features);
    }

    /// The typed arguments are widened, never replaced: both routes reach the same cargo, so what
    /// either of them names is on.
    #[test]
    fn a_passthrough_selector_adds_to_the_typed_one() {
        let args = FeatureArgs {
            features: vec!["typed".to_owned()],
            ..FeatureArgs::default()
        };
        let widened = from_extra(&args, &["--features".to_owned(), "passed".to_owned()]);

        assert_eq!(widened.features, vec!["typed".to_owned(), "passed".to_owned()]);
    }
}
#[test]
#[cfg_attr(coverage_nightly, coverage(off))]
fn survivor_feature_values_that_look_like_flags_are_consumed_as_values() {
    let extra = ["--features", "--all-features", "-F", "--no-default-features", "--features=ordinary"].map(str::to_owned);
    let features = from_extra(&FeatureArgs::default(), &extra);
    assert_eq!(features.features, ["--all-features", "--no-default-features", "ordinary"]);
    assert!(!features.all_features);
    assert!(!features.no_default_features);
}
#[test]
#[cfg_attr(coverage_nightly, coverage(off))]
fn survivor_external_dependencies_do_not_stop_later_workspace_dependency_feature_propagation() {
    use serde_json::json;

    let dependency = |name: &str, features: &[&str]| {
        json!({
            "name": name, "source": null, "req": "*", "kind": null, "rename": null,
            "optional": false, "uses_default_features": false, "features": features,
            "target": null, "registry": null
        })
    };
    let package = |name: &str, dependencies, features| {
        json!({
            "name": name, "version": "0.0.0", "id": name, "source": null,
            "dependencies": dependencies, "targets": [], "features": features,
            "manifest_path": format!("workspace/{name}/Cargo.toml"),
            "edition": "2024"
        })
    };
    let metadata: Metadata = serde_json::from_value(json!({
        "packages": [
            package("alpha", vec![dependency("aaa_external", &[]), dependency("beta", &["leaf"])], json!({})),
            package("beta", vec![], json!({"leaf": ["deep"], "deep": []})),
        ],
        "workspace_members": ["alpha", "beta"], "workspace_default_members": ["alpha", "beta"],
        "workspace_root": "workspace", "target_directory": "workspace/target",
        "resolve": null, "metadata": null, "version": 1
    }))
    .expect("controlled metadata matches Cargo's schema");
    let alpha = metadata
        .workspace_packages()
        .into_iter()
        .find(|package| package.name.as_str() == "alpha")
        .expect("alpha is a workspace member");
    assert_eq!(alpha.dependencies[0].name, "aaa_external");
    assert_eq!(alpha.dependencies[1].name, "beta");
    let enabled = enabled(
        &metadata,
        &FeatureArgs {
            no_default_features: true,
            ..FeatureArgs::default()
        },
    );
    assert_eq!(enabled["beta"], ["deep", "leaf"]);
    assert!(
        !enabled.contains_key("aaa_external"),
        "only workspace members belong in the mutation feature map"
    );
}
