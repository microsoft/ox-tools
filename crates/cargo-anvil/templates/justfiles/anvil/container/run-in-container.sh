#!/usr/bin/env bash
set -euo pipefail

if [[ -n "${ANVIL_IN_CONTAINER:-}" ]]; then
    if (($# == 0)); then exec bash; else exec just "$@"; fi
fi

if (($# > 0)) && [[ ! "$1" =~ ^_?anvil-[A-Za-z0-9-]+$ ]]; then
    echo "anvil-container: expected an anvil-* recipe, got '$1'." >&2
    exit 2
fi

anvil_recipe_needs_github_token() {
    case "$1" in
        anvil-aprz | anvil-pr | _anvil-pr | anvil-pr-fast \
            | anvil-scheduled | _anvil-scheduled | anvil-scheduled-advisories \
            | anvil-full | _anvil-full) return 0 ;;
        *) return 1 ;;
    esac
}

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
ANVIL_CONTAINER_PREPARE_ARGS=()
ANVIL_CONTAINER_PREPARE_COMMAND=()
ANVIL_CONTAINER_RUN_ARGS=()
ANVIL_CONTAINER_CLEANUP=:
github_token_file=""
cleanup() {
    if [[ -n "$github_token_file" ]]; then rm -f -- "$github_token_file"; fi
    "$ANVIL_CONTAINER_CLEANUP"
}
trap cleanup EXIT

if [[ -f "$script_dir/auth.sh" ]]; then
    # shellcheck source=/dev/null
    source "$script_dir/auth.sh"
fi

if ! podman image exists "$image"; then
    if [[ "${ANVIL_CONTAINER_NO_REBUILD:-}" == "1" ]]; then
        echo "anvil-container: image $image is missing and ANVIL_CONTAINER_NO_REBUILD=1." >&2
        exit 1
    else
        podman build \
            --tag "$image" \
            --file "$script_dir/Containerfile" \
            --ignorefile "$script_dir/container.ignore" \
            --build-arg "ANVIL_IMAGE_ID=$image_id" \
            "${ANVIL_CONTAINER_BUILD_ARGS[@]}" \
            "$repo_root"
    fi
fi

run_args=(
    run --rm --pull=never
    --userns keep-id
    --env ANVIL_IN_CONTAINER=1
    --env HOME=/tmp/anvil-user
    --volume "$repo_root:/workspace:Z"
    --volume "anvil-cargo-registry:/usr/local/cargo/registry:U"
    --volume "anvil-cargo-git:/usr/local/cargo/git:U"
    --volume "$target_volume:/workspace/target:U"
    --workdir /workspace
)
prepare_run_args=("${run_args[@]}")
run_args+=("${ANVIL_CONTAINER_RUN_ARGS[@]}")
for name in PR_TITLE BASE_REF ANVIL_INCLUDE_MODIFIED ANVIL_INCLUDE_AFFECTED ANVIL_INCLUDE_REQUIRED GITHUB_BASE_REF SYSTEM_PULLREQUEST_TARGETBRANCH; do
    if [[ -v "$name" ]]; then run_args+=(--env "$name"); fi
done
if ((${#ANVIL_CONTAINER_PREPARE_COMMAND[@]} > 0)); then
    podman "${prepare_run_args[@]}" \
        "${ANVIL_CONTAINER_PREPARE_ARGS[@]}" \
        "$image" \
        "${ANVIL_CONTAINER_PREPARE_COMMAND[@]}"
fi

needs_github_token=false
for recipe in "$@"; do
    if anvil_recipe_needs_github_token "$recipe"; then
        needs_github_token=true
        break
    fi
done
if "$needs_github_token"; then
    github_token="${GITHUB_TOKEN:-}"
    if [[ -z "$github_token" ]]; then
        gh_command=gh
        if ! command -v "$gh_command" >/dev/null 2>&1; then gh_command=gh.exe; fi
        if command -v "$gh_command" >/dev/null 2>&1; then
            github_token="$("$gh_command" auth token --hostname github.com 2>/dev/null | tr -d '\r' || true)"
        fi
    fi
    if [[ -n "$github_token" ]]; then
        github_token_file="$(mktemp)"
        chmod 600 "$github_token_file"
        printf '%s' "$github_token" > "$github_token_file"
        unset github_token
        run_args+=(--volume "$github_token_file:/run/secrets/anvil-github-token:ro,Z")
    fi
fi

if (($# == 0)); then
    podman "${run_args[@]}" --interactive --tty "$image" bash
    exit $?
fi
podman "${run_args[@]}" "$image" just "$@"
