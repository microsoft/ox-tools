// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The `justfiles/anvil/` recipe tree and the `Justfile` imports region.
//!
//! Holds the embedded templates for the owned `.just` files and the registry
//! functions that wrap them as [`Artifact`]s. The engine resolves the on-disk
//! casing of every path generically, so `Justfile` / `justfile` need no
//! special handling here.
//!
//! See [`local.md`](../../../docs/design/local.md) for the recipe surface.

use crate::catalog::Artifact;

/// Contents of `justfiles/anvil/mod.just` baked into the binary.
///
/// This is the single-import entry point: it pulls in the sibling recipe
/// files and defines `alias anvil := anvil-pr`.
const MOD_JUST: &str = include_str!("../../../templates/justfiles/anvil/mod.just");

/// Repo-root-relative path of the entry-point recipe file.
const MOD_JUST_PATH: &str = "justfiles/anvil/mod.just";

/// Contents of `justfiles/anvil/versions.just` baked into the binary.
const VERSIONS_JUST: &str = include_str!("../../../templates/justfiles/anvil/versions.just");

/// Repo-root-relative path of the pinned-versions recipe file.
const VERSIONS_JUST_PATH: &str = "justfiles/anvil/versions.just";

/// Contents of `justfiles/anvil/tools.just` baked into the binary.
const TOOLS_JUST: &str = include_str!("../../../templates/justfiles/anvil/tools.just");

/// Repo-root-relative path of the tools recipe file.
const TOOLS_JUST_PATH: &str = "justfiles/anvil/tools.just";

/// Contents of `justfiles/anvil/helpers.just` baked into the binary.
///
/// Holds the shared helper recipe `_anvil-base-ref` (reused by the impact
/// recipe and anvil-mutants-diff) and the bucket legend documenting how
/// per-check recipes consume the impact cache via `_anvil-impact-include`
/// (which, along with cache production and `_anvil-impact-format`, lives in
/// `impact.just`).
const HELPERS_JUST: &str = include_str!("../../../templates/justfiles/anvil/helpers.just");

/// Repo-root-relative path of the shared-helpers recipe file.
const HELPERS_JUST_PATH: &str = "justfiles/anvil/helpers.just";

/// Contents of `justfiles/anvil/impact.just` baked into the binary.
///
/// The single `anvil-impact` recipe: it snapshots the base ref and the
/// working tree (two independent cache keys), computes the cargo-delta
/// impact set, and writes the `target/anvil/impact/` artifacts that the
/// scoped checks consume via `_anvil-impact-include`. Also owns the tier
/// projection helper `_anvil-impact-format` (which lives here next to its
/// sole caller). The same recipe runs locally and in cloud workflows (which
/// just share the artifacts).
const IMPACT_JUST: &str = include_str!("../../../templates/justfiles/anvil/impact.just");

/// Repo-root-relative path of the impact recipe file.
const IMPACT_JUST_PATH: &str = "justfiles/anvil/impact.just";

/// Contents of `justfiles/anvil/runner.just` baked into the binary.
const RUNNER_JUST: &str = include_str!("../../../templates/justfiles/anvil/runner.just");

/// Repo-root-relative path of the tier execution router.
const RUNNER_JUST_PATH: &str = "justfiles/anvil/runner.just";

/// Emits `(path, include_str!)` pairs for a set of split recipe files that
/// live under a subdirectory of `justfiles/anvil/`. Each file is one owned
/// artifact, so the recipe tree is one file per check / per group rather
/// than a single monolithic `checks.just` / `groups.just`.
macro_rules! split_recipe_files {
    ($subdir:literal, [$($name:literal),* $(,)?]) => {
        &[$(
            (
                concat!("justfiles/anvil/", $subdir, "/", $name, ".just"),
                include_str!(concat!("../../../templates/justfiles/anvil/", $subdir, "/", $name, ".just")),
            ),
        )*]
    };
}

