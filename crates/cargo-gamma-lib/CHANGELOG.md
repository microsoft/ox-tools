# Changelog

## [Unreleased]

## [0.4.0] - 2026-10-28

- ✨ Features

  - cargo-gamma: enforce memory ceilings on macOS by sampling ([#249](https://github.com/microsoft/ox-tools/pull/249))
  - cargo-gamma: filter mutants by regex ([#242](https://github.com/microsoft/ox-tools/pull/242))
  - cargo-anvil: roll out released-surface coverage
  - cargo-gamma: improve campaign reliability and mutation detection ([#282](https://github.com/microsoft/ox-tools/pull/282))

- 🐛 Bug Fixes

  - cargo-gamma: support binary-only package selections ([#241](https://github.com/microsoft/ox-tools/pull/241))

- 🧩 Miscellaneous

  - Kill a bunch of mutants ([#315](https://github.com/microsoft/ox-tools/pull/315))

## [0.3.0] - 2026-10-12

- ✨ Features

  - resume interrupted campaigns and streamline mutant testing ([#248](https://github.com/microsoft/ox-tools/pull/248))
  - improve campaign scheduling and UX ([#208](https://github.com/microsoft/ox-tools/pull/208))
  - optimize campaigns and harden mutation results ([#198](https://github.com/microsoft/ox-tools/pull/198))
  - optimize mutation campaigns ([#187](https://github.com/microsoft/ox-tools/pull/187))

- 🐛 Bug Fixes

  - preserve nonzero measured baselines ([#238](https://github.com/microsoft/ox-tools/pull/238))
  - address post-merge review feedback ([#236](https://github.com/microsoft/ox-tools/pull/236))
  - exclude test modules from coverage ([#229](https://github.com/microsoft/ox-tools/pull/229))
  - skip bin-only doctest scopes ([#218](https://github.com/microsoft/ox-tools/pull/218))
  - support standalone toolchains in environment tests ([#194](https://github.com/microsoft/ox-tools/pull/194))

- 🧩 Miscellaneous

  - Release cargo-gamma 0.3.0 ([#227](https://github.com/microsoft/ox-tools/pull/227))
  - Address Sander's 'Tsunami of Feedback' ([#221](https://github.com/microsoft/ox-tools/pull/221))

## [0.2.0] - 2026-09-03

- 🐛 Bug Fixes

  - harden discovery and process cleanup
  - address hardening feedback ([#144](https://github.com/microsoft/ox-tools/pull/144))

## [0.1.0] - 2026-09-02

- Initial release.
