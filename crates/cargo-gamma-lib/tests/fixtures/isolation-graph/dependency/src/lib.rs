// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub fn stable() -> u32 {
    1
}

#[cfg(feature = "dev-api")]
pub fn dev_only() -> u32 {
    2
}
