# cargo-anvil — Extensibility

> **Status:** design proposal. Nothing in this document is implemented yet. It describes how
> `cargo-anvil` should be refactored so that *other* teams can ship their own cargo
> subcommand — with their own check catalog — while reusing the entire anvil engine
> (decision table, region splicing, manifest tracking, backend resolution, dry-run, summary).

This is a companion to the top-level [design.md](./design.md). It assumes familiarity with the
core concepts defined there and in [checks.md](./checks.md), [local.md](./local.md), and
[updates.md](./updates.md).

## 1. Scope and constraint

A single repository is only ever managed by **one** anvil-family tool. We explicitly do **not**
support two such tools writing to the same repo. That constraint is load-bearing for this design:
it means there is no namespace to isolate, so the on-disk vocabulary never needs to vary per fork.

Concretely, `anvil` is the name of the **engine and the on-disk format**, not of any particular
front-end binary. Every tool built on the engine emits the *same* fixed namespace:

- owned-file tree `justfiles/anvil/…`
- sidecar manifest `.anvil.lock`
- review-sibling suffix `.anvil-proposed`
- managed-region sentinels `# >>> anvil-managed: <id>` … `# <<< anvil-managed: <id>`
- region IDs `anvil-imports`, `anvil-workspace-lints`, `anvil-lints`
- recipe-name prefix `anvil-` (`anvil-pr`, `anvil-clippy`, …)

That shared vocabulary is a feature: it signals "this content is managed by the anvil engine —
don't hand-edit it," independent of which binary wrote it. A fork never rebrands any of it.

## 2. What a fork actually customizes

Because the on-disk format is fixed, a downstream tool varies exactly two things, neither of
which touches the engine vocabulary:

1. **CLI identity** — the cargo subcommand name (`cargo myforge`), `about` text, and `version`.
   This is pure clap metadata with zero engine impact.
2. **Catalog content** — the set of artifacts (owned files and managed regions, including the
   gated cloud-workflow backend files) the tool
   emits. This is the actual substance of extensibility.

Everything else — the update algorithm, drift detection, opt-out semantics, backend
autodetection, `--dry-run`, the summary — is inherited unchanged. So is the on-disk vocabulary in
§1.

## 3. Goal: a trivial downstream binary

The entire downstream binary should be one line:

```rust
// cargo-myforge/src/main.rs
use std::process::ExitCode;

fn main() -> ExitCode {
    cargo_anvil::run_app(myforge::catalog())
}
```

…plus one function that *describes the catalog* by starting from anvil's and customizing it:

```rust
// cargo-myforge/src/lib.rs
use cargo_anvil::Catalog;

pub fn catalog() -> Catalog {
    Catalog::anvil()
        .into_builder()
        .subcommand("myforge")                         // CLI identity only
        .about("MyForge: opinionated Rust build scaffolding for the Foo org")
        .version(env!("CARGO_PKG_VERSION"))
        .with_artifact(Artifact::owned_file(           // append an owned file
            "justfiles/anvil/extra.just",
            include_str!("../templates/extra.just"),
        ))
        .with_artifact(myforge_codeowners_region())    // append a managed region
        .replace_artifact(                             // swap the check recipes wholesale,
            artifacts::justfile::checks()              // derived from the built-in
                .with_body(include_str!("../templates/checks.just")),
        )
        .build()
}
```

That is the whole contract: **one line in `main`, plus a `Catalog` value.** Note the new owned
file still lives under `justfiles/anvil/` and any new region still uses `anvil-managed`
sentinels — the fork extends the anvil namespace, it does not create its own.

## 4. The shape of a catalog

```rust
pub struct Catalog {
    /// CLI identity. Cosmetic only — drives clap, never the on-disk format.
    cli: CliMeta,
    /// Ordered, keyed set of artifacts to emit.
    artifacts: Vec<Artifact>,
}

pub struct CliMeta {
    /// Cargo subcommand token (the word after `cargo`). Defaults to `anvil`.
    pub subcommand: String,
    pub bin_name: String,   // defaults to `cargo-{subcommand}`
    pub about: String,
    pub version: String,
}
```

`CliMeta` feeds clap only. The `subcommand` token is used solely to strip the leading word cargo
injects (`cargo myforge` → argv `myforge …`) and to render `--help`. It is never interpolated
into a path, a sentinel, or a recipe name.

The catalog's content is an ordered set of **artifacts**. There are just two kinds:

