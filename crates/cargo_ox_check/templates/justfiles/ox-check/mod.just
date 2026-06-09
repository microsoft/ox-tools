# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-ox-check; edit via `cargo ox-check update`.
# Entry point for the ox-check just recipe tree. The user's root Justfile
# imports this single file; everything else lives in sibling .just files
# pulled in here.

import 'checks.just'
import 'groups.just'
import 'tiers.just'
import 'tools.just'

# Friendly default: `just ox-check` runs the PR tier.
alias ox-check := ox-check-pr
