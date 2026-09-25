# cargo-unused-deps — Design

> Status: **Implemented.**
> Crate name: `cargo-unused-deps`.
> Home: `github.com/microsoft/ox-tools`, published to crates.io.
>
> Built: the catalog check with `--fix`, the unused and misplaced checks from
> compile evidence, doctest evidence through the shim, package selection, and the
> allow-list. Removing or moving package declarations remains deliberately manual.

## 1. Problem

"Is this dependency actually used?" currently takes two tools, and neither answers it
alone — while one part of the question has no tool at all:

| Tool | Answers | Blind to |
|------|---------|----------|
| `cargo udeps` | Does rustc load this dependency when the crate is built? | Anything not compiled: doctests, target kinds it was not told to build, other platforms and feature selections |
| `cargo machete` | Does this dependency's name appear in the crate's source text? | Uses that never name the crate — macro-generated paths; and *where* the name appeared, so it cannot tell a library use from a test use |
| *(nothing)* | Does any member inherit this `[workspace.dependencies]` entry? | — |

They are not two views of one question; they are different questions with different
*mechanisms*, and each mechanism's blind spot is the other's strength. Running both is
the current answer, and it is a poor one: two installs, two pins, two configuration
dialects, two ignore lists, and an operator left to reconcile reports that disagree by
construction — with the catalog gap still uncovered. In this repository that gap let 48
stale `[workspace.dependencies]` entries accumulate with `udeps` green throughout.

The disagreements are not noise, and — importantly — they are not all false positives
either. A dependency used only through a macro expansion is "unused" to machete and
"used" to udeps: machete is simply wrong. But a dependency declared unconditionally
and used only under `#[cfg(target_os = "macos")]` is "used" to machete and "unused" to
udeps on a Linux runner, and there **udeps is right**: the declaration should have
been `[target.'cfg(target_os = "macos")'.dependencies]`. Treating that report as noise
to be suppressed is how an unconditional dependency stays in every platform's build
graph forever.

The insight this crate is built on: **the mechanisms disagree in structured ways, and
the structure of a disagreement is itself information.** One tool holding the evidence
can say *which* kind of problem it found — dead dependency, wrong section, missing
platform gate, missing feature gate — where two tools shouting past each other can only
say "unused" and "used".

## 2. Mechanisms

The design turns on how each tool actually decides, so the mechanisms are stated
precisely rather than by reputation.

### `cargo udeps` — the tool this replaces

Runs the build through cargo's own library and, for each unit, scans rustc's `.d`
dep-info for the dependency *artifacts* rustc opened, matched by filename — which its
own source calls "obviously only a stupid heuristic". Those artifact entries appear
only under rustc's `-Z binary-dep-depinfo` (a plain build's dep-info lists source files
alone), and driving cargo as a library is unstable too: together, that is the whole of
its nightly requirement.

The evidence is sound for what was compiled, which is also its limit — one platform,
one feature selection, and only the target kinds the invocation asked for. Two
consequences shape the check that replaces it: doctests are never analysed, and this
repository must run udeps **twice**, because the default-targets run is the only one
that reveals a `[dependencies]` entry used solely by tests while the `--all-targets`
run is the only one that compiles dev-dependencies at all.

### `unused_crate_dependencies` — what rustc says on stable

rustc answers the same question directly, as a lint, with no dep-info parsing and no
filename heuristics. On **stable** 1.93.1, `RUSTFLAGS=-W unused_crate_dependencies`
with an ordinary `cargo check --all-targets --message-format=json` yields:

```
[main/lib]  extern crate `unused`  is unused in crate `main`
[main/lib]  extern crate `devonly` is unused in crate `main`
[it/test]   extern crate `unused`  is unused in crate `it`
[it/test]   extern crate `used`    is unused in crate `it`
```

The output is **per compilation unit**, which is a feature rather than noise: a
dependency is unused only when every unit that had it in scope said so. `devonly` is a
dev-dependency the lib never uses but the integration test does; `used` is the reverse.
Aggregating "used" across units leaves exactly `unused`.

