# cargo-unique-target-names — Design

> Status: **Adopted**.
> Crate name: `cargo-unique-target-names`.
> Home: `github.com/microsoft/ox-tools`, published to crates.io.

## 1. Problem

Cargo compiles every unit into `target/<profile>/deps/` under a name carrying a
metadata hash, then *uplifts* the root unit of most target kinds into
`target/<profile>/` (examples into `target/<profile>/examples/`) under a plain
name carrying no hash. The hashed copy is unique by construction; the uplifted
one is not. When two workspace packages own targets that uplift to the same
file, both write that file.

Cargo notices, reports `output filename collision`, warns that it may become a
hard error, and then builds anyway. The result is a workspace that stays green
while the last writer wins, parallel jobs race for a single path, and the
artifact that ends up on disk depends on build order.

The most likely way to reach this state requires no configuration at all. Cargo
normalizes `-` to `_` in the default library target name, so packages `foo-bar`
and `foo_bar` in one workspace both uplift to `libfoo_bar.rlib` without anyone
writing a `[lib]` section. The class has already reached `oxidizer`'s `main`
twice through duplicated example names.

Reviewing for this does not scale: the collision is a property of the whole
workspace, and only one half of a colliding pair appears in the diff that
creates it.

## 2. Goals

1. **Fail before the build.** Detect the collision from workspace metadata, so
   the answer arrives in seconds and does not depend on compiling anything.
2. **Match Cargo exactly.** Report what Cargo warns about — no more, so a clean
   workspace is never rejected, and no less, so the guard is worth having.
3. **Name the contended file.** A diagnostic that names the target, every owning
   package, how each package spells it, and the exact path they contend for is
   actionable without opening a manifest.
4. **Platform-independent verdict.** Every leg of a build matrix reaches the
   same conclusion, so a Windows-only collision is not invisible to a Linux run.
5. **Cargo-native UX.** Ship as `cargo unique-target-names`, composable with the
   rest of the tool chain and with no wrapper script.

## 3. Non-goals

- Building anything. The tool reads `cargo metadata --no-deps` and never invokes
  a compiler.
- Renaming targets. The tool reports; the human chooses the new name.
- Policing duplicate names *within* one package. Cargo already rejects those.
- Enforcing a naming convention. Uniqueness of uplifted paths is the only
  invariant; what the unique names look like is a project decision.

## 4. User-visible shape

### Invocation

```bash
cargo unique-target-names [--manifest-path <PATH>]
```

| Option            | Default            | Meaning                                  |
|-------------------|--------------------|------------------------------------------|
| `--manifest-path` | discovered from cwd | Manifest whose workspace is inspected.   |

### Exit codes

| Code | Meaning                                                              |
|------|----------------------------------------------------------------------|
| 0    | Every uplifted file has exactly one owner.                            |
| 1    | At least one file is contended; each is reported on stderr.           |
| 2    | `cargo metadata` could not be run or understood; the workspace is broken. |

A workspace that cannot be read is an error rather than a pass, so a
misconfigured repository cannot report itself clean — and it carries its own
code, so a caller can tell "broken" from "contended" without parsing output.

### Output

```text
cargo-unique-target-names: target 'basic' is declared by 2 targets: metabench (example 'basic'), observed (example 'basic')
  they uplift to the same files: target/<profile>/examples/basic.pdb, target/<profile>/examples/basic[.exe]

Rename the reported targets so each one uplifts to its own path.
```

File names are shown as patterns rather than resolved per platform, because the
verdict is platform-independent: `[lib]name[.so|.dll|.dylib]` stands for the one
shared-library file whatever the host emits.

## 5. The rule

Targets are keyed by **every file they uplift**, not by crate type. The
relationship between the two runs in both directions, and each of the following
was verified against Cargo rather than assumed:

| Workspace                                     | Cargo warns at                        |
|-----------------------------------------------|---------------------------------------|
| packages `foo-bar` and `foo_bar`, no config   | `libfoo_bar.rlib`                     |
| two packages with `[lib] name = "shared"`     | `libshared.rlib`                      |
| two proc-macros named `shared_pm`             | `shared_pm.dll` (+ `.dll.lib`, `.dll.exp`, `.pdb`) |
| two `[[example]]` with `crate-type = ["lib"]` | `examples/liblibrary_example.rlib`    |
| a proc-macro and a `cdylib` of one name       | `shared_pm.dll`                       |
| a binary `tool` and a `cdylib` `tool`         | `tool.pdb` only — the primary files differ |
| an `rlib` and a `cdylib` of one name          | *nothing*                             |
| a binary and an `rlib` / `staticlib` of one name | *nothing*                          |

So crate types map to artifact families:

