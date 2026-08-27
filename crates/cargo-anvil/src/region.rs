// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Managed-region parser and writer.
//!
//! A managed region is a section of a user-composed file that
//! `cargo-anvil` owns the contents of. It is delimited by sentinel
//! comments:
//!
//! ```text
//! # >>> anvil-managed: <id>
//! …content owned by anvil…
//! # <<< anvil-managed: <id>
//! ```
//!
//! The user's content outside the sentinels is preserved byte-for-byte.
//! The `id` is globally unique within the catalog (e.g. `anvil-imports`,
//! `anvil-workspace-lints`).
//!
//! Empty body (just the sentinels with no content between them) is the
//! opt-out signal — see [`updates.md §6`](../../docs/design/updates.md).

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

/// Where a newly rendered managed region is placed in its host file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionPlacement {
    /// Place the region before user content.
    Start,
    /// Place the region after user content.
    End,
    /// Insert a *new* region at this byte offset. An existing region is still
    /// updated where it is found, so this only decides where an absent one
    /// lands.
    ///
    /// Needed by hosts whose region order is semantic: appending a newly added
    /// region at end-of-file would put it after regions it must precede, which
    /// for a Dockerfile means `FROM` below the layers that depend on it. The
    /// caller knows the declared order, so it computes the offset.
    At(usize),
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
    /// The region's stable id (e.g. `anvil-imports`).
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
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "region opt-out predicate, currently exercised only by unit tests")
    )]
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
pub fn find_region<'a>(text: &'a str, id: &str, syntax: CommentSyntax) -> Result<Option<Region<'a>>, AppError> {
    let opener = format!("{} >>> anvil-managed: {id}", syntax.prefix());
    let closer = format!("{} <<< anvil-managed: {id}", syntax.prefix());

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

    let Some(start) = start_line else {
        // No opening sentinel. Any stray closing sentinel was already
        // rejected in the scan loop above (it bails before reaching
        // here), so there is simply no region to report.
        return Ok(None);
    };
    let Some(end) = end_line else {
        return Err(app_err!("region '{id}' has an opening sentinel but no closing sentinel"));
    };
    // The scan loop only records `end` after `start` is set, and lines
    // are visited in order, so the closing sentinel can never precede
    // the opening one.
    debug_assert!(end.start >= start.end, "closing sentinel recorded before its opener");
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
pub fn upsert_region(text: &str, id: &str, new_body: &str, syntax: CommentSyntax) -> Result<String, AppError> {
    upsert_region_with_placement(text, id, new_body, syntax, RegionPlacement::End)
}