#[test]
fn runner_routes_tiers_and_guards_recursion() {
    assert!(RUNNER_JUST.contains("[windows]"));
    assert!(RUNNER_JUST.contains("[script(\"pwsh\", \"-NoProfile\")]"));
    assert!(RUNNER_JUST.contains("[unix]"));
    assert!(RUNNER_JUST.contains("[script(\"bash\")]"));
    assert_eq!(RUNNER_JUST.matches("[no-exit-message]").count(), 2);
    assert!(RUNNER_JUST.contains("if ($env:ANVIL_IN_CONTAINER)"));
    assert!(RUNNER_JUST.contains("if [[ -n \"${ANVIL_IN_CONTAINER:-}\" ]]"));
    assert!(RUNNER_JUST.contains("replace(just_executable(), \"'\", \"''\")"));
    assert!(RUNNER_JUST.contains("replace(justfile(), \"'\", \"''\")"));
    assert!(RUNNER_JUST.contains("replace(tier, \"'\", \"''\")"));
    assert!(RUNNER_JUST.contains("replace(runner, \"'\", \"''\")"));
    assert!(RUNNER_JUST.contains("& $just --justfile $justfile anvil-container $nativeTier"));
    assert!(RUNNER_JUST.contains("exec \"$just_path\" --justfile \"$justfile\" anvil-container \"$native_tier\""));
    assert_eq!(RUNNER_JUST.matches("expected 'native' or 'container'").count(), 2);
}

#[test]
fn aprz_uses_the_container_secret_and_fails_fast_without_it() {
    let aprz = CHECK_FILES
        .iter()
        .find_map(|(path, body)| path.ends_with("/aprz.just").then_some(*body))
        .expect("aprz.just is registered in CHECK_FILES below");
    assert!(aprz.contains("if ($env:ANVIL_IN_CONTAINER)"));
    assert!(aprz.contains("ANVIL_APRZ_ALREADY_RAN"));
    assert!(aprz.contains("/run/secrets/anvil-github-token"));
    assert!(aprz.contains("Run `gh auth login` on the host"));
}

/// One `justfiles/anvil/checks/<check>.just` file per catalog check
/// (the check recipe plus its paired `*-setup` / `*-validate-prereqs`).
const CHECK_FILES: &[(&str, &str)] = split_recipe_files!(
    "checks",
    [
        "aprz",
        "audit",
        "bench",
        "bolero",
        "careful",
        "cargo-hack",
        "cargo-sort",
        "clippy",
        "deny",
        "doc-build",
        "doc-test",
        "ensure-no-cyclic-deps",
        "ensure-no-default-features",
        "examples",
        "external-types",
        "fmt",
        "license-headers",
        "llvm-cov",
        "loom",
        "miri",
        "miri-race-coverage",
        "miri-strict-provenance",
        "miri-tree-borrows",
        "mutants-diff",
        "mutants-full",
        "pr-title",
        "readme-check",
        "semver-check",
        "spellcheck",
        "udeps",
    ]
);

/// One `justfiles/anvil/groups/<group>.just` file per group (the group
/// recipe plus its paired `*-setup` / `*-validate-prereqs`).
const GROUP_FILES: &[(&str, &str)] = split_recipe_files!(
    "groups",
    [
        "pr-fast",
        "pr-slow",
        "pr-test",
        "pr-runtime-analysis",
        "pr-mutants",
        "scheduled-test",
        "scheduled-advisories",
        "scheduled-runtime-analysis",
        "scheduled-exhaustive",
    ]
);

/// Contents of `justfiles/anvil/tiers.just` baked into the binary.
const TIERS_JUST: &str = include_str!("../../../templates/justfiles/anvil/tiers.just");

/// Repo-root-relative path of the tier aggregator file.
const TIERS_JUST_PATH: &str = "justfiles/anvil/tiers.just";

#[cfg(test)]
pub(crate) fn dependency_recipe_sources() -> impl Iterator<Item = &'static str> {
    std::iter::once(TIERS_JUST).chain(GROUP_FILES.iter().map(|(_, body)| *body))
}

/// Embedded body of the `anvil-imports` region in the user's Justfile.
pub(crate) const JUSTFILE_IMPORTS_BODY: &str = include_str!("../../../templates/regions/justfile-imports.just");

