# Standalone test feature checker

`cargo ensure-no-test-features [--manifest-path PATH] [--policy PATH]` checks
publishable workspace members without building them or requiring Anvil. Cargo
discovers the manifest by default; selecting a member still checks the workspace.

The checker flags denied features requested by direct normal or build
dependencies (including target-specific ones), and by package defaults through
local features and strong or weak dependency forwards. A strong forward to an
optional dependency also follows its same-named local feature, if present.
Private packages, dev dependencies, transitive requests, and dependency defaults
are outside the check.

The bundled policy denies exactly `fakes`, `test-util`, and `insecure`
(case-sensitive). An explicit JSON policy can replace the deny list with
`features` or omit it to inherit the defaults. Exceptions match the package,
feature, and optional dependency manifest key (including aliases). Without that
key, the exception covers only the package's own feature reachable from
`default`. No workspace policy file is loaded implicitly.

The checker exits silently with 0 on success. It reports sorted, deduplicated
findings or operational errors to stderr and exits 1; invalid CLI usage exits 2.