This is the mechanism udeps has contemplated and not adopted
([cargo-udeps#70](https://github.com/est31/cargo-udeps/issues/70), open), and it is
strictly better for this design: stable toolchain, structured JSON, and per-unit
attribution that answers the misplacement question directly instead of by differencing
two whole runs.

#### The lint is a tool input, not a repository setting

Cargo's `[lints]` table can carry the level, and it is tempting to put it in the
workspace lint catalog so every build emits the evidence for free. That is a trap.

The raw lint fires per compilation unit, and a *correct* workspace produces warnings.
Measured on one where every dependency is genuinely used — `lib1` by the library, `dev`
by an integration test — a plain `cargo check --all-targets` says:

```
warning: extern crate `dev`  is unused in crate `main`   (lib test unit)
warning: extern crate `lib1` is unused in crate `it`
warning: extern crate `main` is unused in crate `it`
```

Three warnings, nothing wrong. Each is true of its unit and meaningless in isolation:
dev-dependencies are in scope for the lib's test unit, an integration test does not use
what the library uses, and a test need not mention the crate it tests. Aggregating is
what turns these into an answer, and aggregation is precisely what a compiler warning
cannot do. This is [rust#95513](https://github.com/rust-lang/rust/issues/95513), still
open.

In this repository it would be worse than noisy. Clippy runs with `-D warnings`, so the
same fixture fails outright:

```
error: extern crate `dev` is unused in crate `main`
error: could not compile `main` (lib test) due to 1 previous error
```

So the lint level belongs to *this tool's own invocation* and nowhere else. The tool
installs a chaining `RUSTC_WORKSPACE_WRAPPER` and appends `--force-warn
unused_crate_dependencies` after Cargo has resolved configured and environment flags.
`--force-warn` cannot be lowered by crate attributes. Cargo's outer wrapper remains
outside this one, and an environment-selected workspace wrapper is invoked behind it.
The tool reads the stable Cargo configuration hierarchy for the caller's working
directory and refuses to replace a file-configured workspace wrapper whose executable
cannot be chained safely. Each analysis gets a fresh temporary `--target-dir`
because Cargo does not replay lint diagnostics for cached artifacts; ordinary builds
stay silent and stale evidence cannot cross invocations.

Cargo's own lint, below, is exempt from this: it aggregates before reporting, so it can
sit in a manifest without lying about correct code.

Measured coverage, same fixture style:

| Case | Lint |
|------|------|
| Unused normal dependency | flagged, attributed to the lib unit |
| Unused dev-dependency | flagged under `--all-targets`, attributed to the test unit |
| Unused build-dependency | flagged, attributed to the `custom-build` unit |
| Proc-macro dependency used only through a `#[derive]` | silent — correctly treated as used |
| Dependency used only from a doctest | silent as invoked — rustdoc discards the compiler's stderr on success; recoverable with a `--test-builder` shim (see below) |

The last row is the only one needing extra machinery. Everything else that made udeps' output
awkward — the nightly, the filename matching, the two passes, build scripts and proc
macros — it handles.

What it does **not** change is the *envelope*: like udeps, it only knows about units
that were compiled, on one platform, with one feature selection. That envelope is
where §3's reclassification does the work — outside it, the answer is a manifest
defect rather than missing evidence.

One class needs an allow-list under either mechanism: a dependency linked for its side
effects and never named in source — an allocator, a `-sys` shim, a registrar run by a
constructor. rustc did not load it because nothing referenced it, so "unused" is
literally true and operationally wrong.

#### Availability and suppression

The lint landed in [rust-lang/rust#72342](https://github.com/rust-lang/rust/pull/72342)
("Warn about unused crate deps", merged 2020-05-27) and has been in the stable
allowed-by-default listing since **1.48.0** — absent in 1.47, present in 1.48, which
shipped in November 2020. It is old, not new. Verified firing on stable 1.93.1.

Which raises the obvious question: if rustc has answered this since 2020, why does
anyone run udeps or machete? Four reasons, and this tool has to supply all four or it
is no better:

1. **Nothing aggregates.** The lint reports per compilation unit, so a dependency used
   by the lib but not the integration test warns anyway. Read naively it looks broken,
   which is filed upstream as false positives
   ([#95513](https://github.com/rust-lang/rust/issues/95513), open). Aggregating
   "used" across units is the whole fix, and neither rustc nor cargo does it.
2. **There is no command.** `cargo check` warns; nothing turns those warnings into a
   verdict, an exit code, or a fix. The cargo-side integration that would make it
   turnkey is still unstable ([#98400](https://github.com/rust-lang/rust/issues/98400)).
3. **Doctests are missing**, as above.
4. **It needs a build.** machete answers in seconds on code that does not even compile;
   that cost profile is why it survives its own inaccuracy.

None of that argues for a second tool. It argues for one that enables the lint,
aggregates it, adds the questions the lint cannot answer, and reports.

rustc offers no per-dependency allow attribute. Measured:

| Suppression attempt | Effect |
|---------------------|--------|
| `#![allow(unused_crate_dependencies)]` at crate root | silences the lint for the **whole crate** |
| `#[allow(unused_crate_dependencies)]` on an item | no effect — the lint fires at crate level |
| `use dep as _;` in the crate root | silences it for **that one dependency** |
| `--extern nounused:dep=…` | rustc-side per-dependency switch, still unstable ([#98400](https://github.com/rust-lang/rust/issues/98400)) |

So the two things rustc offers are a blanket off-switch and a source edit. Neither is
acceptable as this tool's allow-list: a blanket allow disables the check exactly where
someone already knows a dependency is odd, and `use dep as _;` writes a lint workaround
into the user's crate root. The allow-list therefore stays in the manifest and is
applied by **filtering the lint's JSON output**, which also keeps one dialect for all
of the tool's checks.

Two upstream issues corroborate the design rather than threaten it.
[#95513](https://github.com/rust-lang/rust/issues/95513) (open) reports the lint's
"false positives" in packages with several targets — which is the per-unit output
described above, read without aggregating; aggregating across units is precisely the
fix. [#78346](https://github.com/rust-lang/rust/issues/78346) reported the doctest gap
and was closed by moving reporting from rustc to cargo, which does not change what is
observable through an ordinary invocation.

### `cargo`'s own `unused_dependencies` lint — the turnkey path

Cargo has since grown the missing half itself: a native `unused_dependencies` lint
that consumes rustc's per-unit information, aggregates it, and reports against the
manifest. It is enabled through `[lints.cargo]` and gated on `-Z cargo-lints`, so it
is nightly-only today:

```toml
[workspace.lints.cargo]
unused_dependencies = "warn"
```

```
warning: unused dependency
 --> main/Cargo.toml:8:1
help: remove the dependency
help: to still use for development builds, move to `dev-dependencies`
```

Measured against the same fixtures (`cargo +nightly check --all-targets -Z cargo-lints`):

| Case | Cargo's lint |
|------|--------------|
| Unused normal dependency | flagged, at its manifest line, with a fix suggestion |
| Normal dependency used only from `tests/` | flagged, **suggesting the move to `dev-dependencies`** — question 3, answered natively |
| Dependency used by the lib, or only by a test | correctly silent: it aggregates across units |
| Doctest-only dev-dependency | silent — no false positive |
| **Dev-dependency used by nothing at all** | **also silent** — dev-dependencies are not analysed |

The last two rows go together: cargo avoids the doctest false positive by not looking
at dev-dependencies at all. That is the right conservative default for a compiler
warning and a real gap for a gate, because unused dev-dependencies are exactly what
this repository's second udeps pass exists to catch.

So the honest position is: cargo is going where this design is going, and will
eventually own the middle layer. Until `-Z cargo-lints` stabilises and grows
dev-dependency coverage, this tool takes evidence from the rustc lint and aggregates it
itself — which works on stable and does cover dev-dependencies. When cargo's lint
covers them, this layer should shrink to consuming cargo's diagnostics instead, and the
tool keeps only the questions cargo does not answer: the workspace catalog, doctests,
optional/feature declarations, and over-broad declarations.

### Manifest evidence — what the TOML already says

Inheritance is written down: a member draws from the catalog by declaring
`dep = { workspace = true }`. Comparing the root's `[workspace.dependencies]` against
every member manifest therefore answers the catalog question outright — no compilation,
no source parsing, and no false positives. It is invisible to both tools above because
an uninherited entry never enters any crate's dependency graph. See
[the catalog design](./workspace-catalog.md).

Feature definitions also carry manifest-level use. A dependency referenced through
`dep:name`, `name/feature`, or `name?/feature` is required even when crate source never
names it. A bare feature member names an implicit optional-dependency feature only when
no explicit feature shadows that name. These declarations are excluded from source
findings before compiler evidence is judged.

Rustc identifies an extern name, not the manifest declaration that introduced it. Normal
and development declarations share one runtime extern in development units, so the tool
suppresses source findings when that extern appears more than once across their target or
dependency tables. Build declarations belong to a separate build-script target and remain
independently attributable: a key repeated once at runtime and once for the build script is
still judged in both domains. The catalog check remains unaffected.

### Also evaluated, and not used

**`cargo machete`** walks every `.rs` file and regex-matches each dependency's name.
No compilation, so it reads code the compiler never built — but a dependency reached
only through a macro expansion never appears as text and looks unused, and it records
no notion of *where* a name occurred, so it cannot distinguish a library use from a
test one. Nothing in this design uses it.

**`cargo shear`** replaces that regex with rust-analyzer's parser, which is precise
about location but shares the blind spot: on this repository it called eight
macro-argument uses unused. Parsing is a better static analysis, not a different kind
of evidence.

**`cargo-unused-workspace-deps`** on crates.io answers the catalog question only, in a
single release from 2025 with no commits since — not something to pin.

## 3. The evidence model

There is exactly one source of truth about source-level use: **what the compiler
loaded**. Package-level checks require a nightly toolchain and gather that evidence
for every unit, doctests included. They do not have a weaker stable fallback. The
selector-free workspace catalog check remains manifest-only and runs on stable.

- **Positive evidence is authoritative.** If rustc loaded the crate while compiling
  any unit, the dependency is used.
- **Negative evidence is authoritative only within the build's envelope.** "No unit
  loaded it" is decisive for the platform, feature selection and target kinds that
  were built — and outside that envelope, as §1 argued, the answer is not "missing
  evidence" but "the declaration is too broad".

That second point is what makes a single source sufficient. A dependency reachable
only under `#[cfg(target_os = "macos")]` and declared unconditionally is *reported*,
because Cargo can express the condition and the manifest should say so:

```toml
[target.'cfg(target_os = "macos")'.dependencies]
core-foundation = { workspace = true }
```

The same applies to every neighbouring case, all of which Cargo can express:

| Where the only uses live | Declaration it should have |
|--------------------------|----------------------------|
| `#[cfg(target_os = …)]`, `#[cfg(windows)]`, `#[cfg(target_arch = …)]` | `[target.'cfg(…)'.dependencies]` |
| `#[cfg(feature = "x")]` | `optional = true`, enabled by feature `x` |
| `#[cfg(test)]`, `tests/`, `benches/`, `examples/` | `[dev-dependencies]` |
| `#[cfg(loom)]`, `#[cfg(fuzzing)]`, other custom cfgs | `[target.'cfg(loom)'.dependencies]` — Cargo evaluates target cfg with `RUSTFLAGS` applied, which is how this repository's `loom` check already declares its dependency |

The report says which of these to reach for. It does not attempt to detect *which* cfg
gates the code — that would mean parsing sources to guess at an author's intent, and
the author is better placed to pick the right expression than a lint is.

One class needs the allow-list: a dependency linked for its side effects and never
named — an allocator, a `-sys` shim, a registrar run by a constructor. rustc did not
load it because nothing referenced it, so "unused" is literally true and operationally
wrong.

### Doctests: invisible to the obvious routes, reachable by one

A doctest is compiled against the crate's dev-dependencies, so a dependency used only
from a `///` example is already declared in the right section — there is no
`[doctest-dependencies]` to move it to and nothing for the author to fix. But
`--all-targets` does not build doctests, so udeps reports it unused
([cargo-udeps#52](https://github.com/est31/cargo-udeps/issues/52), open), and cargo's
own lint sidesteps the problem only by ignoring dev-dependencies entirely.

Every direct route to the evidence is closed, measured on `nightly-2026-05-30`:

| Attempt | Result |
|---------|--------|
| `cargo udeps --doc` | No such flag; `--all-targets` is cargo's and excludes doctests |
| `cargo test --doc --no-run` | `error: Can't skip running doc tests with --no-run`, on stable *and* under `-Z unstable-options` |
| Plain `cargo test --doc`, then read the dep-info | Nothing to read: the doctest run emits no `.d` of its own |
| `RUSTDOCFLAGS=--emit=dep-info` | `error: the --test flag and the --emit flag are not supported together` |
| `rustdoc --emit=dep-info -Z binary-dep-depinfo` in *doc* mode | Emits udeps-shaped dep-info, but documents the crate rather than compiling doctests, so a doctest-only dependency is absent |
| Reading the `--extern` list cargo passes rustdoc | Cargo passes *every* dev-dependency whether the doctest uses it or not |
| `RUSTDOCFLAGS=-W unused_crate_dependencies` | Silent: rustdoc captures the compiler's stderr and discards it when compilation succeeds |

That last row is the tell. The lint is not absent — its output is *swallowed*. rustdoc
lets the compiler behind doctests be replaced (`--test-builder`, `-Z
unstable-options`), and a replacement is free to keep what rustdoc throws away:

The tool installs itself as a chaining `RUSTDOC` executable. After Cargo resolves
configured and environment rustdoc flags, the wrapper appends `-Z unstable-options
--no-run --test-builder <shim>`. The shim runs the real rustc with `--force-warn
unused_crate_dependencies`, replaces only diagnostic-format/color arguments with
`--error-format=human --color=never`, preserves those diagnostics for rustdoc, and
writes the unused-crate names it parses to one unique record per compiler invocation.
Measured, on a crate whose
`doconly` dev-dependency is used only from a doctest and whose `deaddev` is used
nowhere:

```
extern crate `deaddev` is unused in crate `rust_out`
extern crate `main`    is unused in crate `rust_out`
```

`doconly` is correctly absent — real compile evidence for a doctest. Three details
matter for the implementation:

- Every doctest compiles as a crate called `rust_out`, so diagnostics carry no
  per-doctest identity. None is needed: the question is "did *any* doctest use this
  dependency", so the shim's per-process records are unioned after rustdoc exits.
- The crate under test is itself passed as an `--extern` and is reported unused by any
  doctest that does not mention it. It is excluded from the analysis, not counted.
- The shim is this tool in another mode, the way `RUSTC_WRAPPER` tools work — not a
  shell script the user has to install.

`--test-builder` and `--no-run` are both `-Z unstable-options`, so this route is
nightly-only, and the tool requires nightly because of it. That is a deliberate trade:
the alternative is a second, text-based analysis for stable toolchains, which would
double the implementation to answer one question less reliably.

That leaves the honest summary of the composition:

- Compile evidence proves **use**; it never proves absence, because it only ever
  looked at one platform, one feature selection, and the target kinds it built.
- There is no second opinion. Source text is never consulted, so nothing can excuse a
  dependency the compiler did not load; outside the build's envelope the answer is a
  manifest defect, per section 3.
- Manifest evidence decides the questions that never reach source at all.

## 4. Questions answered

The tool answers five questions over one model of the workspace. The catalog question
is manifest-only; the rest need compile evidence.

| # | Question | Evidence | Covered today by |
|---|----------|----------|------------------|
| 1 | Is a `[workspace.dependencies]` entry inherited by any member? | manifest | nothing |
| 2 | Is a declared dependency used by its crate at all? | compile | udeps |
| 3 | Is a `[dependencies]` entry used only by tests, benches or examples? | compile (unit kind) | udeps' two-pass trick |
| 4 | Is a `[dev-dependencies]` entry unused, doctests included? | compile (`--all-targets` + doctest shim) | udeps' second pass, minus doctests |
| 5 | Is a dependency declared more broadly than the build ever uses it? | compile (absence) | nothing |

Question 3 is the clearest simplification. Elsewhere it is inferred from the
*difference* between two whole udeps runs — an encoding so indirect that the invoking
recipe carries a paragraph explaining it. This tool compares target-level counters from
the default-target and all-target passes and reports the declaration directly.

Question 5 is question 2's finding phrased usefully: rather than "unused", the report
names the declarations Cargo offers — a target table, an optional dependency, a
dev-dependency — and lets the author pick. It does not guess which `cfg` gates the
code, and it is not a separate check.

## 5. How a run works

The questions divide cleanly in two, and that division is what makes impact scoping
safe:

- **Per-crate questions** — is this crate's dependency used, misplaced, or a dead
  dev-dependency? A crate's dependencies can only be used by that crate's own code, so
  each verdict needs that crate's compile evidence and nothing else.
- **One workspace-global question** — is a catalog entry inherited by anybody? That
  needs every member's manifest and no compile evidence at all, because inheritance is
  written in TOML.

So the expensive half scopes to whatever packages the caller selected, and the cheap
half always reads the whole workspace.

One invocation has four phases, with no alternate static-analysis path.

**1. Read the workspace.** `cargo metadata --no-deps` for the member set and each
member's declared dependencies by section; the root manifest for the catalog and the
allow-lists. This phase alone answers question 1, and it always reads *every*
member regardless of package selection — see below.

**2. Gather compile evidence.** Twice — once over default targets, once over
`--all-targets` — into separate target directories. Cargo emits fresh
`compiler-artifact` messages for cached units without replaying their lint diagnostics;
sharing artifacts would therefore make the unit count exceed the report count and turn
an unused dependency into false evidence of use. The two runs answer different
questions; see phase 4.

The tool runs `cargo +nightly check <selection> --all-targets --all-features
--target-dir target/unused-deps/run-<unique>/all-targets --message-format=json`
with its chaining rustc wrapper; the default-target pass uses the sibling `plain`
directory. The whole run directory is new for every invocation.
The wrapper preserves Cargo's resolved flags and appends the non-overridable lint for
this run only, never in the workspace lint catalog. Every diagnostic carries the target
it came from, so the output is a stream of `(target, target kind, extern name, unused)`
facts.

**3. Gather doctest evidence.**

The tool runs `cargo +nightly test --doc <package> --all-features` with itself as a
chaining rustdoc executable. That wrapper appends the unstable test-builder options
without replacing Cargo's active rustdoc flags, and chains to `RUSTDOC` when set or
otherwise to the rustdoc in the active rustc's sysroot, so no rustup is required.
`<self>` then acts as the test-builder:
it invokes the real rustc with `--force-warn unused_crate_dependencies`, writes one
unique capture record, and forwards rustc's diagnostics and exit status. Per-process
records avoid concurrent append interleaving.

**4. Aggregate and judge.** The diagnostics are per *unit*, and cargo compiles a
library or binary twice under `--all-targets` — plainly and with `cfg(test)` — while
saying nothing in the message about which of the two spoke. Attribution is therefore by
counting, **per target**:

- Count the units cargo compiled for each target, from its artifact messages.
- Count how many of them reported each dependency unused.
- A target *used* a dependency when fewer of its units reported it than had it in
  scope. The test-profile unit compiles a superset of the plain unit's code, so when
  only one of the two reports, it can only be the plain one — which is exactly the
  signal that a dependency is used from `#[cfg(test)]` code and belongs in
  `[dev-dependencies]`.

The rule is the same for every target, because "compiled twice" is not a property of
libraries. A test, bench or example declared `test = true` is also built plainly and
under `cfg(test)`, and reading a single report there as "unused" would convict a
dependency the target's own `cfg(test)` code uses. The one distinction that does matter
is scope: a dev-dependency is in scope only for the `cfg(test)` unit of a library or
binary, so there a lone report is that unit's and means unused.

A third number comes from the manifest rather than the build: `scope(T, D)`, the units
of `T` that had `D` in scope at all. It equals `units(T)` everywhere except one case —
a dev-dependency is in scope only for the `cfg(test)` unit of a library or binary, so
there it is 1.

Two questions follow, and they are not the same question — nor answerable from the same
run.

**Did any unit use `D`?** — `reports(T, D) < scope(T, D)`. This needs no assumption: a
unit reports exactly when `D` was in scope and went unused, so fewer reports than
in-scope units means some in-scope unit used it. It restates the lint's own semantics.

**Did the *plain* unit use `D`?** — `reports(T, D) == 0`, read from a **second run that
builds default targets only**. There each code target has exactly one unit, so a report
can only be that unit's.

That second run is not a convenience. In an `--all-targets` run the plain and
`cfg(test)` units are indistinguishable, and the tempting inference — one report out of
two units must be the plain one, because `cfg(test)` compiles a superset — is **false**.
`#[cfg(not(test))]` code is excluded from the test unit, so a dependency used only there
is used by the library and reported by the `cfg(test)` unit. Inferring would move a
production dependency into `[dev-dependencies]`. The tool runs the extra pass instead,
and a regression test pins the case.

Library use is the second question, asked of every code target. Development use is the
first, asked of code targets and of test, bench and example targets alike — those are
compiled twice as well when declared `test = true`, so the shape of the rule cannot
depend on the kind of target. A dependency is used if any target says so: the union,
never a pool.

Measured on a package with a library, one binary and one integration test:

| target | units | `libonly` | `binonly` | `devdep` |
|--------|-------|-----------|-----------|----------|
| `lib/main` | 2 | 0 | 2 | 1 |
| `bin/one` | 2 | 2 | 0 | 1 |
| `test/it` | 1 | 1 | 1 | 0 |

`libonly` is used by the library — zero reports there — though the binary reports it
twice. `binonly` is the mirror image. `devdep` reports once against each code target:
that is the `cfg(test)` unit, the only one it was ever in scope for, so those count as
unused. Zero reports against the test target is what spares it. Pooling any row or any
column would convict something.

Doctest evidence joins here, gathered per package because rustdoc feeds each snippet to
the test builder on stdin and the shim cannot tell which crate it came from. Cargo can,
so the doctest pass runs one package at a time. Packages without a library target are
skipped: asking cargo for their doctests is an error, not an empty answer.

Allow-listed names are dropped before judging, as is the crate under test, which
rustdoc passes to every doctest as an `--extern`.

Reading the counters gives every verdict directly:

| Declared in | Used by lib/bin | Used by test/bench/example/doctest | Verdict |
|-------------|-----------------|-----------------------------------|---------|
| `[dependencies]` | yes | — | fine |
| `[dependencies]` | no | yes | **misplaced** — move to `[dev-dependencies]` |
| `[dependencies]` | no | no | **unused** |
| `[dev-dependencies]` | — | yes | fine |
| `[dev-dependencies]` | — | no | **unused dev-dependency** |

Catalog entries are judged in phase 1 and need no evidence at all: an entry no member
inherits cannot be used by anything.

The two halves interact in one direction. A member that inherits a catalog entry it does
not use is a per-crate finding; the catalog entry stays "inherited" and therefore clean
until that member's declaration is removed. A later run may then expose the catalog entry
as uninherited.

## 6. User-visible shape

### Invocation

```text
cargo +nightly unused-deps [--manifest-path <PATH>]
    [-p <SPEC>...] [--workspace [--exclude <SPEC>...]]
    [--check <NAME>...] [--fix] [--require-workspace]
```

| Option                | Default      | Meaning                                                                      |
|-----------------------|--------------|------------------------------------------------------------------------------|
| `--manifest-path`     | `Cargo.toml` | Workspace root or crate manifest to check.                                   |
| `-p`, `--package`     | *(none)*     | Gather compile evidence for an exact package name or `name@version`. Repeatable. |
| `--workspace`         | *(off)*      | Gather compile evidence for every workspace member.                          |
| `--exclude`           | *(none)*     | Exclude an exact package name or `name@version` from `--workspace`. Missing exclusions retain Cargo's warning-only behavior. |
| `--check`             | *(all)*      | Restrict per-package checks to `unused` and/or `misplaced`; catalog still runs. |
| `--fix`               | *(off)*      | Remove uninherited catalog entries.                                          |
| `--require-workspace` | *(off)*      | Treat a manifest with no `[workspace]` table as an error.                    |

The catalog check always reads every workspace member and always runs, including
when no package selector is supplied. Package selection applies only to compile
evidence. This gives impact-scoped callers a safe zero-selection representation:
invoke the tool with no selector to check the global catalog without compiling any
package. An ordinary full-workspace run is explicit as `--workspace`.

Package-level checks require nightly because collecting doctest evidence uses
rustdoc's unstable `--no-run` and `--test-builder` options. A selector-free
catalog-only invocation is manifest-only and runs on stable.

### Manifests without a `[workspace]` table

A manifest with no `[workspace]` table has no catalog, so there is nothing this
check can be wrong about. It reports that on stderr and succeeds.

That default exists because the check is invoked from cargo-anvil, which manages
single-crate repositories as well as workspaces. A generated recipe runs the same
command everywhere, so a hard error here would make the check unusable in exactly
the repositories that never had the problem, and each of them would need a local
opt-out. Succeeding is also the honest answer: the property "no catalog entry goes
uninherited" holds vacuously.

`--require-workspace` restores the strict reading for callers that know they are
pointing at a root manifest and want a misdirected `--manifest-path` to fail rather
than pass quietly.

### Allowed entries

A deliberate exception is declared in the workspace manifest, not on the command
line, because the generated CI recipe invokes the tool with a fixed argument list:

```toml
[workspace.metadata.unused-deps]
allowed = ["kept-on-purpose"]

[package.metadata.unused-deps]
allowed = ["package-local-side-effect"]
```

A workspace-level allowed name suppresses both catalog and source-level findings. A
package-level name suppresses source findings for that package, including in a standalone
non-workspace crate. A workspace-level entry is stale only when neither the workspace
catalog nor any member manifest declares that name. This lets an inherited or direct
side-effect dependency use the same suppression without the catalog phase incorrectly
recommending that the exception be removed. A stale entry produces a warning without
changing the exit code.

### Reporting

Catalog findings are written to stderr in manifest order. Package findings name the
package, exact dependency table (including any target predicate), dependency, verdict,
remediation, and manifest path. Compiler or doctest collection failures are errors, not
ordinary findings. Success summaries go to stdout.

### Exit codes

| Code | Meaning |
|------|---------|
| 0    | No catalog or selected package findings — or, under `--fix`, catalog findings were removed successfully. |
| 1    | An uninherited catalog entry, selected unused/misplaced declaration, invalid selector/configuration, manifest/workspace error, or compiler/doctest evidence failure. |

A `[workspace]` table with no `dependencies` catalog is a pass: no entry can be
uninherited when none is declared. Configured allow-list names are still compared with
member declarations, and only names declared nowhere are reported as stale. A manifest
with no `[workspace]` table at all is a pass with a note on stderr, or an error under
`--require-workspace`.

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

The workspace root manifest is the one file whose loss breaks every other tool in
the repository, so it is never truncated in place. The replacement is written to a
temporary file in the manifest's own directory and renamed over the original, which
is atomic on a single filesystem. Before that rename Cargo re-resolves the member
set and compares it with the original set, then the root manifest and every
original member manifest are re-read and compared against the bytes used by
detection. `cargo metadata` and member scanning run in between, which is a wide
enough window for an editor to save into an input or for a glob to match a new
member. A change that lands there aborts the fix rather than letting the command
remove a catalog entry from evidence that is no longer current. These comparisons
narrow the remaining window rather than closing it; a change landing after them
can still be overwritten or invalidated by the catalog change.

Replacing a file by rename brings the temporary file's identity with it, so two
properties an in-place write would have kept are restored deliberately. The manifest's
permissions are read first and applied to the replacement, because a temporary file is
created owner-only and a rename carries its mode rather than inheriting the target's —
otherwise a world-readable manifest silently comes back owner-only, which git does not
track. A symlinked manifest is resolved first, so the rename lands on the file the link
points at instead of replacing the link with a regular file.

#### Carried comments can be misattributed

A group header and a note about one specific dependency are the same thing to the
parser — comment lines in an entry's decor. Where that decor lives depends on how the
entry is written: on the key for a plain value, on the table for a
`[workspace.dependencies.name]` sub-table, and on the first inner key for a dotted
`name.version = "1"`. Comments are read from, and written to, the same slot; reading
one slot and writing another would render both and put text in the manifest that
nobody wrote.

When the noted entry is the one removed, its note lands on the next surviving entry
and reads as if it were written about that one, which is worse than dropping it: a
dropped comment shows up in the `--fix` diff, a wrong attribution outlives it.

The carry-forward still earns its keep for headers, so it stays, and the relocation is
made visible instead: every move is reported on stderr, naming the entries the
comments came from and the entry they landed on, so whoever reviews the diff knows
which lines to check.

Comments cannot always be placed. When the removed entries are last in the table the
carried text has to go *after* the final survivor rather than ahead of it, and only a
plain value has a suffix to append to — so a dotted key or a sub-table survivor there,
and an emptied table with no survivor at all, lose the comments with the group they
introduced. That is reported as a drop. (A dotted survivor elsewhere in the table is
carried onto normally; it is only appending *after* one that has nowhere to go.) The
report describes what happened rather than what was attempted: claiming a move that
did not happen would send the reviewer hunting for text that is not in the diff.

## 7. CI integration

The complete check replaces both `cargo-udeps` and `cargo-machete`, but Anvil cannot
adopt it in the same change that first publishes the implementation. Generated Anvil
setup installs the pinned release from crates.io, so the sequence is:

1. publish `cargo-unused-deps` 0.2.0 with the complete checker;
2. update Anvil's pin and generated recipes in a follow-up change.

The Anvil recipe always invokes the tool. It obtains the `required` impact tier and
forwards a nonempty result as Cargo-style package selectors. When impact reports
`--skip`, the recipe passes no package selector: the workspace-global catalog check
still runs, while compile-evidence checks have an explicitly empty package set.
With impact disabled, the recipe passes `--workspace`.

This split is essential. A workspace-root manifest change may be invisible to
package-level impact analysis, so allowing `--skip` to bypass the process would miss
an uninherited catalog entry. Conversely, compiling the whole workspace merely to ask
the global manifest question would discard the value of impact scoping.

## 8. Out of scope

- Unused entries in a nested workspace's catalog; each workspace root is checked by
  its own invocation.
- `[patch]`, `[replace]`, and `[profile]` tables.
- Dependencies used only on uncompiled platforms or feature selections. The allow-list
  handles intentional cases; exhaustively cross-compiling every target is not a lint.
- Automatically deleting a package dependency or moving a misplaced declaration.
  Compile evidence is sufficient to fail and explain, not to edit uncompiled paths.
