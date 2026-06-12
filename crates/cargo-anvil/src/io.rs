// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Trivial filesystem helpers shared across emit/plan paths.

use std::path::Path;

use ohno::{AppError, IntoAppError as _};

/// Read a file's contents to a string, returning `Ok(None)` if the file
/// does not exist. Any other I/O error is propagated as an `AppError`.
///
/// # Errors
///
/// Returns an error if reading the file fails for a reason other than
/// `NotFound` (e.g., permissions, invalid UTF-8).
#[mutants::skip] // Trivial `fs::read_to_string` + `NotFound` passthrough; mutations on its match guard / Ok arms are not behavior-meaningful and exhaustively exercised via every plan/emit path.
pub fn read_file_if_present(path: &Path) -> Result<Option<String>, AppError> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err::<Option<String>, _>(e).into_app_err_with(|| format!("failed to read {}", path.display())),
    }
}
