#!/usr/bin/env bash
set -euo pipefail

if ! repo_root="$(git rev-parse --show-toplevel 2>/dev/null)"; then
    echo 'anvil-container must run from a Git repository.' >&2
    exit 1
fi

toolchain_path="$repo_root/rust-toolchain.toml"
if [[ ! -f "$toolchain_path" ]]; then
    echo 'anvil-container requires a repository-owned rust-toolchain.toml.' >&2
    exit 1
fi

container_dir="$repo_root/justfiles/anvil/container"
inputs=(rust-toolchain.toml)
while IFS= read -r path; do
    inputs+=("${path#"$repo_root"/}")
done < <(find "$repo_root/justfiles/anvil" -type f -name '*.just' ! -path "$container_dir/*" -print)

for path in "$container_dir"/*; do
    [[ -f "$path" ]] || continue
    case "${path##*/}" in
        container.just | image-id.ps1 | image-id.sh | README.md \
            | run-in-container.ps1 | run-in-container.sh) continue ;;
    esac
    inputs+=("${path#"$repo_root"/}")
done

if command -v sha256sum >/dev/null 2>&1; then
    hash_command=(sha256sum)
elif command -v shasum >/dev/null 2>&1; then
    hash_command=(shasum -a 256)
else
    echo 'anvil-container: sha256sum or shasum is required.' >&2
    exit 1
fi

write_normalized_file() {
    local path="$1"
    local line status
    while true; do
        line=""
        if IFS= read -r line <&3; then
            status=0
        else
            status=$?
        fi
        if ((status != 0)) && [[ -z "$line" ]]; then
            break
        fi
        printf '%s' "${line%$'\r'}"
        if ((status == 0)); then
            printf '\n'
        else
            break
        fi
    done 3<"$path"
}

while IFS= read -r relative; do
    path="$repo_root/$relative"
    if [[ ! -f "$path" ]]; then
        echo "Container image input is missing: $relative" >&2
        exit 1
    fi
    printf '%s\n' "$relative"
    write_normalized_file "$path"
    printf '\n'
done < <(printf '%s\n' "${inputs[@]}" | LC_ALL=C sort -u) \
    | "${hash_command[@]}" \
    | awk '{print $1}'
