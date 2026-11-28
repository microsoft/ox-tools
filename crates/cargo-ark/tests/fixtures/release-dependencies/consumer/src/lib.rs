// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub fn value() -> u32 {
    ark_fixture_peer::value() + ark_fixture_stable::value()
}