- **`OwnedFile`** — a fully tool-owned file. The justfile tree members live here, and so does
  every cloud-workflow backend file (composite actions / step templates, workflows / stages, root
  workflows / pipelines). An owned file may be **gated** on a backend (see §4.3) so it is emitted
  only when that backend is selected. Identity: its repo-root-relative path.
- **`ManagedRegion`** — a sentinel-delimited region spliced into a user-composed host file
  (Justfile imports, `[workspace.lints]`, `deny.toml`, `rustfmt.toml`, `.delta.toml`,
  spellcheck, per-member `[lints]`). Identity: `(host-selector, region_id)`.

```rust
pub enum Artifact {
    OwnedFile(OwnedFileSpec),
    Region(RegionSpec),
}

pub struct OwnedFileSpec {
    pub path: &'static str,
    pub body: String,
    pub gate: Option<Backend>,     // None = always; Some(b) = only when b is selected (§4.3)
}

/// The cloud-workflow backends. A closed, engine-owned enum — downstream
/// catalogs cannot add to it. Used only to select and gate (§4.3).
pub enum Backend { GitHub, Ado }

/// A managed-region identifier. A newtype, not a bare string, so it can't be
/// confused with a file path, a recipe name, or any other string the API
/// takes. It is the value placed after `anvil-managed:` in the sentinels.
pub struct RegionId(&'static str);

impl RegionId {
    pub const fn new(id: &'static str) -> Self;
}

pub struct RegionSpec {
    pub host: HostSelector,        // where the region goes (see §4.2)
    pub id: RegionId,              // the sentinel id
    pub body: String,              // rendered between the sentinels
    pub syntax: CommentSyntax,     // Hash / SlashSlash
}

impl Artifact {
    /// Derive a variant of this artifact with a new body, preserving every
    /// other field — path, gate, host, id, syntax. This is how a fork
    /// overrides a built-in (§4.1) without being able to alter its identity.
    pub fn with_body(self, body: impl Into<String>) -> Artifact;
}
```

`RegionId` is the only identifier newtype in the public surface. There is **no public
`ArtifactKey`**: the engine identifies "which slot" internally (a path for owned files, a
`(host, id)` pair for regions) for dedup and override, but a fork never constructs or names a key.
Instead it references the built-in artifacts themselves (§4.1).

The `RegionId` newtype lives at the catalog/API boundary; internally it derefs to `&str`, so
`region.rs`'s `find_region` / `render_region` / `upsert_region` are unchanged. Anvil's built-in
region ids are `const RegionId::new("anvil-…")` values.

This is the key reframing: **`run.rs::build_plan` stops calling a fixed list of hand-named
emitters** (`plan_mod_just`, `plan_tools_just`, `plan_cargo_lints`, …) and instead iterates the
catalog's artifact set, dispatching each to the existing generic driver (`plan_owned_file` /
`plan_managed_region`). The per-artifact decision logic, manifest interaction, and orphan
detection are unchanged — only the *source of the list* changes from compiled-in calls to
catalog data.

Because the on-disk format is fixed, **none of the engine internals (`region.rs`, `manifest.rs`,
the templates) need to change to support forks.** `region.rs` keeps its hard-coded
`anvil-managed` sentinel; the templates keep their literal `anvil-` recipe names; the manifest
keeps `.anvil.lock`. The only refactor is data-driving the artifact list and threading
`CliMeta` into clap.

### 4.1 Built-in artifacts are public

To override or drop a base artifact, a fork needs a handle to it. Rather than exposing *keys*
(which split identity from content and let a fork pair a key with mismatched content), the engine
exposes the **artifacts themselves**, content and identity together, in an `artifacts::` registry:

