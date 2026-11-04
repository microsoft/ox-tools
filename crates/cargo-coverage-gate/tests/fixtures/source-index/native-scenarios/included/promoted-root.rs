// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
include!("included-test.rs");
#[path = "included-test.rs"]
mod released;
