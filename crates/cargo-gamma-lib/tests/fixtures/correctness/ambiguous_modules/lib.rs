// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[path = "shared.rs"]
pub mod one;
#[path = "shared.rs"]
pub mod two;

pub fn values() -> Vec<one::Artifact> {
    vec![one::Artifact]
}
