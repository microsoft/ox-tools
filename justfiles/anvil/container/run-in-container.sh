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

version_at_least() {
    local found="${1%%[-+]*}"
    local required="${2%%[-+]*}"
    local found_major found_minor found_patch found_extra
    local required_major required_minor required_patch required_extra
    IFS=. read -r found_major found_minor found_patch found_extra <<<"$found"
    IFS=. read -r required_major required_minor required_patch required_extra <<<"$required"
    found_patch="${found_patch:-0}"
    required_patch="${required_patch:-0}"
    for component in \
        "$found_major" "$found_minor" "$found_patch" \
        "$required_major" "$required_minor" "$required_patch"
    do
        case "$component" in
            '' | *[!0-9]*) return 2 ;;
        esac
    done
    if ((found_major != required_major)); then ((found_major > required_major)); return; fi
    if ((found_minor != required_minor)); then ((found_minor > required_minor)); return; fi
    ((found_patch >= required_patch))
}

command -v podman >/dev/null 2>&1 || {
    echo "anvil-container: Podman is required. See justfiles/anvil/container/README.md." >&2
    exit 1
}

version="$(podman version --format '{{.Client.Version}}' 2>/dev/null || podman --version | awk '{print $3}')"
minimum="4.3.0"
if ! version_at_least "$version" "$minimum"; then
    echo "anvil-container: Podman $minimum or newer is required (found $version)." >&2
    exit 1
fi

if ! repo_root="$(git rev-parse --show-toplevel 2>/dev/null)"; then
    echo 'anvil-container must run from a Git repository.' >&2
    exit 1
fi
script_dir="$repo_root/justfiles/anvil/container"
image_id="$(bash "$script_dir/image-id.sh")"
image_base="${ANVIL_CONTAINER_IMAGE:-anvil-dev}"
image="${image_base}:${image_id}"
if command -v sha256sum >/dev/null 2>&1; then
    repo_id="$(printf '%s' "$repo_root" | sha256sum | cut -c1-12)"
elif command -v shasum >/dev/null 2>&1; then
    repo_id="$(printf '%s' "$repo_root" | shasum -a 256 | cut -c1-12)"
else
    echo 'anvil-container: sha256sum or shasum is required.' >&2
    exit 1
fi
target_volume="anvil-target-${repo_id}-${image_id:0:12}"

needs_github_token=false
for recipe in "$@"; do
    if anvil_recipe_needs_github_token "$recipe"; then
        needs_github_token=true
        break
    fi
done
runs_only_github_check=false
if (($# == 1)) && [[ "$1" == "anvil-aprz" ]]; then
    runs_only_github_check=true
fi
github_token=""
if "$needs_github_token"; then
    gh_command=""
    if command -v gh >/dev/null 2>&1; then
        gh_command=gh
    elif command -v gh.exe >/dev/null 2>&1; then
        gh_command=gh.exe
    fi
    github_token="${GITHUB_TOKEN:-}"
    if [[ -z "$github_token" && -n "$gh_command" ]]; then
        github_token="$("$gh_command" auth token --hostname github.com 2>/dev/null | tr -d '\r' || true)"
    fi
    if [[ -z "$github_token" ]]; then
        if [[ -z "$gh_command" ]]; then
            echo 'anvil-container: GitHub authentication is required for anvil-aprz. Install the GitHub CLI and run `gh auth login --hostname github.com`, or set GITHUB_TOKEN before rerunning.' >&2
            exit 1
        fi
        if [[ ! -t 0 ]]; then
            echo 'anvil-container: GitHub authentication is required for anvil-aprz. Run `gh auth login --hostname github.com` or set GITHUB_TOKEN before rerunning.' >&2
            exit 1
        fi
        echo 'anvil-container: anvil-aprz requires GitHub authentication to avoid the 60 requests/hour unauthenticated API limit.' >&2
        read -r -p 'Run `gh auth login --hostname github.com` in another terminal, then press Enter to continue (Ctrl+C to cancel) '
        github_token="$("$gh_command" auth token --hostname github.com 2>/dev/null | tr -d '\r' || true)"
        if [[ -z "$github_token" ]]; then
            echo 'anvil-container: GitHub authentication is still unavailable. Complete `gh auth login --hostname github.com`, then rerun.' >&2
            exit 1
        fi
    fi
fi

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
            --platform linux/amd64 \
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
    --platform linux/amd64
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
    if declare -p "$name" >/dev/null 2>&1; then run_args+=(--env "$name"); fi
done
if ((${#ANVIL_CONTAINER_PREPARE_COMMAND[@]} > 0)); then
    podman "${prepare_run_args[@]}" \
        "${ANVIL_CONTAINER_PREPARE_ARGS[@]}" \
        "$image" \
        "${ANVIL_CONTAINER_PREPARE_COMMAND[@]}"
fi

if [[ -n "$github_token" ]]; then
    github_token_file="$(mktemp)"
    chmod 600 "$github_token_file"
    printf '%s' "$github_token" > "$github_token_file"
    unset github_token
    github_run_args=("${run_args[@]}" --volume "$github_token_file:/run/secrets/anvil-github-token:ro,Z")
    if "$runs_only_github_check"; then
        run_args=("${github_run_args[@]}")
    else
        podman "${github_run_args[@]}" "$image" just anvil-aprz
        run_args+=(--env ANVIL_APRZ_ALREADY_RAN=1)
    fi
fi

if (($# == 0)); then
    podman "${run_args[@]}" --interactive --tty "$image" bash
    exit $?
fi
podman "${run_args[@]}" "$image" just "$@"
