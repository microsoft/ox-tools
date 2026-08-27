# cargo-ensure-no-unused-workspace-deps — Design

> Status: **Adopted**.
> Crate name: `cargo-ensure-no-unused-workspace-deps`.
> Home: `github.com/microsoft/ox-tools`, published to crates.io.

## 1. Problem

A workspace root declares a dependency catalog in `[workspace.dependencies]`, and
members draw from it with `dep = { workspace = true }`. Nothing requires an entry to
be drawn from. An entry that no member inherits stays in the manifest forever: it is
syntactically valid, it never enters the dependency graph, and no build ever fails
because of it.

Such entries are not inert. They carry version requirements, so they surface in
dependency review, attract Dependabot bumps, and add lines that `cargo sort` and
every manifest diff must carry. They also mislead: a reader takes the catalog as the
list of things this repository depends on.

The existing unused-dependency gate cannot see them. `cargo udeps` resolves the crate
graph and asks which *declared* dependencies go unreferenced; an entry that no member
declares is absent from that graph entirely. The same blind spot applies to
`cargo machete`. In this repository the gap accumulated 48 stale entries before a
manual sweep removed them — with `udeps` green throughout.

The missing check is a different question from the one `udeps` answers, and a much
cheaper one: *is this catalog entry inherited by anybody?* That is a fact about the
manifests, decidable by reading them.

## 2. Goals

1. **Close the catalog blind spot.** Fail when `[workspace.dependencies]` holds an
   entry that no workspace member inherits.
2. **No false positives.** The verdict rests only on manifest text. A gate that
   occasionally accuses a load-bearing dependency would be turned off.
3. **Cheap.** No compilation, no toolchain pin, no network. Fast enough for the
   text/metadata tier that runs on every pull request.
4. **Cargo-native UX.** Ship as `cargo ensure-no-unused-workspace-deps`, matching its
   sibling gates `cargo ensure-no-cyclic-deps` and `cargo ensure-no-default-features`.
5. **Mechanical remediation.** Removing an entry nobody inherits is lossless, so the
   tool offers `--fix` rather than leaving a 48-entry sweep to hand editing.

## 3. Non-goals

- **Judging whether an inherited dependency is used in code.** That is the
  compile-accurate question `udeps` already answers. This tool stops at inheritance.
- **Editing member manifests.** Only the workspace root is ever written.
- **Feature or version opinions.** `ensure-no-default-features` covers the catalog's
  feature hygiene; version policy belongs to `cargo-aprz` and `deny`.

## 4. Detection rule

An entry `name` in `[workspace.dependencies]` is **unused** when no workspace member
manifest contains a dependency declaration whose *key* is `name` and which sets
`workspace = true`.

Keying on the declaration name rather than the resolved package is exact, not an
approximation: inheritance is by catalog key. A member writing
`rustdoc-types-v57 = { workspace = true }` can only be served by the catalog key
`rustdoc-types-v57`, whatever `package = "…"` rename the catalog entry carries.

Members are enumerated from `cargo metadata --no-deps`, which yields the workspace
member set — including a root that is itself a package — after Cargo has applied
`members`, globs, `exclude`, and nested-workspace rules. Re-deriving that set from
the manifest would be cheaper but risks disagreeing with Cargo, and every
disagreement that drops a member is a false positive. `--no-deps` keeps the call
free of dependency resolution.

Each member manifest is scanned for:

- `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`;
- the same three tables under any `[target.'cfg(…)']`;
- both the inline form `dep = { workspace = true }` and the dotted form
  `dep.workspace = true`.

Anything that inherits, anywhere, in any target, marks the entry as used. The scan is
deliberately permissive: it errs toward *used*.

## 5. User-visible shape

### Invocation

```bash
cargo ensure-no-unused-workspace-deps [--manifest-path <PATH>] [--fix] [--require-workspace]
```

| Option                | Default      | Meaning                                                              |
|-----------------------|--------------|----------------------------------------------------------------------|
| `--manifest-path`     | `Cargo.toml` | Workspace root manifest to check, relative to the current directory. |
| `--fix`               | *(off)*      | Remove the unused entries instead of only reporting them.            |
| `--require-workspace` | *(off)*      | Treat a manifest with no `[workspace]` table as an error.            |

### Manifests that are not workspace roots

A manifest with no `[workspace]` table has no catalog, so there is nothing this
check can be wrong about. It reports that on stderr and succeeds.

That default exists because the check is invoked from cargo-anvil, which manages
single-crate repositories as well as workspaces. A generated recipe runs the same
command everywhere, so a hard error here would make the check unusable in exactly
the repositories that never had the problem, and each of them would need a local
opt-out. Succeeding is also the honest answer: the property "no catalog entry goes
uninherited" holds vacuously.

`--require-workspace` restores the strict reading for callers that know they are
pointing at a workspace root and want a misdirected `--manifest-path` to fail rather
than pass quietly.

### Allowed entries

A deliberate exception is declared in the workspace manifest, not on the command
line, because the generated CI recipe invokes the tool with a fixed argument list:

```toml
[workspace.metadata.ensure-no-unused-workspace-deps]
allowed = ["kept-on-purpose"]
```

An allowed name is neither reported nor removed. An `allowed` entry that matches no
unused entry produces a warning on stderr without changing the exit code — a stale
exception is a maintenance smell, and failing the build for one would punish the act
of fixing the underlying problem.

### Reporting

Unused entries are written to stderr, one per line, naming the entry and the
manifest that declares it. Order follows the manifest so the report reads alongside
the file. The success line goes to stdout.

