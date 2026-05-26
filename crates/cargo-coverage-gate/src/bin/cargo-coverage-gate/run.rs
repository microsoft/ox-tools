// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the `cargo coverage-gate` command.

use cargo_coverage_gate::CoverageGateError;
use ohno::AppError;

use crate::cli::CoverageGateArgs;

pub(crate) fn run(_args: &CoverageGateArgs) -> Result<(), AppError> {
    ohno::bail!(CoverageGateError::NotImplemented);
}
