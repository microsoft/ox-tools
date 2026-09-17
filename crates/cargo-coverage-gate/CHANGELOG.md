# Changelog

## [Unreleased]

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
