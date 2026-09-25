# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

# Required by [script]
set unstable

set windows-shell := ["pwsh.exe", "-NoLogo", "-NoProfile", "-NonInteractive", "-Command"]

_default:
    @just --list

# >>> anvil-managed: anvil-imports
import '.anvil/anvil.just'
# <<< anvil-managed: anvil-imports
