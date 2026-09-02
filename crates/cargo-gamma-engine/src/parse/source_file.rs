// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Parsed source text and byte-offset navigation over it.

use core::ops::Range;
use std::fs;

use camino::{Utf8Path, Utf8PathBuf};
use syn::File;

use super::comment::{self, Comment};
use super::nesting;
use crate::Result;
use crate::error::error;
use crate::text::encode_controls;

/// A parsed source file, with everything downstream stages need to work in byte offsets.
#[derive(Debug)]
pub struct SourceFile {
    /// Path as it should appear in reports, relative to the workspace root where possible.
    ///
    /// `pub(crate)` rather than private: the survey relocates a file read by absolute path to its
    /// workspace-relative one once it knows it, through the controlled [`Self::set_path`] rather
    /// than a bare field assignment, but every other read stays inside this crate.
    pub(crate) path: Utf8PathBuf,

    /// The exact bytes that were parsed. All spans index into this.
    pub(crate) text: String,

    /// The syntax tree.
    ///
    /// `pub(crate)` rather than private: this crate's own tests build fixtures by mutating a
    /// parsed tree directly, which a getter-only encapsulation cannot express. Every other crate
    /// sees this only through the read-only [`Self::ast`] accessor.
    pub(crate) ast: File,

    /// Byte offset of the start of each line.
    lines: Vec<usize>,

    /// Every comment in the file, in source order.
    pub(crate) comments: Vec<Comment>,
}

/// Whether source text is too deeply nested to hand to a recursive parser.
#[doc(hidden)]
#[must_use]
pub fn exceeds_nesting_limit(text: &str) -> bool {
    let lines = line_starts(text);
    let comments = comment::scan_comments(text, &lines);

    nesting::beyond(text, &comments, nesting::NESTING_LIMIT).is_some()
}

impl SourceFile {
    /// Parses source text that has already been read.
    ///
    /// The path is used only for diagnostics and reporting; nothing is read from disk here, which
    /// is what lets every test in this crate work on string literals.
    ///
    /// # Errors
    ///
    /// Returns an error if the text is not Rust, or if it nests deeper than
    /// [`NESTING_LIMIT`](nesting::NESTING_LIMIT) — see that constant for why a file can be too
    /// deep to look at.
    pub fn parse(path: impl Into<Utf8PathBuf>, text: String) -> Result<Self> {
        let path = path.into();
        let text = without_bom(text);
        let lines = line_starts(&text);
        let comments = comment::scan_comments(&text, &lines);

        // Before `syn`, deliberately. The parser is the first of the recursive descents over this
        // text and the guard is worth nothing after one of them has already run out of stack.
        if let Some(at) = nesting::beyond(&text, &comments, nesting::NESTING_LIMIT) {
            let line = lines.partition_point(|start| *start <= at);

            // Skippable, because this is the one parse failure that says nothing about whether the
            // workspace is sound. A file that does not parse does not compile either, so refusing
            // the run tells the user something they were about to find out anyway; a file nested
            // past this limit is one `rustc` builds happily and only this tool cannot walk. Killing
            // the whole run over it would make a valid workspace unmeasurable, which is a worse
            // answer than measuring the rest of it and naming what was left out.
            return Err(error!(
                "{}:{line}: nests deeper than {} levels of brackets, prefix operators, chained operators or postfix expressions",
                encode_controls(path.as_str()),
                nesting::NESTING_LIMIT
            )
            .skippable());
        }

        let ast = syn::parse_file(&text).map_err(|cause| {
            let start = cause.span().start();

            // The path is repository-controlled and this message is printed to a terminal, so it is
            // encoded here rather than trusted; `cause` is `syn`'s own prose about a token it read
            // from that same repository, so it is encoded for the same reason.
            error!(
                "{}:{}:{}: could not parse: {}",
                encode_controls(path.as_str()),
                start.line,
                start.column,
                encode_controls(&cause.to_string())
            )
        })?;

        Ok(Self {
            path,
            text,
            ast,
            lines,
            comments,
        })
    }

    /// Reads and parses a file from disk.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read from `path`, or any error [`Self::parse`]
    /// documents for the text once it has been read.
    pub fn read(path: impl AsRef<Utf8Path>) -> Result<Self> {
        let path = path.as_ref();

        let text =
            fs::read_to_string(path).map_err(|cause| error!("could not read `{}`", encode_controls(path.as_str())).caused_by(cause))?;

        Self::parse(path.to_owned(), text)
    }

