# cargo-anvil — Container Backend (opt-in)

This document describes the **optional, local-only container backend**: a way to run any
`anvil-*` recipe inside a pinned Linux container instead of against the host toolchain. It is
**opt-in** and additive — it changes nothing about how the existing recipes, the cloud-workflow
backends, or the update algorithm behave.

See also:

- [design.md](./design.md) for the overall principles (esp. §4 "writes files / `just` runs them").
- [local.md](./local.md) for the `justfiles/anvil/` recipe surface this backend wraps.
- [extensibility.md](./extensibility.md) for the catalog seam a downstream fork uses to supply
  its own image + auth.

## 1. Problem

The local layer assumes a usable host toolchain ([design.md §3 non-goals][design]: "the user owns
it locally"). Two situations break that assumption:

1. **Linux-on-Windows parity.** A developer on Windows cannot reproduce a Linux-only check failure
   (a `cfg(unix)` path, a Linux-specific clippy lint, an `mmap`-shaped test) without a Linux box.
2. **Distro drift.** Even on Linux, the host distro/toolchain can differ from the one CI uses, so a
   green local run is not predictive of CI.

Both are solved the same way: run the recipe in a container whose distro and toolchain are pinned
to match CI. This is a **convenience for the inner loop**, not a new way to run CI — see §7.

## 2. Principles (what this backend is, and is not)

It inherits the engine's stance verbatim and adds three constraints:

- **`cargo-anvil` still only writes files.** The container backend is a handful of additional
  *owned files* under `justfiles/anvil/container/`. The engine is not on the build hot path.
- **The recipes do not change.** `just anvil-pr` is byte-identical whether run natively or in the
  container. The container is a transparent *execution environment*, selected by an explicit entry
  point — never by rewriting recipe bodies.
- **Opt-in with no new engine state.** anvil is one idempotent command with no persisted flags
  ([design.md §5.2][design]). The container files are emitted like any other owned artifact; whether
  to *use* them is a pure runtime choice (run the wrapper recipe, or don't). Nothing enters
  `.anvil.lock` semantics, the decision table, or the cloud-workflow YAML.
- **Local-only, Linux-only (initially).** The container runs `x86_64-unknown-linux-gnu`. Drivers
  request `linux/amd64` explicitly, so ARM hosts require Podman emulation support. Windows-specific
  checks and any Windows cloud-workflow leg still run natively.

## 3. The whole picture

```text
repo/
├── Justfile                                    managed regions: anvil-imports, + optional anvil-runner (§4.1)
└── justfiles/anvil/
    ├── mod.just                                imports container.just (when the backend is present)
    ├── checks.just / groups.just               (unchanged — run natively *inside* the container)
    ├── tiers.just                              public tiers delegate one hop to the runner seam (§4.1)
    ├── runner.just                             the `_anvil-run` execution seam (core: native; container group: routes) (§4.1)
    ├── tools.just / versions.just              ← image is built by running these (§5)
    └── container/                              ← the entire container backend (owned files)
        ├── container.just        the `anvil-container` entry-point recipe (§4)
        ├── Containerfile         generic skeleton: FROM ${BASE_IMAGE} + `RUN just anvil-setup` (§5)
        ├── run-in-container.sh   driver (Unix / WSL): ensure image, podman run (§6)
        ├── run-in-container.ps1  driver (native Windows + Podman Desktop) (§6)
        ├── auth.sh / auth.ps1    OPTIONAL credential hook, sourced if present (§6.3)
        └── README.md             prerequisites, opt-out, troubleshooting
```

The flow for `just anvil-container anvil-pr`:

```
just anvil-container anvil-pr
        │  (container.just dispatches to the OS-appropriate driver)
        ▼
run-in-container.{sh,ps1}
        ├─ compute image tag = sha256(versions.just + tools.just + rust-toolchain.toml + Containerfile)
        ├─ if image missing/stale → podman build (sources auth.* hook if present)
        └─ podman run --rm \
               -v <repo>:/workspace \
               -v anvil-cargo-registry:/…/registry  -v anvil-target:/workspace/target  (named volumes)
               <image>  just anvil-pr           ← same recipe, ANVIL_IN_CONTAINER=1 inside
```

Key property: the image is *defined by the same files that define the recipes*, so it can never
drift from them (§5).

## 4. Entry point — explicit, not a shim

A single owned recipe in `container/container.just`, imported by `mod.just`:

```just
# Run any anvil recipe inside the pinned Linux container (opt-in).
#   just anvil-container anvil-pr        # the whole PR tier, in Linux
#   just anvil-container anvil-clippy    # a single check
#   just anvil-container                 # interactive shell in the image
[group("anvil-container")]
[windows]
[script("pwsh")]
anvil-container *recipe:
    # invoke run-in-container.ps1

[group("anvil-container")]
[unix]
[script("bash")]
anvil-container *recipe:
    # invoke run-in-container.sh
```

- It accepts **any** recipe name (check, group, or tier), so the full `anvil-*` surface is reachable
  without enumerating it.
- It shows up in `just --list` under its own `anvil-container` group, so it is discoverable rather
  than hidden behind PATH magic.
- It guards against recursion via `ANVIL_IN_CONTAINER` (set inside the image): if the recipe is ever
  invoked *inside* the container, the wrapper is a no-op pass-through to native `just`.

By default, `just anvil-pr` (and the other tiers) still run **natively** — the container is reached
only through this explicit `anvil-container` recipe. A repo or a developer can flip that default so
the bare tiers route through the container; that is a deliberate, owned policy toggle, not silent
magic — see §4.1.

### 4.1 Making the container the default (per-project or per-person)

The goal: after a one-time, *owned* gesture, `just anvil-pr` runs in the container instead of
natively while preserving the same generated recipe definitions. The design separates **policy**
(which runner — a user choice) from **mechanism** (how a tier executes — tool-owned).

**Mechanism — a one-hop execution seam (tool-owned).** The public tier recipes delegate to a
`_anvil-run` seam; the real dependency chains move to private `_anvil-<tier>` aggregators (their
bodies unchanged):

```just
# tiers.just — public entry points delegate to the runner seam
anvil-pr:        ; @just _anvil-run pr
anvil-scheduled: ; @just _anvil-run scheduled
anvil-full:      ; @just _anvil-run full

_anvil-pr: anvil-pr-validate-prereqs anvil-pr-fast anvil-pr-slow      # was `anvil-pr`, now private
_anvil-scheduled: anvil-scheduled-validate-prereqs anvil-scheduled-test …
_anvil-full: _anvil-pr _anvil-scheduled
```

The **core** `runner.just` is a native-only default (one extra sub-second `just` spawn; zero
behavior change for consumers who never enable the container backend):

```just
# runner.just (core default): execution is "native"
_anvil-run tier: ; @just _anvil-{{ tier }}
```

When the container backend is present, it **`replace_artifact`s `runner.just`** with a routing
version that consults the toggle and guards against recursion:

```just
# runner.just (container-group override): route per the toggle
_anvil-run tier:
    {{ if env_var_or_default("ANVIL_IN_CONTAINER","") != "" { "@just _anvil-" + tier } \
       else if anvil_runner == "container" { "@just anvil-container _anvil-" + tier } \
       else { "@just _anvil-" + tier } }}
```

Inside the image, `anvil-container` invokes `just _anvil-<tier>` (the private *native* aggregator)
and `ANVIL_IN_CONTAINER=1` is set, so the seam forces native — no recursion, and the actual check
recipes run identically to a native run. Only the three tier entry points route (one `podman run`
per tier — the efficient unit); ad-hoc single checks stay explicit via `just anvil-container
anvil-clippy`.

**Policy — a one-line toggle the user owns (user-co-owned region).** The container backend adds a
second managed region to the repo `Justfile`, beside `anvil-imports`:

```just
# >>> anvil-managed: anvil-runner
anvil_runner := env_var_or_default("ANVIL_RUNNER", "native")
# <<< anvil-managed: anvil-runner
```

This sits in user-co-owned space (the `Justfile`) because "how I want to run things" is a user
policy choice, while the mechanism above stays tool-owned and keeps receiving updates.

**The two opt-in axes** map directly onto anvil's ownership/drift model ([updates.md §5][updates]):

- **Whole project → default to containers.** Edit the `anvil-runner` region, change `"native"` to
  `"container"`, and **commit it**. That is a managed-region edit, so the dirty-region flow
  (`D≠L, T==L` → leave alone, **no proposal**, zero noise) preserves it forever while every other
  anvil file still updates. From then on `just anvil-pr` runs in the container for everyone on the
  repo.
- **One person / one shell, either direction.** Set `ANVIL_RUNNER=container` (or `=native` to opt
  back out of a committed project default) in your environment, or one-off
  `just anvil_runner=container anvil-pr`. No file edit, nothing committed; the env var always wins
  over the committed default because the region reads it via `env_var_or_default`.

The seam adds one behavior-neutral dispatch hop while keeping the tier dependency graph in one
tool-owned location.

## 5. Image identity — built from anvil's own pins

The Containerfile is a thin, generic skeleton:

```dockerfile
ARG BASE_IMAGE                       # the engine default is a rustup + crates.io base
FROM ${BASE_IMAGE}
# install `just`, copy the repo's anvil tree, then install the EXACT catalog toolset
COPY rust-toolchain.toml justfiles/ /build/
RUN just anvil-setup                 # ← installs every pinned tool from tools.just/versions.just
WORKDIR /workspace
```

This is the design's keystone and an advantage unique to anvil:

- **Zero tool-list duplication.** anvil already owns `tools.just` (install recipes) and
  `versions.just` (pins) as the single source of truth ([local.md §3][local]). The image is just
  "base distro + `just anvil-setup`", so the in-container toolset is *the same set the recipes
  expect*, by construction.
- **Content-addressed tag → automatic rebuild.** The driver tags the image
  `anvil-dev:<sha>` where `<sha>` covers `rust-toolchain.toml`, the generated `.just` recipe tree,
  the Containerfile, and other build inputs in `justfiles/anvil/container/`, including optional auth
  hook source. Bump a pin or change non-secret hook-defined build customization, and the next
  `just anvil-container` rebuilds automatically — the same mechanism that keeps the recipes current
  keeps the image current.

The **engine default base** uses public `rustup` + crates.io, consistent with anvil's zero-internal-
dependencies stance ([design.md §2 goal 8][design]). A fork overrides `BASE_IMAGE` (and the toolchain
installer / auth) via the catalog — see §8.

The backend requires a repository-owned `rust-toolchain.toml`. It does not use
floating `stable` as a fallback because that would allow identical image tags
to resolve to different compiler versions over time.

## 6. Drivers

Two scripts implement one contract: *ensure the image, then `podman run <args>` against the tree*.
They are parameterized over image name, mount layout, and cache-volume names — no environment-specific
logic lives here.

### 6.1 Mounts and caches
- The repo is bind-mounted at `/workspace`.
- `target/` and the cargo registry/git caches live in **named volumes**
  (`anvil-target`, `anvil-cargo-registry`, `anvil-cargo-git`), so the hot write path never crosses a
  slow host↔VM boundary (critical on native Windows) and the host `target/` is never touched.
- On Unix, rootless `--userns keep-id` keeps files you create owned by you; on native Windows,
  ownership follows the Windows mount.

### 6.2 Driver knobs (env vars — driver-scoped only)
| Variable | Effect |
|---|---|
| `ANVIL_CONTAINER_IMAGE` | Override the image name (default `anvil-dev`). |
| `ANVIL_CONTAINER_NO_REBUILD=1` | Fail instead of auto-building a missing/stale image. |
| `ANVIL_IN_CONTAINER` | Set *by* the image; the wrapper recipe uses it to avoid recursion (§4). |

### 6.3 GitHub authentication
For checks such as `anvil-aprz`, the driver uses an existing host `GITHUB_TOKEN`
or obtains one non-interactively from the authenticated host `gh` CLI. It writes
the token to a user-only temporary file, mounts that file read-only for the
isolated `anvil-aprz` container invocation, then runs the remaining requested
checks without the token and removes it on exit. The token is not stored in the
image or passed through the OCI environment. If an interactive host has `gh`
installed but is not authenticated, the driver explains why authentication is
needed, waits while the user runs `gh auth login` in another terminal, and
retries after the user presses Enter. Non-interactive runs fail before image
building with the same remediation.

### 6.4 Auth hook (the catalog seam)
The engine driver needs no credentials (crates.io is public). Some downstream catalogs do. The driver therefore
**sources `container/auth.sh` / `auth.ps1` if it exists**, before `podman build`/`podman run`, to
populate registry/toolchain tokens (e.g. as a BuildKit `--secret` at build time and an `--env-file`
at run time). The engine ships no `auth.*`; a fork supplies it via the catalog (§8). This keeps the
hard, environment-specific credential dance out of the open-source engine.

Auth-hook source is hashed as a non-secret build-configuration fingerprint. Hooks must obtain secret
values at runtime rather than embedding them; token values and temporary secret-file contents are not
part of the image identity.

## 7. Relationship to CI

This backend is **local-only**. CI continues to run the recipes natively on its own pool
(see [ado.md][ado] / [github.md][github]). The container image is
*pinned to match* CI's distro, giving local↔CI parity, but it is a separate image, not the CI
environment itself. Running the *CI* jobs inside the same image (for bit-for-bit parity) is possible
but is gated by each backend's constraints (for example, setup tasks may expect to own the toolchain) and
is left as future work, not part of this opt-in.

## 8. Extensibility — how a fork supplies image + auth

The container backend is an ordinary catalog artifact group, so a fork customizes it with the same
`replace_artifact` / `without_artifact` levers as any other artifact ([extensibility.md][ext]):

- **`replace_artifact`** the `Containerfile` to change `BASE_IMAGE` and the toolchain installer
  (for example, an organization-specific distro and toolchain source).
- **`with_artifact`** an `auth.sh` / `auth.ps1` to inject private-registry credentials (§6.3).
- **`without_artifact`** the whole group to ship a catalog with no container backend at all.

The driver scripts, the `anvil-container` recipe, and the image-tagging logic are inherited
unchanged, so a fork writes only the two things that are genuinely environment-specific: the image
base and the credential hook.

## 9. Costs and trade-offs (honest accounting)

- **Host prerequisite:** developers must install Podman (Podman Desktop on Windows). Opt-in cost.
- **Maintenance surface:** ~5 platform files (two drivers, the Containerfile, the recipe, a README)
  shipped as templates the engine must keep working across consumers.
- **Coverage:** Linux-only and local-only; Windows-specific checks and CI are unaffected.
- **First-run latency:** the initial image build takes minutes; subsequent runs reuse the image and
  the persistent caches at near-native speed.

## 10. Verification

- **Catalog unit tests:** the container group is present (or, for a fork that drops it, absent); the
  Containerfile builds the toolset via `just anvil-setup`; the `anvil-container` recipe dispatches
  explicitly to the platform driver.
- **Fixture/golden test:** the emitted `container/` tree is snapshotted like the rest of the catalog.
- **Smoke (manual / opt-in CI):** on a Podman-equipped agent, `just anvil-container anvil-clippy`
  builds the image once and runs the check green.

## 11. References

- `cargo-anvil` overall design — [`design.md`][design]
- `cargo-anvil` local recipe surface — [`local.md`][local]
- `cargo-anvil` extensibility — [`extensibility.md`][ext]

[design]: ./design.md
[local]: ./local.md
[updates]: ./updates.md
[ext]: ./extensibility.md
[ado]: ./ado.md
[github]: ./github.md
