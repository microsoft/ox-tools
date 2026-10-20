# Changelog

## [Unreleased]

## [0.7.0] - 2026-10-20

- ⚠️ Breaking

  - cargo-coverage-gate: filter coverage to released surfaces

- ✨ Features

  - cargo-gamma: resume interrupted campaigns and streamline mutant testing ([#248](https://github.com/microsoft/ox-tools/pull/248))
  - cargo-gamma: improve campaign scheduling and UX ([#208](https://github.com/microsoft/ox-tools/pull/208))

- 🐛 Bug Fixes

  - exclude test modules from coverage ([#229](https://github.com/microsoft/ox-tools/pull/229))

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