    /// Returns the 1-based line and column of a byte offset.
    ///
    /// The column is counted in characters rather than bytes, because it is shown to humans beside
    /// a rendering of the line, and a byte column would point at the wrong place in any line
    /// containing non-ASCII text.
    #[must_use]
    pub fn location(&self, offset: usize) -> (usize, usize) {
        let line_index = match self.lines.binary_search(&offset) {
            Ok(exact) => exact,
            Err(insertion) => insertion.saturating_sub(1),
        };

        let line_start = self.lines.get(line_index).copied().unwrap_or(0);
        let clamped = offset.min(self.text.len());
        let column = self.text.get(line_start..clamped).map_or(0, |s| s.chars().count());

        (line_index + 1, column + 1)
    }

    /// Returns the 1-based line number of a byte offset.
    #[must_use]
    pub fn line_of(&self, offset: usize) -> usize {
        self.location(offset).0
    }

    /// Returns the source text covered by a byte range.
    #[must_use]
    pub fn slice(&self, span: &Range<usize>) -> &str {
        self.text.get(span.start..span.end).unwrap_or("")
    }

    /// Returns the path as it should appear in reports.
    #[must_use]
    pub fn path(&self) -> &Utf8Path {
        &self.path
    }

    /// Relocates the file to a different reporting path, without re-parsing its text.
    ///
    /// A file is often read from an absolute path and then reported relative to the workspace
    /// root once the caller knows it; this is the one controlled way to update that path after
    /// parsing, so every other representation field stays untouched and in agreement with `text`.
    pub fn set_path(&mut self, path: impl Into<Utf8PathBuf>) {
        self.path = path.into();
    }

    /// Returns the exact bytes that were parsed. All spans index into this.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns the syntax tree.
    #[must_use]
    pub fn ast(&self) -> &File {
        &self.ast
    }

    /// Returns every comment in the file, in source order.
    #[must_use]
    pub fn comments(&self) -> &[Comment] {
        &self.comments
    }
}

/// Drops a leading byte-order mark.
///
/// `syn` skips the mark and then counts byte offsets from zero, so a file that has one hands back
/// spans three bytes short of where the construct really is: the recorded original text is the
/// wrong slice, the reported column is wrong, and instrumenting the file on disk splices guards
/// three bytes off — through the middle of a character, if the file is not ASCII.
///
/// Removing it here, at the one place source text enters, is what makes every offset downstream
/// agree. The mark carries no meaning in Rust source, so nothing is lost; callers that rewrite a
/// user's source retain it separately at the file boundary.
#[must_use]
pub fn without_bom(mut text: String) -> String {
    if text.starts_with(BOM) {
        let _removed = text.drain(..BOM.len_utf8());
    }

    text
}

/// The byte-order mark, which is three bytes of UTF-8 and no characters of Rust.
pub const BOM: char = '\u{feff}';

/// Removes a leading byte-order mark without allocating.
///
/// Source parsing owns normalized text, while source editing must retain the original bytes. This
/// lets generation comparisons use the parser's representation without making an edit move or
/// discard the mark.
#[must_use]
pub fn strip_bom(text: &str) -> &str {
    text.strip_prefix(BOM).unwrap_or(text)
}

