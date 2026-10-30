# Changelog

## [Unreleased]

## [0.24.0] - 2026-10-30

- ⚠️ Breaking

  - cargo-anvil: remove SemVer advisory check

- ✨ Features

  - cargo-anvil: release v0.22.0
  - cargo-anvil: split generated recipe roles
  - cargo-anvil: release v0.20.0
  - cargo-anvil: consolidate generated infrastructure

- 🐛 Bug Fixes

  - cargo-anvil: retain consolidated impact action path
  - cargo-anvil: require cargo-each 0.7.0
  - cargo-anvil: reconcile consolidation with current main

- ♻️ Code Refactoring

  - cargo-anvil: unify example target selection
  - cargo-anvil: make README checks shell-free
  - cargo-anvil: make mutants diff shell-free
  - cargo-anvil: make Miri recipes shell-free
  - cargo-anvil: remove portable recipe PowerShell

- 🧩 Miscellaneous

  - Merge current main into cargo-anvil consolidation
  - Merge current main into cargo-anvil consolidation

## [0.23.0] - 2026-10-30

- ✨ Features

  - cargo-anvil: release v0.22.0
  - cargo-anvil: split generated recipe roles
  - cargo-anvil: release v0.20.0
  - cargo-anvil: consolidate generated infrastructure

- 🐛 Bug Fixes

  - cargo-anvil: retain consolidated impact action path
  - cargo-anvil: require cargo-each 0.7.0
  - cargo-anvil: reconcile consolidation with current main

- ♻️ Code Refactoring

  - cargo-anvil: unify example target selection
  - cargo-anvil: make README checks shell-free
  - cargo-anvil: make mutants diff shell-free
  - cargo-anvil: make Miri recipes shell-free
  - cargo-anvil: remove portable recipe PowerShell

- 🧩 Miscellaneous

  - Merge current main into cargo-anvil consolidation
  - Merge current main into cargo-anvil consolidation

## [0.22.0] - 2026-10-30

