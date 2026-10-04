# Release guard

`cargo release-guard` rehearses a release proposal in an isolated publication
workspace (W_b). Development workspace W_a can use unpublished path changes;
W_b retains publishable source only for explicitly selected release candidates.
Private test/support packages remain ordinary workspace members. Their source
and dependency requirements are not changed to repair a failing proposal.

## Implementation and boundaries

The Rust implementation uses Cargo for discovery and resolution, `toml_edit`
for materialization, and versioned JSON artifacts. The binary is a thin entry
point over testable modules.

This is a **source-level publication dependency rehearsal**, not a packaging,
upload-permission, registry-acceptance, own-API SemVer, or minimum-version proof.
It never publishes, changes versions, adds candidates to repair failures, or
infers a dependent's bump severity from a dependency bump.

## Commands

```text
cargo release-guard candidates --base <ref> --manifest-path <root> --output-dir <fresh-dir>
cargo release-guard prepare --candidate-report <candidates.json> --manifest-path <root> --output-dir <fresh-dir>
cargo release-guard check --base <ref> --manifest-path <root> --output-dir <fresh-dir>
```

`--manifest-path` accepts a workspace directory or Cargo.toml. Selection uses
the immutable `git merge-base <ref> HEAD`, not dirty-worktree impact selection.
`--candidate-list <file>` replaces inference with a JSON array of package names
(including `[]`). A report can instead be reused with `--candidate-report`.
`--registry <name>` chooses a permitted publication destination when necessary.
Versions inherited from the workspace, new packages, newly publishable packages,
and version advances are recognized. Publishable version regressions, duplicate
or private selections, ambiguous registries, and stale reports are errors.
Source-only changes never enlarge the candidate set.
Relative manifest and output paths are resolved against the invocation's working
directory. Filesystem failures identify the attempted operation and path.

Selection writes `candidates.json`. Every operation writes `report.json` with
`schema_version: 1`, a status, diagnostics, and recorded execution/provenance.
An empty selection returns success with `no_release` and performs no registry
resolution or builds. Exit 0 means the requested operation succeeded (including
no release); exit 1 means a guard failure; CLI usage errors use exit 2.

Output directories must be fresh or empty. Existing artifacts and user files
are never deleted to make room. Source identity hashes Git-tracked and
nonignored untracked files, including dirty content, excluding guard artifact
directories and build output. Preparation verifies the recorded identity both
before and after copying. Metadata selection and snapshotting accept leaf
symbolic links, hashing their entry kind and literal target without following
them, including dangling links and traversal-fixture cycles. Changing a link
to an ordinary file with identical text, or changing its target, invalidates
the snapshot. Manifest links, linked parent directories, and other filesystem
shortcuts remain errors.
Historical comparison is extracted into the owned output directory, never into
a source worktree. Historical symbolic links are never created or followed:
the metadata-only baseline substitutes inert directories containing an invalid
`Cargo.toml`, recording each original entry path in the report. This allows
unrelated traversal-test fixtures without silently dropping a linked member
matched by a workspace glob. A link needed for a manifest or workspace root
fails with its entry path; link entries remain recognizable even when Git
archive export attributes omit them. These placeholders are never compiled or
used in the publication workspace. Historical `.cargo` entries, including
links, remain omitted so they cannot introduce configuration. Materialization
still rejects every symbolic link in retained current source (candidates,
private support, or shared files): it neither follows nor recreates links.
Links wholly inside omitted publishable packages are not copied and do not
block a rehearsal. This is deliberately not general symlink support for
release source. No source lockfile is updated.
Effective Cargo configuration is fingerprinted too, including registry-index
environment overrides but not credential environment values. Reusing a report
after configuration changes fails rather than quietly testing another source.

## Materialization and registry identity

W_b retains package targets, features, profiles, package data, inherited
workspace dependencies and lints. Workspace path dependencies become registry
requirements, except permitted private test/support edges. Candidate-only
patches provide isolated candidate source when the declared requirement
matches. An incompatible independent registry requirement stays on the registry; path
dependencies cannot ignore incompatible candidate versions. Root patches and
replacement tables are not copied.
An independent registry dependency keeps its declared registry, including
implicit crates.io. A workspace namesake's publication destination is inferred
only when replacing an actual path/Git source shortcut. Inherited feature lists
remain additive, and ignored member-level `default-features = false` overrides
retain Cargo's effective enabled-defaults behavior.

All normal, build, dev, and target-specific tables, including private-package
tables and equivalent inline TOML tables, are rewritten. Unsupported target
dependency shapes fail explicitly rather than bypassing source normalization.
Versionless dev edges and private test/support edges to workspace packages derive the
declared destination version with a report entry. Versionless publishable
production edges and production edges from candidates to private packages
fail. External path dependencies, escaping target/package-data paths, nested
retained/omitted package layouts, and resolver 1 are explicitly unsupported.

