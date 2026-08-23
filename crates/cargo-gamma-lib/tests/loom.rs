// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(loom)]

#[test]
fn concurrency_models() {
    cargo_gamma_lib::run_loom_models();
}
