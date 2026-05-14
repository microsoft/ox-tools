// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Managed-region parser and writer.
//!
//! A managed region is a section of a user-composed file that
//! `cargo-ox-check` owns the contents of. It is delimited by sentinel
//! comments:
//!
//! ```text
//! # >>> ox-check-managed: <id>
//! …content owned by ox-check…
//! # <<< ox-check-managed: <id>
//! ```
//!
//! The user's content outside the sentinels is preserved byte-for-byte.
//! The `id` is globally unique within the catalog (e.g. `ox-check-imports`,
//! `ox-check-workspace-lints`).
//!
//! Empty body (just the sentinels with no content between them) is the
//! opt-out signal — see [updates.md §6](../../docs/design/updates.md).

use ohno::{AppError, app_err, bail};

/// Comment syntax used by the host file.
///
/// Both supported flavors today use `#`-prefixed comments (Justfiles,
/// TOML, YAML). `//` is reserved for future hosts (e.g. JSON5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentSyntax {
    /// `#`-prefixed line comments — Justfile, TOML, YAML.
    Hash,
    /// `//`-prefixed line comments — JSON5 and friends.
    SlashSlash,
}

impl CommentSyntax {
    fn prefix(self) -> &'static str {
        match self {
            Self::Hash => "#",
            Self::SlashSlash => "//",
        }
    }
}

/// One managed region located inside a host file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region<'a> {
    /// The region's stable id (e.g. `ox-check-imports`).
    pub id: String,
    /// Byte range of the opening sentinel line, including the trailing
    /// newline (if any).
    pub start_line: ByteRange,
    /// Byte range of the closing sentinel line, including the trailing
    /// newline (if any).
    pub end_line: ByteRange,
    /// Byte range of the region body — everything between the two
    /// sentinels' line spans.
    pub body: ByteRange,
    /// The full host text (borrowed). Used to extract body content.
    text: &'a str,
}

/// Half-open byte range `[start, end)` into the host text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    /// Inclusive start byte.
    pub start: usize,
    /// Exclusive end byte.
    pub end: usize,
}

impl<'a> Region<'a> {
    /// The current body content, as a string slice into the host text.
    #[must_use]
    pub fn body_str(&self) -> &'a str {
        &self.text[self.body.start..self.body.end]
    }

    /// Whether this region is empty (opted out). An empty region is one
    /// whose body, after trimming line terminators and whitespace,
    /// contains no non-whitespace characters.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.body_str().trim().is_empty()
    }
}

/// Locate the named region in `text`. Returns `Ok(None)` if absent.
///
/// # Errors
///
/// Returns an error if the region is malformed: multiple opening
/// sentinels for the same id, an opening sentinel with no matching close,
/// or a close before its open.
pub fn find_region<'a>(
    text: &'a str,
    id: &str,
    syntax: CommentSyntax,
) -> Result<Option<Region<'a>>, AppError> {
    let opener = format!("{} >>> ox-check-managed: {id}", syntax.prefix());
    let closer = format!("{} <<< ox-check-managed: {id}", syntax.prefix());

    let mut start_line: Option<ByteRange> = None;
    let mut end_line: Option<ByteRange> = None;
    for line in iterate_lines(text) {
        let body = text[line.start..line.end].trim_end_matches(['\n', '\r']);
        let trimmed = body.trim();
        if trimmed == opener {
            if start_line.is_some() {
                bail!("duplicate opening sentinel for region '{id}'");
            }
            start_line = Some(line);
            continue;
        }
        if trimmed == closer {
            if start_line.is_none() {
                bail!("closing sentinel for region '{id}' before its opener");
            }
            if end_line.is_some() {
                bail!("duplicate closing sentinel for region '{id}'");
            }
            end_line = Some(line);
        }
    }

    match (start_line, end_line) {
        (None, None) => Ok(None),
        (Some(_), None) => Err(app_err!(
            "region '{id}' has an opening sentinel but no closing sentinel"
        )),
        // (None, Some(_)) was already caught above; left as a safety net.
        (None, Some(_)) => Err(app_err!(
            "region '{id}' has a closing sentinel with no opener"
        )),
        (Some(start), Some(end)) => {
            if end.start < start.end {
                bail!("closing sentinel for region '{id}' precedes its opener");
            }
            let body = ByteRange {
                start: start.end,
                end: end.start,
            };
            Ok(Some(Region {
                id: id.to_owned(),
                start_line: start,
                end_line: end,
                body,
                text,
            }))
        }
    }
}

