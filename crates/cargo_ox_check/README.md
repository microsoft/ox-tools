# Cargo Ox-Check

> ⚠️ This README is auto-generated from doc-comments in `src/lib.rs`. Run
> `just readme` from the repo root to regenerate after edits.

See [`src/lib.rs`](./src/lib.rs) for the full doc-comment; once `just
readme` is run it will render here.

In the meantime, see:

- [`docs/design/design.md`](./docs/design/design.md) — top-level design.
- [`docs/design/checks.md`](./docs/design/checks.md) — the check catalog.
- [`docs/design/updates.md`](./docs/design/updates.md) — the drift-detection algorithm.
- [`docs/design/github.md`](./docs/design/github.md) — GitHub Actions emission.
- [`docs/design/ado.md`](./docs/design/ado.md) — Azure DevOps Pipelines emission.
- [`docs/design/local.md`](./docs/design/local.md) — the `justfiles/ox-check/` tree.
- [`docs/verification.md`](./docs/verification.md) — continuous-validation strategy.
- [`docs/implementation-plans/0000.md`](./docs/implementation-plans/0000.md) — the
  implementation breakdown this code lives under.

## Installation

```bash
cargo install --locked cargo-ox-check
```

## Usage

```text
cargo ox-check update [--backend <name>]... [--no-backends] [--dry-run]
```
