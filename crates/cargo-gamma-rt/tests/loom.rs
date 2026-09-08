// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(loom)]

#[test]
fn concurrency_models() {
    gamma_rt::run_loom_models();
}
