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
| other| `cargo metadata` could not be run or understood; the workspace is broken. |

A workspace that cannot be read is an error rather than a pass, so a misconfigured
repository cannot report itself clean.

### Output

```text
cargo-unique-target-names: target 'basic' is declared by 2 workspace packages: metabench (example), observed (example)
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
- **Duplicates inside one package.** Already a Cargo error.

### Determinism

Ordering and identity both come from `BTreeMap`, so keys are compared byte-wise
and output is byte-ordered. Two consequences are deliberate:

- Target names differing only in case stay distinct, matching Cargo, which keys
  its own collision check by exact path. Folding them would fail a workspace
  Cargo builds without complaint.
- The report reads identically on every host, with no dependence on locale
  collation.

### Platform independence

The Windows debug-info file is considered on every platform. This tool gates
workspaces that must build everywhere, so a collision that only manifests on
Windows should fail a Linux run too, and every leg of a build matrix should
agree on the verdict. The cost is that a Linux-only workspace could in principle
be told about a file it never emits; that is the intended trade.

## 6. Out of scope

Integration with a specific CI system. The tool is a plain cargo subcommand with
a meaningful exit code; wiring it into a pipeline belongs to whatever drives the
build — in this repository, the `unique-target-names` check in
[`cargo-anvil`](../../../cargo-anvil).