Cargo configuration and credential providers remain authoritative. Configuration
files are passed to Cargo, preserving Cargo's config-relative path semantics,
without copying credentials into reports. Configured source replacement is
never bypassed. Unsafe source overrides (`paths`, config patches), unsupported
config inclusion, relative config environment paths, overridden build-output
directories, and environment source overrides are rejected rather than
ignored. Cargo's home configuration and credential environment remain intact.
`CARGO_BUILD_BUILD_DIR` is rejected before Cargo runs because it bypasses the
guard's isolated target directory.
Original config files are not copied into either historical or publication
artifacts. Commands record configuration arguments, working directories, and
isolated target directories. Registry URLs with user information, query
strings, or fragments cannot become manifest patch keys.
Cargo receives `CARGO_TARGET_DIR=target`, relative to each isolated command's
working directory, so native compilers do not inherit Windows verbatim-path
prefixes. Reports record the corresponding absolute target directory.

Exact candidate registry probes must establish absence before a candidate
patch is permitted. Already published identities fail even if the source might
be identical. Only a direct, exact Cargo "no matching package"/"failed to select
a version" diagnostic establishes absence; authentication, transport, index,
and transitive resolution failures remain failures, never "unpublished".
The immediate requiring package must be the exact generated one-dependency
probe, not a transitive namesake or a later mention of the probe in a dependency
chain. Its dependency identifies the exact candidate requirement and registry.
There is no registry-based candidate discovery or historical-artifact scan.
An offline index cache is not authoritative evidence of nonpublication:
offline probes require an approved directory/local-registry replacement.
Reports distinguish `published`, `absent`, `authentication_error`,
`network_error`, `registry_error`, and `offline_unverified`. Unrecognized Cargo
diagnostics fail conservatively instead of becoming absence.

Resolved Cargo metadata is checked for every feature configuration: every
path package must be an allowed isolated member with its expected identity;
every other package must have registry or Git provenance. A workspace
publishable package may not return through Git or another local path.
Build outputs, transformed manifests, locks, and provenance remain inspectable.
Cargo diagnostics are scrubbed of credential-shaped material before reporting.

## Builds and tests

`check` performs selection, preparation, provenance checks, one ordinary
`cargo build --package <candidate>` invocation per candidate, then the full
W_b test suite. Production builds precede tests to prevent test/dev feature
unification from hiding production failures. Resolver 2 or 3 is required.

`--test-runner cargo` (default) runs `cargo test --workspace --tests`;
`nextest` runs the same workspace test selection. One full-member invocation
preserves Cargo's workspace feature unification, including tests enabled by
private members' dependencies. Cargo handles `test = false` and
`required-features`; ordinary harnessed examples explicitly opting into tests
also run. Production builds remain separate and use resolved package IDs.

The isolated manifests disable `test = true` only for benchmarks and
`harness = false` examples, recording each execution-policy adjustment in the
report. Those arbitrary mains are not test harnesses. Neither
source code nor source manifests are changed; harnessed example tests and
private tests are preserved. The guard does not use
`--all-targets`, which would override that selection policy.
Standard `[[target]]` tables and equivalent top-level inline target arrays
receive the same path-isolation and execution-policy normalization.
Nextest receives the established `--no-tests=pass` option so a member with zero
test cases has the same successful semantics as Cargo; compilation failures and
failed test cases still fail the guard.
Both run `cargo test --workspace --doc` when the workspace has doctests, preserving
the full member feature configuration. Examples are compiled separately
with `cargo build --workspace --examples`, never run as arbitrary programs.
Benchmarks are outside the ordinary test suite.

`--feature-mode default|no-default|all` is repeatable (default: `default`).
`--features <features>`, `--target <triple>`, and `--offline` are forwarded
consistently. CI should explicitly enumerate the repository's supported
feature modes/targets instead of assuming all-features covers every contract.
Private support failures remain failures, not invitations to release omitted
packages. Registry dependencies' own tests are not run.

## Validation

Synthetic directory registries use Cargo's approved source replacement and
real Cargo subprocesses without network access. Negative controls cover
unreleased helper APIs, nominal source identity, incompatible baselines,
support leakage, configuration rejection, stale snapshots, safe output reuse,
and registry errors. These tests are dependency-resolution evidence, not a
claim that arbitrary build scripts or credential providers are sandboxed.
The complete fixture suite also exercises the existing `cargo-nextest` runner;
Git, Cargo, and nextest must be available when running those tests.