/// Region id for the imports block in the user's `Justfile`.
pub(crate) const JUSTFILE_REGION_ID: &str = "anvil-imports";

/// Canonical repo-root-relative path of the user's `Justfile`.
///
/// Capitalized to match the dominant Unix convention for repo-root
/// build-config files (`Makefile`, `Dockerfile`, `Rakefile`, ...). `just`
/// accepts either case, and the engine reuses whatever casing a repo already
/// has on disk.
pub(crate) const JUSTFILE_PATH: &str = "Justfile";

/// `justfiles/anvil/mod.just` — the single-import entry point.
#[must_use]
pub fn entry() -> Artifact {
    Artifact::owned_file(MOD_JUST_PATH, MOD_JUST)
}

/// `justfiles/anvil/versions.just` — pinned toolchain versions.
#[must_use]
pub fn versions() -> Artifact {
    Artifact::owned_file(VERSIONS_JUST_PATH, VERSIONS_JUST)
}

/// `justfiles/anvil/tools.just` — tool install / prereq recipes.
#[must_use]
pub fn tools() -> Artifact {
    Artifact::owned_file(TOOLS_JUST_PATH, TOOLS_JUST)
}

/// `justfiles/anvil/helpers.just` — shared helper recipes + impact contract.
#[must_use]
pub fn helpers() -> Artifact {
    Artifact::owned_file(HELPERS_JUST_PATH, HELPERS_JUST)
}

/// `justfiles/anvil/impact.just` — the shared `anvil-impact` recipe.
#[must_use]
pub fn impact() -> Artifact {
    Artifact::owned_file(IMPACT_JUST_PATH, IMPACT_JUST)
}

/// `justfiles/anvil/runner.just` — native/container tier routing.
#[must_use]
pub fn runner() -> Artifact {
    Artifact::owned_file(RUNNER_JUST_PATH, RUNNER_JUST)
}

/// The `justfiles/anvil/checks/<check>.just` files — one owned artifact
/// per catalog check.
#[must_use]
pub fn check_files() -> Vec<Artifact> {
    CHECK_FILES.iter().map(|&(path, body)| Artifact::owned_file(path, body)).collect()
}

/// The `justfiles/anvil/groups/<group>.just` files — one owned artifact
/// per group.
#[must_use]
pub fn group_files() -> Vec<Artifact> {
    GROUP_FILES.iter().map(|&(path, body)| Artifact::owned_file(path, body)).collect()
}

