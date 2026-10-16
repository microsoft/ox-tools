// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
include!("included-test.rs");

#[cfg(test)]
const INCLUDED_VALUE: u32 = include!("included-expression.rs");