```rust
pub mod artifacts {
    // The `justfiles/anvil/` recipe tree — every member is an owned `.just` file.
    pub mod justfile {
        pub fn entry() -> Artifact;     // justfiles/anvil/mod.just (imports the siblings)
        pub fn versions() -> Artifact;  // justfiles/anvil/versions.just
        pub fn tools() -> Artifact;     // justfiles/anvil/tools.just
        pub fn checks() -> Artifact;    // justfiles/anvil/checks.just
        pub fn groups() -> Artifact;    // justfiles/anvil/groups.just
        pub fn tiers() -> Artifact;     // justfiles/anvil/tiers.just
    }
    // Managed regions spliced into user-composed host files.
    pub mod region {
        pub fn justfile_imports() -> Artifact;  // Justfile / anvil-imports
        pub fn workspace_lints() -> Artifact;   // Cargo.toml / anvil-workspace-lints
        pub fn member_lints() -> Artifact;      // <member>/Cargo.toml / anvil-lints
        pub fn deny() -> Artifact;              // deny.toml / anvil-deny
        pub fn rustfmt() -> Artifact;           // rustfmt.toml / anvil-rustfmt
        pub fn delta() -> Artifact;             // .delta.toml / anvil-delta
        pub fn spellcheck() -> Artifact;        // spellcheck.toml / anvil-spellcheck
        pub fn clippy() -> Artifact;            // clippy.toml / anvil-clippy
    }
    // Backend files are owned files gated on a backend (§4.3), grouped per backend.
    pub mod github {
        pub fn setup_action() -> Artifact;      // .github/actions/anvil-setup/action.yml
        pub fn impact_action() -> Artifact;     // .github/actions/anvil-impact/action.yml
        pub fn pr_root_workflow() -> Artifact;  // .github/workflows/anvil-pr.yml
        // …reusable workflows, per-group actions, scheduled workflows.
    }
    pub mod ado {
        pub fn setup_step() -> Artifact;        // .pipelines/anvil/steps/setup.yml
        pub fn job_wrapper() -> Artifact;       // .pipelines/anvil/steps/job.yml
        pub fn advisory_comments() -> Artifact; // .pipelines/anvil/steps/advisory-comments.yml
        // …per-group step templates, root pipelines.
    }
}
```

(They are functions rather than `const`s only because an `Artifact` carries an owned `String`
body.) With the artifact in hand, the two operations are uniform and identity-safe:

```rust
// Override: derive from the built-in, so path + gate are preserved by construction.
.replace_artifact(artifacts::github::setup_action().with_body(include_str!("../templates/our-setup.yml")))
// Remove: pass the artifact; the engine reads its identity.
.without_artifact(artifacts::ado::advisory_comments())
```

Because an override is *derived* from the real artifact via `with_body`, a fork cannot change a
GitHub-gated file into an ADO-gated one, retarget it to a different path, or un-gate it — the
class of "key paired with the wrong content" mistakes is gone structurally, with no validation
rule needed. The raw `RegionId` sentinel values stay private to the engine; the built-in
artifacts are the sanctioned handles. The previous `pub const *_REGION_ID` items collapse into
this one organized namespace.

### 4.2 Per-member regions (workspace fan-out)

Some regions are not anchored to one literal file. The crate-scope `[lints]` region is spliced
into **every** workspace member's `Cargo.toml`, with the host set discovered at runtime from the
workspace, not known when the catalog is authored. A single `(host, id)` key can't express
"a region in each member's manifest."

The `host` of a `RegionSpec` is therefore a **selector**, not a literal path:

```rust
pub enum HostSelector {
    /// A single literal repo-root-relative path (Justfile, deny.toml, root Cargo.toml).
    Path(String),
    /// Every workspace member's manifest — expands to one `<member>/Cargo.toml`
    /// host per member discovered at plan time.
    EachMemberManifest,
}
```

`build_plan` expands selectors against the discovered `Workspace` exactly as `plan_cargo_lints`
does today: `EachMemberManifest` fans out to one concrete `(member/Cargo.toml, id)` plan item per
member. Everything downstream of expansion is unchanged — the manifest keys on the concrete
expanded `(host, id)` pairs, so per-member orphan detection (a member is removed → its region
entry is dropped) works exactly as it does now.

This makes the fan-out a first-class, reusable capability rather than special-cased engine logic.
A fork that wants its own region in every crate's `Cargo.toml` just adds one artifact:

```rust
.with_artifact(Artifact::region(RegionSpec {
    host: HostSelector::EachMemberManifest,
    id: RegionId::new("myorg-metadata"), // free-form id; unique within the host
    body: my_member_metadata_body(),
    syntax: CommentSyntax::Hash,
}))
// …or, equivalently, the constructor sugar:
.with_artifact(Artifact::member_region(RegionId::new("myorg-metadata"), my_member_metadata_body()))
```

and the engine replicates it across all members, tracks each in `.anvil.lock`, and reconciles
drift per member — no per-fork engine changes.

> Note anvil's own lint regions are modeled as two separate artifacts under this scheme: a
> `HostSelector::Path("Cargo.toml")` workspace-scope region (`anvil-workspace-lints`) plus an
> `EachMemberManifest` member region (`anvil-lints`). The single-crate case (no `[workspace]`
> table) is the engine emitting the full-catalog `anvil-lints` region into the root `Cargo.toml`
> instead — a property of the built-in lints artifacts, transparent to forks.

