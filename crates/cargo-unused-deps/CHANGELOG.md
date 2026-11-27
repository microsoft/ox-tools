# Changelog

## [Unreleased]

## [0.3.0] - 2026-11-26

- ✨ Features

  - unused-deps: accept package selection files ([#395](https://github.com/microsoft/ox-tools/pull/395))

## [0.2.1] - 2026-11-25

- ✨ Features

  - cargo-anvil: add cargo-unused-deps check ([#378](https://github.com/microsoft/ox-tools/pull/378))
  - cargo-gamma: improve campaign reliability and mutation detection ([#282](https://github.com/microsoft/ox-tools/pull/282))
  - cargo-gamma: resume interrupted campaigns and streamline mutant testing ([#248](https://github.com/microsoft/ox-tools/pull/248))
  - cargo-gamma: improve campaign scheduling and UX ([#208](https://github.com/microsoft/ox-tools/pull/208))
  - replace direct anyhow usage with ohno in Cargo tools ([#206](https://github.com/microsoft/ox-tools/pull/206))

- 🐛 Bug Fixes

  - cargo-unused-deps: select workspace packages unambiguously ([#394](https://github.com/microsoft/ox-tools/pull/394))
  - cargo-gamma: strengthen admission proofs and compiler isolation ([#367](https://github.com/microsoft/ox-tools/pull/367))
  - exclude test modules from coverage ([#229](https://github.com/microsoft/ox-tools/pull/229))

- ✔️ Tasks

  - update dependencies, build tools, actions, and Rust ([#330](https://github.com/microsoft/ox-tools/pull/330))

- 🧩 Miscellaneous

  - Adopt cargo-gamma in Anvil and remediate campaign survivors ([#327](https://github.com/microsoft/ox-tools/pull/327))
  - Kill a bunch of mutants ([#315](https://github.com/microsoft/ox-tools/pull/315))
  - Address Sander's 'Tsunami of Feedback' ([#221](https://github.com/microsoft/ox-tools/pull/221))

## [0.2.0] - 2026-09-23

- ✨ Features

  - complete dependency analysis
  - implement the unused and misplaced checks, doctests included
  - adopt cargo-unused-deps

- 🐛 Bug Fixes

  - preserve scoped wrapper evidence
  - resolve member workspace roots
  - harden scoped evidence
  - preserve target scope in remediation
  - preserve feature dependency declarations
  - handle feature-forwarding dependencies
  - report target declaration context
  - isolate evidence runs
  - normalize doctest diagnostics
  - make evidence collection authoritative
  - preserve compiler configuration
  - harden scoped evidence
  - make package scoping sound
  - count doctest reports instead of treating any as disuse
  - stop inferring which unit reported; measure it with a second pass
  - apply the monotonicity rule to development targets too

- ⚡ Performance

  - gather doctest evidence only for packages with findings

- 📚 Documentation

  - refresh release notes
  - refresh release notes
  - refresh release notes
  - refresh release notes
  - refresh release notes
  - refresh release notes
  - refresh release notes
  - refresh release notes
  - refresh 0.2.0 changelog
  - generate 0.2.0 changelog

- ♻️ Code Refactoring

  - keep CLI runner private

## [0.1.0] - 2026-09-15

- Initial release.
