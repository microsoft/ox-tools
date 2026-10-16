// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(feature = "x")]
pub fn released_in_one_visit() {}

#[cfg(test)]
fn test_only_in_every_visit() {}
