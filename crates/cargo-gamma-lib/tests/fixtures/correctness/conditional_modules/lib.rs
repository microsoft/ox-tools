// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg_attr(all(), path = "selected.rs")]
pub mod model;

pub fn values() -> Vec<model::Artifact> {
    vec![model::Artifact]
}
