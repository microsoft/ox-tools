// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub const RELEASED: () = ();

#[cfg(any(test, not(feature = "x")))]
pub fn only_test_when_x_is_required() {}