/// Byte offset of the start of each line, indexing the text as `syn` sees it.
pub(super) fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];

    starts.extend(text.bytes().enumerate().filter(|(_, b)| *b == b'\n').map(|(i, _)| i + 1));

    // A trailing newline does not open a line that anything can be on.
    if starts.last() == Some(&text.len()) && !text.is_empty() {
        let _ = starts.pop();
    }

    starts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> SourceFile {
        SourceFile::parse("test.rs", text.to_owned()).unwrap()
    }

    #[test]
    fn a_parse_failure_names_the_file_and_position() {
        let error = SourceFile::parse("bad.rs", "fn f( {".to_owned()).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("bad.rs"), "{message}");
        assert!(message.contains("could not parse"), "{message}");
    }

    /// A file too deep to walk is refused by name, not by dying.
    ///
    /// Every stage that reads source descends by recursion, and a stack overflow on Linux is a
    /// `SIGSEGV`: the process disappears with no diagnostic, no file named, and nothing to
    /// distinguish it from a bug in the tool. This depth is past the measured overflow point of
    /// the parse-and-collect path on the smallest stack a discovery worker runs on, so the file
    /// has to be turned away before `syn` ever sees it.
    #[test]
    fn a_file_nested_deeper_than_the_limit_is_refused_rather_than_overflowing_the_stack() {
        let depth = 4_096;
        let text = format!("fn f() -> i32 {{ {}1{} }}\n", "(".repeat(depth), ")".repeat(depth));
        let message = SourceFile::parse("deep.rs", text).unwrap_err().to_string();

        assert!(message.contains("deep.rs"), "{message}");
        assert!(message.contains("nests deeper"), "{message}");
    }

    #[test]
    fn a_file_with_a_deep_postfix_chain_is_refused_before_parsing() {
        let chains = [format!("call{}", "()".repeat(4_096)), format!("value{}", "[0]".repeat(4_096))];

        for expression in chains {
            let text = format!("fn f() {{ {expression}; }}\n");
            let message = SourceFile::parse("postfix.rs", text).unwrap_err().to_string();

            assert!(message.contains("postfix.rs"), "{message}");
            assert!(message.contains("nests deeper"), "{message}");
        }
    }

    /// Nesting a human would write is still analyzed, which is the other half of the bargain.
    #[test]
    fn a_file_within_the_nesting_limit_still_parses() {
        let depth = 20;
        let text = format!("fn f() -> i32 {{ {}1{} }}\n", "(".repeat(depth), ")".repeat(depth));

        let parsed = SourceFile::parse("deep_enough.rs", text).expect("nesting a human writes is analyzed");

        assert_eq!(parsed.path, "deep_enough.rs");
    }

    #[test]

    fn read_loads_and_parses_a_file_from_disk() {
        let path = Utf8Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src/parse/source_file.rs"));
        let file = SourceFile::read(path).unwrap();

        assert_eq!(file.path, path);
        assert!(file.text.contains("pub struct SourceFile"));
    }

    #[test]

    fn read_failures_name_the_missing_file() {
        let error = SourceFile::read(Utf8Path::new("target/does-not-exist/source.rs")).unwrap_err();

        assert!(error.to_string().contains("could not read"));
    }

    #[test]
    fn locations_are_one_based() {
        let file = parse("fn a() {}\nfn b() {}\n");

        assert_eq!(file.location(0), (1, 1));
        assert_eq!(file.location(3), (1, 4));
        assert_eq!(file.location(10), (2, 1));
    }

    #[test]
    fn columns_count_characters_not_bytes() {
        let file = parse("fn f() { let s = \"éé\"; let _ = s; }\n");
        let offset = file.text.find("let _").unwrap();
        let (line, column) = file.location(offset);

        assert_eq!(line, 1);
        assert_eq!(file.text.get(..offset).unwrap().chars().count() + 1, column);
    }

    /// Walks the text once, keeping line and column by hand, and reports what every boundary
    /// offset should map to.
    ///
    /// Deliberately shares nothing with `location`: no binary search, no `lines` table, no
    /// `chars().count()` over a slice. `columns_count_characters_not_bytes` computes its
    /// expectation the way the implementation does, on line one where the line start is zero, so
    /// an implementation that dropped the line start entirely would still satisfy it. This one
    /// would not.
    ///
    /// The final offset is excluded because a text ending in a newline has no line there —
    /// `line_starts` pops it — and there is nothing for an independent oracle to agree with.
    fn walked(text: &str) -> Vec<(usize, (usize, usize))> {
        let (mut line, mut column) = (1, 1);
        let mut expected = Vec::new();

        for (offset, character) in text.char_indices() {
            expected.push((offset, (line, column)));

            if character == '\n' {
                line += 1;
                column = 1;
            } else {
                column += 1;
            }
        }

        expected
    }

    #[test]
    fn every_boundary_offset_lands_where_walking_the_text_says_it_should() {
        for text in [
            "",
            "\n",
            "fn a() {}\n",
            "fn a() {}\nfn b() {}\n",
            "fn a() {}\r\nfn b() {}\r\n",
            "fn é() { let ß = 1; }\nfn b() {}\n",
            "\u{feff}fn a() {}\n",
            "\u{feff}",
            "\n\n\nfn a() {}",
            "fn a() {}",
        ] {
            let file = parse(text);

            // The file's own text rather than the literal, because a byte-order mark is dropped on
            // the way in — which is the point of the mark case being here.
            for (offset, expected) in walked(&file.text) {
                assert_eq!(file.location(offset), expected, "offset {offset} of {text:?} is misplaced");
            }
        }
    }

    /// A byte-order mark is dropped, so that spans mean what `syn` meant by them.
    ///
    /// Regression: `syn` skips a leading mark and then counts byte offsets from zero, so on a
    /// file that had one every mutant's span was three bytes short of the construct. The recorded
    /// original text was the wrong slice, the reported column was wrong, and instrumenting the
    /// file on disk spliced guards three bytes off — through the middle of a character, on a file
    /// that was not ASCII.
    #[test]
    fn a_byte_order_mark_is_dropped_so_that_offsets_mean_what_syn_meant() {
        let file = parse("\u{feff}fn a() {}\n");

        assert_eq!(file.text, "fn a() {}\n");
        assert_eq!(file.location(0), (1, 1));

        // A mark anywhere but the front is ordinary text and is left alone.
        let inner = parse("fn a() { let _ = \"\u{feff}\"; }\n");

        assert!(inner.text.contains('\u{feff}'));
    }

    #[test]
    fn strip_bom_removes_only_a_leading_mark() {
        assert_eq!(strip_bom("\u{feff}fn a() {}\n"), "fn a() {}\n");
        assert_eq!(strip_bom("fn a() {}\n"), "fn a() {}\n");
        assert_eq!(
            strip_bom("fn a() { let _ = \"\u{feff}\"; }\n"),
            "fn a() { let _ = \"\u{feff}\"; }\n"
        );
    }

    #[test]
    fn a_trailing_newline_does_not_open_a_line() {
        assert_eq!(parse("fn a() {}\n").lines.len(), 1);
        assert_eq!(parse("fn a() {}\nfn b() {}\n").lines.len(), 2);
    }
}