/// Replace or insert a managed region at the requested host-file position.
///
/// Existing start-placed regions are moved to the beginning as part of an
/// update. This is required for TOML root keys, which cannot be appended after
/// a table header without becoming members of that table.
///
/// # Errors
///
/// Returns an error if an existing region is malformed.
pub fn upsert_region_with_placement(
    text: &str,
    id: &str,
    new_body: &str,
    syntax: CommentSyntax,
    placement: RegionPlacement,
) -> Result<String, AppError> {
    let rendered = render_region(id, new_body, syntax);

    if let Some(region) = find_region(text, id, syntax)? {
        if placement == RegionPlacement::Start {
            let without_region = remove_region(text, id, syntax)?;
            return Ok(prepend_region(&without_region, &rendered));
        }
        let mut out = String::with_capacity(text.len() + rendered.len());
        out.push_str(&text[..region.start_line.start]);
        out.push_str(&rendered);
        out.push_str(&text[region.end_line.end..]);
        return Ok(out);
    }

    if placement == RegionPlacement::Start {
        return Ok(prepend_region(text, &rendered));
    }

    if let RegionPlacement::At(offset) = placement {
        let offset = offset.min(text.len());
        // Land on a line boundary. An offset in the middle of a line would
        // split it around the sentinels, which for a Dockerfile is the
        // difference between a valid instruction and two invalid halves.
        let offset = text[offset..].find('\n').map_or(text.len(), |index| offset + index + 1);
        let (before, after) = text.split_at(offset);
        let mut out = String::with_capacity(text.len() + rendered.len() + 2);
        out.push_str(before);
        if !before.is_empty() && !before.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str(&rendered);
        if !after.is_empty() && !after.starts_with('\n') {
            out.push('\n');
        }
        out.push_str(after);
        return Ok(out);
    }

    // No region present — append at the end with one blank line of separation
    // if the file is non-empty and doesn't end in two newlines.
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

fn prepend_region(text: &str, rendered: &str) -> String {
    let mut out = String::with_capacity(text.len() + rendered.len() + 1);
    out.push_str(rendered);
    if !text.is_empty() && !text.starts_with('\n') {
        out.push('\n');
    }
    out.push_str(text);
    out
}

/// Render an isolated region — sentinels plus body — without splicing it
/// into a host.
#[must_use]
pub fn render_region(id: &str, body: &str, syntax: CommentSyntax) -> String {
    let prefix = syntax.prefix();
    let mut out = String::with_capacity(body.len() + 80);
    out.push_str(prefix);
    out.push_str(" >>> anvil-managed: ");
    out.push_str(id);
    out.push('\n');
    out.push_str(body);
    if !body.is_empty() && !body.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(prefix);
    out.push_str(" <<< anvil-managed: ");
    out.push_str(id);
    out.push('\n');
    out
}

/// Splice the named region out of `text`, returning the host content
/// with the markers + body excised entirely.
///
/// To avoid leaving an asymmetric blank-line gap, one adjacent blank
/// line is consumed: the trailing blank if present, else the leading
/// blank if the region sits at end-of-file.
///
/// If the region is not present the input is returned unchanged.
///
/// # Errors
///
/// Returns an error if the host file contains a malformed region with
/// the requested id (mismatched/missing sentinels).
pub fn remove_region(text: &str, id: &str, syntax: CommentSyntax) -> Result<String, AppError> {
    let Some(region) = find_region(text, id, syntax)? else {
        return Ok(text.to_owned());
    };

    let mut cut_start = region.start_line.start;
    let mut cut_end = region.end_line.end;

    // Prefer to eat the trailing blank line — that mirrors upsert's
    // "add one blank line of separation" when the region was first
    // inserted, and it preserves a single blank between user content
    // when the region sits in the middle of the file.
    let trailing_blank = text[cut_end..].starts_with('\n');
    if trailing_blank {
        cut_end += 1;
    } else {
        // Region sits at end-of-file: there's no trailing blank to
        // eat. Pull back the leading blank instead so the file doesn't
        // end with an orphan blank line where the region used to be.
        let prefix = &text[..cut_start];
        if prefix.ends_with("\n\n") {
            cut_start -= 1;
        }
    }

    let mut out = String::with_capacity(text.len() - (cut_end - cut_start));
    out.push_str(&text[..cut_start]);
    out.push_str(&text[cut_end..]);
    Ok(out)
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
        // Progress guard: every yielded line must strictly advance `pos`.
        // Catches infinite-loop regressions (and infinite-loop mutants
        // generated by `cargo mutants` against the arithmetic / comparison
        // operators above) in debug builds.
        debug_assert!(end > start, "LineIter::next must make progress");
        self.pos = end;
        Some(ByteRange { start, end })
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    const SYN: CommentSyntax = CommentSyntax::Hash;

    #[test]
    fn missing_region_returns_none() {
        assert_eq!(find_region("user content\n", "anvil-x", SYN).unwrap(), None);
    }

    #[test]
    fn finds_region_with_body() {
        let text = "before\n\
                    # >>> anvil-managed: anvil-x\n\
                    body line 1\n\
                    body line 2\n\
                    # <<< anvil-managed: anvil-x\n\
                    after\n";
        let region = find_region(text, "anvil-x", SYN).unwrap().unwrap();
        assert_eq!(region.body_str(), "body line 1\nbody line 2\n");
        assert!(!region.is_empty());
    }

    #[test]
    fn finds_empty_region() {
        let text = "\
            # >>> anvil-managed: anvil-x\n\
            # <<< anvil-managed: anvil-x\n";
        let region = find_region(text, "anvil-x", SYN).unwrap().unwrap();
        assert_eq!(region.body_str(), "");
        assert!(region.is_empty());
    }

    #[test]
    fn region_with_only_whitespace_is_empty() {
        let text = "\
            # >>> anvil-managed: anvil-x\n\
            \n\
            \t\n\
            # <<< anvil-managed: anvil-x\n";
        let region = find_region(text, "anvil-x", SYN).unwrap().unwrap();
        assert!(region.is_empty());
    }

    #[test]
    fn duplicate_opener_errors() {
        let text = "\
            # >>> anvil-managed: x\n\
            # >>> anvil-managed: x\n\
            # <<< anvil-managed: x\n";
        let err = find_region(text, "x", SYN).unwrap_err();
        assert!(err.to_string().contains("duplicate opening sentinel"));
    }

    #[test]
    fn unterminated_region_errors() {
        let text = "# >>> anvil-managed: x\nbody\n";
        let err = find_region(text, "x", SYN).unwrap_err();
        assert!(err.to_string().contains("no closing sentinel"));
    }

    #[test]
    fn closer_before_opener_errors() {
        let text = "# <<< anvil-managed: x\n# >>> anvil-managed: x\n";
        let err = find_region(text, "x", SYN).unwrap_err();
        assert!(err.to_string().contains("before its opener"));
    }

    #[test]
    fn upsert_replaces_existing_body() {
        let text = "before\n\
                    # >>> anvil-managed: x\n\
                    old body\n\
                    # <<< anvil-managed: x\n\
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
        assert!(new.contains("# >>> anvil-managed: x\nbody\n# <<< anvil-managed: x\n"));
    }

    #[test]
    fn upsert_appends_with_exactly_one_blank_separator() {
        // Text ends with single \n: must add one extra blank line so there is
        // exactly one blank line between user content and the sentinel.
        let text = "user file\n";
        let new = upsert_region(text, "x", "body\n", SYN).unwrap();
        assert_eq!(new, "user file\n\n# >>> anvil-managed: x\nbody\n# <<< anvil-managed: x\n");
    }

    #[test]
    fn upsert_does_not_add_extra_blank_when_text_ends_with_double_newline() {
        // Catches mutation of the `&&` in `!ends_with("\n\n") && !is_empty()`:
        // if flipped to `||`, an extra blank line would be inserted here.
        let text = "user file\n\n";
        let new = upsert_region(text, "x", "body\n", SYN).unwrap();
        assert_eq!(new, "user file\n\n# >>> anvil-managed: x\nbody\n# <<< anvil-managed: x\n");
    }

    #[test]
    fn upsert_into_empty_file() {
        let new = upsert_region("", "x", "body\n", SYN).unwrap();
        assert_eq!(new, "# >>> anvil-managed: x\nbody\n# <<< anvil-managed: x\n");
    }

    /// `At` exists for hosts whose region order is semantic: a region added in a
    /// later release has to land at its declared position, not at end-of-file.
    /// The offset the caller computes points at the end of the preceding
    /// region's sentinel, so the split must fall on the following line boundary.
    #[test]
    fn at_placement_inserts_after_the_line_containing_the_offset() {
        let host = "# syntax=docker/dockerfile:1\nFROM base\n";
        // Offset lands mid-way through line 1; the region must go *after* that
        // whole line, never inside it.
        let new = upsert_region_with_placement(host, "x", "body\n", SYN, RegionPlacement::At(5)).unwrap();
        assert_eq!(
            new,
            "# syntax=docker/dockerfile:1\n\n# >>> anvil-managed: x\nbody\n# <<< anvil-managed: x\n\nFROM base\n"
        );
    }

    #[test]
    fn at_placement_into_empty_text_adds_no_leading_blank() {
        let new = upsert_region_with_placement("", "x", "body\n", SYN, RegionPlacement::At(0)).unwrap();
        assert_eq!(new, "# >>> anvil-managed: x\nbody\n# <<< anvil-managed: x\n");
    }

    #[test]
    fn at_placement_does_not_double_the_separating_blank_line() {
        // Preceding content already ends with a blank line, so no second one.
        let host = "FROM base\n\nRUN later\n";
        let new = upsert_region_with_placement(host, "x", "body\n", SYN, RegionPlacement::At(10)).unwrap();
        assert_eq!(
            new,
            "FROM base\n\n# >>> anvil-managed: x\nbody\n# <<< anvil-managed: x\n\nRUN later\n"
        );
    }

    #[test]
    fn at_placement_separates_the_region_from_the_content_that_follows() {
        // The tail does not start with a newline, so one is inserted -- without
        // it the closing sentinel and the next instruction would share a line.
        let host = "FROM base\nRUN later\n";
        let new = upsert_region_with_placement(host, "x", "body\n", SYN, RegionPlacement::At(0)).unwrap();
        assert_eq!(
            new,
            "FROM base\n\n# >>> anvil-managed: x\nbody\n# <<< anvil-managed: x\n\nRUN later\n"
        );
    }

    #[test]
    fn at_placement_updates_an_existing_region_where_it_already_is() {
        // The offset only decides where an *absent* region lands. One already
        // present is replaced in place, so a stale offset cannot move it.
        let host = "FROM base\n# >>> anvil-managed: x\nold\n# <<< anvil-managed: x\nRUN later\n";
        let new = upsert_region_with_placement(host, "x", "new\n", SYN, RegionPlacement::At(0)).unwrap();
        assert_eq!(new, "FROM base\n# >>> anvil-managed: x\nnew\n# <<< anvil-managed: x\nRUN later\n");
    }

    #[test]
    fn start_placement_prepends_absent_region() {
        let new = upsert_region_with_placement(
            "[git]\nremote_branch = \"origin/main\"\n",
            "x",
            "trip_wire_patterns = []\n",
            SYN,
            RegionPlacement::Start,
        )
        .unwrap();
        assert_eq!(
            new,
            "# >>> anvil-managed: x\ntrip_wire_patterns = []\n# <<< anvil-managed: x\n\n[git]\nremote_branch = \"origin/main\"\n"
        );
        let _: toml_edit::DocumentMut = new.parse().expect("root key before table must be valid TOML");
    }

    #[test]
    fn start_placement_moves_existing_region_from_end() {
        let text = "[git]\nremote_branch = \"origin/main\"\n\n# >>> anvil-managed: x\nold = true\n# <<< anvil-managed: x\n";
        let new = upsert_region_with_placement(text, "x", "trip_wire_patterns = []\n", SYN, RegionPlacement::Start).unwrap();
        assert!(new.starts_with("# >>> anvil-managed: x\ntrip_wire_patterns = []\n"));
        assert_eq!(new.matches("# >>> anvil-managed: x").count(), 1);
        let _: toml_edit::DocumentMut = new.parse().expect("moved root key must be valid TOML");
    }

    #[test]
    fn upsert_empties_region() {
        let text = "# >>> anvil-managed: x\nfilled\n# <<< anvil-managed: x\n";
        let new = upsert_region(text, "x", "", SYN).unwrap();
        let region = find_region(&new, "x", SYN).unwrap().unwrap();
        assert!(region.is_empty());
    }

    #[test]
    fn render_region_with_empty_body() {
        let s = render_region("x", "", SYN);
        assert_eq!(s, "# >>> anvil-managed: x\n# <<< anvil-managed: x\n");
    }

    #[test]
    fn render_region_adds_trailing_newline() {
        let s = render_region("x", "body", SYN);
        assert_eq!(s, "# >>> anvil-managed: x\nbody\n# <<< anvil-managed: x\n");
    }

    #[test]
    fn remove_region_excises_markers_and_body() {
        // The dual of upsert_region: takes a host with a region and
        // returns the host without it. Adjacent blank lines on both
        // sides of the region are consumed so the spliced result
        // doesn't leave a visible gap where the region used to be.
        let text = "before\n\
                    \n\
                    # >>> anvil-managed: x\n\
                    body line 1\n\
                    body line 2\n\
                    # <<< anvil-managed: x\n\
                    \n\
                    after\n";
        let out = remove_region(text, "x", SYN).unwrap();
        assert_eq!(out, "before\n\nafter\n");
    }

    #[test]
    fn remove_region_absent_region_is_a_noop() {
        let text = "no region in sight\n";
        let out = remove_region(text, "x", SYN).unwrap();
        assert_eq!(out, text);
    }

    #[test]
    fn remove_region_at_eof_drops_trailing_blank() {
        let text = "before\n\n# >>> anvil-managed: x\nbody\n# <<< anvil-managed: x\n";
        let out = remove_region(text, "x", SYN).unwrap();
        assert_eq!(out, "before\n");
    }

    #[test]
    fn slash_slash_syntax_works() {
        let text = "// >>> anvil-managed: x\nbody\n// <<< anvil-managed: x\n";
        let region = find_region(text, "x", CommentSyntax::SlashSlash).unwrap().unwrap();
        assert_eq!(region.body_str(), "body\n");
    }

    #[test]
    fn hash_syntax_ignores_slash_slash_sentinels() {
        let text = "// >>> anvil-managed: x\nbody\n// <<< anvil-managed: x\n";
        assert_eq!(find_region(text, "x", SYN).unwrap(), None);
    }

    #[test]
    fn finds_multiple_distinct_regions() {
        let text = "\
            # >>> anvil-managed: a\n\
            body-a\n\
            # <<< anvil-managed: a\n\
            user content between\n\
            # >>> anvil-managed: b\n\
            body-b\n\
            # <<< anvil-managed: b\n";
        let a = find_region(text, "a", SYN).unwrap().unwrap();
        let b = find_region(text, "b", SYN).unwrap().unwrap();
        assert_eq!(a.body_str(), "body-a\n");
        assert_eq!(b.body_str(), "body-b\n");
    }

    #[test]
    fn region_sentinels_indented_still_recognized() {
        // Sentinels with leading whitespace should be recognized — useful
        // in YAML where indentation matters in the host file.
        let text = "  # >>> anvil-managed: x\nbody\n  # <<< anvil-managed: x\n";
        let region = find_region(text, "x", SYN).unwrap().unwrap();
        assert_eq!(region.body_str(), "body\n");
    }

    #[test]
    fn duplicate_closing_sentinel_errors() {
        let text = "\
            # >>> anvil-managed: x\n\
            body\n\
            # <<< anvil-managed: x\n\
            # <<< anvil-managed: x\n";
        let err = find_region(text, "x", SYN).unwrap_err();
        assert!(err.to_string().contains("duplicate closing sentinel"), "{err}");
    }

    #[test]
    fn upsert_appends_to_file_without_trailing_newline() {
        // Host text that is non-empty but does NOT end in a newline:
        // upsert must insert the missing newline before the blank-line
        // separator (covers the `!text.ends_with('\n')` append branch).
        let new = upsert_region("user file", "x", "body\n", SYN).unwrap();
        assert_eq!(new, "user file\n\n# >>> anvil-managed: x\nbody\n# <<< anvil-managed: x\n");
    }

    #[test]
    fn finds_region_when_closing_sentinel_lacks_trailing_newline() {
        // The closing sentinel is the last line and the file does not end
        // in a newline: exercises the LineIter end-of-text branch.
        let text = "# >>> anvil-managed: x\nbody\n# <<< anvil-managed: x";
        let region = find_region(text, "x", SYN).unwrap().unwrap();
        assert_eq!(region.body_str(), "body\n");
    }
}
