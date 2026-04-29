// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(docsrs, feature(doc_cfg))]

//! # cargo-heather
//!
//! Library for validating and rewriting license headers in Rust (`.rs`) and
//! TOML (`.toml`) source files. The accompanying `cargo-heather` binary uses
//! this library to discover files on disk and apply the rewrites.
//!
//! ## Public API
//!
//! The library is intentionally minimal: a pair of stream-based functions
//! that operate on any [`std::io::Read`] / [`std::io::Write`].
//!
//! - [`check`] reads content and reports whether the expected header is
//!   present, missing, or mismatched.
//! - [`fix`] reads content and writes the fixed-up content.
//!
//! Callers are responsible for opening files, deciding which paths to
//! process, and writing results back to disk.
//!
//! ```no_run
//! use cargo_heather::{check, fix, CheckResult, FileKind};
//!
//! let input = b"fn main() {}\n";
//! let header = "Licensed under the MIT License.";
//!
//! // Check whether the header is present.
//! let result = check(&input[..], header, FileKind::Rust).unwrap();
//! assert_eq!(result, CheckResult::Missing);
//!
//! // Produce a fixed copy.
//! let mut output: Vec<u8> = Vec::new();
//! fix(&input[..], &mut output, header, FileKind::Rust).unwrap();
//! assert!(output.starts_with(b"// Licensed under the MIT License.\n"));
//! ```
//!
//! ## Supported file kinds
//!
//! - [`FileKind::Rust`] — regular Rust source (`//` comments).
//! - [`FileKind::Toml`] — TOML files (`#` comments).
//! - [`FileKind::CargoScript`] — Rust script with shebang + `---`
//!   frontmatter; the header lives inside the frontmatter using `#`.
//!
//! Use [`FileKind::detect`] (or [`is_cargo_script`]) to classify a file
//! from its path and content before calling [`check`] / [`fix`].
//!
//! ## License header lookup
//!
//! The [`license`] module maps SPDX identifiers to canonical short header
//! strings; this is what the binary uses when no custom header is supplied.

#![doc(html_logo_url = "https://media.githubusercontent.com/media/microsoft/ox-tools/refs/heads/main/crates/cargo_heather/logo.png")]
#![doc(html_favicon_url = "https://media.githubusercontent.com/media/microsoft/ox-tools/refs/heads/main/crates/cargo_heather/favicon.ico")]
#![deny(unsafe_code)]

mod checker;
mod comment;
mod error;
mod process;

pub mod license;

pub use checker::CheckResult;
pub use comment::{CommentStyle, FileKind, is_cargo_script};
pub use error::HeatherError;
pub use process::{check, fix};