### 4.3 Backends: a fixed set, overridable in parts

There are exactly two cloud-workflow backends, `github` and `ado`, and **the set is closed**:
`Backend` is an engine-owned enum that downstream catalogs cannot extend. A fork never *adds* a
backend. What it can do — easily — is **override or drop the individual files** a backend emits.

Backends are not a separate artifact kind. Each backend's files (composite actions / step
templates, reusable workflows / stages, root workflows / pipelines, including per-group fan-out;
see [github.md](./github.md) / [ado.md](./ado.md)) are ordinary `OwnedFile` artifacts whose
`gate` is set to that backend:

```rust
OwnedFileSpec {
    path: ".github/actions/anvil-setup/action.yml",
    body: /* … */,
    gate: Some(Backend::GitHub),   // emitted only when github is selected
}
```

Selection is unchanged from [design.md §5.2](./design.md): the engine resolves a backend set from
explicit `--backend` flags or autodetection over the *fixed* `{github, ado}`, and only emits an
owned file whose `gate` is `None` or names a selected backend. Each built-in backend file is
exposed as a public artifact (§4.1), and a fork manipulates them — and adds new ones — with the
same uniform verbs used everywhere else:

```rust
// Override one built-in: derive from it, so path + gate are preserved.
.replace_artifact(artifacts::github::setup_action().with_body(include_str!("../templates/our-setup.yml")))
// Drop one built-in entirely.
.without_artifact(artifacts::ado::advisory_comments())
// Add a brand-new file gated on an existing backend.
.with_artifact(Artifact::backend_file(
    Backend::GitHub,
    ".github/workflows/anvil-myorg-release.yml",
    include_str!("../templates/release.yml"),
))
```

`Artifact::backend_file(backend, path, body)` is the gated constructor used to **add** a new
backend file. It takes the closed `Backend` enum, so a fork can gate only on `github` or `ado` —
it can add files *to* an existing backend but cannot invent a backend. Adding is safe because
`with_artifact` errors if the path already exists, so a new gated file can never silently shadow a
built-in.

Overriding a built-in is different from adding: prefer `artifacts::…().with_body(…)`, which keeps
the original path and gate, over reconstructing the file by hand. (`backend_file` + `replace`
would also work, but restating the path and backend invites the mismatch — wrong gate, wrong path
— that deriving via `with_body` avoids by construction.)

This gives fork authors fine-grained control — replace one action, drop one step, add one
workflow — without the ability to invent backends, and end users keep the normal dirty-file
ownership flow for one-off local edits.

## 5. The engine API

The public surface gains a small, thin layer; the existing modules (`decision`, `region`,
`manifest`, `plan`, `workspace`, `emit::*`) stay as-is internally.

```rust
// Build / customize a catalog.
impl Catalog {
    pub fn anvil() -> Catalog;                       // the built-in base catalog
    pub fn builder(cli: CliMeta) -> CatalogBuilder;   // start from empty
    pub fn into_builder(self) -> CatalogBuilder;      // start from an existing catalog
}

impl CatalogBuilder {
    pub fn subcommand(self, name: impl Into<String>) -> Self;
    pub fn about(self, s: impl Into<String>) -> Self;
    pub fn version(self, s: impl Into<String>) -> Self;

    // The three artifact verbs are uniform — all operate on the `Artifact` unit.
    pub fn with_artifact(self, artifact: Artifact) -> Self;     // add; errors if identity present
    pub fn replace_artifact(self, artifact: Artifact) -> Self;  // override; errors if identity absent
    pub fn without_artifact(self, artifact: Artifact) -> Self;  // remove; errors if identity absent

    pub fn build(self) -> Result<Catalog, AppError>;
}

// Constructors for fork-authored artifacts. Override an existing built-in by
// deriving from `artifacts::…` via `with_body` instead of reconstructing it.
impl Artifact {
    pub fn owned_file(path: &'static str, body: impl Into<String>) -> Artifact;  // gate: None
    pub fn backend_file(backend: Backend, path: &'static str, body: impl Into<String>) -> Artifact; // gate: Some
    pub fn region(spec: RegionSpec) -> Artifact;
    pub fn member_region(id: RegionId, body: impl Into<String>) -> Artifact; // EachMemberManifest + Hash sugar
}

// Drive the engine.
impl Cli {
    /// Parse argv against a catalog, stripping the `catalog.cli.subcommand`
    /// token cargo injects, and rendering help/version/about from `CliMeta`.
    pub fn parse_from_cargo_args(catalog: &Catalog, args: I) -> Result<Cli, clap::Error>;
}

pub fn run(catalog: &Catalog, cli: &Cli) -> Result<(), AppError>;
pub fn run_update(catalog: &Catalog, cli: &Cli, start_dir: &Path) -> Result<RunOutcome, AppError>;

/// One-call entry point: tracing init + parse + run + ExitCode mapping.
/// This is the body of today's `main.rs`, generalized over a catalog.
#[must_use]
pub fn run_app(catalog: Catalog) -> ExitCode;
```

