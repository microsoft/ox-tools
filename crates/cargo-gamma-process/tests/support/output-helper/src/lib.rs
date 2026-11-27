// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The native process fixture, compiled before its consumers' tests execute.

/// Exact path of the host executable prepared by Cargo for this test build.
pub const PATH: &str = env!("CARGO_GAMMA_PROCESS_HELPER");
