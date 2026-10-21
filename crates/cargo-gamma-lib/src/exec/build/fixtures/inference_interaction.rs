// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub fn inferred() {
    let value = Some(11);
    drop(value.or(Some(22)));
    let _neighbor = 33;
}
