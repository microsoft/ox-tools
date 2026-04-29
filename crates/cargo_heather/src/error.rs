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

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
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

    #[test]
    fn unsupported_file_type_error_display() {
        let err = HeatherError::UnsupportedFileType {
            path: PathBuf::from("data.csv"),
        };
        assert!(err.to_string().contains("unsupported file type"));
        assert!(err.to_string().contains("data.csv"));
    }

    #[test]
    fn error_source_file_read() {
        use std::error::Error;
        let err = HeatherError::FileRead {
            path: PathBuf::from("missing.rs"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "gone"),
        };
        assert!(err.source().is_some());
    }

    #[test]
    fn error_source_none_for_other_variants() {
        use std::error::Error;
        let variants: Vec<HeatherError> = vec![
            HeatherError::ConfigNotFound(PathBuf::from("x")),
            HeatherError::ConfigInvalid("bad".into()),
            HeatherError::UnknownLicense("X".into()),
            HeatherError::UnsupportedFileType { path: PathBuf::from("x") },
            HeatherError::ValidationFailed(1),
            HeatherError::ConfigParse {
                path: PathBuf::from("x"),
                message: "y".into(),
            },
        ];
        for v in &variants {
            assert!(v.source().is_none());
        }
    }
}
