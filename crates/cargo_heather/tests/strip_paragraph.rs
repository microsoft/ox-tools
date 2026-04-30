// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Targeted test for the paragraph-separator branch of `find_header_end`
//! in `src/checker/strip.rs`. When a header block is followed by a blank
//! comment line (e.g. `//`) used as a paragraph separator, that separator
//! must be consumed by `fix` so it doesn't leak into the rewritten file.

mod common;

use cargo_heather::FileKind;
use common::{HEADER_MIT, fix_to_string};

#[test]
fn fix_consumes_paragraph_separator_after_header() {
    // Existing header is just one line ("// Old header.") that does not
    // match `HEADER_MIT`, followed by a `//` paragraph separator, then a
    // body comment, then code.
    //
    // After fix, the new MIT header is inserted and the OLD header lines
    // (including the `//` separator) must be consumed. If the `idx += 1`
    // in strip.rs were mutated to `*=` or `-=`, the separator would leak
    // into the output (or the function would panic), making this test fail.
    const INPUT: &str = "\
// Old header line.
//
// Body comment that should remain.

fn main() {}
";
    let (output, _) = fix_to_string(INPUT, HEADER_MIT, FileKind::Rust);

    // The new header replaces the old one.
    assert!(
        output.starts_with("// Licensed under the MIT License."),
        "new header should be at the top: {output:?}"
    );

    // The old separator `//` (as its own paragraph break before the body
    // comment) must NOT survive into the body. The body comment "// Body
    // comment that should remain." MUST appear, but the bare `//` line
    // immediately after the new header must not.
    assert!(
        output.contains("// Body comment that should remain."),
        "body comment should be preserved: {output:?}"
    );

    // Reconstruct the body and verify there's no stray `//` paragraph
    // separator between the new header and the surviving body comment.
    let after_header = output
        .strip_prefix("// Licensed under the MIT License.\n")
        .expect("output must start with new header");
    assert!(
        !after_header.starts_with("//\n"),
        "paragraph separator was not consumed: {after_header:?}"
    );
}