| Family         | Crate types                       | Uplifted files                               |
|----------------|-----------------------------------|----------------------------------------------|
| executable     | `bin`                             | `name[.exe]`, `name.pdb`                     |
| rust library   | `lib`, `rlib`                     | `libname.rlib`                               |
| shared library | `cdylib`, `dylib`, `proc-macro`   | `[lib]name[.so\|.dll\|.dylib]`, `name.pdb`   |
| static library | `staticlib`                       | `[lib]name[.a\|.lib]`                        |

`cargo metadata` reports an ordinary library as crate type `lib`, not `rlib`, so
both spellings map to the rust-library family.

### What is exempt, and why

- **`test`, `bench`, and `custom-build` targets.** These are never uplifted;
  they keep their metadata hash in `deps/`, so duplicate names cannot contend.
- **Derived file names.** The import library and export file beside a Windows
  DLL, and the `<artifact>.cargo-sbom.json` sidecar, all take their name from
  another artifact. They contend exactly when that artifact does, so keying them
  separately would only duplicate findings. The debug-info file is *not* derived
  that way — it is `name.pdb`, not `name.exe.pdb` — which is why it is keyed.
- **Nothing on the basis of package boundaries.** Cargo permits a `[lib]` and a
  `[[bin]]` of one name in a single package, and on Windows they contend for the
  debug-info file — cargo warns, and the link itself can fail with `LNK1201`.
  Owners are therefore keyed per *target*, not per package, so a package that
  contends with itself is reported like any other pair. (Two targets of the
  *same* kind sharing a name is a Cargo error, and never reaches this tool.)

### Determinism

Ordering and identity both come from `BTreeMap`, so keys are compared byte-wise
and output is byte-ordered. Two consequences are deliberate:

- Target names differing only in case stay distinct, matching Cargo, which keys
  its own collision check by exact path. Folding them would fail a workspace
  Cargo builds without complaint. This is a deliberate *miss* on
  case-insensitive filesystems — see §6.
- The report reads identically on every host, with no dependence on locale
  collation.

### Platform independence

The Windows debug-info file is considered on every platform. This tool gates
workspaces that must build everywhere, so a collision that only manifests on
Windows should fail a Linux run too, and every leg of a build matrix should
agree on the verdict. The cost is that a Linux-only workspace could in principle
be told about a file it never emits; that is the intended trade.

## 6. Known misses

The rule reports what Cargo reports. Cargo's own `output filename collision`
check does not cover every way a workspace can lose a build artifact, so the
following hazards are real and are **not** currently reported. Both were
verified on Windows; neither produces a Cargo warning.

### Dep-info files collapse by file stem

Cargo writes a dep-info file named after the artifact's *file stem*, not its
full name: `tool.d`, not `tool.lib.d`. On MSVC the stems of `tool.exe`,
`tool.lib`, and `tool.dll` all collapse to `tool`, so families this document
treats as disjoint do contend for the dep-info file. A workspace with
`staticlib tool` in one package and `cdylib tool` in another produces a single
`tool.d` describing only the `cdylib`; the static library's dep-info is gone. By
the same naming rule this also applies on Linux, where `libtool.a` and
`libtool.so` both stem to `libtool.d`.

Extending the model would mean keying a `{stem}.d` artifact per family, with the
stem differing per platform (`tool` on Windows, `libtool` on Unix for the
library families). Under the platform-independence rule above that would make
almost every same-named pair contend, leaving only the executable/rust-library
pair disjoint — a materially stricter tool than "what Cargo warns about".

### Case-insensitive filesystems

On NTFS and on a default macOS volume, two libraries whose target names differ
only in case resolve to one file. A workspace with `[lib] name = "Shared_lib"`
in one package and `[lib] name = "shared_lib"` in another builds without a
Cargo warning and produces a single `libShared_lib.rlib`. The ordinal keying
described above reproduces Cargo's verdict and therefore reproduces this miss.

Reporting it would mean treating case-insensitive duplicates as a distinct
finding, and — to keep verdicts identical across a build matrix — reporting them
on case-sensitive hosts too, where no file is actually lost.

Both extensions are deliberate open questions rather than oversights: they trade
Goal 2 (match Cargo) against Goal 1 (prevent the race), and that trade should be
made explicitly rather than by accident.

## 7. Out of scope

Integration with a specific CI system. The tool is a plain cargo subcommand with
a meaningful exit code; wiring it into a pipeline belongs to whatever drives the
build.

In this repository that wiring is **follow-up work**, not yet present on `main`:
a `unique-target-names` check in [`cargo-anvil`](../../../cargo-anvil) will
invoke this command once the crate is published, because Anvil pins tools by
published crates.io version. When it lands, the check must be **unscoped** — it
must not be impact-gated to modified packages, because a collision is a property
of the whole workspace and only one member of a contending pair needs to appear
in a diff to create one.
