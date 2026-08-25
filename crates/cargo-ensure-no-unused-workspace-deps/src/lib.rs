// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A cargo sub-command that ensures every `[workspace.dependencies]` entry is
//! inherited by at least one workspace member.
//!
//! A workspace root declares a dependency catalog that members draw from with
//! `dep = { workspace = true }`. Nothing requires an entry to be drawn from, so
//! an entry nobody inherits stays in the manifest forever: it never enters the
//! dependency graph, and no build fails because of it. It still carries a
//! version requirement, so it keeps attracting dependency-bump traffic and
//! keeps misleading readers about what the workspace depends on.
//!
//! Unused-dependency tools resolve the crate graph and ask which *declared*
//! dependencies go unused, so an entry that no member declares is invisible to
//! them. This crate answers the prior question -- is the entry inherited at
//! all? -- from the manifests alone.
//!
//! # Status
//!
//! Skeleton. The design is under review and the implementation has not landed,
//! so this crate exposes no API and installs no binary yet.