`run_app` is what makes the downstream `main` a single line. It owns exactly what
`cargo-anvil`'s `main.rs` owns today (subscriber setup, `parse_from_cargo_args`, the
`Ok/Err → ExitCode` mapping, and the `--dry-run` exit-1 behavior), so all of that logic lives in
one tested place rather than being copy-pasted into every fork.

`cargo-anvil`'s own `main.rs` collapses to
`fn main() -> ExitCode { cargo_anvil::run_app(Catalog::anvil()) }`, proving the seam by
dogfooding it.

### 5.1 Tool identity, catalog checksum, and the single-tool guard

The lock file's provenance fields (see [updates.md §1](./updates.md#1-the-manifest)) come straight
from the catalog:

- **`tool`** is `Catalog`'s `CliMeta.subcommand` — the same token that names the cargo subcommand.
  It is the identity the **single-tool guard** keys on: at startup the engine compares the loaded
  lock's `tool` field against `catalog.cli.subcommand`, and refuses (writing nothing, even under
  `--dry-run`) when they differ, unless `--force` is passed to switch ownership to this tool. This
  is the runtime enforcement of the one-tool-per-repo constraint in §1 — the constraint that lets
  the on-disk `anvil` namespace stay fixed across forks. Because every fork keeps that fixed
  namespace, a `myforge` lock and an `anvil` lock are the same format; the `tool` field is what
  keeps the two tools from clobbering each other's lock.
- **`tool_version`** is the binding crate's version (`CliMeta.version`).
- **`catalog_checksum`** is a `sha256` over the whole `Catalog` — every artifact's identity and
  rendered body in canonical order. Two builds that share a `tool_version` but differ in any
  artifact (an extra owned file, an overridden region body, a swapped backend file) produce
  different checksums, which is what makes it useful during development. `--version` prints it.

`run_app` owns all of this: it computes `catalog_checksum` from the passed `Catalog`, folds it
into the `--version` output, performs the single-tool guard check (honoring `--force`) before
dispatching to `run`, and records the provenance fields on save. A fork inherits everything for
free — the same reason its `main` is one line.

For an extension chain (§7), `catalog_checksum` is taken over the *fully composed* catalog
`forge3` builds, so it reflects every ancestor's contribution plus `forge3`'s own edits; and the
guard's `tool` is `forge3`'s subcommand, since the composed binary is the single tool managing the
repo.

## 6. Artifact-level extensibility

A fork appends, replaces, or drops artifacts — owned files (including the gated backend files,
§4.3) and managed regions. That covers the common case: "anvil's catalog plus my org's extra
`.just` file and a CODEOWNERS region, with our own GitHub setup action." The check/group/tier
content inside the justfile tree is an opaque blob; a fork that needs different checks replaces
the relevant `OwnedFile` (e.g. `checks.just`) wholesale rather than editing individual recipes.

This is a modest, low-risk refactor: it data-drives the artifact list (§4) without disturbing the
engine internals or the template format.

## 7. Multi-level catalogs (extension chains)

Extension is transitive: a third tool can extend a second tool's catalog exactly as the second
extends anvil's. There is no special "base" status — `Catalog::anvil()` is just the catalog the
engine ships; any catalog is a valid starting point for the next.

```rust
// cargo-forge3/src/lib.rs
pub fn catalog() -> Catalog {
    forge2::catalog()                  // start from forge2's catalog, not anvil's
        .into_builder()
        .subcommand("forge3")
        .replace_artifact(             // override a region forge2 introduced
            forge2::artifacts::telemetry().with_body(forge3_telemetry_body()),
        )
        .without_artifact(forge2::artifacts::extra()) // drop one of forge2's files
        .build()
}
```

