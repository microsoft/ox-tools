// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Error types for `cargo-heather`.

use std::path::PathBuf;

use thiserror::Error;

/// Errors that can occur during `cargo-heather` operation.
#[derive(Error, Debug)]
pub enum HeatherError {
    /// Failed to read a file from disk.
    #[error("failed to read file '{path}': {source}")]
    FileRead { path: PathBuf, source: std::io::Error },

    /// Failed to parse the configuration file.
    #[error("failed to parse config '{path}': {message}")]
    ConfigParse { path: PathBuf, message: String },

    /// Configuration file not found.
    #[error("config file not found: {0}")]
    ConfigNotFound(PathBuf),

    /// Configuration is invalid (e.g., both `license` and `header` specified).
    #[error("invalid config: {0}")]
    ConfigInvalid(String),

    /// Unknown SPDX license identifier.
    #[error("unknown SPDX license identifier: '{0}'")]
    UnknownLicense(String),

    /// File type not supported for header checking.
    #[error("unsupported file type: '{path}'")]
    UnsupportedFileType { path: PathBuf },

    /// Header validation failed for one or more files.
    #[error("{0} file(s) have missing or incorrect license headers")]
    ValidationFailed(usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_read_error_display() {
        let err = HeatherError::FileRead {
            path: PathBuf::from("src/main.rs"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
        };
        let msg = err.to_string();
        assert!(msg.contains("src/main.rs"));
        assert!(msg.contains("not found"));
    }

    #[test]
    fn config_parse_error_display() {
        let err = HeatherError::ConfigParse {
            path: PathBuf::from(".cargo-heather.toml"),
            message: "bad syntax".into(),
        };
        assert!(err.to_string().contains("bad syntax"));
    }

    #[test]
    fn config_not_found_error_display() {
        let err = HeatherError::ConfigNotFound(PathBuf::from(".cargo-heather.toml"));
        assert!(err.to_string().contains(".cargo-heather.toml"));
    }

    #[test]
    fn config_invalid_error_display() {
        let err = HeatherError::ConfigInvalid("both license and header specified".into());
        assert!(err.to_string().contains("both license and header"));
    }

    #[test]
    fn unknown_license_error_display() {
        let err = HeatherError::UnknownLicense("FAKE-1.0".into());
        assert!(err.to_string().contains("FAKE-1.0"));
    }

    #[test]
    fn validation_failed_error_display() {
        let err = HeatherError::ValidationFailed(3);
        assert!(err.to_string().contains("3 file(s)"));
    }
}
