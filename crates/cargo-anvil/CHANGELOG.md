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

  - bump the `toml_edit` requirement from 0.22.22 to 0.25.12 ([#44](https://github.com/microsoft/ox-tools/pull/44))
  - bump `syn` from 2.0.117 to 2.0.119 ([#57](https://github.com/microsoft/ox-tools/pull/57)) —
    lockfile only; `syn` reaches `cargo-anvil` transitively through the
    `clap_derive` and `ohno_macros` proc macros

- 📝 Notes

  - Tier recipes now route through `_anvil-run`, and a new `anvil-runner`
    managed region is emitted into the root `Justfile`. Repositories that do
    not opt into the container backend keep running natively by default, but
    regenerating does change the emitted tree.
  - Non-recipe container assets live under `.anvil/container/`; the entry
    recipe stays in `justfiles/` as a single `anvil/container.just` file.

## [0.3.0] - 2026-07-22

Reconstructed from commit history; this crate had no changelog at the time.

- ✨ Features

  - expand the GitHub backend and refresh the ADO pipeline templates ([#60](https://github.com/microsoft/ox-tools/pull/60))

- 🐛 Bug Fixes

  - run semver-checks against the pull request's target-branch baseline ([#56](https://github.com/microsoft/ox-tools/pull/56))

## [0.2.1] - 2026-07-13

Reconstructed from commit history; this crate had no changelog at the time.

- ✨ Features

  - catalog extensibility for downstream tools ([#45](https://github.com/microsoft/ox-tools/pull/45))
  - expanded check catalog, multi-region host files, and Windows/msrustup
    setup robustness ([#48](https://github.com/microsoft/ox-tools/pull/48))

- 🐛 Bug Fixes

  - correct parsing of affected packages in `anvil-loom` ([#54](https://github.com/microsoft/ox-tools/pull/54))

## [0.1.0] - 2026-06-16

Reconstructed from commit history; this crate had no changelog at the time.

- ✨ Features

  - introduce `cargo-anvil` for unified Rust build and CI scaffolding ([#33](https://github.com/microsoft/ox-tools/pull/33))
