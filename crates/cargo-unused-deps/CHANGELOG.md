# Changelog

## [Unreleased]

## [0.2.0] - 2026-09-18

- ✨ Features

  - complete dependency analysis
  - implement the unused and misplaced checks, doctests included
  - adopt cargo-unused-deps

- 🐛 Bug Fixes

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
  - refresh 0.2.0 changelog
  - generate 0.2.0 changelog

## [0.1.0] - 2026-09-15

- Initial release.
