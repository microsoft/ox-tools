// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]
#![expect(clippy::unwrap_used, reason = "panic-on-failure idioms are appropriate in integration tests")]

//! End-to-end contracts for the generated recipe surface.

use std::collections::BTreeSet;
use std::path::Path;

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
fn coverage_is_one_domain_tool_invocation() {
    let recipes = generated().checks;
    assert!(recipes.contains("cargo +{{ rust_nightly }} each {{ anvil_affected_selection }} --once --"));
    assert!(recipes.contains("{{ anvil_coverage_gate_command }} {{ anvil_explicit_package_args }} run"));
    assert!(recipes.contains("--configuration all-features --configuration no-default-features"));
    assert!(recipes.contains("--coverage-dir target/coverage"));
    assert!(recipes.contains("--no-coverage-target aarch64-pc-windows-msvc"));
    assert!(!recipes.contains("Invoke-AnvilLcovReport"));
    assert!(!recipes.contains("llvm-cov.rsp"));
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
        "cargo_coverage_gate_version := \"0.7.5\"",
        "cargo_delta_version := \"0.5.0\"",
        "cargo_each_version := \"0.8.0\"",
        "cargo_gamma_version := \"0.6.0\"",
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
    assert!(dockerfile.contains("import '.anvil/anvil.just'"));
    assert!(dockerfile.contains("ARG ANVIL_ROOT_MSRV"));
    assert!(dockerfile.contains("setup_toolchain=stable"));
    assert!(dockerfile.contains("RUSTUP_TOOLCHAIN=\"${setup_toolchain}\" just anvil-setup"));
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
