# Changelog

## [Unreleased]

## [0.7.2] - 2026-10-26

- 🐛 Bug Fixes

  - cargo-coverage-gate: verify compiler source provenance
  - cargo-coverage-gate: retain ambiguous include coverage
  - cargo-coverage-gate: correct feature guards and editions ([#280](https://github.com/microsoft/ox-tools/pull/280))

- 📚 Documentation

  - cargo-coverage-gate: use clear ownership wording

- ✔️ Tasks

  - cargo-coverage-gate: prepare 0.7.2 release

## [0.7.1] - 2026-10-20

- 🐛 Bug Fixes

  - cargo-coverage-gate: preserve alternate dependency activation paths
  - cargo-coverage-gate: honor dependency and edition metadata

## [0.7.0] - 2026-10-19

- ⚠️ Breaking

  - coverage-gate: document 0.7 migration

- ✨ Features

  - cargo-gamma: resume interrupted campaigns and streamline mutant testing ([#248](https://github.com/microsoft/ox-tools/pull/248))
  - cargo-gamma: improve campaign scheduling and UX ([#208](https://github.com/microsoft/ox-tools/pull/208))

- 🐛 Bug Fixes

  - coverage-gate: model implicit dependency features
  - coverage-gate: distinguish dependency features
  - coverage-gate: attribute external target roots
  - coverage-gate: honor target feature contracts
  - coverage-gate: satisfy CI and review contracts
  - release: allow same-version metadata refresh
  - coverage-gate: close final filtering gaps
  - coverage-gate: close source traversal gaps
  - coverage-gate: compose cfg guards across sources
  - coverage-gate: normalize cfg string atoms
  - coverage-gate: harden collection diagnostics
  - coverage-gate: compose source guards and includes
  - coverage-gate: label malformed collected reports
  - coverage-gate: recognize qualified include macros
  - coverage-gate: close source ownership gaps
  - coverage-gate: preserve symlink module resolution
  - coverage-gate: close source analysis gaps
  - coverage-gate: harden source discovery contracts
  - coverage-gate: preserve module inspection errors
  - coverage-gate: address PR review findings
  - coverage-gate: address self-review findings
  - coverage: make test attribute policy explicit
  - coverage: harden cfg and module resolution
  - coverage-api: preserve raw evaluation compatibility
  - coverage: filter reports to released surfaces
  - exclude test modules from coverage ([#229](https://github.com/microsoft/ox-tools/pull/229))

- ⚡ Performance

  - coverage-gate: tighten source classification

- 📚 Documentation

  - coverage-gate: scope filtering contracts

- ✔️ Tasks

  - coverage-gate: prepare 0.7.0 release
  - coverage: defer Anvil rollout until release

- ♻️ Code Refactoring

  - coverage-api: default to released surfaces

- 🧩 Miscellaneous

  - Address Sander's 'Tsunami of Feedback' ([#221](https://github.com/microsoft/ox-tools/pull/221))

## [0.6.0] - 2026-09-23

- ✨ Features

  - orchestrate coverage collection ([#179](https://github.com/microsoft/ox-tools/pull/179))

- 🐛 Bug Fixes

  - eliminate unkillable format_delta mutant ([#197](https://github.com/microsoft/ox-tools/pull/197))

## [0.5.0] - 2026-09-14

- ✨ Features

  - replace legacy CI with aggregate gate ([#158](https://github.com/microsoft/ox-tools/pull/158))

- 🐛 Bug Fixes

  - stop the fake rustc tests flaking on ETXTBSY ([#157](https://github.com/microsoft/ox-tools/pull/157))

- 🧩 Miscellaneous

  - Preserve directional coverage deltas
  - Bump cargo-coverage-gate to 0.5.0
  - Display coverage percentages conservatively
  - Fix coverage threshold rounding

## [0.4.0] - 2026-08-28

- ✨ Features

  - add target-specific policies
  - add cargo-aprz and cargo-ensure-no-default-features ([#76](https://github.com/microsoft/ox-tools/pull/76))
  - run a command per workspace member with cargo-style selection ([#61](https://github.com/microsoft/ox-tools/pull/61))
  - add expect-no-coverable-lines assertion ([#51](https://github.com/microsoft/ox-tools/pull/51))

- 🐛 Bug Fixes

  - preserve cross-package coverage
  - resolve effective target policies lazily

- 📚 Documentation

  - document configuration capabilities

- ♻️ Code Refactoring

  - tighten target policy contracts
  - reuse thresholds for target opt-outs
