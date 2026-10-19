// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[test]
fn coverage_resolves_literal_ids_with_the_instrumenting_toolchain() {
    let recipe = include_str!("../templates/justfiles/anvil/checks/llvm-cov.just");
    let selection: Vec<_> = recipe.lines().filter(|line| line.contains("Get-AnvilPackages ")).collect();
    assert_eq!(
        selection,
        ["    $selected = @(Get-AnvilPackages $include -Toolchain '+{{ rust_nightly }}')"]
    );
}

#[test]
fn all_targets_resolves_each_manifest_and_keeps_checking_after_failure() {
    let recipe = include_str!("../templates/justfiles/anvil/checks/check-all-targets.just");
    assert!(recipe.contains("$packages = @(Get-AnvilPackages $include -Toolchain {{_anvil_stable_toolchain_args}})"));
    assert!(recipe.contains(
        "        foreach ($p in $packages) {\n\
         \x20           $packageArgs = @(Get-AnvilPackageArguments @($p) -Adapter manifest)\n\
         \x20           & cargo {{_anvil_stable_toolchain_args}} check @packageArgs --all-targets @featureArgs --locked\n\
         \x20           if ($LASTEXITCODE -ne 0) { $checkExit = $LASTEXITCODE }\n\
         \x20       }\n\
         \x20       if ($checkExit -ne 0) { exit $checkExit }"
    ));
    assert!(recipe.contains("@{ name = 'default';    flags = @() }"));
    assert!(recipe.contains("@{ name = 'no-default'; flags = @('--no-default-features') }"));
    assert!(!recipe.contains("{spec}"));
    assert!(!recipe.contains("cargo-each"));
}

#[test]
fn msrv_test_preserves_manifest_selection_for_both_feature_configurations() {
    let recipe = include_str!("../templates/justfiles/anvil/checks/msrv-test.just");
    assert!(recipe.contains("$pkg = @(Resolve-AnvilPackageArguments $include -Toolchain {{_anvil_stable_toolchain_args}})"));
    let commands: Vec<_> = recipe.lines().filter(|line| line.contains(" test @pkg ")).collect();
    assert_eq!(
        commands,
        [
            "    & cargo \"+$toolchain\" test @pkg --tests --all-features --locked",
            "    & cargo \"+$toolchain\" test @pkg --tests --locked",
        ]
    );
}
