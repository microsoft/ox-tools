# Container execution

Container execution is an explicit alternate environment for the generated
checks: `just anvil-container <command>...` ensures an image exists, then runs
the command with the repository mounted. `just anvil-container` opens an
interactive shell. Native `just anvil-pr` does not route itself into a container.
There is no repository container-settings file or second check implementation.

The driver lives in
[`container.just`](../../templates/justfiles/anvil/container.just);
the [artifact registry](../../src/anvil/artifacts/container.rs) defines the image
composition. The default image is Linux/amd64, including on ARM hosts, where
execution depends on the engine's emulation support.

## Artifact and ownership boundary

The default group contains two owned files:

- `justfiles/anvil/container.just`, the runtime driver.
- `.anvil/container/Dockerfile.dockerignore`, the admitted build context.

`.anvil/container/Dockerfile` is instead a composed host with a one-time
BuildKit syntax-directive scaffold and five ordered managed regions:

| Region ID suffix (`anvil-container-…`) | Responsibility |
|---|---|
| `base-image` | Default `ARG BASE_IMAGE`, digest-pinned |
| `base` | `FROM` and base environment |
| `tools` | Bootstrap prerequisites and toolchain |
| `setup` | Install generated catalog tools |
| `entry` | Runtime working directory/default command |

Repository content belongs in the gaps. The syntax directive must precede
comments, so it cannot be inside a sentinel-delimited region and is not
reconciled after initial creation.

