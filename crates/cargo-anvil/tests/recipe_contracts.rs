// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]
#![expect(clippy::unwrap_used, reason = "panic-on-failure idioms are appropriate in integration tests")]

//! End-to-end contracts for the generated recipe surface.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use cargo_anvil::Catalog;
use cargo_anvil::test_support::{Cli, run_update};
use tempfile::TempDir;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

struct Generated {
    temp: TempDir,
    hub: String,
    checks: String,
    groups: String,
    helpers: String,
    setup: String,
    container: String,
}

impl Generated {
    fn all_recipes(&self) -> String {
        [&self.hub, &self.checks, &self.groups, &self.helpers, &self.setup, &self.container]
            .into_iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn generated_with_catalog(catalog: &Catalog) -> Generated {
    let temp = TempDir::new().unwrap();
    write(
        &temp.path().join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"crate\"]\n\
         [workspace.package]\nrust-version = \"1.95\"\n",
    );
    write(
        &temp.path().join("crate/Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
         rust-version = \"1.95\"\n",
    );
    write(&temp.path().join("crate/src/lib.rs"), "pub fn value() -> u8 { 1 }\n");
    run_update(
        catalog,
        &Cli {
            backends: Vec::new(),
            no_backends: true,
            dry_run: false,
            force: false,
        },
        temp.path(),
    )
    .unwrap();
    let read = |name: &str| std::fs::read_to_string(temp.path().join(".anvil").join(name)).unwrap_or_default();
    Generated {
        hub: read("anvil.just"),
        checks: read("checks.just"),
        groups: read("groups.just"),
        helpers: read("helpers.just"),
        setup: read("setup.just"),
        container: read("container.just"),
        temp,
    }
}

fn generated() -> Generated {
    generated_with_catalog(&Catalog::anvil())
}

#[test]
fn generated_recipes_are_split_by_responsibility() {
    let generated = generated();
    for import in ["setup.just", "helpers.just", "checks.just", "groups.just", "container.just"] {
        let declaration = if import == "container.just" {
            format!("import? '{import}'")
        } else {
            format!("import '{import}'")
        };
        assert!(generated.hub.contains(&declaration));
    }
    assert!(generated.hub.contains("alias anvil := anvil-pr"));
    assert!(generated.checks.contains("anvil-clippy:"));
    assert!(generated.checks.contains("anvil-build package="));
    assert!(generated.checks.contains("anvil-readme fix="));
    assert!(generated.checks.contains("anvil-impact:"));
    assert!(!generated.checks.contains("anvil-pr-fast:"));
    assert!(!generated.checks.lines().any(|line| line.starts_with("_anvil-readme ")));
    assert!(generated.groups.contains("anvil-pr-fast:"));
    assert!(!generated.groups.lines().any(|line| line.starts_with("_anvil-unscoped ")));
    assert!(!generated.helpers.lines().any(|line| line.starts_with("anvil-impact:")));
    assert!(generated.helpers.lines().any(|line| line.starts_with("_anvil-readme ")));
    assert!(generated.helpers.lines().any(|line| line.starts_with("_anvil-unscoped ")));
    assert!(!generated.checks.contains("anvil-clippy-setup"));
    assert!(generated.setup.contains("anvil-clippy-setup"));
    assert!(generated.setup.contains("cargo_delta_version"));
    assert!(!generated.setup.contains("anvil-clippy:"));
    assert!(generated.container.contains("anvil-container *command:"));
    assert!(!generated.container.contains("anvil-clippy:"));
    assert!(!generated.groups.contains("anvil-clippy:"));
    assert!(!generated.helpers.contains("anvil-clippy:"));
}

#[test]
fn unused_deps_keeps_scope_and_fixes_explicit() {
    let generated = generated();
    assert!(generated.checks.contains(r#"[arg("fix", long, value="true")]"#));
    assert!(generated.checks.contains(r#"anvil-unused-deps fix="false":"#));
    assert!(generated.checks.contains(r#"anvil_impact_input_dir + "/required.packages""#));
    assert!(generated.checks.contains("$pkg += @('--package', $spec)"));
    assert!(generated.checks.contains("Where-Object { -not [string]::IsNullOrWhiteSpace($_) }"));
    assert!(generated.setup.contains("cargo_unused_deps_version"));
    assert!(generated.setup.contains("anvil-tool-cargo-unused-deps-validate-prereqs"));
}

#[test]
fn unused_deps_runs_immediately_before_udeps_in_pr_fast() {
    assert!(
        cargo_anvil::artifacts::justfile::groups::pr_fast()
            .body()
            .contains("    anvil-unused-deps \\\n    anvil-udeps \\\n"),
        "execution order must keep unused-deps immediately before udeps"
    );
}

#[test]
fn unused_deps_rejects_whitespace_only_package_files() {
    let generated = generated();
    assert!(generated.checks.contains("IsNullOrWhiteSpace($packageText)"));
    assert!(generated.checks.contains("contains whitespace but no package specs"));
}

#[test]
fn common_checks_are_direct_cargo_each_invocations() {
    let recipes = generated().checks;
    for command in [
        "cargo each {{ anvil_affected_selection }} --once -- cargo {{ anvil_stable_toolchain_arg }} clippy '{packages}'",
        "cargo each {{ anvil_required_selection }} --once -- cargo {{ anvil_stable_toolchain_arg }} hack '{packages}'",
        "cargo +{{ rust_nightly_external_types }} each {{ anvil_affected_selection }} --filter target-kind:lib",
        "cargo each {{ anvil_affected_selection }} --filter doctest -- cargo {{ anvil_stable_toolchain_arg }} test --doc --package '{cargo-spec}'",
        "cargo each {{ anvil_affected_selection }} --filter feature:loom --each-target test --target-filter required-feature:loom --require-target-per-package",
        "--each-target example --target-filter",
        "not name-in-metadata:anvil.examples.no-run",
        "--timeout {{ timeout }}s --keep-going",
    ] {
        assert!(recipes.contains(command), "missing direct command shape: {command}");
    }
    for retired in [
        "_anvil-examples-run",
        "--target-required-feature",
        r##"{{ if example == "" { "" } else { "# " } }}cargo each"##,
        r##"{{ if example == "" { "# " } else { "" } }}cargo each"##,
        "anvil-doc-test: anvil-doc-test-validate-prereqs anvil-impact\n    $ErrorActionPreference",
    ] {
        assert!(!recipes.contains(retired), "retired recipe implementation survived: {retired}");
    }
    assert!(
        recipes.contains(
            r#"--target-filter {{ quote(if example == "" { "not name-in-metadata:anvil.examples.no-run" } else { "name:" + example }) }} {{ if example == "" { "" } else { "--require-target-per-package" } }}"#
        ),
        "examples must use one target-mode command for automatic and explicit selection"
    );
}

#[test]
fn formatting_does_not_require_impact_or_package_dispatch() {
    let recipe = include_str!("../templates/justfiles/anvil/checks/fmt.just");
    let setup = include_str!("../templates/justfiles/anvil/setup/checks/fmt.just");
    assert!(recipe.contains(r#"cargo +{{ rust_nightly }} fmt --all {{ if fix == "true" { "" } else { "--check" } }}"#));
    for contents in [recipe, setup] {
        for scoped_dependency in [
            "anvil-impact",
            "anvil_modified_selection",
            "cargo-each",
            "cargo each",
            "cargo-delta",
        ] {
            assert!(!contents.contains(scoped_dependency), "unexpected dependency: {scoped_dependency}");
        }
    }
}

#[test]
fn lightweight_workspace_checks_do_not_require_impact() {
    for (recipe, command) in [
        (
            include_str!("../templates/justfiles/anvil/checks/cargo-sort.just"),
            "cargo each --workspace --once -- cargo {{ anvil_stable_toolchain_arg }} sort --workspace --grouped --check --check-format",
        ),
        (
            include_str!("../templates/justfiles/anvil/checks/license-headers.just"),
            "cargo each --workspace --once -- cargo {{ anvil_stable_toolchain_arg }} heather",
        ),
    ] {
        assert!(recipe.lines().any(|line| line.trim() == command));
        assert!(!recipe.contains("anvil-impact"));
        assert!(!recipe.contains("anvil_modified_selection"));
    }
}

#[test]
fn coverage_is_one_domain_tool_invocation() {
    let generated = generated();
    let recipes = &generated.checks;
    assert!(recipes.contains("cargo +{{ rust_nightly }} each {{ anvil_affected_selection }} --once --"));
    assert!(recipes.contains("{{ anvil_coverage_gate_command }} {{ anvil_explicit_package_args }} run"));
    assert!(recipes.contains("--configuration all-features --configuration no-default-features"));
    assert!(recipes.contains("--coverage-dir target/coverage"));
    assert!(recipes.contains("--no-coverage-target aarch64-pc-windows-msvc"));
    assert!(!recipes.contains("Invoke-AnvilLcovReport"));
    assert!(!recipes.contains("llvm-cov.rsp"));
}

#[test]
fn codecov_uploads_the_canonical_merged_report_only() {
    for workflow in [
        include_str!("../templates/github/pr-impl-workflow.yml"),
        include_str!("../templates/github/scheduled-impl-workflow.yml"),
    ] {
        assert!(workflow.contains("hashFiles('target/coverage/lcov-merged.info') != ''"));
        assert!(workflow.contains("files: target/coverage/lcov-merged.info"));
        assert!(workflow.contains("disable_search: true"));
        assert!(!workflow.contains("files: target/coverage/lcov-all-features.info,target/coverage/lcov-no-default.info"));
    }
}

#[test]
fn runtime_analysis_workflows_shard_long_checks_without_changing_family_fan_in() {
    let pr = include_str!("../templates/github/pr-impl-workflow.yml");
    assert!(pr.contains("check: [miri, careful, loom]"));
    assert!(pr.contains("shard: [0, 1]"));
    assert!(pr.contains("          - check: careful\n            shard: 1"));
    assert!(pr.contains("          - check: loom\n            shard: 1"));
    assert!(pr.contains("group: pr-runtime-analysis-${{ matrix.check }}"));
    assert!(pr.contains("status_identity: pr-runtime-analysis-${{ matrix.check }}-${{ matrix.shard }}"));
    assert!(pr.contains("ANVIL_MIRI_PACKAGE_SHARD: ${{ matrix.shard }}"));
    assert!(pr.contains("      - pr-runtime-analysis\n"));
    assert!(pr.contains("PR_RUNTIME_ANALYSIS: ${{ needs.pr-runtime-analysis.result }}"));

    let scheduled = include_str!("../templates/github/scheduled-impl-workflow.yml");
    for profile in ["miri", "miri-tree-borrows", "miri-strict-provenance", "miri-race-coverage"] {
        assert!(scheduled.contains(&format!("          - {profile}\n")));
    }
    assert!(scheduled.contains("        shard: [0, 1]"));
    assert!(scheduled.contains("group: scheduled-runtime-analysis-${{ matrix.profile }}"));
    assert!(scheduled.contains("status_identity: scheduled-runtime-analysis-${{ matrix.profile }}-${{ matrix.shard }}"));
    assert!(scheduled.contains("ANVIL_MIRI_PACKAGE_SHARD: ${{ matrix.shard }}"));
    assert!(scheduled.contains("      - scheduled-runtime-analysis\n"));

    let generated = generated();
    assert!(generated.helpers.contains("if ($index % 2 -eq $shardIndex) { $specs[$index] }"));
    assert!(
        generated
            .helpers
            .contains("[Array]::Sort($specs, [System.StringComparer]::Ordinal)")
    );
    assert!(
        generated
            .helpers
            .contains("[System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::Ordinal)")
    );
    for recipe in [
        "anvil-pr-runtime-analysis-miri",
        "anvil-pr-runtime-analysis-careful",
        "anvil-pr-runtime-analysis-loom",
        "anvil-scheduled-runtime-analysis-miri",
        "anvil-scheduled-runtime-analysis-miri-tree-borrows",
        "anvil-scheduled-runtime-analysis-miri-strict-provenance",
        "anvil-scheduled-runtime-analysis-miri-race-coverage",
    ] {
        assert!(
            generated.groups.lines().any(|line| line.starts_with(&format!("{recipe}:"))),
            "missing runtime-analysis recipe: {recipe}"
        );
    }
    for setup in [
        "anvil-pr-runtime-analysis-miri-setup",
        "anvil-pr-runtime-analysis-careful-setup",
        "anvil-pr-runtime-analysis-loom-setup",
        "anvil-scheduled-runtime-analysis-miri-setup",
        "anvil-scheduled-runtime-analysis-miri-tree-borrows-setup",
        "anvil-scheduled-runtime-analysis-miri-strict-provenance-setup",
        "anvil-scheduled-runtime-analysis-miri-race-coverage-setup",
    ] {
        assert!(
            generated.setup.lines().any(|line| line.starts_with(&format!("{setup} "))),
            "missing runtime-analysis setup: {setup}"
        );
    }
}

#[test]
fn miri_package_shards_cover_each_affected_package_once() {
    let generated = generated();
    let root = generated.temp.path();
    let package_file = root.join("target/anvil/impact/affected.packages");
    write(
        &package_file,
        "delta@0.1.0\nalpha@0.1.0\ncharlie@0.1.0\nbravo@0.1.0\nalpha@0.1.0\necho@0.1.0\n",
    );
    write(&root.join("target/anvil/impact/modified.packages"), "");
    write(&root.join("target/anvil/impact/required.packages"), "");
    write(
        &root.join("Justfile"),
        &format!(
            "{}\n\npartition shard output: (_anvil-miri-partition shard output \"\")\n",
            std::fs::read_to_string(root.join("Justfile")).unwrap()
        ),
    );

    for shard in ["0", "1"] {
        let output = Command::new("just")
            .args(["partition", shard, &format!("target/shard-{shard}.packages")])
            .current_dir(root)
            .env("ANVIL_IMPACT", "consume")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "Miri partition {shard} failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let read_shard = |shard: &str| {
        std::fs::read_to_string(root.join(format!("target/shard-{shard}.packages")))
            .unwrap()
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect::<BTreeSet<_>>()
    };
    let shard_zero = read_shard("0");
    let shard_one = read_shard("1");
    let expected = BTreeSet::from([
        "alpha@0.1.0".to_owned(),
        "bravo@0.1.0".to_owned(),
        "charlie@0.1.0".to_owned(),
        "delta@0.1.0".to_owned(),
        "echo@0.1.0".to_owned(),
    ]);

    assert!(shard_zero.is_disjoint(&shard_one));
    assert_eq!(shard_zero.union(&shard_one).cloned().collect::<BTreeSet<_>>(), expected);
}

#[test]
fn runtime_actions_forward_an_independent_status_identity() {
    let run_group = include_str!("../templates/github/run-group-action.yml");
    assert!(run_group.contains("  status_identity:"));
    assert!(run_group.contains("status_identity: ${{ inputs.status_identity }}"));

    let report_status = include_str!("../templates/github/report-status-action.yml");
    assert!(report_status.contains("  status_identity:"));
    assert!(report_status.contains("ANVIL_STATUS_IDENTITY: ${{ inputs.status_identity }}"));
    assert!(report_status.contains("const statusIdentity = process.env.ANVIL_STATUS_IDENTITY || group;"));
}

#[test]
fn coverage_setup_prefers_a_workspace_catalog_tool_before_external_installation() {
    let setup = include_str!("../templates/justfiles/anvil/setup/checks/llvm-cov.just");
    assert!(
        setup.contains(r"{{ just_executable() }} --justfile {{ quote(justfile()) }} _anvil-llvm-cov-setup-coverage-gate {{ installer }}")
    );
    assert!(setup.contains(
        r#"{{ if anvil_coverage_gate_local_command == "" { just_executable() + " --justfile " + quote(justfile()) + " anvil-tool-cargo-coverage-gate-install " + installer }"#
    ));
    assert!(setup.contains(
        r#"{{ if anvil_coverage_gate_local_command == "" { just_executable() + " --justfile " + quote(justfile()) + " anvil-tool-cargo-coverage-gate-validate-prereqs" }"#
    ));
}

#[test]
fn spellcheck_uses_portable_helpers_with_validated_setup() {
    let spellcheck = include_str!("../templates/justfiles/anvil/checks/spellcheck.just");
    let setup = include_str!("../templates/justfiles/anvil/setup/checks/spellcheck.just");
    let tools = include_str!("../templates/justfiles/anvil/setup/tools.just");
    assert!(spellcheck.contains("cargo spellcheck-helper prepare-dictionary --input .spelling --output target/spelling.dic"));
    assert!(spellcheck.contains("cargo each --workspace --once -- cargo {{ anvil_stable_toolchain_arg }} spellcheck"));
    assert!(!spellcheck.contains("[script(\"pwsh\", \"-NoProfile\")]\nanvil-spellcheck:"));
    assert!(setup.contains(
        "anvil-spellcheck-validate-prereqs: anvil-tool-cargo-spellcheck-helper-validate-prereqs anvil-tool-cargo-spellcheck-validate-prereqs anvil-tool-cargo-each-validate-prereqs"
    ));
    assert!(
        tools.contains("anvil-tool-cargo-spellcheck-install installer=\"install\": (anvil-tool-cargo-spellcheck-helper-install installer)")
    );
    assert!(tools.contains(
        "anvil-tool-cargo-spellcheck-helper-install installer=\"install\": (_install-tool \"cargo-spellcheck-helper\" cargo_spellcheck_helper_version installer)"
    ));
    assert!(tools.contains(
        "anvil-tool-cargo-spellcheck-source-deps-check: anvil-tool-cargo-spellcheck-helper-validate-prereqs\n    cargo spellcheck-helper check-source-deps"
    ));
    assert!(!setup.contains("[script(\"pwsh\""));
    assert!(!tools.contains("[script(\"pwsh\""));
}

#[test]
fn setup_uses_lazy_inventory_and_exact_install_policy() {
    let generated = generated();
    let recipes = &generated.setup;
    for contract in [
        "installed_cargo_tools := `cargo install --list`",
        "semver_matches(installed, \">=\" + minimum)",
        "cargo install --locked --version =",
        "cargo binstall --no-confirm --locked --version =",
        r#"anvil-tool-cargo-each-install installer="install": (_install-tool "cargo-each" cargo_each_version installer)"#,
    ] {
        assert!(recipes.contains(contract), "missing setup contract: {contract}");
    }
    assert!(!recipes.contains("_install-bootstrap-tool"));
    assert!(!recipes.contains(r#"anvil-tool-cargo-delta-install installer="install": (anvil-toolchain-stable-install installer)"#));
    assert!(!recipes.contains(r#"source_command=(if name == "cargo-each""#));
    assert!(!recipes.contains("anvil-toolchain-stable-install"));
    assert!(generated.setup.contains("set lazy"));
    assert!(recipes.contains("--disable-strategies compile"));
    assert!(!recipes.contains("`rustup run {{"));
    assert!(!generated.helpers.contains("`rustc +{{"));
}

#[test]
fn released_domain_tool_versions_are_pinned() {
    let generated = generated();
    let recipes = &generated.setup;
    for pin in [
        "cargo_aprz_version := \"1.2.2\"",
        "cargo_coverage_gate_version := \"0.7.7\"",
        "cargo_delta_version := \"0.5.0\"",
        "cargo_each_version := \"0.8.0\"",
        "cargo_gamma_version := \"0.8.0\"",
    ] {
        assert!(recipes.contains(pin), "missing released tool pin: {pin}");
    }
}

#[test]
fn msrv_recipes_use_only_the_declared_root_version() {
    let generated = generated();
    assert!(generated.checks.contains("cargo '+{workspace-rust-version}' test"));
    assert!(
        generated
            .setup
            .contains("rustup toolchain install \" + quote(workspace_rust_version) + \" --profile minimal")
    );
    assert!(
        generated
            .setup
            .contains("rustup run \" + quote(workspace_rust_version) + \" rustc --version")
    );
    assert!(generated.setup.contains(
        "anvil-msrv-test-setup installer=\"install\": (anvil-tool-cargo-each-install installer)\n    \
         {{ just_executable() }} --justfile {{ quote(justfile()) }} _anvil-msrv-test-setup"
    ));
    assert!(generated.setup.contains(
        "anvil-msrv-test-validate-prereqs: anvil-tool-cargo-each-validate-prereqs\n    \
         {{ just_executable() }} --justfile {{ quote(justfile()) }} _anvil-msrv-test-validate-prereqs"
    ));
    assert!(!generated.setup.contains("cargo each {{ anvil_msrv_workspace_selection }}"));
    assert!(
        generated
            .helpers
            .contains("anvil_msrv_selection := if workspace_rust_version == \"\"")
    );
    assert!(!generated.all_recipes().contains("ANVIL_MSRV_TOOLCHAIN"));
}

#[test]
fn miri_setup_includes_the_cargo_each_driver() {
    let setup = generated().setup;
    assert!(setup.contains(
        "anvil-miri-setup installer=\"install\": anvil-component-nightly-miri-install \
         anvil-component-nightly-rust-src-install (anvil-tool-cargo-each-install installer)"
    ));
    assert!(setup.contains(
        "anvil-miri-validate-prereqs: anvil-component-nightly-miri-validate-prereqs \
         anvil-component-nightly-rust-src-validate-prereqs \
         anvil-tool-cargo-each-validate-prereqs"
    ));
}

#[test]
fn mutation_diff_setup_includes_the_impact_driver() {
    let setup = generated().setup;
    assert!(setup.contains(
        "anvil-mutants-diff-setup installer=\"install\": (anvil-tool-cargo-gamma-install installer) \
         (anvil-tool-cargo-each-install installer)"
    ));
    assert!(setup.contains(
        "anvil-mutants-diff-validate-prereqs: anvil-tool-cargo-gamma-validate-prereqs \
         anvil-tool-cargo-each-validate-prereqs"
    ));
}

#[test]
fn impact_delegates_snapshot_management_to_delta_without_a_shell_script() {
    let check = include_str!("../templates/justfiles/anvil/checks/impact.just");
    let helper = include_str!("../templates/justfiles/anvil/helpers/impact.just");
    for tier in ["modified", "affected", "required"] {
        assert!(check.contains(&format!(
            "impact --base-ref {{{{ quote(anvil_base_ref) }}}} --{tier} --format packages --output {{{{ quote(anvil_impact_input_dir + \"/{tier}.packages\") }}}}"
        )));
        assert!(check.contains(&format!("(_anvil-impact-consume-file \"{tier}\")")));
    }
    assert!(check.contains(r#"[env("RUSTUP_TOOLCHAIN", anvil_impact_toolchain)]"#));
    assert!(check.contains(r#"[env("GIT_LFS_SKIP_SMUDGE", "1")]"#));
    assert!(helper.contains(r#"shell("rustc " + anvil_stable_toolchain_arg + " --print sysroot")"#));
    assert!(helper.contains("cloud impact computation requires a clean working tree"));
    assert!(helper.contains(r#""target/anvil/impact-invocations/" + ANVIL_IMPACT_INVOCATION_ID"#));
    assert!(helper.contains(r#"if read(file) == """#));
    assert!(helper.contains("download a complete impact artifact or correct ANVIL_IMPACT_INPUT_DIR"));
    for recipes in [check, helper] {
        assert!(!recipes.contains("[script("));
        assert!(!recipes.contains("_anvil-impact-snapshot"));
    }
}

#[test]
fn shell_scripts_are_limited_to_domain_exceptions() {
    let recipes = generated().all_recipes();
    let recipe_names = recipes
        .lines()
        .filter_map(|line| {
            let head = line.split_once(':')?.0.split_whitespace().next()?;
            (head.starts_with("anvil-") || head.starts_with("_anvil-")).then_some(head)
        })
        .collect::<BTreeSet<_>>();
    for portable in [
        "anvil-impact",
        "anvil-clippy",
        "anvil-fmt",
        "anvil-llvm-cov",
        "anvil-external-types",
        "anvil-loom",
    ] {
        assert!(recipe_names.contains(portable));
    }
    for retired in ["_anvil-impact-format", "_anvil-resolve-stable", "_anvil-stable-toolchain-args"] {
        assert!(!recipes.contains(retired), "retired helper survived: {retired}");
    }
}

#[test]
fn miri_profiles_set_their_distinct_environment() {
    let checks = generated().all_recipes();
    for (miri_flags, rust_flags) in [
        ("-Zmiri-tree-borrows ", "--cfg miri_tree_borrows "),
        ("-Zmiri-strict-provenance ", "--cfg miri_strict_provenance "),
        ("-Zmiri-many-seeds=", "--cfg miri_race_coverage "),
    ] {
        assert!(checks.contains(miri_flags), "{checks}");
        assert!(checks.contains(rust_flags), "{checks}");
    }
    for inherited in [r#"env("MIRIFLAGS", "")"#, r#"env("RUSTFLAGS", "")"#] {
        assert!(checks.contains(inherited), "{checks}");
    }
    assert!(checks.contains("anvil_miri_race_seed_window"), "{checks}");
    assert!(
        checks.contains("reason=compiler-artifact and profile.test=true and executable"),
        "{checks}"
    );
    assert!(checks.contains("--json-work-dir-parent manifest_path"), "{checks}");
    assert!(checks.contains(r#"[env("__CARGO_TEMPORARY_BUILD_DIR_NEW_LAYOUT_OPT_OUT", anvil_miri_build_dir_layout_opt_out)]"#));
    assert!(!checks.contains(r#"anvil_cargo_env_config("__CARGO_TEMPORARY_BUILD_DIR_NEW_LAYOUT_OPT_OUT""#));
    assert!(!checks.contains("ForEach-Object -Parallel"), "{checks}");
    assert!(!checks.contains("[script(\"pwsh\", \"-NoProfile\")]\n_anvil-miri-test"), "{checks}");
}

#[test]
fn container_context_and_setup_use_all_generated_recipe_files() {
    let generated = generated();
    let root = generated.temp.path();
    let ignore = std::fs::read_to_string(root.join(".anvil/container/Dockerfile.dockerignore")).unwrap();
    let dockerfile = std::fs::read_to_string(root.join(".anvil/container/Dockerfile")).unwrap();
    for path in [
        "anvil.just",
        "checks.just",
        "groups.just",
        "helpers.just",
        "setup.just",
        "container.just",
    ] {
        assert!(ignore.contains(&format!("!.anvil/{path}")));
        assert!(generated.container.contains(&format!("'.anvil/{path}'")));
    }
    assert!(dockerfile.contains(r#"printf "set unstable\nimport '.anvil/anvil.just'\n" > Justfile"#));
    assert!(dockerfile.contains("ARG ANVIL_ROOT_MSRV"));
    assert!(dockerfile.contains("setup_toolchain=stable"));
    assert!(dockerfile.contains("unset RUSTUP_TOOLCHAIN"));
    assert!(dockerfile.contains("rustup toolchain install \\\n"));
    assert!(dockerfile.contains(r#"ANVIL_COVERAGE_GATE_LOCAL_COMMAND="" just anvil-setup binstall"#));
    assert!(dockerfile.contains(r#"RUSTUP_TOOLCHAIN="${setup_toolchain}" ANVIL_COVERAGE_GATE_LOCAL_COMMAND="" just anvil-setup binstall"#));
    assert!(generated.helpers.contains("[env(\"ANVIL_IMPACT\", \"off\")]"));
}

#[test]
fn generated_root_imports_only_the_hub() {
    let generated = generated();
    let root = std::fs::read_to_string(generated.temp.path().join("Justfile")).unwrap();
    assert!(root.contains("import '.anvil/anvil.just'"));
    assert!(!root.contains("checks.just"));
    assert!(!root.contains("setup.just"));
    assert!(!root.contains("container.just"));
}