This works with no new mechanism, because:

- **A catalog is a flat, provenance-free artifact set.** By the time `forge3` sees it, anvil's
  and forge2's artifacts are indistinguishable entries with the same identity scheme. Overriding a
  forge2 artifact is identical to overriding an anvil one — the engine never asks "who first added
  this." `into_builder()` accepts any `Catalog`, whoever assembled it.
- **The artifact API is engine-public, not anvil-specific.** `Artifact`, `with_body`, `RegionId`,
  and `HostSelector` belong to the engine. Any catalog author can export its own artifacts —
  `forge2::artifacts::telemetry()` — exactly as anvil exposes `artifacts::region::deny()`.

For an intermediate tool to be a good extension base, it follows the same contract anvil does:

1. **Expose its catalog** as `pub fn catalog() -> Catalog` so descendants can start from it.
2. **Export its artifacts** (an `artifacts::` module of `fn … -> Artifact`) so descendants can
   derive overrides via `with_body` and pass them to `without_artifact` — the same content-plus-
   identity handles anvil ships, no separate key registry to maintain.
3. **Use unique region ids.** The sentinel keyword stays the fixed engine namespace
   (`anvil-managed`), but the id *after* it is free-form and only needs to be unique within a
   host file. A per-tool id prefix (`forge2-telemetry`) keeps a chain's regions from colliding in
   a shared host like `Cargo.toml`.

Note this does **not** reintroduce "multiple tools per repo" (§1): a chain compiles to a *single*
binary (`forge3`). The ancestors are build-time libraries, not separately-installed tools, and
the on-disk namespace stays the fixed `anvil` format — so `forge3` reconciles the regions its
ancestors defined seamlessly, as one tool managing one namespace.

## 8. Verification

- **Dogfooding.** `cargo-anvil` is `Catalog::anvil()` through `run_app`; its existing fixture,
  snapshot, and schema tests (see [verification.md](../verification.md)) pin that the
  base-catalog output is byte-identical to today. Because the on-disk format is fixed, those
  snapshots do not need to change at all.
- **A second-front-end fixture.** Add a tiny in-repo example catalog (`Catalog::anvil()` with
  subcommand `demoforge` and one extra owned file) and a fixture test asserting: the subcommand
  parses, the extra file is emitted under `justfiles/anvil/`, and the output is otherwise
  identical to the base catalog — i.e. nothing in the on-disk vocabulary shifted.

## 9. Non-goals

- **Multiple anvil-family tools per repo.** Out of scope by deliberate constraint (§1). This is
  what lets the on-disk vocabulary stay fixed, with no per-fork rebranding of paths or sentinels.
- **Per-fork on-disk rebranding.** A fork cannot rename `.anvil.lock`, the `anvil-managed`
  sentinels, `justfiles/anvil/`, or the `anvil-` recipe prefix. Those belong to the engine.
- **Runtime plugins / dynamic loading.** A catalog is Rust code compiled into the downstream
  binary, not a config file discovered at runtime. This keeps the "writes files, then exits"
  stance ([design.md §3](./design.md)) and avoids a plugin ABI.
- **Fork-authored backends.** The backend set is closed (`github`, `ado`); a fork cannot add a
  backend (§4.3). It can override, drop, or add individual files gated on an *existing* backend,
  but the `Backend` enum and backend selection/autodetection are engine-owned.
- **Changing the update algorithm per fork.** The decision table, opt-out semantics, and orphan
  handling are fixed engine behavior. Forks customize *what* is emitted, never *how* drift is
  reconciled.

## 10. Design decisions

1. **Single crate.** The engine and the `Catalog` API live in the `cargo-anvil` crate; forks
   depend on it as a library and provide their own thin binary. We do not split out an
   `anvil-core`. Keeping everything in one crate avoids a premature library/binary boundary and
   keeps the base tool and the extensibility seam evolving together.
2. **Distinct verbs for add / override / remove, each loud on mismatch.** `with_artifact` is
   append-only (errors if an artifact with that identity already exists); `replace_artifact`
   overrides (errors if it does *not*); `without_artifact` removes (errors if absent). To change a
   base-catalog artifact a fork must say so explicitly via `replace_artifact`, deriving the
   replacement from the public built-in (`artifacts::…().with_body(…)`) so its identity and gate
   are preserved by construction. This makes collisions loud rather than silently
   last-write-wins, so a fork can never shadow a base artifact by accident.
