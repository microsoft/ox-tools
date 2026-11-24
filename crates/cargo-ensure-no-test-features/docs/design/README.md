# Standalone test feature checker

Test-only features should not be requested by production dependency declarations
or the default features of a publishable crate. The checker enforces that rule
without building the workspace or requiring Anvil.

Run `cargo ensure-no-test-features [--manifest-path PATH] [--policy PATH]`.
Cargo discovers the manifest from the current directory unless a path is
provided. Relative paths are resolved from the current directory. Selecting a
member manifest still checks the entire workspace.

The checker uses `cargo metadata --no-deps --format-version 1` so it sees
Cargo's effective declarations, including workspace inheritance and aliases,
without resolving the transitive dependency graph. It skips members
with `publish = false`; registry-restricted packages are checked. It checks all
explicit feature requests on direct normal and build dependencies, including
optional, inherited and target-specific dependencies, regardless of the host.
Starting at each package's `default` feature, it follows local feature edges and
checks the package's own reached names and both `dep/feature` and `dep?/feature`
forwarding to production dependency keys. A reachable weak forward counts as a
request even when activation is conditional. A strong forward to an optional
dependency also follows the same-named local feature if one exists, as Cargo does.
A `dep:key` entry merely activates a dependency and is not a feature request.
Dev dependencies, transitive feature
activation, and features enabled implicitly by dependency defaults are outside
the check.

Without `--policy`, the bundled JSON denies exactly `fakes`, `test-util`, and
`insecure`. Names are case-sensitive. An explicit JSON policy can replace the
denied names with a nonempty `features` list, or omit `features` to inherit the
bundled list. Exceptions match the checked `package`, denied `feature`, and
optional `dependency` key (the manifest alias when renamed). Omitting the key
exempts only the package's own default activation. A dependency exception
applies to both explicit and forwarded requests for that key. Invalid entries,
unknown fields (except the top-level `$comment`), and duplicates are errors.
An explicitly selected policy is required to exist; there is no implicit
workspace policy file lookup.

The checker exits silently with 0 on success. It sorts and deduplicates
findings, reports them to stderr with manifest paths and remediation guidance,
and exits 1. Policy and metadata errors also result in exit 1, even if stderr
cannot be written. Invalid CLI usage exits 2.
