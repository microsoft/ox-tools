#!/usr/bin/env bash
set -euo pipefail

if [[ -n "${ANVIL_IN_CONTAINER:-}" ]]; then
    if (($# == 0)); then exec bash; else exec just "$@"; fi
fi

if (($# > 0)) && [[ ! "$1" =~ ^_?anvil-[A-Za-z0-9-]+$ ]]; then
    echo "anvil-container: expected an anvil-* recipe, got '$1'." >&2
    exit 2
fi

command -v podman >/dev/null 2>&1 || {
    echo "anvil-container: Podman is required. See justfiles/anvil/container/README.md." >&2
    exit 1
}

version="$(podman version --format '{{.Client.Version}}' 2>/dev/null || podman --version | awk '{print $3}')"
minimum="4.3.0"
if [[ "$(printf '%s\n%s\n' "$minimum" "$version" | sort -V | head -n1)" != "$minimum" ]]; then
    echo "anvil-container: Podman $minimum or newer is required (found $version)." >&2
    exit 1
fi

repo_root="$(git rev-parse --show-toplevel)"
script_dir="$repo_root/justfiles/anvil/container"
image_id="$(pwsh -NoProfile -File "$script_dir/image-id.ps1")"
image_base="${ANVIL_CONTAINER_IMAGE:-anvil-dev}"
image="${image_base}:${image_id}"
repo_id="$(printf '%s' "$repo_root" | sha256sum | cut -c1-12)"
target_volume="anvil-target-${repo_id}-${image_id:0:12}"

ANVIL_CONTAINER_BUILD_ARGS=()
ANVIL_CONTAINER_RUN_ARGS=()
ANVIL_CONTAINER_CLEANUP=:
if [[ -f "$script_dir/auth.sh" ]]; then
    # shellcheck source=/dev/null
    source "$script_dir/auth.sh"
fi

cleanup() { "$ANVIL_CONTAINER_CLEANUP"; }
trap cleanup EXIT

if ! podman image exists "$image"; then
    if [[ "${ANVIL_CONTAINER_NO_REBUILD:-}" == "1" ]]; then
        echo "anvil-container: image $image is missing and ANVIL_CONTAINER_NO_REBUILD=1." >&2
        exit 1
    else
        podman build \
            --tag "$image" \
            --file "$script_dir/Containerfile" \
            --ignorefile "$script_dir/container.ignore" \
            "${ANVIL_CONTAINER_BUILD_ARGS[@]}" \
            "$repo_root"
    fi
fi

run_args=(
    run --rm
    --userns keep-id
    --env ANVIL_IN_CONTAINER=1
    --env HOME=/tmp/anvil-user
    --volume "$repo_root:/workspace:Z"
    --volume "anvil-cargo-registry:/usr/local/cargo/registry:U"
    --volume "anvil-cargo-git:/usr/local/cargo/git:U"
    --volume "$target_volume:/workspace/target:U"
    --workdir /workspace
)
run_args+=("${ANVIL_CONTAINER_RUN_ARGS[@]}")

if (($# == 0)); then
    podman "${run_args[@]}" --interactive --tty "$image" bash
    exit $?
fi
podman "${run_args[@]}" "$image" just "$@"
