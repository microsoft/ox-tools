# Standalone test feature checker

`cargo ensure-no-test-features [--manifest-path PATH] [--policy PATH]` checks
workspace members without building them or requiring Anvil. It skips
`publish = false`, not members restricted to named registries. Cargo
discovers the manifest by default; selecting a member still checks the workspace.

The checker flags denied features a member requests on its direct normal or build
dependencies (including target-specific ones) or through its default feature
graph, including local features and dependency forwards. A strong forward
(`dependency/feature`) can activate an optional dependency; a weak forward
(`dependency?/feature`) cannot. A strong forward to an
optional dependency also follows its same-named local feature, if present.
Dev dependencies, transitive requests, and dependency defaults are outside the check.

The bundled policy denies exactly `fakes`, `test-util`, and `insecure`
(case-sensitive). An explicit JSON policy can replace the deny list with
`features` or omit it to inherit the defaults. Exception objects match the package,
feature, and optional dependency manifest key (including aliases). Without that
key, the exception covers only the package's own feature reachable from
`default`. No workspace policy file is loaded implicitly.

The checker exits silently with 0 on success. It reports sorted, deduplicated
findings or operational errors to stderr and exits 1; invalid CLI usage exits 2.
