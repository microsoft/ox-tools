# Changelog

## [1.1.0] - 2026-02-16

- ✨ Features

  - support plain (non-workspace) crates by checking `[dependencies]`
  - support workspace member crates, automatically skipping dependencies that use `workspace = true`
  - check both sections when `[workspace.dependencies]` and `[dependencies]` are present

## [1.0.2] - 2026-02-16

- 🔧 Maintenance

  - republish to deal with prior snafu

## [1.0.1] - 2026-02-16

- 📝 Documentation

  - improve command-line help

- 🔧 Maintenance

  - CI cleanup

## [1.0.0] - 2025-11-28

- ✨ Features

  - add `--exceptions` command-line option

## [0.2.0] - 2025-11-27

- 🐛 Bug Fixes

  - reduce MSRV to 1.88

## [0.1.0] - 2025-11-27

- ✨ Features

  - initial release
