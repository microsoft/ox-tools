// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Public stream-based API for `cargo-heather`.
//!
//! These functions are the only "business logic" entry points exposed by
//! the library. They read content from any [`Read`], optionally rewrite
//! it to any [`Write`], and report the [`CheckResult`].

use std::io::{self, Read, Write};

use crate::checker::{self, CheckResult};
use crate::comment::FileKind;

/// Read content from `reader` and check whether it begins with the
/// expected license header for the given [`FileKind`].
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if the reader fails or the
/// content is not valid UTF-8.
pub fn check<R: Read>(mut reader: R, expected_header: &str, kind: FileKind) -> io::Result<CheckResult> {
    let mut content = String::new();
    reader.read_to_string(&mut content)?;
    Ok(checker::check(&content, expected_header, kind))
}

/// Read content from `reader`, normalize the header to `expected_header`,
/// and write the rewritten content to `writer`.
///
/// The returned [`CheckResult`] describes the state of the *input* (so
/// callers can tell whether anything actually needed fixing). The full
/// rewritten content is always written to `writer` — when the input
/// already has the correct header, the output is byte-equivalent to the
/// input.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if the reader fails, the content
/// is not valid UTF-8, or the writer fails.
pub fn fix<R: Read, W: Write>(mut reader: R, mut writer: W, expected_header: &str, kind: FileKind) -> io::Result<CheckResult> {
    let mut content = String::new();
    reader.read_to_string(&mut content)?;
    let (result, new_content) = checker::fix(&content, expected_header, kind);
    writer.write_all(new_content.as_bytes())?;
    Ok(result)
}
