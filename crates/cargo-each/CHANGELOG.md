# Changelog

## [Unreleased]

## [0.6.0] - 2026-10-19

- ✨ Features

  - add `{cargo-spec}` for source-qualified Cargo package selection without changing `{spec}` ([#278](https://github.com/microsoft/ox-tools/pull/278))
  - add `--list-json` with manifest paths, package names, Cargo selectors, and literal metadata IDs ([#278](https://github.com/microsoft/ox-tools/pull/278))
  - add `--metadata-toolchain` to select the toolchain for metadata queries independently of child commands ([#278](https://github.com/microsoft/ox-tools/pull/278))

## [0.5.0] - 2026-10-13

- ⚠️ Breaking

  - remove --exclude-filter; use --filter 'not ...'

- ✨ Features

  - resume interrupted campaigns and streamline mutant testing ([#248](https://github.com/microsoft/ox-tools/pull/248))
  - add doctest capability filter

- ✔️ Tasks

  - release v0.5.0

- ♻️ Code Refactoring

  - unify package filtering

- 🧩 Miscellaneous

  - Merge remote-tracking branch 'origin/main' into makolnek-microsoft-cargo-each-doctest-filter

## [0.4.0] - 2026-10-06

- ✨ Features

  - add optional workspace version and JSON records ([#216](https://github.com/microsoft/ox-tools/pull/216))

- 🐛 Bug Fixes

  - harden command scheduling and process-boundary failure handling ([#208](https://github.com/microsoft/ox-tools/pull/208))
  - simplify timeout cleanup ([#222](https://github.com/microsoft/ox-tools/pull/222))

- 🔧 Maintenance

  - make parallel output ordering tests deterministic and simplify internal failure plumbing ([#221](https://github.com/microsoft/ox-tools/pull/221))

## [0.3.0] - 2026-09-23

- ✨ Features

  - add portable execution inputs ([#176](https://github.com/microsoft/ox-tools/pull/176))

## [0.2.0] - 2026-09-11

- Initial release.
