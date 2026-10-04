# Release-dependency validation

The standalone `anvil-release-dependency-validation` recipe checks whether the
intended release set builds from packaged sources. Ordinary workspace builds
can accidentally use a new API from a local dependency whose version was not
bumped for release.

These recipes are not included in any Anvil tier or cloud workflow yet:

```text
just anvil-release-candidates
just anvil-release-dependency-validation-setup
just anvil-release-dependency-validation
```

## Candidate selection

`anvil-release-candidates` compares the current on-disk workspace with the
commit resolved by the existing `_anvil-base-ref` helper. Its precedence is
`BASE_REF`, the ADO PR target, the GitHub PR target, then an available
`origin/main` or `origin/master`. This is a direct comparison with that
reference, not a separate merge-base calculation. No automatic fetch occurs.

Cargo metadata resolves inherited versions and publication restrictions.
Candidates are current workspace members with publishing enabled that are
new by package name or have a different effective version. Renaming a package
counts as adding one; moving it without changing its name/version does not.
Removed packages, `publish = false` packages and same-version changes are not
selected. Enabling publication without changing the version is also outside
this selector's scope. It neither checks whether versions are already
published nor determines the correct SemVer bump.

The baseline is read in a temporary detached worktree using the current
toolchain. The caller's checkout, index and local edits are never switched or
stashed. A baseline with no workspace manifest means all current publishable
packages are new; invalid existing manifests and unavailable references fail.

Selection is recomputed each time and written to
`target/anvil/release/candidates.packages`: sorted UTF-8 `name@version`
specifications, one per line. Empty selection is explicit and successful.
Errors invalidate the previous computed file, rather than allowing stale
selection to be reused.

## Reusing the selection

The generated `release.just` exposes `anvil_release_selection`, a quoted
`--package-file` argument suitable for `cargo each`. Other checks can depend on
`anvil-release-candidates` and consume this variable without invoking packaging.

Set `ANVIL_RELEASE=consume` to reuse an existing file without Git comparison.
`ANVIL_RELEASE_INPUT_DIR` then names its directory, defaulting to
`target/anvil/release`. Missing, unreadable or malformed inputs fail; an empty
file skips execution. Consume mode never overwrites its input. The caller must
provide the file for the matching source checkout; package names and versions
alone do not prove source provenance.

There is no whole-workspace fallback or `off` mode. `ANVIL_IMPACT` does not
change release selection, since adding unselected dependencies to the set
would conceal missing release bumps.

## Packaged compilation

The check invokes `cargo package` once for the complete candidate set with
`--all-features --locked` and verification enabled. It never publishes, bumps versions,
creates tags or invokes release scripts.

Cargo compiles the extracted archives, respecting packaging `include`/`exclude`
rules. Co-selected candidates can resolve against each other's packaged
versions. Dependencies outside the set, including unselected workspace
packages, resolve through registry sources rather than their workspace paths.
This is library/binary build verification, not unit, integration or doc tests,
an exhaustive feature/target matrix, or a SemVer/release-cascade analysis.

Grouped packaging was exercised with Cargo 1.95 and 1.97. `--locked` accepts a
valid workspace lockfile while resolving the packaged candidate overlay.
Cargo can also package without an existing workspace lockfile, leaving it
absent. A stale existing lockfile fails rather than being rewritten; after
editing release manifests, refresh it through the normal development workflow
before invoking this check.

Cargo infers the registry from effective package `publish` settings, including
workspace inheritance, and reports ambiguous/conflicting settings itself.
URLs, credentials, dependency-specific registries and source replacement stay
in normal Cargo configuration. An internal staging feed may contain versions
that are not public; the check cannot establish that a mirror reflects public
publication.

Local runs pass `--allow-dirty` to validate uncommitted source. Following
existing Anvil recipe conventions, GitHub Actions (`GITHUB_ACTIONS=true`) and
ADO (`TF_BUILD=True`) omit that flag; comparisons are case-insensitive. Cargo's
normal packaged-source dirty checks apply in those environments. This is not
a cleanliness check for every unrelated file in the repository.

The recipe's prerequisites follow the existing toolchain-selection and
installation policy. Cargo must support grouped multi-package packaging.
Selection failures, registry resolution errors and compilation failures fail
the check; none is converted to a successful skip.
