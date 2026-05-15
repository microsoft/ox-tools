// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Error types for `cargo-heather`.

use std::fmt;
use std::path::PathBuf;

/// Errors that can occur during `cargo-heather` operation.
#[derive(Debug)]
pub enum HeatherError {
    /// Failed to read a file from disk.
    FileRead {
        /// Path to the file that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// Failed to parse the configuration file.
    ConfigParse {
        /// Path to the configuration file.
        path: PathBuf,
        /// Description of the parse error.
        message: String,
    },

    /// Configuration file not found.
    ConfigNotFound(PathBuf),

    /// Configuration is invalid (e.g., both `license` and `header` specified).
    ConfigInvalid(String),

    /// Unknown SPDX license identifier.
    UnknownLicense(String),

    /// File type not supported for header checking.
    UnsupportedFileType {
        /// Path to the unsupported file.
        path: PathBuf,
    },

    /// Header validation failed for one or more files.
    ValidationFailed(usize),
}

impl fmt::Display for HeatherError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileRead { path, source } => {
                write!(f, "failed to read file '{}': {source}", path.display())
            }
            Self::ConfigParse { path, message } => {
                write!(f, "failed to parse config '{}': {message}", path.display())
            }
            Self::ConfigNotFound(path) => write!(f, "config file not found: {}", path.display()),
            Self::ConfigInvalid(msg) => write!(f, "invalid config: {msg}"),
            Self::UnknownLicense(id) => write!(f, "unknown SPDX license identifier: '{id}'"),
            Self::UnsupportedFileType { path } => {
                write!(f, "unsupported file type: '{}'", path.display())
            }
            Self::ValidationFailed(count) => {
                write!(f, "{count} file(s) have missing or incorrect license headers")
            }
        }
    }
}

impl std::error::Error for HeatherError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::FileRead { source, .. } => Some(source),
            _ => None,
        }
    }
}