/// Replace the body of region `id` in `text`, or append a new region if
/// none exists.
///
/// `new_body` is inserted between the sentinel lines verbatim, with a
/// single newline between each sentinel and the body. If `new_body` does
/// not end with `\n`, one is added before the closing sentinel.
///
/// # Errors
///
/// Returns an error if an existing region is malformed.
pub fn upsert_region(
    text: &str,
    id: &str,
    new_body: &str,
    syntax: CommentSyntax,
) -> Result<String, AppError> {
    let rendered = render_region(id, new_body, syntax);

    if let Some(region) = find_region(text, id, syntax)? {
        let mut out = String::with_capacity(text.len() + rendered.len());
        out.push_str(&text[..region.start_line.start]);
        out.push_str(&rendered);
        out.push_str(&text[region.end_line.end..]);
        return Ok(out);
    }

    // No region present — append at the end with one blank line of
    // separation if the file is non-empty and doesn't end in two newlines.
    let mut out = String::with_capacity(text.len() + rendered.len() + 1);
    out.push_str(text);
    if !text.is_empty() {
        if !text.ends_with('\n') {
            out.push('\n');
        }
        if !text.ends_with("\n\n") && !text.is_empty() {
            out.push('\n');
        }
    }
    out.push_str(&rendered);
    Ok(out)
}

/// Render an isolated region — sentinels plus body — without splicing it
/// into a host.
#[must_use]
pub fn render_region(id: &str, body: &str, syntax: CommentSyntax) -> String {
    let prefix = syntax.prefix();
    let mut out = String::with_capacity(body.len() + 80);
    out.push_str(prefix);
    out.push_str(" >>> ox-check-managed: ");
    out.push_str(id);
    out.push('\n');
    out.push_str(body);
    if !body.is_empty() && !body.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(prefix);
    out.push_str(" <<< ox-check-managed: ");
    out.push_str(id);
    out.push('\n');
    out
}

fn iterate_lines(text: &str) -> LineIter<'_> {
    LineIter { text, pos: 0 }
}

struct LineIter<'a> {
    text: &'a str,
    pos: usize,
}

