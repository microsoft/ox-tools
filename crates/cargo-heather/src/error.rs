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

    /// Failed to write a file to disk.
    FileWrite {
        /// Path to the file that could not be written.
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
            Self::FileWrite { path, source } => {
                write!(f, "failed to write file '{}': {source}", path.display())
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
            Self::FileWrite { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::error::Error;

    use super::*;

    #[test]
    fn file_read_error_exposes_source() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let err = HeatherError::FileRead {
            path: PathBuf::from("/tmp/missing"),
            source: io_err,
        };
        let source = err.source().expect("FileRead should have a source");
        assert!(source.to_string().contains("gone"));

        let write_err = HeatherError::FileWrite {
            path: PathBuf::from("/tmp/denied"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        };
        let write_source = write_err.source().expect("FileWrite should have a source");
        assert!(write_source.to_string().contains("denied"));
    }

    #[test]
    fn non_io_variants_have_no_source() {
        let err = HeatherError::ConfigInvalid("bad".into());
        assert!(err.source().is_none());
    }

    #[test]
    fn display_renders_every_variant() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        assert!(
            HeatherError::FileRead {
                path: PathBuf::from("/p"),
                source: io_err,
            }
            .to_string()
            .contains("failed to read file")
        );
        let write_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        assert!(
            HeatherError::FileWrite {
                path: PathBuf::from("/w"),
                source: write_err,
            }
            .to_string()
            .contains("failed to write file")
        );
        assert!(
            HeatherError::ConfigParse {
                path: PathBuf::from("/c"),
                message: "boom".into(),
            }
            .to_string()
            .contains("failed to parse config")
        );
        assert!(
            HeatherError::ConfigNotFound(PathBuf::from("/n"))
                .to_string()
                .contains("config file not found")
        );
        assert!(HeatherError::ConfigInvalid("x".into()).to_string().contains("invalid config"));
        assert!(
            HeatherError::UnknownLicense("ZZZ".into())
                .to_string()
                .contains("unknown SPDX license identifier")
        );
        assert!(
            HeatherError::UnsupportedFileType { path: PathBuf::from("/u") }
                .to_string()
                .contains("unsupported file type")
        );
        assert!(HeatherError::ValidationFailed(3).to_string().contains("3 file(s)"));
    }
}
