# Changelog

## [0.4.0] - 2026-08-04

- ✨ Features

  - add Docker-in-WSL container backend ([#59](https://github.com/microsoft/ox-tools/pull/59))

- ♻️ Code Refactoring

  - move container assets out of justfiles ([#63](https://github.com/microsoft/ox-tools/pull/63))

- 📝 Notes

  - Tier recipes now route through `_anvil-run`, and a new `anvil-runner`
    managed region is emitted into the root `Justfile`. Repositories that do
    not opt into the container backend keep running natively by default, but
    regenerating does change the emitted tree.
  - Non-recipe container assets live under `.anvil/container/`; only
    `container.just` remains under `justfiles/anvil/container/`.
