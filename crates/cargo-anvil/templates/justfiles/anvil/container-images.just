# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Managed by cargo-anvil. Update the corresponding template in the cargo-anvil
# crate unless the change is repository-specific.
# Update behaviour: https://github.com/microsoft/ox-tools/blob/main/crates/cargo-anvil/docs/design/updates.md
#
# Generic OCI image build recipes. Emitted only when `[container] enabled =
# true` and at least one `[[image]]` is configured in anvil.toml.
#
# `anvil-image <name>` builds one image; `anvil-images` builds them all in
# dependency order. Every image is built by the same logic, so there is one
# recipe body and a generated spec table -- the per-image data (dockerfile,
# context, target, stage-artifacts, build-args, repository) lives in `$specs`.
# Run `just anvil-image` with no name to list the configured images.
#
# Every image is built from a STAGED context (never the repo root): its
# prebuilt binaries are copied into the context by `stage-artifacts` before the
# build — nothing is compiled in-image. The recipe refuses a context outside
# the configured image output dir so the whole repo can never be sent to the
# engine as build context.
#
# There is deliberately no registry push, no ACR, no auth, and no promotion:
# images are built locally.
#
# Recipe parameters shared by both recipes:
#   profile   selects the staged binaries ({profile} token); default "debug".
#   tag       image tag and {tag} token; default "dev".
#   registry  image registry prefix; default "local" (podman normalizes an
#             unqualified "local/..." ref to "localhost/local/...").
#
# The `_anvil-container-engine` helper (docker/podman resolution) is provided
# by container.just, imported alongside this file.

__IMAGE_RECIPES__
