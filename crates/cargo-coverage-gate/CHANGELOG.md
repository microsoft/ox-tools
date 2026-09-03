# Changelog
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

