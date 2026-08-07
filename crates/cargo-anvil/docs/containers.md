# Containers

`cargo-anvil` can run your generated `just` recipes inside a container, build OCI images, and stand up a throwaway
Kubernetes cluster for integration tests.

All three are **opt-in and independent**. They are configured in a single file — `anvil.toml` at the repo root — which is
itself optional. A repository with no `anvil.toml` regenerates byte-for-byte identically to a build with no container
support at all, so adopting nothing costs nothing.

- [1. Quick start](#1-quick-start)
- [2. Usage](#2-usage)
- [3. Configuration reference](#3-configuration-reference)
- [4. How it works](#4-how-it-works)
- [5. Extension points](#5-extension-points)
- [6. Prerequisites and limits](#6-prerequisites-and-limits)

## 1. Quick start

Create `anvil.toml` in the repo root:

```toml
[container]
enabled = true
image = "docker.io/library/rust:1.85"
```

Re-run the generator and start working:

```bash
cargo anvil                  # regenerate; now emits the container recipes
just anvil-container-up      # pull the image
just anvil-pr                # runs inside the container
```

That is the whole adoption story for containerized execution. Image builds and the cluster harness are added by declaring
`[[image]]` and `[cluster]` sections — see [Configuration reference](#3-configuration-reference).

## 2. Usage

Everything is a `just` recipe. `cargo anvil` writes the recipes; it is never on the runtime path.

### Containerized execution

Emitted when `[container] enabled = true`.

| Recipe | Purpose |
| --- | --- |
| `just anvil-container-up` | Pull the configured image so the next run starts warm. |
| `just anvil-container-status` | Report the resolved engine, image, workdir, and whether the image is present locally. |
| `just anvil-container-shell` | Interactive shell in the image with the same mounts a recipe run uses. |
| `just anvil-container-rebuild` | Force a fresh pull of the image. |
| `just anvil-container-down` | Remove this repo's cache volumes. The image is left in place. |

The tier and group recipes (`anvil-pr`, `anvil-scheduled`, `anvil-full`, and each `anvil-pr-*` / `anvil-scheduled-*`
group) gain a re-entry guard, so the command you already use is unchanged:

```bash
just anvil-pr      # transparently re-invoked inside the container
```

Set `ANVIL_IN_CONTAINER=1` on the host to force a single invocation to run natively — the escape hatch when you need to
bypass the container without editing config.

### Image builds

Emitted when at least one `[[image]]` is declared.

| Recipe | Purpose |
| --- | --- |
| `just anvil-image-<name>` | Build one declared image. |
| `just anvil-images` | Build every declared image in dependency order. |

Both take the same three optional parameters:

```bash
just anvil-images <profile> <tag> <registry>    # defaults: debug dev local
```

`profile` selects which prebuilt binaries are staged, `tag` is the image tag, and `registry` is the ref prefix. The final
reference is `<registry>/<name|repository>:<tag>`.

### Cluster harness

Emitted when a `[cluster]` section is declared.

| Recipe | Purpose |
| --- | --- |
| `just anvil-cluster-preflight` | Verify the engine and `kind` / `kubectl` / `helm` are present. Installs nothing. |
| `just anvil-cluster-bootstrap` | Install the pinned, checksum-verified cluster tooling on the host. |
| `just anvil-cluster-up` | Create the cluster if absent. Idempotent. |
| `just anvil-cluster-load` | Load the declared images into the cluster. |
| `just anvil-cluster-deploy` | Apply dependencies, then install or upgrade the declared charts. |
| `just anvil-cluster-test` | The full flow with bounded retries — the one CI calls. |
| `just anvil-cluster-diagnostics` | Dump the configured resources and logs. Runs automatically on failure. |
| `just anvil-cluster-down` | Delete the cluster. |
| `just anvil-cluster-clean` | `mode=cluster` deletes the cluster; `mode=full` also removes the built images. |

`up`, `load`, `deploy`, and `test` take the same `profile` / `tag` / `registry` parameters as the image recipes, so the
references loaded into the cluster always match the references built.

## 3. Configuration reference

`anvil.toml` has four sibling top-level sections plus one bare key. Each is independently optional. Unknown keys are a
hard error, so a typo is reported rather than silently ignored.

Because `image-output-dir` is a bare key, TOML requires it to appear **before** any `[section]` header.

### `[container]` — containerized execution

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enabled` | bool | `false` | Master switch for containerized execution. |
| `image` | string | — | Image to run recipes in. Required when `enabled = true`. |
| `engine` | `auto` \| `docker` \| `podman` | `auto` | `auto` probes for `docker`, then `podman`. |
| `name` | string | directory name | Repo identity; prefixes cache-volume names so repos on one host do not collide. |
| `workdir` | string | `/workspaces/<name>` | Mount point for the repo root inside the container. |
| `cache-volumes` | array | `["cargo", "rustup"]` | Named volumes to persist across runs. |
| `forward-env` | array | `[]` | Glob patterns; matching host environment variables are forwarded in. |
| `devcontainer` | bool | `false` | Also emit `.devcontainer/devcontainer.json` from the same settings. |
| `native-when` | table | — | Host match that runs natively instead of containerizing. |

`native-when` accepts `os-release-id` and `version-id`, matched against `/etc/os-release`. When the host matches, recipes
run directly — useful when the host already *is* the target environment.

Cache volume names map to conventional in-container paths: `cargo` and `rustup` to the official Rust image's
`CARGO_HOME` and `RUSTUP_HOME`, `target` to `<workdir>/target`, and any other name to `/anvil-cache/<name>`.

```toml
[container]
enabled = true
image = "docker.io/library/rust:1.85"
cache-volumes = ["cargo", "rustup", "target"]
forward-env = ["CARGO_*", "RUST_LOG"]
devcontainer = true
```

### `[[image]]` — locally built OCI images

Repeat the table once per image.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | string | — | Required. Must match `[A-Za-z][A-Za-z0-9_-]*` — it becomes a recipe name. |
| `repository` | string | `name` | Published path when it differs from the recipe name (a recipe name cannot contain `/`). |
| `dockerfile` | string | — | Required. Path to the Dockerfile. |
| `target` | string | — | Build stage to target in a multi-stage Dockerfile. |
| `context` | string | — | Build context. Must be nested under `image-output-dir`. |
| `stage-artifacts` | array of tables | `[]` | `{ from, to }` copies made into the staged context before the build. |
| `build-args` | table | `{}` | Build arguments, in declaration order. |
| `depends-on` | array | `[]` | Other image names that must build first. |

Set the guard root with the bare `image-output-dir` key (default `out`).

Every image is built from a **staged context**, never the repo root: prebuilt binaries are copied in by
`stage-artifacts`, and nothing is compiled in-image. A `context` outside `image-output-dir` is rejected, so the whole
repository can never be sent to the engine as build context.

`stage-artifacts` and `build-args` values may use the `{profile}` and `{tag}` tokens, expanded from the recipe
parameters at run time.

`depends-on` is validated: unknown targets and cycles are hard errors, and the build order is a deterministic
topological sort with declaration order breaking ties.

There is deliberately **no registry push, no authentication, and no promotion**. Images are built locally and consumed
locally.

```toml
image-output-dir = "out"

[[image]]
name = "api"
dockerfile = "docker/service.Dockerfile"
context = "out/api"
build-args = { RUST_PROFILE = "{profile}" }
stage-artifacts = [{ from = "target/{profile}/api", to = "api" }]
```

### `[cluster]` — ephemeral Kubernetes cluster

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | string | `anvil-kind` | Cluster name. |
| `node-image` | string | — | Pin the Kind node image. |
| `workers` | integer | `0` | Worker nodes in addition to the control plane. |
| `load-images` | array | `[]` | Which declared `[[image]]` names to load. Validated against the image set. |

Sub-tables:

| Table | Repeats | Keys |
| --- | --- | --- |
| `[[cluster.dependency]]` | yes | `name`, `manifest`, `version`, `namespace`, `preload-images`, `wait` |
| `[[cluster.chart]]` | yes | `name`, `path`, `namespace`, `crds`, `set`, `wait` |
| `[cluster.diagnostics]` | no | `resources`, `logs`, `namespace` |
| `[cluster.retry]` | no | `attempts` (default `1`), `delay-seconds` (default `0`) |
| `[cluster.hooks]` | no | `pre-install`, `post-install`, `pre-test`, `on-failure` |

Dependencies are external, pinned charts or manifests applied **before** your own charts. A chart's `crds` path is
installed and established before the chart itself, which is what makes a chart that depends on a custom resource install
reliably rather than racing a webhook that is not yet serving.

`wait` entries are readiness targets, passed to `kubectl rollout status` after the item is applied and resolved in the
enclosing dependency's or chart's `namespace`. Set that `namespace` — without it the wait resolves against `default` and
will appear to pass while the real workload is still starting.

```toml
[cluster]
name = "svc-test"
load-images = ["api"]

[[cluster.dependency]]
name = "cert-manager"
manifest = "https://github.com/cert-manager/cert-manager/releases/download/v1.16.1/cert-manager.yaml"
namespace = "cert-manager"
wait = ["deployment/cert-manager-webhook"]

[[cluster.chart]]
name = "api"
path = "charts/api"
namespace = "svc"
set = { "image.tag" = "dev" }
wait = ["deployment/api"]

[cluster.retry]
attempts = 3
delay-seconds = 10

[cluster.diagnostics]
namespace = "svc"
resources = ["pods", "events"]
logs = ["deployment/api"]
```

### `[anvil]` — partial adoption

By default anvil manages every artifact in its catalog. The `artifacts` allow-list narrows that to named groups:

```toml
[anvil]
artifacts = ["recipes", "container"]
```

| Group | Covers |
| --- | --- |
| `recipes` | The `justfiles/anvil/` tree and the `Justfile` imports region. |
| `config` | Managed regions in `Cargo.toml`, `deny.toml`, `rustfmt.toml`, and friends. |
| `backends` | The generated GitHub Actions / Azure DevOps files. |
| `container` | Everything documented on this page. |

This lets an established repository adopt container support without simultaneously handing over its lint and dependency
configuration.

- **Omitting the key selects every group** — identical to the behaviour before the allow-list existed.
- An unknown name is a hard error listing the valid groups; an empty list is an error too.
- Removing a group later cleanly retracts what it emitted: owned files anvil created are deleted and managed regions it
  spliced are removed. Nothing anvil never owned is touched.

Group selection **composes with** the existing gates, it never overrides them — an artifact is emitted only when its
group is selected *and* its own gate is open.

## 4. How it works

### Everything is baked at generation time

`cargo anvil` resolves your configuration and writes the resulting values directly into the emitted recipes. Nothing
re-reads `anvil.toml` at run time, so the shim, the image recipes, the cluster harness, and the devcontainer descriptor
cannot drift from each other or from the file that produced them. Changing configuration means re-running the generator.

```text
anvil.toml ──► cargo anvil ──► justfiles/anvil/container.just         (the shim)
                               justfiles/anvil/container-images.just  (per-image recipes)
                               justfiles/anvil/cluster.just           (cluster harness)
                               justfiles/anvil/cluster-bootstrap.just (host tooling)
                               .devcontainer/devcontainer.json        (optional)
                               ── plus a re-entry guard spliced into
                                  the tier and group recipes
```

### Layered configuration

Effective settings come from three layers, each overriding the previous **field by field**:

1. Built-in defaults.
2. Catalog defaults, if you consume a downstream tool built on anvil's engine (see
   [extensibility.md](./design/extensibility.md)). An organization can ship a fork that pre-fills `image` so its
   repositories only need `enabled = true`.
3. Your `anvil.toml`.

A value you set wins for that field only; every unset field falls through.

### The re-entry guard

There is exactly one indirection point, `_anvil-container-run`, and two environment variables:

| Variable | Set by | Meaning |
| --- | --- | --- |
| `ANVIL_CONTAINER` | the generated shim | Container execution is configured for this repo. |
| `ANVIL_IN_CONTAINER` | the shim, when starting the container | We are already inside; run directly. |

A guarded recipe re-invokes itself through the shim only when `ANVIL_CONTAINER=1` and `ANVIL_IN_CONTAINER` is unset.
Inside the container the second variable is set, so nested invocations run natively and the work happens exactly once —
one container per top-level command, not one per check.

The guard is applied to the three tier recipes and the group recipes only, never to individual checks. That keeps a
fast local check on the host instead of paying container start-up for it.

### What the shim does

`_anvil-container-run` mounts the repo root at `workdir`, maps your current working directory to its in-container
equivalent so relative paths keep working, mounts the configured cache volumes, forwards environment variables matching
`forward-env`, and propagates the exit code faithfully.

Credentials are never mounted. If a recipe needs a token inside the container, forward it through `forward-env`.

### CI

When container execution is enabled, the generated CI for both backends gains a job-level container so each check group
runs in the same pinned image as your local runs. When it is disabled, the generated CI is byte-identical to a build
without container support.

### Devcontainer

With `devcontainer = true`, the emitted `.devcontainer/devcontainer.json` is rendered from the same resolved settings —
same image, same workdir, same volumes — so the editor and the command line cannot disagree.

## 5. Extension points

No generic harness fits every repository. `[cluster.hooks]` names ordinary `just` recipes that anvil invokes at fixed
points, without ever modelling what they do:

| Hook | When |
| --- | --- |
| `pre-install` | Before the charts are installed. |
| `post-install` | After the charts are installed. |
| `pre-test` | After deploy, before the readiness re-check. |
| `on-failure` | On a failed attempt, before diagnostics are dumped. |

An absent hook means no invocation. A failing hook fails the step.

```toml
[cluster.hooks]
pre-install = "discover-issuer"
on-failure = "dump-extra-state"
```

Because a hook lives in your own `.just` file, it survives regeneration — you never edit a generated file to extend the
harness. The four escape valves described in the crate README apply here too.

## 6. Prerequisites and limits

- A container engine on the host: Docker is the supported path. Podman is detected best-effort by the same `engine`
  knob; there are no podman-specific recipe variants.
- The cluster harness additionally needs `kind`, `kubectl`, and `helm`. Run `just anvil-cluster-preflight` to check, or
  `just anvil-cluster-bootstrap` to install pinned, checksum-verified versions.
- Images destined for the cluster must be built without provenance attestations. The generated recipes already do this;
  a hand-rolled build that emits a multi-manifest artifact will be silently refused by the cluster loader.
- No registry integration: no push, no authentication, no promotion. Images are built and consumed locally.
- The cluster harness targets Kind. Other Kubernetes distributions are out of scope.
