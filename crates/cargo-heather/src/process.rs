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
/// Line endings are preserved: if the input uses CRLF (`\r\n`), the
/// output will too. The detected line-ending style is passed through
/// to all internal formatting and reassembly helpers.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if the reader fails, the content
/// is not valid UTF-8, or the writer fails.
pub fn fix<R: Read, W: Write>(mut reader: R, mut writer: W, expected_header: &str, kind: FileKind) -> io::Result<CheckResult> {
    let mut content = String::new();
    reader.read_to_string(&mut content)?;
    if checker::check(&content, expected_header, kind) == CheckResult::Ok {
        writer.write_all(content.as_bytes())?;
        Ok(CheckResult::Ok)
    } else {
        let line_ending = if content.contains("\r\n") { "\r\n" } else { "\n" };
        let (result, new_content) = checker::fix(&content, expected_header, kind, line_ending);
        writer.write_all(new_content.as_bytes())?;
        Ok(result)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    const HEADER: &str = "Copyright (c) Microsoft Corporation.\nLicensed under the MIT License.";

    #[test]
    fn fix_preserves_crlf_when_adding_missing_header() {
        let input = b"fn main() {}\r\n";
        let mut output: Vec<u8> = Vec::new();
        let result = fix(&input[..], &mut output, HEADER, FileKind::Rust).unwrap();
        assert_eq!(result, CheckResult::Missing);
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("\r\n"), "output must use CRLF when input uses CRLF, got: {text:?}");
        assert!(!text.contains("\r\n\r\n\r\n"), "must not have triple CRLF, got: {text:?}");
        assert!(text.ends_with("fn main() {}\r\n"));
    }

    #[test]
    fn fix_preserves_lf_when_adding_missing_header() {
        let input = b"fn main() {}\n";
        let mut output: Vec<u8> = Vec::new();
        let result = fix(&input[..], &mut output, HEADER, FileKind::Rust).unwrap();
        assert_eq!(result, CheckResult::Missing);
        let text = String::from_utf8(output).unwrap();
        assert!(!text.contains("\r\n"), "output must use LF when input uses LF, got: {text:?}");
    }

    #[test]
    fn fix_preserves_crlf_when_replacing_wrong_header() {
        let input = b"// Wrong header.\r\n\r\nfn main() {}\r\n";
        let mut output: Vec<u8> = Vec::new();
        let result = fix(&input[..], &mut output, HEADER, FileKind::Rust).unwrap();
        assert!(matches!(result, CheckResult::Missing | CheckResult::Mismatch { .. }));
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("\r\n"), "output must use CRLF when input uses CRLF, got: {text:?}");
        assert!(
            !text.contains('\n') || !text.contains("\r\n") || text.replace("\r\n", "").find('\n').is_none(),
            "must not mix bare LF with CRLF"
        );
    }

    #[test]
    fn fix_correct_crlf_header_is_byte_equivalent() {
        let input = "// Copyright (c) Microsoft Corporation.\r\n// Licensed under the MIT License.\r\n\r\nfn main() {}\r\n";
        let mut output: Vec<u8> = Vec::new();
        let result = fix(input.as_bytes(), &mut output, HEADER, FileKind::Rust).unwrap();
        assert_eq!(result, CheckResult::Ok);
        assert_eq!(output, input.as_bytes(), "correct header must be written unchanged");
    }

    #[test]
    fn fix_preserves_crlf_toml_missing_header() {
        let input = b"[package]\r\nname = \"foo\"\r\n";
        let mut output: Vec<u8> = Vec::new();
        let result = fix(&input[..], &mut output, HEADER, FileKind::Toml).unwrap();
        assert_eq!(result, CheckResult::Missing);
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("\r\n"), "TOML output must preserve CRLF");
        assert!(text.ends_with("[package]\r\nname = \"foo\"\r\n"));
    }
}
