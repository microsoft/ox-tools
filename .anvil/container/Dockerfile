# syntax=docker/dockerfile:1

# >>> anvil-managed: anvil-container-header
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Anvil execution image.
#
# The `anvil-managed:` regions are cargo-anvil's and are kept current. The gaps
# between them are yours and are preserved. Add to a gap, not inside a region:
# anvil will not overwrite an edit inside one, so editing there silently freezes
# the base digest and the tool pins at that moment.
#
# Which gap, since position decides whether a line works at all:
#
#   after base-image   re-declare ARG BASE_IMAGE to build on your own base
#   after base         root CA, proxy, apt mirror -- anything the first download needs
#   after tools        libraries a catalog tool needs to compile from source
#   after setup        what your own checks need at run time
#
# `# syntax=` stays on line 1: BuildKit honors it only when nothing precedes it,
# and a region sentinel is a comment.
# <<< anvil-managed: anvil-container-header

# >>> anvil-managed: anvil-container-base-image
# Tracks the Linux runner the generated workflows use (`ubuntu-latest`).
# `anvil-setup binstall` installs prebuilt binaries that need that runner's
# glibc, so this moves when the runner does. Digest-pinned because a floating
# tag can change under an image reference that claims to name fixed content.
#
# To build on another base, re-declare this in the gap below rather than editing
# here: a later ARG wins, and anvil keeps updating the pins you did not touch.
ARG BASE_IMAGE=docker.io/library/ubuntu:24.04@sha256:561618e2c15bf2397621dd04f96926663a3b5616c189cf7e38db7e82f5c538ea
# <<< anvil-managed: anvil-container-base-image

# >>> anvil-managed: anvil-container-base
FROM ${BASE_IMAGE}

ARG JUST_VERSION=1.56.0
ARG JUST_SHA256=fa2a8ec1015d9df5330941ade12437488fc40d33f9c9f8cd4eb70a26de11b639
ARG POWERSHELL_VERSION=7.6.3
ARG POWERSHELL_SHA256=856d0765d2332377f9d7a4aea76efdfde4de51446e7738dde2dfda41dba9e2a7
ARG RUSTUP_VERSION=1.29.0
ARG RUSTUP_SHA256=4acc9acc76d5079515b46346a485974457b5a79893cfb01112423c89aeb5aa10
ARG CARGO_BINSTALL_VERSION=1.21.1
ARG CARGO_BINSTALL_SHA256=630c8f8803a686aa6779497f0f0fb51d49822fb5fc3c514d8ced33b34e338e6e

ENV DEBIAN_FRONTEND=noninteractive \
    CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    RUSTUP_NO_UPDATE_CHECK=1 \
    PATH=/usr/local/cargo/bin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
# <<< anvil-managed: anvil-container-base