/// `justfiles/anvil/tiers.just` — the tier aggregators.
#[must_use]
pub fn tiers() -> Artifact {
    Artifact::owned_file(TIERS_JUST_PATH, TIERS_JUST)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// A check's impact-scoping policy: either unscoped (always runs the full
    /// workspace) or scoped to one cargo-delta impact category.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ImpactPolicy {
        Unscoped,
        Modified,
        Affected,
        Required,
    }

    impl ImpactPolicy {
        /// The `_anvil-impact-include` category argument this policy emits, or
        /// `None` when the check is unscoped and takes no impact dependency.
        fn category(self) -> Option<&'static str> {
            match self {
                Self::Unscoped => None,
                Self::Modified => Some("modified"),
                Self::Affected => Some("affected"),
                Self::Required => Some("required"),
            }
        }
    }

    /// The intended impact policy for every catalog check -- the canonical
    /// mapping `every_check_matches_its_declared_impact_policy` enforces
    /// against the emitted recipes. Keep in sync with the check mapping table
    /// in the design docs.
    const EXPECTED_CHECK_POLICY: &[(&str, ImpactPolicy)] = {
        use ImpactPolicy::{Affected, Modified, Required, Unscoped};
        &[
            ("aprz", Unscoped),
            ("audit", Unscoped),
            ("bench", Affected),
            ("bolero", Affected),
            ("careful", Affected),
            ("cargo-hack", Required),
            ("cargo-sort", Modified),
            ("clippy", Affected),
            ("deny", Unscoped),
            ("doc-build", Required),
            ("doc-test", Affected),
            ("ensure-no-cyclic-deps", Modified),
            ("ensure-no-default-features", Modified),
            ("examples", Affected),
            ("external-types", Affected),
            ("fmt", Modified),
            ("license-headers", Modified),
            ("llvm-cov", Affected),
            ("loom", Affected),
            ("miri", Affected),
            ("miri-race-coverage", Affected),
            ("miri-strict-provenance", Affected),
            ("miri-tree-borrows", Affected),
            ("mutants-diff", Affected),
            ("mutants-full", Unscoped),
            ("pr-title", Unscoped),
            // readme-check + spellcheck are unscoped: their inputs (workspace
            // README template, root .spelling dictionary) are repo-level files
            // cargo-delta does not map to a package, so scoping them would
            // silently skip a changed template/dictionary.
            ("readme-check", Unscoped),
            ("semver-check", Affected),
            ("spellcheck", Unscoped),
            ("udeps", Required),
        ]
    };

    #[test]
    fn tools_just_template_is_not_empty() {
        assert!(TOOLS_JUST.contains("anvil-tool-cargo-spellcheck-source-deps-check"));
        assert!(TOOLS_JUST.contains("anvil-tool-cargo-deny-install"));
        assert!(TOOLS_JUST.contains("anvil-tool-cargo-deny-validate-prereqs"));
        assert!(TOOLS_JUST.contains("anvil-component-default-clippy-install"));
        assert!(TOOLS_JUST.contains("anvil-toolchain-nightly-install"));
    }

    /// All `checks/*.just` bodies concatenated, for content assertions.
    fn all_check_bodies() -> String {
        CHECK_FILES.iter().map(|(_, b)| *b).collect::<Vec<_>>().join("\n")
    }

    /// All `groups/*.just` bodies concatenated, for content assertions.
    fn all_group_bodies() -> String {
        GROUP_FILES.iter().map(|(_, b)| *b).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn checks_just_template_includes_all_catalog_checks() {
        let checks = all_check_bodies();
        for needle in [
            "anvil-fmt:",
            "anvil-clippy:",
            "anvil-license-headers:",
            "anvil-pr-title:",
            "anvil-llvm-cov:",
            "anvil-doc-test:",
            "anvil-mutants-diff:",
            "anvil-miri:",
            "anvil-mutants-full:",
            "anvil-bench:",
        ] {
            assert!(checks.contains(needle), "checks tree missing recipe '{needle}'");
        }
    }

    #[test]
    fn checks_fail_closed_and_preserve_opted_out_tests() {
        let checks = all_check_bodies();
        for needle in [
            "anvil-bolero: target discovery failed",
            "anvil-readme-check: cargo metadata failed",
            "running tests without coverage for opted-out packages",
            "all affected packages opted out of coverage",
            "could not resolve the cargo-careful executable",
            "anvil-mutants-full: aarch64-pc-windows-msvc",
        ] {
            assert!(checks.contains(needle), "checks tree missing safety behavior '{needle}'");
        }
        for needle in ["Inconclusive comparisons", "an unbuildable baseline is not evidence"] {
            assert!(checks.contains(needle), "checks tree missing advisory behavior '{needle}'");
        }
        assert!(
            checks.contains("bolero list --profile release --package $packageName"),
            "bolero discovery must use the execution profile once per affected package"
        );
        assert!(
            !checks.contains("bolero list @bareArgs"),
            "bolero discovery must not pass repeated --package arguments"
        );
        assert!(!checks.contains("bolero list failed; assuming no targets"));
    }

    #[test]
    fn spellcheck_checks_source_prerequisites_before_source_builds() {
        assert!(
            TOOLS_JUST.contains("if ($sourcePrereq)"),
            "binstall compile strategy must only be disabled for tools with source prerequisites"
        );
        assert!(
            TOOLS_JUST.contains("$binstallArgs += @('--disable-strategies', 'compile')"),
            "binstall must not compile before Anvil checks source prerequisites"
        );
        assert!(
            TOOLS_JUST.contains("anvil-tool-cargo-spellcheck-source-deps-check"),
            "spellcheck installer must run libclang validation before source builds"
        );
        let checks = all_check_bodies();
        assert!(
            !checks.contains("anvil-spellcheck-setup installer=\"install\": anvil-tool-cargo-spellcheck-source-deps-check"),
            "spellcheck setup must not require libclang before binstall"
        );
        assert!(
            !TOOLS_JUST.contains("ANVIL_SPELLCHECK_SKIP_UNSUPPORTED_ARM64") && !checks.contains("ANVIL_SPELLCHECK_SKIP_UNSUPPORTED_ARM64"),
            "spellcheck must run normally on ARM64"
        );
    }

    #[test]
    fn each_check_file_defines_its_own_check_recipe() {
        // The file `checks/<name>.just` must define `anvil-<name>:` -- guards
        // against a mis-split that files a check's recipe under the wrong name.
        for (path, body) in CHECK_FILES {
            let stem = path
                .strip_prefix("justfiles/anvil/checks/")
                .and_then(|p| p.strip_suffix(".just"))
                .expect("check file path has the expected shape");
            let needle = format!("anvil-{stem}:");
            assert!(body.contains(&needle), "{path} must define '{needle}'");
        }
    }

    #[test]
    fn impact_recipe_is_defined_and_reuses_shared_helpers() {
        // The single impact building block: snapshot + compute + resolve.
        for needle in ["anvil-impact:", "_anvil-impact-snapshot:", "_anvil-impact-include tier:"] {
            assert!(IMPACT_JUST.contains(needle), "impact.just missing recipe '{needle}'");
        }
        // It orchestrates around the shared helpers rather than duplicating
        // base-ref resolution or the tier -> `--package` projection.
        for needle in ["_anvil-base-ref", "_anvil-impact-format", "cargo delta impact"] {
            assert!(IMPACT_JUST.contains(needle), "impact.just must use '{needle}'");
        }
        // The ANVIL_IMPACT=off escape hatch guards every entry point.
        assert!(
            IMPACT_JUST.contains("$env:ANVIL_IMPACT -eq 'off'"),
            "impact.just must honor ANVIL_IMPACT=off"
        );
    }

    #[test]
    fn every_check_matches_its_declared_impact_policy() {
        use std::collections::{BTreeMap, BTreeSet};

        // Single source of truth for each check's impact policy. The catalog
        // encodes the policy structurally -- an `_anvil-impact-include
        // <category>` call, or its absence for unscoped checks -- and this
        // table pins the intended value. Pinning the exact category per check
        // (rather than a bare count) makes a check silently changing category,
        // or gaining/losing scoping, fail here instead of slipping through.
        let expected: BTreeMap<&str, ImpactPolicy> = EXPECTED_CHECK_POLICY.iter().copied().collect();
        assert_eq!(
            expected.len(),
            EXPECTED_CHECK_POLICY.len(),
            "EXPECTED_CHECK_POLICY contains a duplicate check entry"
        );

        let mut seen = BTreeSet::new();
        for (path, body) in CHECK_FILES {
            let stem = path
                .strip_prefix("justfiles/anvil/checks/")
                .and_then(|p| p.strip_suffix(".just"))
                .expect("check file path has the expected shape");
            seen.insert(stem);
            let policy = *expected
                .get(stem)
                .unwrap_or_else(|| panic!("check '{stem}' is missing from EXPECTED_CHECK_POLICY; classify it explicitly"));

            // Parse the actual category calls, matched as whole tokens so
            // `_anvil-impact-include affected` cannot collide with a longer
            // word. A recipe must resolve exactly one category, never two.
            let calls: Vec<&str> = ["modified", "affected", "required"]
                .into_iter()
                .filter(|cat| body.contains(&format!("_anvil-impact-include {cat}")))
                .collect();
            assert!(
                calls.len() <= 1,
                "{path} makes contradictory impact-include calls {calls:?}; a check resolves exactly one category"
            );

            match policy.category() {
                None => {
                    // Unscoped: no cache dependency, no include call.
                    assert!(
                        !body.contains("_anvil-impact-include"),
                        "{path} is declared Unscoped but calls _anvil-impact-include"
                    );
                    assert!(
                        !body.contains("-validate-prereqs anvil-impact"),
                        "{path} is declared Unscoped but depends on anvil-impact"
                    );
                }
                Some(category) => {
                    assert_eq!(
                        calls.as_slice(),
                        &[category],
                        "{path}: declared {policy:?} but its _anvil-impact-include category is {calls:?}"
                    );
                    // A scoped check must depend on anvil-impact so the cache is
                    // fresh, and capture the scope into a local $include -- no
                    // ANVIL_INCLUDE_* env-var indirection.
                    assert!(
                        body.contains("-validate-prereqs anvil-impact"),
                        "{path} reads the impact cache but does not depend on anvil-impact"
                    );
                    assert!(
                        body.contains("$include = (& \"{{ just_executable() }}\" _anvil-impact-include"),
                        "{path} must capture _anvil-impact-include into a local $include variable"
                    );
                }
            }
            assert!(
                !body.contains("ANVIL_INCLUDE_"),
                "{path} must not reference the removed ANVIL_INCLUDE_* env vars"
            );
        }

        // Bijection: every declared check exists as a file, and every file is
        // declared -- so adding or removing a check forces an explicit policy.
        let declared: BTreeSet<&str> = expected.keys().copied().collect();
        assert_eq!(
            declared, seen,
            "EXPECTED_CHECK_POLICY and the check catalog disagree on the set of checks"
        );

        // Guard the headline scoped/unscoped split so a wholesale policy shift
        // is a deliberate, reviewed edit rather than an accident.
        let scoped = EXPECTED_CHECK_POLICY.iter().filter(|(_, p)| p.category().is_some()).count();
        let unscoped = EXPECTED_CHECK_POLICY.len() - scoped;
        assert_eq!(
            (scoped, unscoped),
            (23, 7),
            "impact scoped/unscoped split changed; update EXPECTED_CHECK_POLICY deliberately"
        );
    }

    #[test]
    fn semver_check_compares_against_the_pr_branch_baseline() {
        let (_, body) = CHECK_FILES
            .iter()
            .find(|(path, _)| *path == "justfiles/anvil/checks/semver-check.just")
            .expect("semver check template is registered");

        // The recipe must resolve the PR target branch via the shared
        // resolver _anvil-base-ref, and pass it to cargo-semver-checks
        // as the baseline rather than comparing against the last crates.io release.
        for needle in [
            "_anvil-base-ref",
            "git rev-parse --verify \"$base^{commit}\"",
            "git cat-file -e $baselineManifest",
            "cargo semver-checks --package $p --baseline-rev $base",
        ] {
            assert!(body.contains(needle), "semver check template missing '{needle}'");
        }
    }

    #[test]
    fn groups_just_template_includes_all_groups_and_pr_slow_sub_recipes() {
        let groups = all_group_bodies();
        for needle in [
            "anvil-pr-fast:",
            "anvil-pr-slow:",
            "anvil-pr-test:",
            "anvil-pr-runtime-analysis:",
            "anvil-pr-mutants:",
            "anvil-scheduled-test:",
            "anvil-scheduled-advisories:",
            "anvil-scheduled-exhaustive:",
        ] {
            assert!(groups.contains(needle), "groups tree missing '{needle}'");
        }
        for needle in ["anvil-pr-slow1:", "anvil-pr-slow2:", "anvil-pr-slow3:"] {
            assert!(!groups.contains(needle), "groups tree still contains stale '{needle}'");
        }
        assert!(groups.contains("anvil-pr-slow: anvil-pr-slow-validate-prereqs anvil-pr-test anvil-pr-runtime-analysis anvil-pr-mutants"));
        // PR group recipes list their own validate-prereqs aggregate first so
        // all tool checks run up front (just dedups the per-check ones).
        for needle in [
            "anvil-pr-fast: anvil-pr-fast-validate-prereqs",
            "anvil-pr-test: anvil-pr-test-validate-prereqs",
            "anvil-pr-runtime-analysis: anvil-pr-runtime-analysis-validate-prereqs",
            "anvil-pr-mutants: anvil-pr-mutants-validate-prereqs",
        ] {
            assert!(
                groups.contains(needle),
                "group recipe must run its validate-prereqs first: '{needle}'"
            );
        }
        // Scheduled groups are the full-workspace backstop: the public recipe
        // routes through `_anvil-run` with impact "off" (forcing
        // ANVIL_IMPACT=off before the deps run), and the private `_anvil-<group>`
        // fan-out lists its validate-prereqs aggregate first.
        for g in [
            "scheduled-test",
            "scheduled-advisories",
            "scheduled-runtime-analysis",
            "scheduled-exhaustive",
        ] {
            assert!(
                groups.contains(&format!("anvil-{g}: (_anvil-run \"{g}\" anvil_runner \"off\")")),
                "scheduled group {g} must route through _anvil-run with impact off"
            );
            assert!(
                groups.contains(&format!("_anvil-{g}: anvil-{g}-validate-prereqs")),
                "scheduled group {g} private fan-out must run its validate-prereqs first"
            );
        }
    }

    #[test]
    fn impact_scoped_groups_declare_cargo_delta_prereq() {
        // Every PR group whose checks are impact-scoped depends (transitively,
        // via each scoped check) on `anvil-impact`, which invokes cargo-delta
        // when it (re)computes the impact set. The group's setup +
        // validate-prereqs must therefore install / verify cargo-delta, so a
        // missing tool fails fast at setup rather than mid-run. (pr-slow is an
        // umbrella and inherits this via pr-test / pr-runtime-analysis /
        // pr-mutants.) Scheduled groups force ANVIL_IMPACT=off and never
        // recompute the impact set, so they deliberately do NOT depend on
        // cargo-delta.
        let groups = all_group_bodies();
        for g in ["pr-fast", "pr-test", "pr-runtime-analysis", "pr-mutants"] {
            assert!(
                groups.contains(&format!(
                    "anvil-{g}-setup installer=\"install\": \\\n    (anvil-tool-cargo-delta-install installer)"
                )),
                "group {g} setup must install cargo-delta"
            );
            assert!(
                groups.contains(&format!(
                    "anvil-{g}-validate-prereqs: \\\n    anvil-tool-cargo-delta-validate-prereqs"
                )),
                "group {g} validate-prereqs must verify cargo-delta"
            );
        }
        // Scheduled groups force impact off and never recompute, so they must
        // NOT carry cargo-delta as a prerequisite.
        for g in [
            "scheduled-test",
            "scheduled-advisories",
            "scheduled-runtime-analysis",
            "scheduled-exhaustive",
        ] {
            assert!(
                !groups.contains(&format!(
                    "anvil-{g}-setup installer=\"install\": \\\n    (anvil-tool-cargo-delta-install installer)"
                )),
                "scheduled group {g} must not install cargo-delta (it forces ANVIL_IMPACT=off and never recomputes)"
            );
            assert!(
                !groups.contains(&format!(
                    "anvil-{g}-validate-prereqs: \\\n    anvil-tool-cargo-delta-validate-prereqs"
                )),
                "scheduled group {g} must not verify cargo-delta (it forces ANVIL_IMPACT=off and never recomputes)"
            );
        }
    }

    #[test]
    fn tiers_just_template_has_three_tiers() {
        for needle in [
            "anvil-pr:",
            "anvil-scheduled:",
            "anvil-full:",
            "_anvil-pr:",
            "_anvil-scheduled:",
            "_anvil-full:",
        ] {
            assert!(TIERS_JUST.contains(needle), "tiers.just missing '{needle}'");
        }
        // Every public tier entry point routes through the `_anvil-run`
        // native/container router. The private `_anvil-<tier>` recipe carries
        // the validate-prereqs aggregate (run first) so a missing tool fails
        // up front rather than mid-run. The scheduled and full tiers pass the
        // `"off"` impact argument so `_anvil-run` exports ANVIL_IMPACT=off --
        // they are the full-workspace backstop for PR-tier impact scoping.
        for needle in [
            "anvil-pr: (_anvil-run \"pr\" anvil_runner)",
            "_anvil-pr: anvil-pr-validate-prereqs",
            "anvil-scheduled: (_anvil-run \"scheduled\" anvil_runner \"off\")",
            "_anvil-scheduled: anvil-scheduled-validate-prereqs",
            "anvil-full: (_anvil-run \"full\" anvil_runner \"off\")",
            "_anvil-full: anvil-full-validate-prereqs",
        ] {
            assert!(TIERS_JUST.contains(needle), "tier wrapper missing '{needle}'");
        }
        // The runner forces impact off for the full-workspace tiers.
        assert!(
            RUNNER_JUST.contains("ANVIL_IMPACT"),
            "runner must be able to force ANVIL_IMPACT=off for the scheduled/full tiers"
        );
        // The scheduled tier must fan out to every scheduled group, including
        // runtime-analysis (a separate group from exhaustive).
        for needle in [
            "anvil-scheduled-test",
            "anvil-scheduled-advisories",
            "anvil-scheduled-runtime-analysis",
            "anvil-scheduled-exhaustive",
        ] {
            assert!(TIERS_JUST.contains(needle), "scheduled tier must reference group '{needle}'");
        }
        // The catch-all `anvil-setup` / `anvil-validate-prereqs` must also
        // install / verify cargo-delta (the impact tool), so a local
        // `just anvil-setup` provisions a complete environment even though
        // cargo-delta isn't wired into any per-group setup.
        assert!(
            TIERS_JUST
                .contains("anvil-setup installer=\"install\": (anvil-full-setup installer) (anvil-tool-cargo-delta-install installer)"),
            "anvil-setup must install cargo-delta"
        );
        assert!(
            TIERS_JUST.contains("anvil-validate-prereqs: anvil-full-validate-prereqs anvil-tool-cargo-delta-validate-prereqs"),
            "anvil-validate-prereqs must verify cargo-delta"
        );
    }

    #[test]
    fn versions_just_has_known_tools() {
        for needle in [
            "cargo_nextest_version",
            "cargo_llvm_cov_version",
            "cargo_deny_version",
            "cargo_mutants_version",
        ] {
            assert!(VERSIONS_JUST.contains(needle), "versions.just missing variable '{needle}'");
        }
    }

    #[test]
    fn mod_just_imports_siblings_and_defines_alias() {
        for needle in [
            "import 'helpers.just'",
            "import 'impact.just'",
            "import 'checks/fmt.just'",
            "import 'checks/miri.just'",
            "import 'container.just'",
            "import 'groups/pr-fast.just'",
            "import 'groups/scheduled-exhaustive.just'",
            "import 'runner.just'",
            "import 'tiers.just'",
            "import 'tools.just'",
            "import 'versions.just'",
            "alias anvil := anvil-pr",
        ] {
            assert!(MOD_JUST.contains(needle), "mod.just missing '{needle}'");
        }
        // Every split recipe file must be imported by mod.just.
        for (path, _) in CHECK_FILES.iter().chain(GROUP_FILES.iter()) {
            let import = format!(
                "import '{}'",
                path.strip_prefix("justfiles/anvil/").expect("path under justfiles/anvil/")
            );
            assert!(MOD_JUST.contains(&import), "mod.just missing '{import}'");
        }
    }

    #[test]
    fn versions_just_defines_both_nightly_pins() {
        assert!(VERSIONS_JUST.contains("rust_nightly :="), "versions.just missing rust_nightly");
        assert!(
            VERSIONS_JUST.contains("rust_nightly_external_types :="),
            "versions.just missing rust_nightly_external_types"
        );
    }

    #[test]
    fn checks_just_has_no_floating_nightly_invocations() {
        let checks = all_check_bodies();
        for line in checks.lines() {
            let stripped = line.split('#').next().unwrap_or("");
            assert!(
                !stripped.contains("+nightly "),
                "checks tree has a floating '+nightly' invocation: {line}"
            );
            assert!(
                !stripped.contains("'+nightly'"),
                "checks tree has a floating '+nightly' invocation: {line}"
            );
        }
    }

    #[test]
    fn justfile_imports_body_is_a_single_import_line() {
        let body = JUSTFILE_IMPORTS_BODY.trim();
        assert_eq!(body, "import 'justfiles/anvil/mod.just'");
    }
}
