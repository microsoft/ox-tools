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
        // Snap to a line boundary only when the offset is not already on one.
        // Callers point at the start of the line the region should displace
        // (typically a preceding region's `end_line.end`); advancing
        // unconditionally would skip that line, landing the region after the
        // first line of the repository's gap content and splitting it. An
        // offset that does fall mid-line is rounded forward, because splitting
        // a line around the sentinels turns one valid instruction into two
        // invalid halves.
        //
        // Everything below indexes the bytes rather than the `str`, because a
        // caller's offset need not fall on a character boundary and slicing a
        // `str` at one panics. A `'\n'` byte never occurs inside a multi-byte
        // character, so rounding forward to just past one always lands on a
        // character boundary as well as a line boundary, and a mid-character
        // offset is mid-line by definition and therefore always rounded.
        let offset = offset.min(text.len());
        let bytes = text.as_bytes();
        let on_line_boundary = offset == 0 || bytes[offset - 1] == b'\n';
        let offset = if on_line_boundary {
            offset
        } else {
            bytes[offset..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(text.len(), |index| offset + index + 1)
        };
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

/// Drop an outside-region copy of a TOML table that the region body already
/// declares **identically**, so introducing the region adopts a hand-written
/// table instead of appending a duplicate that TOML will not parse.
///
/// A managed region body such as `[lints]\nworkspace = true` is a whole table,
/// and TOML rejects a duplicate table header outright — so appending it beside
/// an identical hand-written `[lints]` does not produce redundant text, it
/// produces a manifest that will not parse and takes the workspace with it.
///
/// Adoption is deliberately limited to a table whose keys the managed body
/// **already contains**. A hand-written table carrying anything extra is left
/// exactly where it is: dropping it would silently delete a user's
/// configuration, which is a worse failure than the duplicate this function
/// exists to prevent — a `deny.toml` whose `[advisories]` lists the repository's own
/// `ignore` entries is the case that matters, and it is covered by a fixture.
/// Comments and blank lines are ignored when comparing, since neither carries
/// configuration, and an array-of-tables (`[[bin]]`) is never adopted at all:
/// TOML lets those repeat, so a second one is not a duplicate and dropping it
/// would delete a genuine array element.
///
/// Text inside an existing managed region is never examined, so a region that
/// legitimately owns the same table elsewhere in the file is untouched, and a
/// host containing a multi-line string is left alone entirely — its content is
/// beyond what a line-oriented scanner can judge.
#[must_use]
pub fn adopt_unmanaged_toml_tables(text: &str, body: &str, syntax: CommentSyntax) -> String {
    // A multi-line string is content this line-oriented scanner cannot read.
    // Its quote state does not survive the line break, so a `#` inside one
    // looks like a comment and a bracketed line inside one looks like a table
    // header — either of which would corrupt the comparison and could delete
    // a table that genuinely differs. Rather than guess, decline adoption
    // outright: leaving a visible duplicate-table failure is the documented
    // preference over silently losing user configuration.
    if contains_multi_line_string(text) || contains_multi_line_string(body) {
        return text.to_owned();
    }

    let managed = toml_tables(body, syntax);
    if managed.is_empty() {
        return text.to_owned();
    }

    let open = syntax.prefix().to_owned() + " >>> anvil-managed:";
    let close = syntax.prefix().to_owned() + " <<< anvil-managed:";

    // Which unmanaged tables are safe to drop, decided up front so the rewrite
    // below is a single pass with no lookahead.
    let mut adoptable: Vec<&str> = Vec::new();
    for (header, keys) in toml_tables(text, syntax) {
        if !is_array_of_tables(header)
            && let Some(managed_keys) = managed.iter().find(|(name, _)| *name == header).map(|(_, keys)| keys)
            && keys.iter().all(|key| managed_keys.contains(key))
        {
            adoptable.push(header);
        }
    }
    if adoptable.is_empty() {
        return text.to_owned();
    }

    let mut out = String::with_capacity(text.len());
    let mut in_managed = false;
    // Set while skipping an adopted table's body; cleared by the next table
    // header or by a managed region's opener, so only that table is dropped
    // and what follows survives.
    let mut dropping = false;

    for line in iterate_lines(text) {
        let raw = &text[line.start..line.end];
        let trimmed = raw.trim();
        if trimmed.starts_with(&open) {
            in_managed = true;
            // An adopted table's body ends here: whatever a managed region
            // holds is the region's, and whatever follows its closer is the
            // user's. Leaving the skip set would swallow both.
            dropping = false;
        }

        if !in_managed {
            if let Some(header) = toml_table_header(trimmed) {
                dropping = adoptable.contains(&header);
            }
            if dropping {
                continue;
            }
        }

        out.push_str(raw);

        if trimmed.starts_with(&close) {
            in_managed = false;
        }
    }

    out
}

/// Collect each top-level TOML table outside any managed region, as its header
/// and the key lines beneath it. Blank lines and whole-line comments are
/// dropped and a trailing comment is stripped from every header and key line,
/// since a comment carries no configuration and must not defeat a comparison.
fn toml_tables(text: &str, syntax: CommentSyntax) -> Vec<(&str, Vec<&str>)> {
    let prefix = syntax.prefix();
    let open = prefix.to_owned() + " >>> anvil-managed:";
    let close = prefix.to_owned() + " <<< anvil-managed:";

    let mut tables: Vec<(&str, Vec<&str>)> = Vec::new();
    let mut in_managed = false;

    for line in iterate_lines(text) {
        let trimmed = text[line.start..line.end].trim();
        if trimmed.starts_with(&open) {
            in_managed = true;
            continue;
        }
        if trimmed.starts_with(&close) {
            in_managed = false;
            continue;
        }
        if in_managed || trimmed.is_empty() || trimmed.starts_with(prefix) {
            continue;
        }
        if let Some(header) = toml_table_header(trimmed) {
            tables.push((header, Vec::new()));
        } else if let Some((_, keys)) = tables.last_mut() {
            keys.push(strip_trailing_comment(trimmed));
        }
    }

    tables
}

/// Return a trimmed TOML table header, without its trailing comment.
///
/// This is a boundary test: an array-of-tables header (`[[bin]]`) counts, so
/// that the keys beneath it are not attributed to the table above it. Whether
/// such a header may be *adopted* is a separate question, decided by
/// [`is_array_of_tables`].
fn toml_table_header(line: &str) -> Option<&str> {
    let header = strip_trailing_comment(line);
    (header.starts_with('[') && header.ends_with(']')).then_some(header)
}

/// Whether a table header declares an array of tables (`[[bin]]`).
///
/// TOML allows these to repeat, so a second one is not a duplicate and there
/// is no parse failure for adoption to fix. Adopting one would let a later
/// array element be deleted as though it were a duplicate of the first.
fn is_array_of_tables(header: &str) -> bool {
    header.starts_with("[[")
}

/// Whether `text` contains a TOML multi-line string delimiter.
///
/// Both the basic (`"""`) and literal (`'''`) forms count. This is a coarse
/// test on purpose: it decides only whether the line-oriented scanner is out
/// of its depth, and being wrong in the cautious direction merely declines an
/// adoption that would otherwise have been safe.
fn contains_multi_line_string(text: &str) -> bool {
    text.contains("\"\"\"") || text.contains("'''")
}

/// Return `line` without a trailing TOML comment, if it has one.
///
/// A `#` inside a quoted string is data rather than a comment, so quoting is
/// tracked: truncating there would corrupt the value and could make two
/// genuinely different keys compare equal.
fn strip_trailing_comment(line: &str) -> &str {
    let mut quote = None;
    let mut escaped = false;
    let comment = line.char_indices().find_map(|(index, character)| match (quote, character) {
        (None, '#') => Some(index),
        (None, '\'' | '"') => {
            quote = Some(character);
            None
        }
        (Some('"'), '\\') if !escaped => {
            escaped = true;
            None
        }
        (Some(active), character) if character == active && !escaped => {
            quote = None;
            None
        }
        _ => {
            escaped = false;
            None
        }
    });
    line[..comment.unwrap_or(line.len())].trim_end()
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

    /// A multi-line string is content this line-oriented scanner cannot read:
    /// its lines are values, not keys, and the quote state does not survive
    /// the line break. Rather than guess at their meaning, adoption declines
    /// outright — leaving a visible duplicate-table failure is the documented
    /// preference over silently deleting user configuration.
    #[test]
    fn a_multi_line_string_declines_adoption_entirely() {
        let text = "[lints]\nworkspace = true\n\n[package]\ndescription = \"\"\"\nnote # not a comment\n\"\"\"\n";
        let adopted = adopt_unmanaged_toml_tables(text, "[lints]\nworkspace = true\n", SYN);

        assert_eq!(adopted, text, "nothing is adopted while a multi-line string is present:\n{adopted}");
    }

    /// The sharp edge of the same problem: a bracketed line *inside* a
    /// multi-line string is a value, not a table header. Reading it as one
    /// would start dropping in the middle of the user's string and mangle it.
    #[test]
    fn a_bracketed_line_inside_a_multi_line_string_is_not_a_table_header() {
        let text = "[package]\ndescription = \"\"\"\n[lints]\nworkspace = true\n\"\"\"\n";
        let adopted = adopt_unmanaged_toml_tables(text, "[lints]\nworkspace = true\n", SYN);

        assert_eq!(adopted, text, "the string's content is left intact:\n{adopted}");
    }

    /// A single-line literal string uses a different delimiter run and must
    /// not be mistaken for a multi-line one, or ordinary adoption would stop
    /// working wherever a quoted value appears.
    #[test]
    fn an_ordinary_quoted_value_still_permits_adoption() {
        let text = "[advisories]\nyanked = \"deny\"\n";
        let adopted = adopt_unmanaged_toml_tables(text, "[advisories]\nyanked = \"deny\"\n", SYN);

        assert_eq!(adopted, "", "the table is still adopted:\n{adopted}");
    }

    /// The quote tracking in `strip_trailing_comment` decides whether a `#` is
    /// a comment or data. Getting it wrong in either direction is harmful: a
    /// `#` treated as a comment truncates a value, which can make two
    /// genuinely different keys compare equal and adopt -- delete -- a table
    /// that differs; a comment treated as data leaves it attached and defeats
    /// adoption, leaving the duplicate table this module exists to remove.
    /// Each case below is a distinct piece of that state machine.
    #[test]
    fn strip_trailing_comment_tracks_quoting() {
        // A plain trailing comment goes, with its leading whitespace.
        assert_eq!(strip_trailing_comment("a = 1 # note"), "a = 1");
        // No comment at all: the line is returned whole.
        assert_eq!(strip_trailing_comment("a = 1"), "a = 1");
        // A `#` inside a quoted value is data, under either quote style.
        assert_eq!(strip_trailing_comment("a = \"x#y\""), "a = \"x#y\"");
        assert_eq!(strip_trailing_comment("a = 'x#y'"), "a = 'x#y'");
        // A quote closes, so a comment after a quoted value is still a comment.
        assert_eq!(strip_trailing_comment("a = \"x\" # note"), "a = \"x\"");
        // Only the matching quote character closes: an apostrophe inside a
        // double-quoted value must not end it and expose the `#`.
        assert_eq!(strip_trailing_comment("a = \"it's #1\""), "a = \"it's #1\"");
        // An escaped quote does not close the value either.
        assert_eq!(strip_trailing_comment("a = \"x\\\"#y\""), "a = \"x\\\"#y\"");
        // ...but an escaped backslash is not itself an escape, so the quote
        // that follows it does close, and the comment after it is a comment.
        assert_eq!(strip_trailing_comment("a = \"x\\\\\" # note"), "a = \"x\\\\\"");
    }

    /// The consequence of that tracking, at the level that matters: two values
    /// differing only inside a quoted `#` must not be judged equal. Were the
    /// `#` treated as a comment, both would truncate to the same prefix and
    /// the hand-written table would be dropped -- deleting a real setting.
    #[test]
    fn a_quoted_hash_keeps_two_differing_values_distinct() {
        let text = "[advisories]\nignore = [\"a#b\"]\n";
        let adopted = adopt_unmanaged_toml_tables(text, "[advisories]\nignore = [\"a#c\"]\n", SYN);

        assert_eq!(adopted, text, "the differing table is preserved:\n{adopted}");
    }

    /// A trailing comment on a key line carries no configuration, so it must
    /// not defeat the key comparison. If it did, the hand-written table would
    /// be judged un-adoptable and the duplicate header would be appended --
    /// which is precisely the unparseable manifest this module exists to
    /// prevent.
    #[test]
    fn a_key_line_with_a_trailing_comment_is_still_adoptable() {
        let text = "[lints]\nworkspace = true # our policy\n";
        let adopted = adopt_unmanaged_toml_tables(text, "[lints]\nworkspace = true\n", SYN);

        assert_eq!(adopted, "", "the hand-written table is adopted whole:\n{adopted}");
    }

    /// A `#` inside a quoted value is data, not a comment. Stripping it would
    /// truncate the value and could make two genuinely different keys compare
    /// equal, adopting -- and therefore deleting -- a table that differs.
    #[test]
    fn a_hash_inside_a_quoted_value_is_not_treated_as_a_comment() {
        let text = "[advisories]\nignore = [\"RUSTSEC-1#1\"]\n";
        let adopted = adopt_unmanaged_toml_tables(text, "[advisories]\nignore = [\"RUSTSEC-2#2\"]\n", SYN);

        assert_eq!(adopted, text, "differing quoted values are not adoptable:\n{adopted}");
    }

    /// TOML allows an array-of-tables header to repeat, so a second `[[bin]]`
    /// is not a duplicate and there is no parse failure to fix. Treating one
    /// as an ordinary table would let adoption delete a genuine array element.
    #[test]
    fn an_array_of_tables_is_never_adopted() {
        let text = "[[bin]]\nname = \"a\"\n\n[[bin]]\nname = \"b\"\n";
        let adopted = adopt_unmanaged_toml_tables(text, "[[bin]]\nname = \"a\"\n", SYN);

        assert_eq!(adopted, text, "every array element survives:\n{adopted}");
    }

    /// An array-of-tables header still marks a table boundary even though it
    /// can never be adopted. Were it not treated as one, the keys beneath it
    /// would be attributed to the table above, making that table look like it
    /// carried extra user keys and silently defeating adoption.
    #[test]
    fn an_array_of_tables_bounds_the_table_above_it() {
        let text = "[lints]\nworkspace = true\n\n[[bin]]\nname = \"a\"\n";
        let adopted = adopt_unmanaged_toml_tables(text, "[lints]\nworkspace = true\n", SYN);

        assert_eq!(
            adopted, "[[bin]]\nname = \"a\"\n",
            "[lints] is adopted, the array is not:\n{adopted}"
        );
    }

    /// An adopted table's body ends where a managed region begins. If the
    /// skip were allowed to survive that boundary it would swallow whatever
    /// follows the region's closing sentinel, up to the next table header --
    /// silently deleting user content that adoption never examined.
    #[test]
    fn adoption_stops_at_a_managed_region_that_follows_the_adopted_table() {
        let text = "[lints]\nworkspace = true\n\n\
                    # >>> anvil-managed: other\n\
                    other = true\n\
                    # <<< anvil-managed: other\n\
                    # a user comment\n";
        let adopted = adopt_unmanaged_toml_tables(text, "[lints]\nworkspace = true\n", SYN);

        assert!(
            adopted.contains("# a user comment"),
            "content after the region survives:\n{adopted}"
        );
    }

    #[test]
    fn adoption_preserves_an_existing_managed_region() {
        let text = "[lints]\nworkspace = true\n\n\
                    # >>> anvil-managed: existing\n\
                    [lints]\n\
                    workspace = true\n\
                    # <<< anvil-managed: existing\n";
        let adopted = adopt_unmanaged_toml_tables(text, "[lints]\nworkspace = true\n", SYN);

        assert!(adopted.starts_with("# >>> anvil-managed: existing"));
        assert!(adopted.contains("[lints]\nworkspace = true\n# <<< anvil-managed: existing"));
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
    fn at_placement_on_a_line_boundary_does_not_skip_the_following_line() {
        // The offset callers actually pass is a line start -- a preceding
        // region's `end_line.end`. Advancing past the next newline would put
        // the region after the first line of the gap and split it in two.
        let host = "# >>> anvil-managed: a\nbody\n# <<< anvil-managed: a\n# my gap line\nRUN later\n";
        let offset = host.find("# my gap line").unwrap();
        let new = upsert_region_with_placement(host, "x", "body\n", SYN, RegionPlacement::At(offset)).unwrap();
        assert_eq!(
            new,
            "# >>> anvil-managed: a\nbody\n# <<< anvil-managed: a\n\n# >>> anvil-managed: x\nbody\n# <<< anvil-managed: x\n\n# my gap line\nRUN later\n",
            "the region belongs above the gap content, not inside it"
        );
    }

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
    fn at_placement_inside_a_multi_byte_character_rounds_to_the_next_line() {
        // Nothing in the type says a caller's byte offset falls on a character
        // boundary, and splitting a `str` at one that does not panics. An
        // offset inside a character is mid-line by definition, so it rounds
        // forward to the next line just as any other mid-line offset does.
        let host = "# rünlevel note\nFROM base\n";
        let inside = host.find('ü').unwrap() + 1;
        assert!(!host.is_char_boundary(inside), "the fixture must place the offset mid-character");
        let new = upsert_region_with_placement(host, "x", "body\n", SYN, RegionPlacement::At(inside)).unwrap();
        assert_eq!(
            new,
            "# rünlevel note\n\n# >>> anvil-managed: x\nbody\n# <<< anvil-managed: x\n\nFROM base\n"
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
            "# >>> anvil-managed: x\nbody\n# <<< anvil-managed: x\n\nFROM base\nRUN later\n"
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
