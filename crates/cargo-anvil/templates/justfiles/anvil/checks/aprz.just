# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-anvil; edit via `cargo anvil`.
# See ../../../docs/design/checks.md for the catalog and rationale.

# cargo-aprz queries the GitHub advisory API. Unauthenticated access is
# capped at 60 requests/hour and fails on a full run; an authenticated
# token raises the cap to 5000/hour. CI injects GITHUB_TOKEN
# (github.token). Container drivers mount an existing host GITHUB_TOKEN
# or the host gh CLI's stored token as a temporary read-only secret.
# Native runs borrow the gh CLI token directly. Native runs warn and
# proceed unauthenticated if neither is available; container runs fail
# before cargo-aprz can exhaust the unauthenticated rate limit.
#
# Unscoped (consults external risk DB).
[script("pwsh")]
anvil-aprz: anvil-aprz-validate-prereqs
    $ErrorActionPreference = 'Stop'
    if (-not $env:GITHUB_TOKEN) {
        $tok = $null
        $containerTokenFile = '/run/secrets/anvil-github-token'
        if ($env:ANVIL_IN_CONTAINER -and (Test-Path -LiteralPath $containerTokenFile -PathType Leaf)) {
            try { $tok = Get-Content -LiteralPath $containerTokenFile -Raw } catch { $tok = $null }
        } elseif (Get-Command gh -ErrorAction SilentlyContinue) {
            try { $tok = (gh auth token --hostname github.com 2>$null) } catch { $tok = $null }
        }
        if ($tok) {
            $env:GITHUB_TOKEN = $tok.Trim()
        } else {
            if ($env:ANVIL_IN_CONTAINER) {
                throw 'anvil-aprz: GitHub authentication is unavailable. Run `gh auth login` on the host or set host GITHUB_TOKEN, then re-run the container command.'
            }
            Write-Warning 'anvil-aprz: GITHUB_TOKEN is not set and no token could be obtained from the gh CLI.'
            Write-Warning 'cargo-aprz will use the unauthenticated GitHub API (60 requests/hour) and may fail on a full run.'
            Write-Warning 'To fix: run `gh auth login` (recommended), or set $env:GITHUB_TOKEN to a GitHub token, then re-run.'
        }
    }
    cargo aprz deps --error-if-high-risk --console appraisal
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

[group("anvil-setup")]
anvil-aprz-setup installer="install": (anvil-tool-cargo-aprz-install installer)

[group("anvil-setup")]
anvil-aprz-validate-prereqs: anvil-tool-cargo-aprz-validate-prereqs