# >>> anvil-managed: anvil-container-tools
# clang/libclang are required by cargo-spellcheck; the rest is the usual Rust
# link-time set. A bare base has no C runtime development files, so every link
# step fails without build-essential.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential ca-certificates clang libclang-dev curl git libicu-dev \
        libssl-dev pkg-config tar \
    && rm -rf /var/lib/apt/lists/*

# pwsh is not optional: every generated anvil recipe is a `script("pwsh",
# "-NoProfile")` recipe.
RUN curl -fsSLo /tmp/powershell.tar.gz \
        "https://github.com/PowerShell/PowerShell/releases/download/v${POWERSHELL_VERSION}/powershell-${POWERSHELL_VERSION}-linux-x64.tar.gz" \
    && echo "${POWERSHELL_SHA256}  /tmp/powershell.tar.gz" | sha256sum -c - \
    && mkdir -p /opt/microsoft/powershell/7 \
    && tar -xzf /tmp/powershell.tar.gz -C /opt/microsoft/powershell/7 \
    && chmod 755 /opt/microsoft/powershell/7/pwsh \
    && ln -s /opt/microsoft/powershell/7/pwsh /usr/local/bin/pwsh \
    && rm /tmp/powershell.tar.gz

RUN curl -fsSLo /tmp/just.tar.gz \
        "https://github.com/casey/just/releases/download/${JUST_VERSION}/just-${JUST_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
    && echo "${JUST_SHA256}  /tmp/just.tar.gz" | sha256sum -c - \
    && tar -xzf /tmp/just.tar.gz -C /usr/local/bin just \
    && chmod 755 /usr/local/bin/just \
    && rm /tmp/just.tar.gz

RUN curl --proto '=https' --tlsv1.2 -fsSLo /tmp/rustup-init \
        "https://static.rust-lang.org/rustup/archive/${RUSTUP_VERSION}/x86_64-unknown-linux-gnu/rustup-init" \
    && echo "${RUSTUP_SHA256}  /tmp/rustup-init" | sha256sum -c - \
    && chmod 755 /tmp/rustup-init \
    && /tmp/rustup-init -y --profile minimal --default-toolchain none --no-modify-path \
    && rm /tmp/rustup-init

# Without this, the first `_install-tool` that needs binstall bootstraps it by
# compiling it from source, which costs minutes on every image build and is one
# more source build a toolchain bump can break. CI installs the prebuilt binary
# for the same reason.
RUN curl -fsSLo /tmp/cargo-binstall.tgz \
        "https://github.com/cargo-bins/cargo-binstall/releases/download/v${CARGO_BINSTALL_VERSION}/cargo-binstall-x86_64-unknown-linux-musl.tgz" \
    && echo "${CARGO_BINSTALL_SHA256}  /tmp/cargo-binstall.tgz" | sha256sum -c - \
    && mkdir -p "${CARGO_HOME}/bin" \
    && tar -xzf /tmp/cargo-binstall.tgz -C "${CARGO_HOME}/bin" cargo-binstall \
    && chmod 755 "${CARGO_HOME}/bin/cargo-binstall" \
    && rm /tmp/cargo-binstall.tgz

# <<< anvil-managed: anvil-container-tools

# >>> anvil-managed: anvil-container-setup
# Install the pinned toolchain and cargo subcommands from the generated catalog.
# The whole recipe tree is copied because `just` has to parse it, and the whole
# tree is hashed into the image tag: `anvil-setup` reaches the install recipes
# through the tier, group and check recipes, so any of them can change what this
# layer installs.
#
# `binstall` matches what CI passes. Not only a speed-up: some catalog tools do
# not compile from source on every pinned toolchain.
#
# The credential files are removed in the same layer as the install. A build
# secret never lands in a layer, but anything the install *writes* with it is
# ordinary content, and the `chmod` below would publish it world-readable.
# Deleting them in a later `RUN` would not help -- the earlier layer keeps them.
#
# `registry` and `git` are created before the `chmod` because the run mounts a
# named volume over each, and an engine seeds a new volume from the image path
# it covers. A path that does not exist seeds as root-owned 0755, which the
# `--user` mapping cannot write, so the first cargo fetch fails with EACCES.
WORKDIR /opt/anvil
COPY justfiles ./justfiles
COPY rust-toolchain.toml ./
RUN printf "import 'justfiles/anvil/mod.just'\n" > Justfile \
    && just anvil-setup binstall \
    && rm -rf "${CARGO_HOME}/registry/cache" "${CARGO_HOME}/registry/src" \
    && rm -f "${CARGO_HOME}/credentials" "${CARGO_HOME}/credentials.toml" "${HOME}/.netrc" \
    && mkdir -p "${CARGO_HOME}/registry" "${CARGO_HOME}/git" \
    && chmod -R a+rwX "${CARGO_HOME}" "${RUSTUP_HOME}"
# <<< anvil-managed: anvil-container-setup

# >>> anvil-managed: anvil-container-entry
# Consumed by `anvil-container` itself: a nested invocation from inside the
# image runs the recipe natively instead of launching another container.
ENV ANVIL_IN_CONTAINER=1

WORKDIR /workspace
CMD ["bash"]
# <<< anvil-managed: anvil-container-entry