- ✨ Features

  - cargo-anvil: exclude generated actions from downstream Dependabot updates ([#316](https://github.com/microsoft/ox-tools/pull/316))
  - cargo-anvil: split generated recipe roles
  - cargo-anvil: release v0.20.0
  - cargo-anvil: consolidate generated infrastructure

- 🐛 Bug Fixes

  - cargo-anvil: retain consolidated impact action path
  - cargo-anvil: isolate recipe tests from host rustup ([#324](https://github.com/microsoft/ox-tools/pull/324))
  - cargo-anvil: require cargo-each 0.7.0
  - cargo-anvil: install APRZ 1.2.1 from crates.io ([#313](https://github.com/microsoft/ox-tools/pull/313))
  - cargo-anvil: reconcile consolidation with current main

- ✔️ Tasks

  - cargo-anvil: pin cargo-delta 0.5.0 ([#311](https://github.com/microsoft/ox-tools/pull/311))

- ♻️ Code Refactoring

  - cargo-anvil: unify example target selection
  - cargo-anvil: make README checks shell-free
  - cargo-anvil: make mutants diff shell-free
  - cargo-anvil: make Miri recipes shell-free
  - cargo-anvil: remove portable recipe PowerShell

- 🧩 Miscellaneous

  - Merge current main into cargo-anvil consolidation
  - Kill a bunch of mutants ([#315](https://github.com/microsoft/ox-tools/pull/315))
  - Merge current main into cargo-anvil consolidation

## [0.21.0] - 2026-10-29

- ✨ Features

  - cargo-anvil: exclude generated actions from downstream Dependabot updates

- 🐛 Bug Fixes

  - cargo-anvil: retain protected Dependabot configs in ambiguity checks
  - cargo-anvil: bound repeated Dependabot merge lookups
  - cargo-anvil: honor inherited Dependabot exclusions
  - cargo-anvil: preserve YAML job boundaries and refusal ownership
  - cargo-anvil: preserve complete aliased Dependabot exclusions
  - cargo-anvil: preserve Dependabot configuration edge cases
  - cargo-anvil: install APRZ 1.2.1 from crates.io ([#313](https://github.com/microsoft/ox-tools/pull/313))

- ✔️ Tasks

  - cargo-anvil: keep the pending feature release at 0.21.0
  - cargo-anvil: prepare 0.21.1 review fixes release
  - cargo-anvil: prepare 0.21.0 Dependabot integration
  - cargo-anvil: pin cargo-delta 0.5.0 ([#311](https://github.com/microsoft/ox-tools/pull/311))

- 🧩 Miscellaneous

  - Kill a bunch of mutants ([#315](https://github.com/microsoft/ox-tools/pull/315))

## [0.20.3] - 2026-10-29

- ✔️ Tasks

  - cargo-anvil: pin cargo-delta 0.5.0 ([#311](https://github.com/microsoft/ox-tools/pull/311))

- 🧩 Miscellaneous

  - Kill a bunch of mutants ([#315](https://github.com/microsoft/ox-tools/pull/315))

## [0.20.2] - 2026-10-28

- 🐛 Bug Fixes

  - cargo-anvil: use published APRZ 1.2.1
  - cargo-anvil: install APRZ from the fixed parser revision

## [0.20.1] - 2026-10-28

- 🐛 Bug Fixes

  - cargo-anvil: ship github-script 9.0.0, download-artifact 8.0.2 and SHA-pinned sticky comments
  - cargo-anvil: update GitHub action pins
  - cargo-anvil: pin coverage-gate 0.7.2

- ✔️ Tasks

  - cargo-anvil: prepare 0.20.1 catalog release

## [0.20.0] - 2026-10-26

- ✨ Features

  - cargo-anvil: adopt cargo-each 0.6 package selection
  - cargo-anvil: manage JSON array entries ([#283](https://github.com/microsoft/ox-tools/pull/283))

- 🐛 Bug Fixes

  - cargo-anvil: reuse the validated MSRV for package discovery
  - cargo-anvil: skip package discovery when no MSRV is declared
  - cargo-anvil: run examples from the canonical workspace root
  - cargo-anvil: build example specs from manifest paths and fix test shims
  - cargo-anvil: select examples and Miri opt-outs by exact identity
  - cargo-anvil: avoid ambiguous package specs in remaining recipes

- 📚 Documentation

  - cargo-anvil: note coverage still passes name@version specs
  - cargo-anvil: explain shared license allow list

- ✔️ Tasks

  - cargo-anvil: release v0.20.0 after rebasing package selection

- ♻️ Code Refactoring

  - cargo-anvil: drop old-Cargo workarounds and document Cargo 1.93 minimum
  - cargo-anvil: remove obsolete package-name handling

## [0.19.0] - 2026-10-24

- ✨ Features

  - cargo-anvil: manage JSON array entries

- 🐛 Bug Fixes

  - cargo-anvil: stabilize GitHub mutation gate
  - cargo-anvil: repair body-local JSON corruption
  - cargo-anvil: validate projected JSON placement
  - cargo-anvil: sort manifest sections
  - cargo-anvil: preserve changelog history
  - cargo-anvil: address final review feedback
  - cargo-anvil: preserve legacy retirement scanning
  - cargo-anvil: verify generated JSON regions
  - cargo-anvil: validate scanner-only splices
  - cargo-anvil: require active JSON regions
  - cargo-anvil: preserve structural provenance edges
  - cargo-anvil: refuse unmatched XML closes
  - cargo-anvil: refuse incomplete JSON scans
  - cargo-anvil: guard retained JSON syntax
  - cargo-anvil: retain scanner on edited retirement
  - cargo-anvil: harden JSON scanner lifecycle
  - cargo-anvil: persist JSON scanner hosts
  - cargo-anvil: preserve renamed JSON tracking
  - cargo-anvil: scan bare CR line endings
  - cargo-anvil: retain scanner provenance on refusal
  - cargo-anvil: retain JSON scanner provenance
  - cargo-anvil: scan mixed JSON retirement safely
  - cargo-anvil: scan mixed JSON hosts safely
  - cargo-anvil: reject moved JSON regions
  - cargo-anvil: isolate structural JSON scanning
  - cargo-anvil: preserve JSON region provenance
  - cargo-anvil: sequence JSON requirement checks
  - cargo-anvil: validate required JSON entries
  - cargo-anvil: validate composed JSON hosts
  - cargo-anvil: harden JSON array splicing

- 📚 Documentation

  - cargo-anvil: record JSONC parser policy
  - cargo-anvil: clarify JSON retirement provenance

- ♻️ Code Refactoring

  - cargo-anvil: key JSON specs by region
  - cargo-anvil: clarify JSON array invariants

- 🧩 Miscellaneous

  - Merge origin/main into makolnek-microsoft-json-array-regions
  - Merge origin/main into makolnek-microsoft-json-array-regions
  - Merge origin/main into makolnek-microsoft-json-array-regions
  - Merge remote-tracking branch 'origin/main' into makolnek-microsoft-json-array-regions

## [0.18.0] - 2026-10-24

- ✨ Features

  - cargo-anvil: manage JSON array entries

- 🐛 Bug Fixes

  - cargo-anvil: repair body-local JSON corruption
  - cargo-anvil: validate projected JSON placement
  - cargo-anvil: sort manifest sections
  - cargo-anvil: preserve changelog history
  - cargo-anvil: address final review feedback
  - cargo-anvil: preserve legacy retirement scanning
  - cargo-anvil: verify generated JSON regions
  - cargo-anvil: validate scanner-only splices
  - cargo-anvil: require active JSON regions
  - cargo-anvil: preserve structural provenance edges
  - cargo-anvil: refuse unmatched XML closes
  - cargo-anvil: refuse incomplete JSON scans
  - cargo-anvil: guard retained JSON syntax
  - cargo-anvil: retain scanner on edited retirement
  - cargo-anvil: harden JSON scanner lifecycle
  - cargo-anvil: persist JSON scanner hosts
  - cargo-anvil: preserve renamed JSON tracking
  - cargo-anvil: scan bare CR line endings
  - cargo-anvil: retain scanner provenance on refusal
  - cargo-anvil: retain JSON scanner provenance
  - cargo-anvil: scan mixed JSON retirement safely
  - cargo-anvil: scan mixed JSON hosts safely
  - cargo-anvil: reject moved JSON regions
  - cargo-anvil: isolate structural JSON scanning
  - cargo-anvil: preserve JSON region provenance
  - cargo-anvil: sequence JSON requirement checks
  - cargo-anvil: validate required JSON entries
  - cargo-anvil: validate composed JSON hosts
  - cargo-anvil: harden JSON array splicing

- 📚 Documentation

  - cargo-anvil: record JSONC parser policy
  - cargo-anvil: clarify JSON retirement provenance

- ♻️ Code Refactoring

  - cargo-anvil: key JSON specs by region
  - cargo-anvil: clarify JSON array invariants

- 🧩 Miscellaneous

  - Merge origin/main into makolnek-microsoft-json-array-regions
  - Merge origin/main into makolnek-microsoft-json-array-regions
  - Merge origin/main into makolnek-microsoft-json-array-regions
  - Merge remote-tracking branch 'origin/main' into makolnek-microsoft-json-array-regions

## [0.17.0] - 2026-10-23

- ✨ Features

  - cargo-anvil: manage JSON array entries

- 🐛 Bug Fixes

  - cargo-anvil: validate projected JSON placement
  - cargo-anvil: sort manifest sections
  - cargo-anvil: preserve changelog history
  - cargo-anvil: address final review feedback
  - cargo-anvil: preserve legacy retirement scanning
  - cargo-anvil: verify generated JSON regions
  - cargo-anvil: validate scanner-only splices
  - cargo-anvil: require active JSON regions
  - cargo-anvil: preserve structural provenance edges
  - cargo-anvil: refuse unmatched XML closes
  - cargo-anvil: refuse incomplete JSON scans
  - cargo-anvil: guard retained JSON syntax
  - cargo-anvil: retain scanner on edited retirement
  - cargo-anvil: harden JSON scanner lifecycle
  - cargo-anvil: persist JSON scanner hosts
  - cargo-anvil: preserve renamed JSON tracking
  - cargo-anvil: scan bare CR line endings
  - cargo-anvil: retain scanner provenance on refusal
  - cargo-anvil: retain JSON scanner provenance
  - cargo-anvil: scan mixed JSON retirement safely
  - cargo-anvil: scan mixed JSON hosts safely
  - cargo-anvil: reject moved JSON regions
  - cargo-anvil: isolate structural JSON scanning
  - cargo-anvil: preserve JSON region provenance
  - cargo-anvil: sequence JSON requirement checks
  - cargo-anvil: validate required JSON entries
  - cargo-anvil: validate composed JSON hosts
  - cargo-anvil: harden JSON array splicing

- 📚 Documentation

  - cargo-anvil: clarify JSON retirement provenance

- ♻️ Code Refactoring

  - cargo-anvil: key JSON specs by region
  - cargo-anvil: clarify JSON array invariants

- 🧩 Miscellaneous

  - Merge origin/main into makolnek-microsoft-json-array-regions
  - Merge origin/main into makolnek-microsoft-json-array-regions
  - Merge remote-tracking branch 'origin/main' into makolnek-microsoft-json-array-regions

## [0.16.0] - 2026-10-23

- ✨ Features

  - cargo-anvil: execute examples during verification ([#286](https://github.com/microsoft/ox-tools/pull/286))

## [0.15.2] - 2026-10-23

- ✨ Features

  - cargo-anvil: roll out filtered coverage reports

- 🐛 Bug Fixes

  - cargo-anvil: clarify coverage adoption boundaries

- 📚 Documentation

  - cargo-anvil: explain Windows ARM coverage fallback
  - align coverage topology and helper names

- ✔️ Tasks

  - cargo-anvil: prepare 0.15.2 release
  - cargo-anvil: pin coverage gate 0.7.1

- 🧩 Miscellaneous

  - Merge cargo-anvil 0.15.1 from main
  - Merge cargo-deny cleanup from main
  - Merge current main into coverage rollout
  - Merge main into coverage rollout
  - Merge origin/main into anvil coverage rollout

## [0.15.1] - 2026-10-22

- ✨ Features

  - cargo-gamma: improve campaign reliability and mutation detection ([#282](https://github.com/microsoft/ox-tools/pull/282))

- 🐛 Bug Fixes

  - cargo-anvil: hide internal recipes from just listings ([#272](https://github.com/microsoft/ox-tools/pull/272))
  - cargo-anvil: preserve immutable action tags
  - cargo-anvil: synchronize advisory and coverage action pins
  - pass release and container inputs explicitly ([#265](https://github.com/microsoft/ox-tools/pull/265))
  - cargo-anvil: use just_executable for nested invocations ([#266](https://github.com/microsoft/ox-tools/pull/266))

- ⚡ Performance

  - cargo-anvil: keep Miri under 30 minutes ([#279](https://github.com/microsoft/ox-tools/pull/279))

- ✔️ Tasks

  - clean up cargo-deny warnings without narrowing license policy ([#287](https://github.com/microsoft/ox-tools/pull/287))
  - cargo-anvil: prepare 0.15.1 release

- 🧩 Miscellaneous

  - Merge main after #287 and regenerate Anvil
  - Merge main into pin alignment

## [0.15.0] - 2026-10-13

- ✨ Features

  - resume interrupted campaigns and streamline mutant testing ([#248](https://github.com/microsoft/ox-tools/pull/248))
  - support managed TOML array entries ([#235](https://github.com/microsoft/ox-tools/pull/235))
  - support XML managed regions

- ✔️ Tasks

  - release v0.14.0 ([#252](https://github.com/microsoft/ox-tools/pull/252))

- 🧩 Miscellaneous

  - Merge main and harden XML managed regions

## [0.14.1] - 2026-10-15

- 🐛 Bug Fixes

  - restore eager libclang setup ([#262](https://github.com/microsoft/ox-tools/pull/262))

## [0.14.0] - 2026-10-12

- ✨ Features

  - support managed TOML array entries ([#235](https://github.com/microsoft/ox-tools/pull/235))
  - Add `TomlArrayRegionSpec` and `CatalogBuilder::with_toml_array_region`
    so custom catalogs can manage required string entries without owning
    the enclosing TOML array or table. `Catalog::toml_array_path` exposes
    the selector for a region.
  - Preserve repository-owned entries, comments, table headers, and
    surrounding formatting during adoption, updates, and retirement.
    Adoption moves one matching unmanaged string per required entry into
    the managed region; extra copies remain repository-owned.
  - Validate array-host composition before producing an applicable plan.
    Invalid TOML, overlapping ownership, and structural dependencies
    refuse without applying unrelated pending writes.

- Bug Fixes

  - Use unique temporary baseline worktree paths in the generated impact
    recipe to avoid collisions when process IDs are reused. Clean up
    partial baseline checkouts and surface cleanup failures
    ([#248](https://github.com/microsoft/ox-tools/pull/248)).

- Upgrade notes

  - Array regions are opt-in for custom catalogs. Hosts must have a
    `.toml` suffix and use hash comments. Selectors contain literal key
    components through normal TOML tables.
  - Templates must contain at least one string entry and end with a
    comma. Generated compound values, inline-table scaffolding, and
    automatic migration from whole-table regions are not supported.
  - The `.anvil.lock` schema remains version 1. Array selectors contribute
    to custom catalog checksums. The generated impact recipe update also
    changes the built-in catalog checksum; updating generated
    infrastructure follows the existing managed-file ownership rules.

- Dependencies

  - Add `toml_parser` 1.1.3 as a direct dependency for TOML-aware
    managed-region scanning.

## [0.13.0] - 2026-10-08

- ✨ Features

  - Select `.cargo/mutants.<os>.toml` for both mutation recipes when
    present. Platform files replace the default configuration rather
    than overlay it; otherwise normal cargo-mutants discovery applies
    ([#203](https://github.com/microsoft/ox-tools/pull/203)).
  - Extend the generated Copilot code-review skill with
    `.github/skills/code-review/test-review.md`. The guide covers
    deterministic inputs, mocked collaborators, state isolation, exact
    assertions, doctests, tests in examples, runtime limits, Miri, and
    root-cause fixes for flaky tests. Review applies to changed tests,
    not unrelated existing tests
    ([#228](https://github.com/microsoft/ox-tools/pull/228)).
  - Add `anvil-check-all-targets` to the fast PR tier. Check each affected
    package independently with default features and with defaults
    disabled, preventing workspace feature unification from hiding
    missing test or example dependencies
    ([#224](https://github.com/microsoft/ox-tools/pull/224)).

- 🐛 Bug Fixes

  - Filter doctest runs by Cargo's `doctest` capability instead of
    selecting bin-only packages. An empty eligible set is a successful
    no-op. Cache the capability list with impact metadata and fall back
    to locked discovery for older caches or full-workspace paths
    ([#218](https://github.com/microsoft/ox-tools/pull/218),
    [#221](https://github.com/microsoft/ox-tools/pull/221)).
  - Fetch the current GitHub PR title through the REST API so workflow
    reruns validate renamed titles rather than the original event
    payload. Title retrieval failures remain errors
    ([#234](https://github.com/microsoft/ox-tools/pull/234)).
  - Prefer exact path spelling and reject ambiguous case-insensitive
    matches. Report directory-enumeration and permission errors instead
    of treating them as missing files
    ([#221](https://github.com/microsoft/ox-tools/pull/221)).
  - Return dry-run status through the application entry point instead
    of terminating inside the engine. Dirty dry runs still exit with
    failure; clean runs exit successfully
    ([#221](https://github.com/microsoft/ox-tools/pull/221)).
  - Distinguish invalid generated TOML bodies from invalid existing
    hosts and table-ownership conflicts in refusal guidance
    ([#221](https://github.com/microsoft/ox-tools/pull/221)).

- Documentation

  - Clarify generated configuration comments, including empty impact
    reports, coverage opt-outs, and the pinned nightly formatting policy
    ([#223](https://github.com/microsoft/ox-tools/pull/223)).

- Build and maintenance

  - Raise the generated `cargo-each` pin from 0.1.0 to 0.3.0 for the
    package-isolated checks
    ([#224](https://github.com/microsoft/ox-tools/pull/224)).
  - Exclude test-only modules from production coverage
    ([#229](https://github.com/microsoft/ox-tools/pull/229)).
  - Record the crate's coverage floor as 99.86445 percent
    ([#221](https://github.com/microsoft/ox-tools/pull/221)).
  - Expand engine regression and mutation coverage, and refactor
    catalog, manifest, region, and workspace helpers without changing
    the managed-file ownership contract
    ([#208](https://github.com/microsoft/ox-tools/pull/208),
    [#221](https://github.com/microsoft/ox-tools/pull/221)).

## [0.12.0] - 2026-09-18

- ✨ Features

  - split managed lint regions
  - lint empty braced structs

- 🐛 Bug Fixes

  - scope coverage reports to affected packages ([#190](https://github.com/microsoft/ox-tools/pull/190))

- ♻️ Code Refactoring

  - centralize lint overrides

- 🧩 Miscellaneous

  - Merge origin/main into feat/anvil-empty-struct-lint

## [0.11.1] - 2026-09-21

- 🐛 Bug Fixes

  - normalize shell-quoted coverage arguments
  - parse single-quoted llvm-cov paths

## [0.11.0] - 2026-09-17

- 🐛 Bug Fixes

  - scope coverage reports to affected packages

- ✔️ Tasks

  - add 0.10.1 changelog
  - bump version to 0.10.1

## [0.10.0] - 2026-09-16

- ✨ Features

  - pass engine context to image resolver ([#185](https://github.com/microsoft/ox-tools/pull/185))
  - add ADO before and after check hooks ([#183](https://github.com/microsoft/ox-tools/pull/183))
  - make impact cache inputs injectable ([#169](https://github.com/microsoft/ox-tools/pull/169))

- 🐛 Bug Fixes

  - stop writing an unparsable TOML host when a table is hand-written ([#162](https://github.com/microsoft/ox-tools/pull/162))
  - let a repository with no root toolchain file build the container image ([#161](https://github.com/microsoft/ox-tools/pull/161))

- 📚 Documentation

  - regenerate README with required tool version

- ✔️ Tasks

  - release v0.10.0

- 🧩 Miscellaneous

  - Merge remote-tracking branch 'origin/main' into u/mhavelka/update-release-0100

## [0.9.0] - 2026-09-08

- 🐛 Bug Fixes

  - retry oversized Windows coverage exports ([#166](https://github.com/microsoft/ox-tools/pull/166))
  - make anvil-fmt actually run the pinned nightly rustfmt ([#159](https://github.com/microsoft/ox-tools/pull/159))
  - address validation follow-ups ([#131](https://github.com/microsoft/ox-tools/pull/131))
  - remove Bash from ADO steps ([#160](https://github.com/microsoft/ox-tools/pull/160))

- ⚡ Performance

  - parallelize Miri test artifacts ([#123](https://github.com/microsoft/ox-tools/pull/123))

- 🏗️ Build System

  - bump cargo-coverage-gate and cargo-heather catalog pins ([#167](https://github.com/microsoft/ox-tools/pull/167))

## [0.8.0] - 2026-09-04

- ⚠️ Breaking

  - identify the ADO stage on every job wrapper invocation ([#154](https://github.com/microsoft/ox-tools/pull/154))

- ✨ Features

  - replace legacy CI with aggregate gate ([#158](https://github.com/microsoft/ox-tools/pull/158))

- 🐛 Bug Fixes

  - let the container image resolve the declared MSRV ([#155](https://github.com/microsoft/ox-tools/pull/155))

## [0.7.0] - 2026-09-03

- ✨ Features

  - emit a code-review skill for PR reviewers ([#152](https://github.com/microsoft/ox-tools/pull/152))

- 🐛 Bug Fixes

  - stop anvil-msrv-test from executing benchmarks ([#150](https://github.com/microsoft/ox-tools/pull/150))
  - adopt an unmanaged TOML table instead of duplicating it ([#140](https://github.com/microsoft/ox-tools/pull/140))
  - preserve recipe failure diagnostics ([#110](https://github.com/microsoft/ox-tools/pull/110))

## [0.6.0] - 2026-09-01

- ✨ Features

  - containerized execution - simplified ([#83](https://github.com/microsoft/ox-tools/pull/83))
  - local impact scoping via the anvil-impact recipe ([#47](https://github.com/microsoft/ox-tools/pull/47))
  - introduce cargo-gamma ([#93](https://github.com/microsoft/ox-tools/pull/93))

- ✔️ Tasks

  - update workspace dependencies and docs ([#111](https://github.com/microsoft/ox-tools/pull/111))

## [0.5.0] - 2026-08-25

- ✨ Features

  - publish scheduled failures as issues ([#65](https://github.com/microsoft/ox-tools/pull/65))
  - name dynamic failure statuses
  - report concrete GitHub failures
  - add cargo-aprz and cargo-ensure-no-default-features ([#76](https://github.com/microsoft/ox-tools/pull/76))

- 🐛 Bug Fixes

  - skip group runner tests under Miri
  - address Anvil status review feedback
  - refine GitHub check hierarchy
  - clarify aggregate check names
  - shorten failure status descriptions
  - prioritize detailed PR statuses
  - improve PR title check diagnostics ([#64](https://github.com/microsoft/ox-tools/pull/64))

- 📚 Documentation

  - design per-job status reporting

- 🏗️ Build System

  - unbreak CI by bumping the static-analysis toolchain and cargo-spellcheck ([#88](https://github.com/microsoft/ox-tools/pull/88))

- 🧩 Miscellaneous

  - Tolerate unreproducible SemVer baselines
  - Handle all Just failure diagnostics
  - Align Anvil permission documentation
  - Harden Anvil group validation output
  - Fix Anvil reusable workflow permissions
  - Merge origin/main into anvil-job-status-reporting

## [0.4.0] - 2026-08-04

- ✨ Features

  - add Docker-in-WSL container backend ([#59](https://github.com/microsoft/ox-tools/pull/59))

- ♻️ Code Refactoring

  - move container assets out of justfiles ([#63](https://github.com/microsoft/ox-tools/pull/63))

- 🐛 Bug Fixes

  - address validated CI bugs: impact scoping, failure handling, cache
    correctness, platform behavior, generated ownership markers, and tool
    catalog synchronization ([#62](https://github.com/microsoft/ox-tools/pull/62))

- 📝 Documentation

  - rename the design doc to `README.md` and update the in-code links that
    referenced its numbered sections ([#61](https://github.com/microsoft/ox-tools/pull/61))
  - add the crate logo ([#61](https://github.com/microsoft/ox-tools/pull/61))

- 🔧 Dependencies

  - bump the `toml_edit` requirement from 0.22.22 to 0.25.12 ([#44](https://github.com/microsoft/ox-tools/pull/44))
  - bump `syn` from 2.0.117 to 2.0.119 ([#57](https://github.com/microsoft/ox-tools/pull/57)) —
    lockfile only; `syn` reaches `cargo-anvil` transitively through the
    `clap_derive` and `ohno_macros` proc macros

- 📝 Notes

  - Tier recipes now route through `_anvil-run`, and a new `anvil-runner`
    managed region is emitted into the root `Justfile`. Repositories that do
    not opt into the container backend keep running natively by default, but
    regenerating does change the emitted tree.
  - Non-recipe container assets live under `.anvil/container/`; the entry
    recipe stays in `justfiles/` as a single `anvil/container.just` file.

## [0.3.0] - 2026-07-22

Reconstructed from commit history; this crate had no changelog at the time.

- ✨ Features

  - expand the GitHub backend and refresh the ADO pipeline templates ([#60](https://github.com/microsoft/ox-tools/pull/60))

- 🐛 Bug Fixes

  - run semver-checks against the pull request's target-branch baseline ([#56](https://github.com/microsoft/ox-tools/pull/56))

## [0.2.1] - 2026-07-13

Reconstructed from commit history; this crate had no changelog at the time.

- ✨ Features

  - catalog extensibility for downstream tools ([#45](https://github.com/microsoft/ox-tools/pull/45))
  - expanded check catalog, multi-region host files, and Windows/msrustup
    setup robustness ([#48](https://github.com/microsoft/ox-tools/pull/48))

- 🐛 Bug Fixes

  - correct parsing of affected packages in `anvil-loom` ([#54](https://github.com/microsoft/ox-tools/pull/54))

## [0.1.0] - 2026-06-16

Reconstructed from commit history; this crate had no changelog at the time.

- ✨ Features

  - introduce `cargo-anvil` for unified Rust build and CI scaffolding ([#33](https://github.com/microsoft/ox-tools/pull/33))