Each body uses [strict region ownership](./updates.md#strict-ownership): template
or tracked-pristine bodies update, empty bodies regenerate, edited bodies
refuse. The engine validates region order and places missing known regions
relative to the scaffold/surviving regions. It does not reorder repository gaps.

An unrecognized preexisting Dockerfile is not adopted. A tracked pristine
legacy whole-file Dockerfile can transition to regions; an edited legacy file
refuses. Lost markers with region provenance also refuse rather than treating
the file as new. The host must use exact `Dockerfile` casing so its paired
Dockerfile-specific ignore file is selected consistently.

The default group does not generate `hooks.ps1`. A repository or downstream
catalog can supply it; the driver loads it when present.

## Image lifecycle

The driver computes the desired local tag, then:

1. Reuses that tag when present locally.
2. Optionally invokes `Anvil-ResolveImage` to obtain an already-built reference.
3. Builds locally if resolution is absent or unsuccessful.
4. Runs the selected reference with `--pull=never`.

An image returned by a resolver is inspected for local presence and used under
its returned reference, not relabeled as locally built. Presence does not prove
its layers match the source identity. Registry immutability/access controls and
the trusted hook determine that assurance.

`ANVIL_CONTAINER_NO_RESOLVE=1` bypasses remote resolution,
`ANVIL_CONTAINER_NO_CACHE=1` forces a from-scratch build, and
`ANVIL_CONTAINER_NO_REBUILD=1` refuses a needed build. The no-rebuild guard also
applies when no-cache is set. `anvil-container-status` queries this lifecycle
without unexpectedly compiling an image.

## Image identity

`anvil-container-tag` hashes the bytes that define the default environment:

- All files under `.anvil/container/` and `justfiles/anvil/`, including hidden
  files and repository gap/hook content, excluding `.anvil-proposed` files.
- Root `rust-toolchain`/`rust-toolchain.toml` when present.
- The resolved root MSRV, rather than the entire root Cargo manifest.

Records are sorted ordinally and length-framed with repository-relative path,
file mode and content. Known text inputs normalize CRLF to LF; other files are
hashed as bytes. This avoids Windows checkout line endings invalidating an
otherwise equivalent environment while preserving significant binary data.

Tracked modes come from the Git index, and unstaged mode drift refuses until
the index is updated. Untracked regular files are allowed and use the
filesystem mode on Unix or the normal file mode on Windows. Symlinks/reparse
points and tracked symlink entries refuse instead of following content outside
the input boundary.

The tag uses a sanitized repository-directory prefix and the first eight bytes
of SHA-256 (16 hex digits). It is a practical source cache key, not a
collision-free identity, a signed attestation, or a hermetic-build guarantee.
Unpinned network inputs can still change a rebuild. Directory-name-based cache
volumes can also be shared by unrelated checkouts with the same directory name.

If customization copies additional paths outside the hashed trees into the
image, its author must extend identity together with the Dockerfile and ignore
file. Merely admitting another source through the ignore file does not hash its
contents. Conversely, files already inside the hashed trees—including gap
instructions—are not invisible to identity.

## Build contract

The [Dockerfile templates](../../templates/anvil/container) bootstrap the native
prerequisites, Just, PowerShell and Rust, then copy the admitted generated
recipes/configuration to `/opt/anvil`. A synthetic Justfile imports `mod.just`
and runs `just anvil-setup binstall`. Importing the entry module, not copying
arbitrary files alone, determines what Just parses.

The setup uses root toolchain declarations and MSRV for compiler provisioning.
Without an overriding root toolchain file, the image explicitly selects the
declared MSRV as its default when one exists. The setup-only root Cargo manifest
is removed afterwards. Rustup auto-provisioning is disabled for runtime checks;
the image must contain the required toolchains/components.

Build credentials use secret mounts, not build arguments. Setup removes
credential material (including the relevant netrc locations) in the same layer
and prepares writable Cargo registry/git directories. Custom setup layers must
preserve that boundary; copying a secret into another layer is not made safe by
the driver's secret transport.

## Engine and host boundary

`ANVIL_CONTAINER_ENGINE` selects Docker or Podman explicitly. On Windows, if the
requested engine binary is absent, the driver can invoke that same engine in
the default WSL distribution. It does not switch engines or recover a failed
daemon by silently trying WSL. WSL invocation uses `--exec`, and mounted paths
are translated at that boundary.

The driver checks that the daemon runs Linux containers. A Windows Podman client
has additional secret and user-mapping limitations handled by the driver's
platform-specific branches; engine availability alone does not imply complete
Docker BuildKit compatibility.

Runtime mounts include the repository and its working directory, persistent
Cargo registry/git volumes, and extra Git metadata locations for linked
worktrees. The volumes do not mask preinstalled toolchains. Linux uid/gid
handling aims to leave generated files owned by the host user. A container
shares the writable checkout; it is not a sandbox for untrusted check code.

## Runtime environment

The driver forwards set values for `PR_TITLE`, `BASE_REF`, `GITHUB_BASE_REF`,
`SYSTEM_PULLREQUEST_TARGETBRANCH`, `ANVIL_IMPACT` and `ANVIL_MIRI_JOBS`, preserving
the recipe inputs that would otherwise disappear at the process boundary.

An existing `GITHUB_TOKEN` is forwarded. Otherwise a logged-in `gh` token is
obtained only when the command's dry-run recipe graph needs it, or for an
interactive shell. Additional credentials require `Anvil-RunEnv`.

Consume mode with a nonempty `ANVIL_IMPACT_INPUT_DIR` refuses: an arbitrary host
cache path cannot safely be forwarded unchanged or silently replaced with the
container's default cache. Unset that override or run natively. Other modes do
not read the override.

Environment values travel by name (`-e NAME`), not as command-line literals.
The driver also exposes the names to WSL when that is where the engine runs.
This reduces accidental command-line disclosure; container processes and build
scripts can still read forwarded credentials.

The command interface currently flattens and splits arguments on whitespace.
Arguments requiring embedded whitespace are not preserved as a general argv
transport. Prefer supported simple invocations or an interactive shell instead
of assuming shell quoting survives every boundary.

## 8. Customization

### Dockerfile gaps

Use the gap corresponding to the prerequisites an instruction needs:

| After region | Appropriate repository additions |
|---|---|
| `base-image` | Redeclare `ARG BASE_IMAGE` before `FROM` consumes it |
| `base` | CA roots, proxies and internal package mirrors before downloads |
| `tools` | Native libraries needed to compile catalog tools |
| `setup` | Runtime dependencies needed by repository checks |

This lets base/tool pins continue updating without freezing the entire
Dockerfile after one local addition. A downstream catalog can replace selected
region bodies instead; keep the setup/entry contract compatible with the driver.
Any additional copied inputs must also be admitted by `Dockerfile.dockerignore`.

### Trusted PowerShell hooks

`.anvil/container/hooks.ps1` is dot-sourced on the **host**. It is executable
repository code, not a declarative settings file; reviewing/trusting it comes
before invoking the container driver.

- `Anvil-ResolveImage($image)` may fetch an image and return the reference it
  made available. If it declares both `Engine` and `EnginePrefix` parameters,
  the selected engine invocation is supplied. The last nonempty output is the
  candidate reference. Missing images and resolution errors warn and fall back
  to local build.
- `Anvil-BuildSecrets` returns an object with a `Secrets` mapping of secret IDs
  to values. A defined hook returning no secrets, empty values or an error fails
  closed. Omit the function when no secrets are required.
- `Anvil-RunEnv` returns an object with an `Env` mapping. A defined but empty,
  erroneous or empty-valued mapping likewise fails closed. The driver logs names,
  never values, and clears hook-provided values in its process after use.

Resolution is an optimization, so its failure can fall back. Credential hooks
state that credentials are required; silently ignoring failure would run a
different, unauthenticated operation. Hook output and publisher trust do not
become verifiable merely because the expected tag contains a digest.

### Removing the feature

A downstream catalog can omit the container artifacts; `mod.just`'s optional
container import leaves native checks usable. Do not leave a driver that refers
to missing image inputs. Removing artifacts uses normal pristine-only
retirement rules, preserving edited repository content.
