// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[path = "../src/shared.rs"]
mod shared;

#[path = "../src/helper.rs"]
mod helper;

#[cfg(feature = "x")]
include!("../src/included-test.rs");

#[cfg(not(feature = "x"))]
include!("../src/included-test.rs");
