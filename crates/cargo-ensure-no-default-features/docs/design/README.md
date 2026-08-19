# cargo-ensure-no-default-features — Design

> Status: **Adopted**.
> Crate name: `cargo-ensure-no-default-features`.
> Home: `github.com/microsoft/ox-tools`, published to crates.io.

## 1. Problem

Cargo enables a dependency's *default* feature set unless a manifest explicitly
opts out with `default-features = false`. Those implicit features are invisible in
the manifest, so they accumulate silently: build times grow, binaries get larger,
and the set of code that has to be audited and kept patched expands without anyone
making a decision to expand it.

Reviewing this by eye does not scale. A single forgotten `default-features = false`
in a dependency bump is easy to miss in a diff and impossible to spot later without
re-reading the whole manifest. What the repo wants is a mechanical gate: a check
that fails the build the moment an implicitly-featured dependency is added.

## 2. Goals

1. **Mechanical enforcement.** Every dependency declaration must be a table
   carrying `default-features = false`; anything else is an error, including the
   terse `name = "1.0"` string form.
2. **Cargo-native UX.** Ship as a cargo sub-command (`cargo ensure-no-default-features`)
   so it composes with the rest of the tool chain and needs no wrapper script.
3. **Workspace-aware.** Recognize the workspace/member split that Cargo itself
   uses, rather than requiring separate invocations with hand-written paths.
4. **Escape hatch with feedback.** Allow deliberate exceptions, but tell the user
   when a listed exception no longer matches anything so the list cannot rot.

## 3. Non-goals

- Resolving the dependency graph. The tool reads manifests textually and never
  invokes `cargo metadata`; it is intentionally cheap enough to run on every PR.
- Judging *which* features are enabled. Feature selection is a project decision;
  the tool only requires that the selection be explicit.
- Fixing manifests. The tool reports; the human edits.

## 4. User-visible shape

### Invocation

```bash
cargo ensure-no-default-features [--manifest-path <PATH>] [--exceptions <NAMES>]
```

| Option            | Default      | Meaning                                                |
|-------------------|--------------|--------------------------------------------------------|
| `--manifest-path` | `Cargo.toml` | Manifest to check, relative to the current directory.   |
| `--exceptions`/`-e` | *(empty)*  | Comma-separated dependency names exempt from the check. |

### Sections checked

The manifest is inspected once and up to two sections are checked:

- `[workspace.dependencies]` — checked when a `[workspace]` table is present.
- `[dependencies]` — checked when a `[package]` table is present.

Both are checked when both are present, which is the normal shape of a workspace
root that also builds a crate. A manifest with neither section is an error rather
than a silent pass: it almost always means the wrong file was pointed at.

Dependencies declared with `workspace = true` are skipped. They inherit their
feature configuration from the workspace root, which is itself checked, so
flagging them would report the same problem twice and at the wrong location.

### Exit codes

| Code | Meaning                                                                    |
|------|----------------------------------------------------------------------------|
| 0    | Every checked dependency is explicit; the run succeeded.                    |
| 1    | At least one dependency is missing `default-features = false`, or the manifest could not be read or parsed. |

Violations are written to stderr, one per line, naming the dependency and the
specific defect (missing key, `default-features = true`, non-boolean value, or the
bare version-string form). The success message goes to stdout.

An exception naming a dependency that does not appear in any checked section
produces a warning on stderr but does not change the exit code: a stale exception
is a maintenance smell, not a correctness failure, and failing the build for it
would make removing a dependency needlessly disruptive.

The exit code is returned from `run` as an `ExitCode` rather than raised with
`std::process::exit`, so `main` unwinds normally. That matters under coverage
instrumentation, where an abrupt exit can skip the profile flush on some platforms.

## 5. CI integration

The check is one of the fast pull-request gates and runs as
`cargo ensure-no-default-features` from the repo root, where the workspace manifest
holds the dependency declarations that member crates inherit.

## 6. Out of scope

- Checking `[build-dependencies]` and `[dev-dependencies]` tables. In the
  workspace-inheritance model these overwhelmingly use `workspace = true`, so the
  declarations that matter are already covered by the workspace root.
- Any form of automatic remediation.