### Exit codes

| Code | Meaning                                                                                          |
|------|--------------------------------------------------------------------------------------------------|
| 0    | No unused entries — or, under `--fix`, all unused entries were removed and the manifest written. |
| 1    | Unused entries found without `--fix`, or a manifest could not be read, parsed, or enumerated.     |

A `[workspace]` table with no `dependencies` catalog is a clean pass — there is
nothing to be stale. A manifest with no `[workspace]` table at all is a pass with a
note on stderr, or an error under `--require-workspace`.

The exit code is returned from `run` as an `ExitCode` rather than raised with
`std::process::exit`, so `main` unwinds normally. That matters under coverage
instrumentation, where an abrupt exit can skip the profile flush on some platforms.

### `--fix`

The manifest is rewritten with `toml_edit`, so formatting, ordering, and comments on
surviving entries are preserved. Comment handling follows the manifest's own reading
order:

- Comments attached to a removed entry are carried forward to the next surviving
  entry, so a group header such as `# --- external dependencies ---` keeps labeling
  the group it introduces.
- When the removed entries are the last in the table, the carried comments are
  appended after the final surviving entry's *value*, keeping them at the end of the
  table where they were written. Attaching them to that entry's key prefix would
  hoist them above it and relabel a surviving dependency.
- When every entry is removed, the carried comments go with them. A header for a
  group that no longer exists is not worth preserving.

Only comment-bearing decor is carried; blank-line padding from a removed entry is
dropped.

`Cargo.lock` is unaffected by construction: an entry no member inherits never
contributed a node to the dependency graph. The tool never touches the lockfile, and
a lockfile that changes after a fix indicates unrelated drift.

The workspace root is the one file whose loss breaks every other tool in the
repository, so it is never truncated in place. The replacement is written to a
temporary file in the manifest's own directory and renamed over the original, which
is atomic on a single filesystem. Before that rename the file is re-read and compared
against the bytes that were parsed: `cargo metadata` runs in between as a subprocess,
which is a wide enough window for an editor to save into, and an edit that lands there
aborts the fix rather than being overwritten.

Replacing a file by rename brings the temporary file's identity with it, so two
properties an in-place write would have kept are restored deliberately. The manifest's
permissions are read first and applied to the replacement, because a temporary file is
created owner-only and a rename carries its mode rather than inheriting the target's —
otherwise a world-readable manifest silently comes back owner-only, which git does not
track. A symlinked manifest is resolved first, so the rename lands on the file the link
points at instead of replacing the link with a regular file.

#### Carried comments can be misattributed

A group header and a note about one specific dependency are the same thing to the
parser — comment lines in an entry's decor, whether that decor sits on the key or, for
a `[workspace.dependencies.name]` entry, on the table. When the noted entry is the one
removed, its note lands on the next surviving entry and reads as if it were written
about that one, which is worse than dropping it: a dropped comment shows up in the
`--fix` diff, a wrong attribution outlives it.

The carry-forward still earns its keep for headers, so it stays, and the relocation is
made visible instead: every move is reported on stderr, naming the entries the
comments came from and the entry they landed on, so whoever reviews the diff knows
which lines to check.

Comments cannot always be placed. Only a plain value has a suffix to append to, so when
the removed entries are last in the table and the final survivor is a dotted key or a
sub-table — and when every entry is removed and nothing survives — the comments go with
the group they introduced, reported as a drop. The report describes what happened
rather than what was attempted: claiming a move that did not happen would send the
reviewer hunting for text that is not in the diff.

## 6. Relationship to the other dependency checks

| Question                                                | Answered by                  |
|---------------------------------------------------------|------------------------------|
| Is this catalog entry inherited by any member?          | this tool                    |
| Is an inherited dependency actually referenced in code? | `udeps`                      |
| Is it declared with explicit features?                  | `ensure-no-default-features` |

The first two compose without overlap and without gaps: this tool is manifest-only
and cannot be fooled by macro-hidden imports; `udeps` is compile-accurate and cannot
see uninherited entries.

**`cargo-shear` was evaluated and rejected as the vehicle.** It does implement a
`shear/unused_workspace_dependency` diagnostic, but derives it from static
source-usage analysis, so its verdict inherits that analysis's macro-expansion blind
spots — on this repository it reports eight entries that are inherited and genuinely
used through macro arguments. It also skips the check for single-member workspaces.
Its other diagnostics remain independently interesting; that is a separate decision.
The dormant `cargo-unused-workspace-deps` crate (one release in 2025, no commits
since) was likewise rejected as a pinned dependency.

## 7. CI integration

The check belongs in the `modified` tier, running in the `pr-fast` group as
`cargo ensure-no-unused-workspace-deps` alongside `ensure-no-cyclic-deps` and
`ensure-no-default-features`. It is a text/metadata check: one platform is enough and
no toolchain pin is required.

**Not yet wired.** Anvil installs pinned tools from crates.io, so the wiring — a
pinned version in `versions.just`, install and validate recipes in `tools.just`, a
check recipe in `checks/`, and the `pr-fast` entry — follows the crate's first
release. Until then the tool is published and runnable but enforces nothing here.

Because the tool reads the workspace root, it runs once from the repository root
rather than per affected package. Single-crate repositories run the same command and
pass without configuration.

## 8. Out of scope

- Unused entries in the `[workspace.dependencies]` of a *nested* workspace. Each
  workspace root is checked on its own terms by its own invocation.
- `[patch]`, `[replace]`, and `[profile]` tables.
- Any judgment about whether an inherited dependency *should* be inherited.
