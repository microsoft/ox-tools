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
