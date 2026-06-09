# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-ox-check; edit via `cargo ox-check update`.
# See ../../docs/design/local.md for the policy.

# Public: assert that every catalog tool is present at or above its
# minimum version. Called by `ox-check-tools-check` and as a dependency
# of every check group via individual `_ox-check-require` invocations.
[group("ox-check")]
[script("bash")]
ox-check-tools-check:
    set -euo pipefail
    catalog="justfiles/ox-check/tool-minimums.txt"
    fail=0
    while IFS='=' read -r tool min; do
        case "$tool" in '' | '#'*) continue ;; esac
        if ! just _ox-check-require "$tool" ; then
            fail=1
        fi
    done < "$catalog"
    just _ox-check-require rustc || fail=1
    exit $fail

# Public: install (or upgrade) every catalog tool to its minimum
# version. Idempotent — `cargo install --locked` is a no-op when the
# installed version already satisfies the requirement.
[group("ox-check")]
[script("bash")]
ox-check-tools-install:
    set -euo pipefail
    catalog="justfiles/ox-check/tool-minimums.txt"
    while IFS='=' read -r tool min; do
        case "$tool" in '' | '#'*) continue ;; esac
        cargo install --locked "$tool" --version ">=$min"
    done < "$catalog"

# Internal: assert that `name` meets its catalog minimum (or, for tools
# not in the catalog, that it's at least present). Three lookup paths:
#
#   - "rustc": presence-only; the workspace's rust-toolchain.toml drives
#     the actual version requirement (rustup auto-installs).
#   - "pwsh": presence-only with a per-OS install hint.
#   - Anything else: looked up in tool-minimums.txt; if present, the
#     installed version is parsed from `cargo install --list` and
#     compared via `sort -V`. If not in the catalog, presence-only
#     fallback (covers system binaries like `just` itself, `git`).
[script("bash")]
_ox-check-require name:
    set -euo pipefail
    catalog="justfiles/ox-check/tool-minimums.txt"
    case "{{name}}" in
        rustc)
            if ! command -v rustc >/dev/null 2>&1 ; then
                echo "ox-check: rustc not found. Install via rustup: https://rustup.rs" >&2
                exit 1
            fi
            exit 0
            ;;
        pwsh)
            if ! command -v pwsh >/dev/null 2>&1 ; then
                echo "ox-check: pwsh (PowerShell Core) not found." >&2
                case "$(uname -s 2>/dev/null)" in
                    Darwin*) echo "Install: brew install --cask powershell" >&2 ;;
                    *)       echo "Install: see https://github.com/PowerShell/PowerShell" >&2 ;;
                esac
                exit 1
            fi
            exit 0
            ;;
    esac

    # Catalog lookup: trim CRLF on Windows checkouts and skip
    # comments / blank lines while we're at it.
    spec=$(grep -E "^{{name}}=" "$catalog" 2>/dev/null | tr -d '\r' || true)
    if [ -z "$spec" ] ; then
        # Not in catalog: presence-only fallback for tools we don't
        # version-gate (`just` itself, `git`, system binaries).
        if ! command -v "{{name}}" >/dev/null 2>&1 ; then
            echo "ox-check: required tool '{{name}}' not found." >&2
            echo "Install hint: cargo install --locked {{name}}" >&2
            exit 1
        fi
        exit 0
    fi

    min="${spec#*=}"
    installed=$(cargo install --list 2>/dev/null \
        | grep -E "^{{name}} v" \
        | head -n1 \
        | sed -E 's/^[^ ]+ v([0-9][^ :]*).*/\1/' || true)
    if [ -z "$installed" ] ; then
        echo "ox-check: '{{name}}' not installed (need >=v$min)." >&2
        echo "Install: cargo install --locked {{name}} --version '>=$min'" >&2
        exit 1
    fi
    # `sort -V` orders by SemVer; if the minimum is the lowest of the
    # two, the installed version is >= the minimum.
    lowest=$(printf '%s\n%s\n' "$installed" "$min" | sort -V | head -n1)
    if [ "$lowest" != "$min" ] && [ "$installed" != "$min" ] ; then
        echo "ox-check: '{{name}}' is at v$installed; need >=v$min." >&2
        echo "Upgrade: cargo install --locked {{name}} --version '>=$min'" >&2
        exit 1
    fi