impl Iterator for LineIter<'_> {
    type Item = ByteRange;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.text.len() {
            return None;
        }
        let start = self.pos;
        let rest = &self.text[start..];
        let end = match rest.find('\n') {
            Some(i) => start + i + 1,
            None => self.text.len(),
        };
        self.pos = end;
        Some(ByteRange { start, end })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYN: CommentSyntax = CommentSyntax::Hash;

    #[test]
    fn missing_region_returns_none() {
        assert_eq!(find_region("user content\n", "ox-check-x", SYN).unwrap(), None);
    }

    #[test]
    fn finds_region_with_body() {
        let text = "before\n\
                    # >>> ox-check-managed: ox-check-x\n\
                    body line 1\n\
                    body line 2\n\
                    # <<< ox-check-managed: ox-check-x\n\
                    after\n";
        let region = find_region(text, "ox-check-x", SYN).unwrap().unwrap();
        assert_eq!(region.body_str(), "body line 1\nbody line 2\n");
        assert!(!region.is_empty());
    }

    #[test]
    fn finds_empty_region() {
        let text = "\
            # >>> ox-check-managed: ox-check-x\n\
            # <<< ox-check-managed: ox-check-x\n";
        let region = find_region(text, "ox-check-x", SYN).unwrap().unwrap();
        assert_eq!(region.body_str(), "");
        assert!(region.is_empty());
    }

    #[test]
    fn region_with_only_whitespace_is_empty() {
        let text = "\
            # >>> ox-check-managed: ox-check-x\n\
            \n\
            \t\n\
            # <<< ox-check-managed: ox-check-x\n";
        let region = find_region(text, "ox-check-x", SYN).unwrap().unwrap();
        assert!(region.is_empty());
    }

    #[test]
    fn duplicate_opener_errors() {
        let text = "\
            # >>> ox-check-managed: x\n\
            # >>> ox-check-managed: x\n\
            # <<< ox-check-managed: x\n";
        let err = find_region(text, "x", SYN).unwrap_err();
        assert!(err.to_string().contains("duplicate opening sentinel"));
    }

    #[test]
    fn unterminated_region_errors() {
        let text = "# >>> ox-check-managed: x\nbody\n";
        let err = find_region(text, "x", SYN).unwrap_err();
        assert!(err.to_string().contains("no closing sentinel"));
    }

    #[test]
    fn closer_before_opener_errors() {
        let text = "# <<< ox-check-managed: x\n# >>> ox-check-managed: x\n";
        let err = find_region(text, "x", SYN).unwrap_err();
        assert!(err.to_string().contains("before its opener"));
    }

    #[test]
    fn upsert_replaces_existing_body() {
        let text = "before\n\
                    # >>> ox-check-managed: x\n\
                    old body\n\
                    # <<< ox-check-managed: x\n\
                    after\n";
        let new = upsert_region(text, "x", "new body line 1\nnew body line 2\n", SYN).unwrap();
        assert!(new.contains("new body line 1"));
        assert!(!new.contains("old body"));
        // User content outside the region is preserved byte-for-byte.
        assert!(new.starts_with("before\n"));
        assert!(new.ends_with("after\n"));
    }

    #[test]
    fn upsert_appends_when_absent() {
        let text = "user file\n";
        let new = upsert_region(text, "x", "body\n", SYN).unwrap();
        assert!(new.starts_with("user file\n"));
        assert!(new.contains("# >>> ox-check-managed: x\nbody\n# <<< ox-check-managed: x\n"));
    }

    #[test]
    fn upsert_into_empty_file() {
        let new = upsert_region("", "x", "body\n", SYN).unwrap();
        assert_eq!(
            new,
            "# >>> ox-check-managed: x\nbody\n# <<< ox-check-managed: x\n"
        );
    }

    #[test]
    fn upsert_empties_region() {
        let text = "# >>> ox-check-managed: x\nfilled\n# <<< ox-check-managed: x\n";
        let new = upsert_region(text, "x", "", SYN).unwrap();
        let region = find_region(&new, "x", SYN).unwrap().unwrap();
        assert!(region.is_empty());
    }

    #[test]
    fn render_region_with_empty_body() {
        let s = render_region("x", "", SYN);
        assert_eq!(s, "# >>> ox-check-managed: x\n# <<< ox-check-managed: x\n");
    }

    #[test]
    fn render_region_adds_trailing_newline() {
        let s = render_region("x", "body", SYN);
        assert_eq!(
            s,
            "# >>> ox-check-managed: x\nbody\n# <<< ox-check-managed: x\n"
        );
    }

    #[test]
    fn slash_slash_syntax_works() {
        let text = "// >>> ox-check-managed: x\nbody\n// <<< ox-check-managed: x\n";
        let region = find_region(text, "x", CommentSyntax::SlashSlash)
            .unwrap()
            .unwrap();
        assert_eq!(region.body_str(), "body\n");
    }

    #[test]
    fn hash_syntax_ignores_slash_slash_sentinels() {
        let text = "// >>> ox-check-managed: x\nbody\n// <<< ox-check-managed: x\n";
        assert_eq!(find_region(text, "x", SYN).unwrap(), None);
    }

    #[test]
    fn finds_multiple_distinct_regions() {
        let text = "\
            # >>> ox-check-managed: a\n\
            body-a\n\
            # <<< ox-check-managed: a\n\
            user content between\n\
            # >>> ox-check-managed: b\n\
            body-b\n\
            # <<< ox-check-managed: b\n";
        let a = find_region(text, "a", SYN).unwrap().unwrap();
        let b = find_region(text, "b", SYN).unwrap().unwrap();
        assert_eq!(a.body_str(), "body-a\n");
        assert_eq!(b.body_str(), "body-b\n");
    }

    #[test]
    fn region_sentinels_indented_still_recognized() {
        // Sentinels with leading whitespace should be recognized — useful
        // in YAML where indentation matters in the host file.
        let text = "  # >>> ox-check-managed: x\nbody\n  # <<< ox-check-managed: x\n";
        let region = find_region(text, "x", SYN).unwrap().unwrap();
        assert_eq!(region.body_str(), "body\n");
    }
}
