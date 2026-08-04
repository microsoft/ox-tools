# Changelog

## [0.4.0] - 2026-08-04

- ✨ Features

  - add Docker-in-WSL container backend ([#59](https://github.com/microsoft/ox-tools/pull/59))

- ♻️ Code Refactoring

  - move container assets out of justfiles ([#63](https://github.com/microsoft/ox-tools/pull/63))

- 📝 Documentation

  - rename the design doc to `README.md` and update the in-code links that
    referenced its numbered sections ([#61](https://github.com/microsoft/ox-tools/pull/61))
  - add the crate logo ([#61](https://github.com/microsoft/ox-tools/pull/61))

- 🔧 Dependencies

  - bump `toml_edit` from 0.22.27 to 0.25.12+spec-1.1.0 ([#44](https://github.com/microsoft/ox-tools/pull/44))
  - bump `syn` from 2.0.117 to 2.0.119 ([#57](https://github.com/microsoft/ox-tools/pull/57)) —
    lockfile only; `cargo-anvil` does not depend on `syn`

- 📝 Notes

  - Tier recipes now route through `_anvil-run`, and a new `anvil-runner`
    managed region is emitted into the root `Justfile`. Repositories that do
    not opt into the container backend keep running natively by default, but
    regenerating does change the emitted tree.
  - Non-recipe container assets live under `.anvil/container/`; only
    `container.just` remains under `justfiles/anvil/container/`.
