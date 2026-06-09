# cargo-ox-check ![License: MIT](https://img.shields.io/badge/license-MIT-blue) [![cargo-ox-check on crates.io](https://img.shields.io/crates/v/cargo-ox-check)](https://crates.io/crates/cargo-ox-check) [![cargo-ox-check on docs.rs](https://docs.rs/cargo-ox-check/badge.svg)](https://docs.rs/cargo-ox-check) [![Source Code Repository](https://img.shields.io/badge/Code-On%20GitHub-blue?logo=GitHub)](https://github.com/microsoft/ox-tools/tree/main/crates/cargo_ox_check) [![Rust Version: 1.88.0](https://img.shields.io/badge/rustc-1.88.0-orange.svg)](https://github.com/rust-lang/rust/releases/tag/1.88.0)

## cargo-ox-check

Opinionated, unified Rust build/CI scaffolding for GitHub Actions and
Azure DevOps Pipelines. One opinionated check catalog, two CI
backends, generated from the same source of truth.

### What it does

`cargo-ox-check` writes files. `just` runs them. The repo composes
everything. The tool itself is not on the local-build hot path or in
the CI graph at runtime — it is a code generator that you re-run when
you want to upgrade the opinionated baseline.

Each run of `cargo ox-check update` writes:

* The `justfiles/ox-check/` recipe tree (`tools.just`, `checks.just`,
  `groups.just`, `tiers.just`) — owned files.
* A managed region in your `Justfile` that imports them.
* A managed region in your workspace `Cargo.toml` carrying
  `[workspace.lints]` in dotted-key form, plus a `[lints] workspace = true` region in each workspace member.
* Managed regions in `deny.toml`, `rustfmt.toml`, and `.delta.toml`.
* For each selected CI backend (`github`, `ado`), the full set of
  composite actions / step templates, reusable workflows / stages
  templates, and root workflows / pipelines.

Outside the managed regions, your content is preserved byte-for-byte.

### Installation

```bash
cargo install --locked cargo-ox-check
```

Only the maintainer who runs updates needs the binary. Everyone else
uses `just` (or plain `cargo`).

### Usage

```text
cargo ox-check update [--backend <name>]... [--no-backends] [--dry-run]
```

`update` is the only subcommand. There is no separate `init`,
`migrate`, `check`, `enable`, or `disable`. The algorithm is uniform
— first runs and subsequent runs go through the same decision table.

Flags:

* `--backend <name>` — repeatable. Valid values: `github`, `ado`. If
  omitted, the backend is autodetected from the `origin` git remote.
* `--no-backends` — emit only local files; skip every CI backend.
  Mutually exclusive with `--backend`.
* `--dry-run` — analyze without writing. Exits 1 if anything would be
  written or proposed.

### Daily driver

After the first run, your daily workflow is plain `just`:

```text
$ just ox-check          # alias for `just ox-check-pr`
$ just ox-check-pr       # the PR tier
$ just ox-check-nightly  # the nightly tier
$ just ox-check-full     # both, sequentially
```

CI invokes the same recipes. Local and CI are bit-identical because
they share one implementation in the imported `.just` files.

### Customization

Four escape valves, in increasing severity:

1. **Compose around the tool**: add your own `.just` files or
   workflows; the tool never touches anything not prefixed
   `ox-check-`.
1. **Extend managed regions** outside the sentinels — add lints,
   deny rules, etc. The tool preserves everything outside.
1. **Opt out by emptying** a managed region or owned file. The tool
   will skip the item on every future `update` and only emit a
   `.ox-check-proposed` sibling when the template actually changes.
1. **Take ownership by editing inside** an owned file or managed
   region. The next `update` detects the dirt and writes a
   `.ox-check-proposed` sibling instead of overwriting.

### Design docs

See `docs/design/` for the full architecture:

* `design.md` — overall principles and CLI shape.
* `checks.md` — the opinionated check catalog.
* `local.md` — the `justfiles/ox-check/` tree.
* `updates.md` — the drift-detection algorithm.
* `github.md` — GitHub Actions emission.
* `ado.md` — Azure DevOps Pipelines emission.

And `docs/verification.md` for the continuous-validation strategy.