#[cfg(all(test, not(miri)))]
mod fuzz {
    use super::{SourceFile, line_starts};

    /// A file whose navigation tables describe `text`.
    ///
    /// The syntax tree comes from a fixed stub and is never looked at: `location` reads only the
    /// text and the line table. Going through `SourceFile::parse` instead would mean discarding
    /// every generated text that is not valid Rust, which is very nearly all of them, and the
    /// property would then be checked against almost nothing.
    fn navigable(text: &str) -> SourceFile {
        let mut file = SourceFile::parse("fuzz.rs", "fn f() {}\n".to_owned()).expect("the stub parses");

        file.lines = line_starts(text);
        text.clone_into(&mut file.text);

        file
    }

    /// Arbitrary text maps every boundary offset to a location that is really where it is.
    ///
    /// Everything downstream of the parser is byte-offset arithmetic over source nobody in this
    /// project wrote, and every hand-written fixture here is text somebody chose. The two
    /// properties are the ones the rest of the pipeline relies on and that a table of examples can
    /// only sample: that the line and column of an offset are what counting from the start of the
    /// text gives, and that slicing back to that line and column lands on the same offset.
    #[test]
    fn locations_over_arbitrary_text_agree_with_counting_from_the_start() {
        // Generated as lines rather than as one string: a `String` generator emits a newline
        // about as often as any other character, so a flat one would leave the line table barely
        // exercised — an implementation that ignored the line start entirely survived that
        // version of this test. Each line carries its own terminator so that `\r\n`, which is
        // ordinary in real repositories and is *not* a line start of its own, is covered too.
        bolero::check!().with_type::<Vec<(String, bool)>>().for_each(|lines| {
            let mut text = String::new();

            for (line, crlf) in lines {
                text.push_str(line);
                text.push_str(if *crlf { "\r\n" } else { "\n" });
            }

            let text = &text;
            let file = navigable(text);
            let (mut line, mut column) = (1, 1);
            let mut line_start = 0;

            for (offset, character) in text.char_indices() {
                let located = file.location(offset);

                assert_eq!(located, (line, column), "offset {offset} of {text:?} is misplaced");

                // The other direction: the location has to name a real place in the text, so
                // taking that many characters from the start of that line must arrive back here.
                let back = text
                    .get(line_start..)
                    .and_then(|rest| rest.char_indices().nth(located.1 - 1))
                    .map(|(index, _)| line_start + index);

                assert_eq!(back, Some(offset), "the column of {offset} in {text:?} does not lead back");

                if character == '\n' {
                    line += 1;
                    column = 1;
                    line_start = offset + 1;
                } else {
                    column += 1;
                }
            }
        });
    }
}
