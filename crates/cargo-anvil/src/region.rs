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
//! The user's content outside the sentinels is preserved byte-for-byte, with
//! a single exception: introducing a region into a TOML host removes a
//! hand-written table whose configuration the region body already covers,
//! because TOML rejects a duplicate table header outright. See
//! [`adopt_unmanaged_toml_tables`].
//! The `id` is globally unique within the catalog (e.g. `anvil-imports`,
//! `anvil-workspace-lints`).
//!
//! Empty body (just the sentinels with no content between them) is the
//! opt-out signal — see [`updates.md`](../../docs/design/updates.md).

use std::collections::BTreeMap;

use ohno::{AppError, app_err, bail};
use toml_edit::{Item, RawString, Table};

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

/// Insert `extra` directly after region `id`'s closing sentinel.
///
/// Used to re-emit the hand-written configuration that adoption kept (see
/// [`TomlAdoption::Adopted`]). The position matters: TOML attributes a key to
/// whichever table header precedes it, so text placed here belongs to the table
/// the region just opened, which is exactly the table it was written under.
///
/// # Errors
///
/// Returns an error if the region is missing or malformed.
pub fn insert_after_region(text: &str, id: &str, extra: &str, syntax: CommentSyntax) -> Result<String, AppError> {
    if extra.is_empty() {
        return Ok(text.to_owned());
    }
    let Some(region) = find_region(text, id, syntax)? else {
        return Err(app_err!("region '{id}' is missing from the host it was just spliced into"));
    };
    let at = region.end_line.end;

    let mut out = String::with_capacity(text.len() + extra.len() + 1);
    out.push_str(&text[..at]);
    if !text[..at].ends_with('\n') {
        out.push('\n');
    }
    out.push_str(extra);
    if !extra.ends_with('\n') {
        out.push('\n');
    }
    let rest = &text[at..];
    // The gap that followed the region is preserved, but a residue block that
    // already ends in a newline must not be run straight into the next line of
    // the file: that would attach the following header's comment to it.
    if !rest.is_empty() && !rest.starts_with('\n') {
        out.push('\n');
    }
    out.push_str(rest);
    Ok(out)
}

/// Adopt an outside-region copy of a TOML table whose configuration the region
/// body already covers, so introducing the region takes over a hand-written
/// table instead of appending a duplicate that TOML will not parse.
///
/// A managed region body such as `[lints]\nworkspace = true` is a whole table,
/// and TOML rejects a duplicate table header outright — so appending it beside
/// a hand-written `[lints]` does not produce redundant text, it produces a
/// manifest that will not parse and takes the workspace with it.
///
/// Each hand-written entry is classified against the managed table:
///
/// * declared by both, with the same value — **covered**, and dropped, since
///   the region re-emits it verbatim.
/// * declared only by hand — **residue**, which is kept: it is returned
///   separately so the caller can re-emit it after the region's closing
///   sentinel, where it continues the very table the region opens. That is
///   what lets a `deny.toml` whose `[advisories]` carries the repository's own
///   `ignore` list be adopted at all, rather than declining and leaving a
///   duplicate header behind.
/// * declared by both with **different** values — a [`TomlAdoption::Conflict`].
///   Keeping both would repeat one key inside one table, and dropping either
///   would lose configuration somebody chose, so this reports rather than
///   guesses.
///
/// Both sides are compared through the TOML parser rather than as source text,
/// so formatting that TOML itself ignores — the spacing in `workspace=true`,
/// the order two entries appear in — cannot defeat the comparison. Residue is
/// carried across as its original source slice, so a user's comments and
/// spacing survive byte-for-byte.
///
/// An array-of-tables (`[[bin]]`) is never adopted: TOML lets those repeat, so
/// a second one is not a duplicate and dropping it would delete a genuine array
/// element. Text inside an existing managed region is never examined, so a
/// region that legitimately owns the same table elsewhere in the file is
/// untouched.
///
/// A host that does not parse is returned untouched: a table this cannot read
/// is one it must not delete.
#[must_use]
pub fn adopt_unmanaged_toml_tables(text: &str, body: &str, syntax: CommentSyntax) -> TomlAdoption {
    let Some(managed) = headed_tables(body) else {
        return TomlAdoption::Unchanged;
    };
    if managed.is_empty() {
        return TomlAdoption::Unchanged;
    }

    // Parse the host with its managed regions blanked out. Two things fall out
    // of that. The region's own tables are invisible, so a region that
    // legitimately owns the same table elsewhere in the file is never a
    // candidate; and a host that already carries both a region copy and a
    // hand-written copy still parses, even though as written it is the very
    // duplicate-header file TOML rejects — which is exactly the file adoption
    // exists to repair. Masking preserves length, so every span still indexes
    // the original text.
    let masked = mask_managed_regions(text, syntax);
    let Some(candidates) = headed_tables(&masked) else {
        return TomlAdoption::Unchanged;
    };

    let protected = managed_region_ranges(text, syntax);
    // Every header in the document, in order, bounds the table above it: a
    // table's content runs until the next one starts. A managed region's
    // opening sentinel bounds it too, so an adopted table can never swallow the
    // sentinel of the region that follows it.
    let mut boundaries: Vec<usize> = candidates.iter().map(|table| table.header.start).collect();
    boundaries.extend(protected.iter().map(|range| range.start));
    boundaries.sort_unstable();

    let mut deletions: Vec<ByteRange> = Vec::new();
    let mut residue = String::new();

    for candidate in &candidates {
        if candidate.array_of_tables {
            continue;
        }
        let Some(managed_values) = managed
            .iter()
            .find(|table| !table.array_of_tables && table.path == candidate.path)
            .map(|table| &table.values)
        else {
            continue;
        };
        let end = boundary_after(&boundaries, candidate.header.start, text.len());

        let mut kept = String::new();
        for entry in &candidate.entries {
            match managed_values.get(&entry.path) {
                Some(managed_value) if *managed_value == entry.value => {}
                Some(managed_value) => {
                    return TomlAdoption::Conflict {
                        table: candidate.path.join("."),
                        key: entry.path.join("."),
                        managed: managed_value.clone(),
                        hand_written: entry.value.clone(),
                    };
                }
                None => kept.push_str(&text[entry.span.start..entry.span.end.min(end)]),
            }
        }

        deletions.push(ByteRange {
            start: candidate.header.start,
            end,
        });
        residue.push_str(&kept);
    }

    if deletions.is_empty() {
        return TomlAdoption::Unchanged;
    }

    // The output is the gaps between the deletions, copied in order. There is
    // no streaming state to get wrong: the ranges are non-overlapping by
    // construction, because each one ends where the next header or sentinel
    // begins.
    deletions.sort_unstable_by_key(|range| range.start);
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for range in &deletions {
        debug_assert!(range.start >= cursor, "adoption deletion ranges must not overlap");
        out.push_str(&text[cursor..range.start]);
        cursor = range.end;
    }
    out.push_str(&text[cursor..]);

    TomlAdoption::Adopted {
        text: out,
        residue: tidy_residue(&residue),
    }
}

/// Trim the blank lines that bounded the residue inside the table it came
/// from, leaving exactly one trailing newline when anything is left.
///
/// Only the edges are touched: a blank line the user put *between* two of their
/// own keys is theirs, and survives.
fn tidy_residue(residue: &str) -> String {
    let trimmed = trim_leading_blank_lines(residue).trim_end();
    if trimmed.is_empty() {
        String::new()
    } else {
        let mut out = String::with_capacity(trimmed.len() + 1);
        out.push_str(trimmed);
        out.push('\n');
        out
    }
}

/// What examining a TOML host for hand-written copies of a region's tables
/// found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TomlAdoption {
    /// The host declares none of the body's tables outside a managed region —
    /// or could not be parsed, and so must not be edited.
    Unchanged,
    /// `text` is the host with the adopted tables removed. `residue` is the
    /// hand-written configuration the managed body does not declare, to be
    /// re-emitted directly after the region's closing sentinel so that it stays
    /// inside the table the region opens.
    Adopted { text: String, residue: String },
    /// A hand-written entry and a managed entry declare the same key with
    /// different values. There is no output that keeps both — TOML forbids the
    /// repeated key — and no way to choose between them, so the caller must
    /// refuse rather than write.
    Conflict {
        /// Dotted path of the table both declare.
        table: String,
        /// Dotted path of the key they disagree on.
        key: String,
        /// The value the managed region body declares.
        managed: String,
        /// The value the host declares by hand.
        hand_written: String,
    },
}

/// One explicitly headed TOML table, with the byte range of its header and the
/// configuration it declares.
struct HeadedTable {
    path: Vec<String>,
    header: ByteRange,
    array_of_tables: bool,
    values: TableValues,
    entries: Vec<TableEntry>,
}

/// One key/value entry of a headed table, with the source slice that carries
/// it — including any comment lines attached above it and its trailing comment.
struct TableEntry {
    path: Vec<String>,
    value: String,
    span: ByteRange,
}

/// The configuration a TOML table declares, as canonical path/value pairs.
type TableValues = BTreeMap<Vec<String>, String>;

/// Every explicitly headed table in `text`, in document order.
///
/// Returns `None` when `text` is not valid TOML. Parsing the document rather
/// than scanning for lines that look like headers is what lets a host
/// containing a multi-line string be adopted: a bracketed line inside a `"""`
/// value is a value to the parser, and cannot be mistaken for a header.
///
/// The document is parsed immutably, because [`toml_edit::DocumentMut`]
/// discards the source spans this needs.
fn headed_tables(text: &str) -> Option<Vec<HeadedTable>> {
    let document = toml_edit::Document::parse(text).ok()?;
    let mut tables = Vec::new();
    collect_headed_tables(document.as_table(), &mut Vec::new(), text, &mut tables);
    tables.sort_by_key(|table| table.header.start);
    Some(tables)
}

/// Walk a table's children, recording every explicitly headed table and
/// recursing through the implicit ones a nested header creates.
fn collect_headed_tables(table: &Table, path: &mut Vec<String>, text: &str, out: &mut Vec<HeadedTable>) {
    for (key, item) in table {
        path.push(key.to_owned());
        match item {
            Item::Table(child) => {
                // An implicit table was never written as a header of its own —
                // `[a.b]` creates one for `a` — so it is not a candidate, but
                // its children still are.
                if !child.is_implicit()
                    && let Some(header) = child.span()
                {
                    out.push(HeadedTable {
                        path: path.clone(),
                        header: ByteRange {
                            start: header.start,
                            end: header.end,
                        },
                        array_of_tables: false,
                        values: table_values(child),
                        entries: table_entries(child, text),
                    });
                }
                collect_headed_tables(child, path, text, out);
            }
            Item::ArrayOfTables(array) => {
                for child in array {
                    if let Some(header) = child.span() {
                        out.push(HeadedTable {
                            path: path.clone(),
                            header: ByteRange {
                                start: header.start,
                                end: header.end,
                            },
                            array_of_tables: true,
                            values: TableValues::new(),
                            entries: Vec::new(),
                        });
                    }
                }
            }
            Item::Value(_) | Item::None => {}
        }
        path.pop();
    }
}

/// The configuration a table declares, as canonical path/value pairs.
///
/// Descends through dotted keys so `rust.unsafe_op_in_unsafe_fn` is one entry
/// rather than a nested table, and stops at a nested *headed* table, which is
/// a candidate in its own right rather than part of this one.
fn table_values(table: &Table) -> TableValues {
    let mut values = TableValues::new();
    collect_values(table, &mut Vec::new(), &mut values);
    values
}

fn collect_values(table: &Table, path: &mut Vec<String>, values: &mut TableValues) {
    for (key, item) in table {
        path.push(key.to_owned());
        match item {
            Item::Value(value) => {
                values.insert(path.clone(), value.to_string().trim().to_owned());
            }
            Item::Table(child) if child.is_dotted() => collect_values(child, path, values),
            _ => {}
        }
        path.pop();
    }
}

/// The top-level entries of a table, each with the source slice that carries
/// it.
///
/// An entry's slice runs from the start of its own leading trivia — the blank
/// lines and comments the parser attached to its key — to the start of the next
/// entry's, so relocating it carries its comments along and leaves nothing of
/// the next entry behind. The last entry runs to the end of the table, which
/// the caller clamps to the next header.
fn table_entries(table: &Table, text: &str) -> Vec<TableEntry> {
    let mut starts: Vec<(Vec<String>, String, usize)> = Vec::new();
    for (key, item) in table {
        // Iteration hands back the key as a `&str`, dropping the `Key` that
        // carries the decor and span this needs. Looking it straight back up is
        // infallible — the key came from this very table — and skipping an
        // entry that failed the lookup would silently drop the user's
        // configuration, which is the whole failure this module exists to stop.
        let (key, _) = table.get_key_value(key).expect("a key yielded by a table is present in it");
        let start = key
            .leaf_decor()
            .prefix()
            .and_then(RawString::span)
            .map_or_else(|| key.span().map_or(0, |span| span.start), |span| span.start);
        match item {
            Item::Value(value) => starts.push((vec![key.get().to_owned()], value.to_string().trim().to_owned(), start)),
            Item::Table(child) if child.is_dotted() => {
                let mut nested = TableValues::new();
                collect_values(child, &mut vec![key.get().to_owned()], &mut nested);
                for (path, value) in nested {
                    starts.push((path, value, start));
                }
            }
            _ => {}
        }
    }
    starts.sort_by_key(|(_, _, start)| *start);

    let mut entries = Vec::with_capacity(starts.len());
    for index in 0..starts.len() {
        let (path, value, start) = &starts[index];
        let end = starts.get(index + 1).map_or(text.len(), |(_, _, next)| *next);
        entries.push(TableEntry {
            path: path.clone(),
            value: value.clone(),
            span: ByteRange { start: *start, end },
        });
    }
    entries
}

/// The first boundary strictly after `start`, or `fallback` when none follows.
fn boundary_after(boundaries: &[usize], start: usize, fallback: usize) -> usize {
    boundaries.iter().copied().find(|boundary| *boundary > start).unwrap_or(fallback)
}

/// Replace every managed region's bytes with spaces, keeping the newlines and
/// therefore every byte offset in the file.
///
/// The masked copy is what the adoption parser reads. Blanking rather than
/// deleting is what keeps the spans it reports usable against the original
/// text.
fn mask_managed_regions(text: &str, syntax: CommentSyntax) -> String {
    mask_regions(text, &managed_region_ranges(text, syntax))
}

/// Blank every managed region except `keep`, so what remains is the region
/// under consideration plus the repository's own hand-written content.
///
/// This is the view a TOML validity check has to take. Two managed regions can
/// legitimately declare the same key while a migration is in flight — the old
/// combined region is removed in the same pass that writes the sections
/// replacing it — so judging the intermediate text as a whole would refuse a
/// migration that is about to become valid. What matters is whether the region
/// being introduced collides with text nothing is going to remove.
#[must_use]
pub fn mask_other_managed_regions(text: &str, syntax: CommentSyntax, keep: &str) -> String {
    let ranges: Vec<ByteRange> = managed_region_ranges_with_ids(text, syntax)
        .into_iter()
        .filter_map(|(id, range)| (id != keep).then_some(range))
        .collect();
    mask_regions(text, &ranges)
}

fn mask_regions(text: &str, ranges: &[ByteRange]) -> String {
    if ranges.is_empty() {
        return text.to_owned();
    }
    // Copied through as text rather than mutated as bytes, so the result is
    // valid UTF-8 by construction and no fallible conversion is needed. Every
    // masked byte becomes a one-byte space and the line breaks are kept, so the
    // copy has the same length as the original and every offset still lands on
    // the same character.
    let mut masked = String::with_capacity(text.len());
    let mut cursor = 0;
    for range in ranges {
        masked.push_str(&text[cursor..range.start]);
        for byte in text[range.start..range.end].bytes() {
            masked.push(if byte == b'\n' || byte == b'\r' { char::from(byte) } else { ' ' });
        }
        cursor = range.end;
    }
    masked.push_str(&text[cursor..]);
    masked
}

/// Byte ranges of the managed regions in `text`, from opening sentinel line to
/// closing sentinel line inclusive.
fn managed_region_ranges(text: &str, syntax: CommentSyntax) -> Vec<ByteRange> {
    managed_region_ranges_with_ids(text, syntax)
        .into_iter()
        .map(|(_, range)| range)
        .collect()
}

/// As [`managed_region_ranges`], paired with each region's id.
fn managed_region_ranges_with_ids(text: &str, syntax: CommentSyntax) -> Vec<(String, ByteRange)> {
    let open = syntax.prefix().to_owned() + " >>> anvil-managed:";
    let close = syntax.prefix().to_owned() + " <<< anvil-managed:";

    let mut ranges = Vec::new();
    let mut start = None;
    for line in iterate_lines(text) {
        let trimmed = text[line.start..line.end].trim();
        if let Some(id) = trimmed.strip_prefix(&open) {
            start = Some((id.trim().to_owned(), line.start));
        } else if trimmed.starts_with(&close)
            && let Some((id, open_at)) = start.take()
        {
            ranges.push((
                id,
                ByteRange {
                    start: open_at,
                    end: line.end,
                },
            ));
        }
    }
    // An unterminated region still shields everything below it: its body is the
    // region's, not the user's, and `find_region` rejects the file separately.
    if let Some((id, open_at)) = start {
        ranges.push((
            id,
            ByteRange {
                start: open_at,
                end: text.len(),
            },
        ));
    }
    ranges
}

/// Drop leading blank lines, so relocated residue does not carry the gap that
/// separated it from the header it used to sit under.
fn trim_leading_blank_lines(text: &str) -> &str {
    let mut rest = text;
    loop {
        let trimmed = rest.trim_start_matches([' ', '\t']);
        match trimmed.strip_prefix('\n').or_else(|| trimmed.strip_prefix("\r\n")) {
            Some(next) => rest = next,
            None => return rest,
        }
    }
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

    /// The host text adoption produces, for the cases that expect no residue.
    ///
    /// Asserting the residue is empty here rather than discarding it keeps
    /// these tests honest: a change that started keeping hand-written entries
    /// would otherwise pass unnoticed.
    fn adopted_text(text: &str, body: &str) -> String {
        match adopt_unmanaged_toml_tables(text, body, SYN) {
            TomlAdoption::Unchanged => text.to_owned(),
            TomlAdoption::Adopted { text, residue } => {
                assert_eq!(residue, "", "unexpected residue kept from the hand-written table");
                text
            }
            TomlAdoption::Conflict { table, key, .. } => panic!("unexpected conflict on `{key}` in `[{table}]`"),
        }
    }

    /// The host text and the residue adoption kept, for the cases that expect
    /// hand-written entries to survive.
    fn adopted_with_residue(text: &str, body: &str) -> (String, String) {
        match adopt_unmanaged_toml_tables(text, body, SYN) {
            TomlAdoption::Adopted { text, residue } => (text, residue),
            other => panic!("expected the table to be adopted, got {other:?}"),
        }
    }

    #[test]
    fn missing_region_returns_none() {
        assert_eq!(find_region("user content\n", "anvil-x", SYN).unwrap(), None);
    }

    /// The line-oriented scanner could not read a multi-line string — its quote
    /// state does not survive the line break — so adoption used to decline for
    /// the whole host whenever one appeared anywhere in it, disabling the
    /// feature rather than handling the case. The parser has no such trouble.
    #[test]
    fn a_multi_line_string_no_longer_defeats_adoption() {
        let text = "[lints]\nworkspace = true\n\n[package]\ndescription = \"\"\"\nnote # not a comment\n\"\"\"\n";
        let adopted = adopted_text(text, "[lints]\nworkspace = true\n");

        assert_eq!(
            adopted, "[package]\ndescription = \"\"\"\nnote # not a comment\n\"\"\"\n",
            "the adoptable table is taken and the string is left alone:\n{adopted}"
        );
    }

    /// A bracketed line *inside* a multi-line string is a value, not a table
    /// header. Reading it as one would start dropping in the middle of the
    /// user's string and remove part of a user-authored value.
    #[test]
    fn a_bracketed_line_inside_a_multi_line_string_is_not_a_table_header() {
        let text = "[package]\ndescription = \"\"\"\n[lints]\nworkspace = true\n\"\"\"\n";
        let adopted = adopted_text(text, "[lints]\nworkspace = true\n");

        assert_eq!(adopted, text, "the string's content is left intact:\n{adopted}");
    }

    /// A single-line basic string uses a different delimiter run and must
    /// not be mistaken for a multi-line one, or ordinary adoption would stop
    /// working wherever a quoted value appears.
    #[test]
    fn an_ordinary_quoted_value_still_permits_adoption() {
        let text = "[advisories]\nyanked = \"deny\"\n";
        let adopted = adopted_text(text, "[advisories]\nyanked = \"deny\"\n");

        assert_eq!(adopted, "", "the table is still adopted:\n{adopted}");
    }

    /// A value differing only inside a quoted `#` is a genuine disagreement,
    /// not a comment to be stripped. Treating the `#` as a comment would
    /// truncate both values to the same prefix and adopt — that is, delete —
    /// a table that declares something else.
    #[test]
    fn a_quoted_hash_keeps_two_differing_values_distinct() {
        let adoption = adopt_unmanaged_toml_tables("[advisories]\nignore = [\"a#b\"]\n", "[advisories]\nignore = [\"a#c\"]\n", SYN);

        assert_eq!(
            adoption,
            TomlAdoption::Conflict {
                table: "advisories".to_owned(),
                key: "ignore".to_owned(),
                managed: "[\"a#c\"]".to_owned(),
                hand_written: "[\"a#b\"]".to_owned(),
            },
            "the disagreement is reported rather than resolved"
        );
    }

    /// The defect behind issue #148: a hand-written table carrying
    /// configuration the managed body does not declare cannot simply be
    /// deleted, and appending the region beside it produces two identical
    /// headers, which TOML rejects. The hand-written entry is kept and handed
    /// back for the caller to re-emit after the region, inside the table the
    /// region opens.
    #[test]
    fn a_hand_written_entry_the_body_does_not_declare_is_kept_as_residue() {
        let text = "[advisories]\nignore = [\"RUSTSEC-9999-0001\"]\n";
        let (adopted, residue) = adopted_with_residue(text, "[advisories]\nyanked = \"deny\"\n");

        assert_eq!(adopted, "", "the hand-written header is adopted:\n{adopted}");
        assert_eq!(residue, "ignore = [\"RUSTSEC-9999-0001\"]\n", "the user's entry survives");
    }

    /// Residue is carried across as its original source slice, so the comments
    /// a user wrote to explain a setting travel with the setting. Rebuilding it
    /// from parsed values would silently discard the reasoning and leave a bare
    /// key behind.
    #[test]
    fn residue_keeps_the_comments_written_around_it() {
        let text = "[advisories]\nyanked = \"deny\"\n# waiting on upstream\nignore = [\"RUSTSEC-9999-0001\"] # ours\n";
        let (_, residue) = adopted_with_residue(text, "[advisories]\nyanked = \"deny\"\n");

        assert_eq!(
            residue, "# waiting on upstream\nignore = [\"RUSTSEC-9999-0001\"] # ours\n",
            "both the leading and the trailing comment travel with the entry"
        );
    }

    /// Residue stops at the table it came from. An entry belonging to a later
    /// table must not be dragged along, or it would silently change meaning:
    /// relocated under the region's header it becomes a setting of a different
    /// table entirely.
    #[test]
    fn residue_does_not_reach_past_the_adopted_table() {
        let text = "[advisories]\nignore = [\"X\"]\n\n[bans]\nmultiple-versions = \"warn\"\n";
        let (adopted, residue) = adopted_with_residue(text, "[advisories]\nyanked = \"deny\"\n");

        assert_eq!(residue, "ignore = [\"X\"]\n", "only the adopted table's entry is taken");
        assert_eq!(
            adopted, "[bans]\nmultiple-versions = \"warn\"\n",
            "the following table is left where it is:\n{adopted}"
        );
    }

    /// hand-written `workspace=true` declares exactly what the rendered
    /// `workspace = true` does, and TOML does not care about the spacing. A
    /// source-text comparison judged them different, declined adoption, and
    /// appended the duplicate header this module exists to remove.
    #[test]
    fn spacing_around_the_assignment_does_not_defeat_adoption() {
        let adopted = adopted_text("[lints]\nworkspace=true\n", "[lints]\nworkspace = true\n");

        assert_eq!(adopted, "", "the table is adopted despite the spacing:\n{adopted}");
    }

    /// Entry order is not configuration either. Two tables listing the same
    /// settings in a different order are the same table to TOML, so adoption
    /// must not turn the ordering into a duplicate header.
    #[test]
    fn entry_order_does_not_defeat_adoption() {
        let text = "[advisories]\nyanked = \"deny\"\nunmaintained = \"warn\"\n";
        let adopted = adopted_text(text, "[advisories]\nunmaintained = \"warn\"\nyanked = \"deny\"\n");

        assert_eq!(adopted, "", "the table is adopted despite the order:\n{adopted}");
    }

    /// `["a.b"]` is one table whose name contains a dot; `[a.b]` is table `b`
    /// nested in table `a`. Reducing a header to a joined string would
    /// make them compare equal and delete a table the body never declared,
    /// which is why the path is compared as segments.
    #[test]
    fn a_quoted_dotted_key_is_not_a_nested_path() {
        let text = "[\"a.b\"]\nx = 1\n";
        let adopted = adopted_text(text, "[a.b]\nx = 1\n");

        assert_eq!(adopted, text, "the differently-named table is preserved:\n{adopted}");
    }

    /// `[bin]` beside `[[bin]]` is not a file TOML accepts at all — the second
    /// header is a duplicate key — so the parser cannot read it and adoption
    /// declines. That is the safe answer: the array element is never deleted,
    /// which is what the exclusion exists to guarantee. The line scanner this
    /// replaced did read such a file, and had to carry the array-of-tables
    /// exclusion into the rewrite to avoid deleting the element.
    #[test]
    fn an_array_of_tables_survives_a_table_sharing_its_name() {
        let text = "[bin]\nname = \"x\"\n\n[[bin]]\nname = \"x\"\n";
        let adopted = adopted_text(text, "[bin]\nname = \"x\"\n");

        assert_eq!(adopted, text, "the array element survives:\n{adopted}");
    }

    /// A trailing comment on a key line carries no configuration, so it must
    /// not defeat the key comparison. If it did, the hand-written table would
    /// be judged un-adoptable and the duplicate header would be appended --
    /// which is precisely the unparseable manifest this module exists to
    /// prevent.
    #[test]
    fn a_key_line_with_a_trailing_comment_is_still_adoptable() {
        let text = "[lints]\nworkspace = true # our policy\n";
        let adopted = adopted_text(text, "[lints]\nworkspace = true\n");

        assert_eq!(adopted, "", "the hand-written table is adopted whole:\n{adopted}");
    }

    /// A `#` inside a quoted value is data, not a comment. Stripping it would
    /// truncate the value and could make two genuinely different keys compare
    /// equal, adopting -- and therefore deleting -- a table that differs.
    #[test]
    fn a_hash_inside_a_quoted_value_is_not_treated_as_a_comment() {
        let adoption = adopt_unmanaged_toml_tables(
            "[advisories]\nignore = [\"RUSTSEC-1#1\"]\n",
            "[advisories]\nignore = [\"RUSTSEC-2#2\"]\n",
            SYN,
        );

        assert!(
            matches!(adoption, TomlAdoption::Conflict { ref key, .. } if key == "ignore"),
            "differing quoted values are a conflict, not a match: {adoption:?}"
        );
    }

    /// TOML allows an array-of-tables header to repeat, so a second `[[bin]]`
    /// is not a duplicate and there is no parse failure to fix. Treating one
    /// as an ordinary table would let adoption delete a genuine array element.
    #[test]
    fn an_array_of_tables_is_never_adopted() {
        let text = "[[bin]]\nname = \"a\"\n\n[[bin]]\nname = \"b\"\n";
        let adopted = adopted_text(text, "[[bin]]\nname = \"a\"\n");

        assert_eq!(adopted, text, "every array element survives:\n{adopted}");
    }

    /// An array-of-tables header still marks a table boundary even though it
    /// can never be adopted. Were it not treated as one, the keys beneath it
    /// would be attributed to the table above, making that table look like it
    /// carried extra user keys and silently defeating adoption.
    #[test]
    fn an_array_of_tables_bounds_the_table_above_it() {
        let text = "[lints]\nworkspace = true\n\n[[bin]]\nname = \"a\"\n";
        let adopted = adopted_text(text, "[lints]\nworkspace = true\n");

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
        let adopted = adopted_text(text, "[lints]\nworkspace = true\n");

        assert!(
            adopted.contains("# a user comment"),
            "content after the region survives:\n{adopted}"
        );
    }

    /// A whole-line element of a multi-line array has the shape of a table
    /// header. Reading it as one splits the table it belongs to: the lines
    /// above it lose the rest of the array, no longer parse, and the table is
    /// dropped from the candidates -- leaving the duplicate this exists to
    /// remove.
    #[test]
    fn an_array_element_on_its_own_line_is_not_a_table_header() {
        let text = "[lints]\nworkspace = true\npairs = [\n  [1, 2]\n]\n";
        let adopted = adopted_text(text, text);

        assert_eq!(adopted, "", "the table is adopted whole:\n{adopted}");
    }

    #[test]
    fn adoption_preserves_an_existing_managed_region() {
        let text = "[lints]\nworkspace = true\n\n\
                    # >>> anvil-managed: existing\n\
                    [lints]\n\
                    workspace = true\n\
                    # <<< anvil-managed: existing\n";
        let adopted = adopted_text(text, "[lints]\nworkspace = true\n");

        assert!(adopted.starts_with("# >>> anvil-managed: existing"));
        assert!(adopted.contains("[lints]\nworkspace = true\n# <<< anvil-managed: existing"));
    }

    /// A table inside an existing managed region is the region's, not a
    /// candidate. Were its lines read as though they were the user's, the
    /// region's own copy would supply the coverage the hand-written table
    /// lacks, and the extra key beneath the hand-written header would be
    /// deleted as part of a duplicate it never was.
    #[test]
    fn a_table_inside_a_managed_region_does_not_make_an_unmanaged_one_adoptable() {
        let text = "[lints]\nworkspace = true\nrust.unsafe_code = \"forbid\"\n\n\
                    # >>> anvil-managed: existing\n\
                    [lints]\n\
                    workspace = true\n\
                    # <<< anvil-managed: existing\n";
        let (adopted, residue) = adopted_with_residue(text, "[lints]\nworkspace = true\n");

        assert_eq!(
            residue, "rust.unsafe_code = \"forbid\"\n",
            "the unmanaged-only key is kept, not deleted"
        );
        assert!(
            adopted.contains("# >>> anvil-managed: existing"),
            "the existing region survives:\n{adopted}"
        );
    }

    /// Two tables under one parent are different tables. Comparing only the
    /// first segment of a dotted header would make `[workspace.lints]` and
    /// `[workspace.package]` collide, and adoption would delete a table the
    /// managed body never declared.
    #[test]
    fn a_dotted_header_is_compared_past_its_first_segment() {
        let text = "[workspace.package]\nedition = \"2024\"\n";
        let adopted = adopted_text(text, "[workspace.lints]\nedition = \"2024\"\n");

        assert_eq!(adopted, text, "the differently-named table is preserved:\n{adopted}");
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

    /// Residue is inserted after the region the same pass spliced it in, so a
    /// region that is not there means the splice did not do what it reported.
    /// Reporting that is what keeps the user's configuration from being
    /// dropped in silence.
    #[test]
    fn residue_insertion_reports_a_region_the_splice_did_not_leave_behind() {
        let err = insert_after_region("user content\n", "x", "ignore = []\n", SYN).unwrap_err();

        assert!(err.to_string().contains("region 'x' is missing"), "the region is named:\n{err}");
    }

    /// A host whose last line is the closing sentinel, and a residue block
    /// written without one, both lack the newline the next line needs. Without
    /// them the residue would be appended to the sentinel and to whatever
    /// follows it, turning two lines into one.
    #[test]
    fn residue_insertion_supplies_the_newlines_the_host_and_the_residue_lack() {
        let text = "# >>> anvil-managed: x\nyanked = \"deny\"\n# <<< anvil-managed: x";
        let out = insert_after_region(text, "x", "ignore = []", SYN).unwrap();

        assert_eq!(
            out,
            "# >>> anvil-managed: x\nyanked = \"deny\"\n# <<< anvil-managed: x\nignore = []\n"
        );
    }

    /// Residue that already ends in a newline still needs a blank line before
    /// the content that followed the region. Run straight together, the next
    /// line's leading comment would read as part of the relocated entry.
    #[test]
    fn residue_insertion_separates_the_residue_from_what_followed_the_region() {
        let text = "# >>> anvil-managed: x\nyanked = \"deny\"\n# <<< anvil-managed: x\n[bans]\nmultiple-versions = \"warn\"\n";
        let out = insert_after_region(text, "x", "ignore = []\n", SYN).unwrap();

        assert_eq!(
            out,
            "# >>> anvil-managed: x\nyanked = \"deny\"\n# <<< anvil-managed: x\nignore = []\n\n[bans]\nmultiple-versions = \"warn\"\n"
        );
    }

    /// A region left unterminated still owns everything below it — that text is
    /// the region's, not the user's. Masking only as far as a closing sentinel
    /// that never arrives would expose the region's own tables to adoption as
    /// though a human had written them.
    #[test]
    fn an_unterminated_region_is_masked_to_the_end_of_the_file() {
        let text = "[advisories]\n# >>> anvil-managed: x\nyanked = \"deny\"\n";
        let masked = mask_managed_regions(text, SYN);

        assert_eq!(masked.len(), text.len(), "masking leaves every byte offset where it was");
        assert!(
            masked.starts_with("[advisories]\n"),
            "text above the region is untouched:\n{masked}"
        );
        assert_eq!(
            masked["[advisories]\n".len()..].trim(),
            "",
            "everything from the opening sentinel down is blanked:\n{masked}"
        );
    }

    /// An entry's source slice starts at its leading trivia, so it carries the
    /// blank lines that separated it from the header it used to sit under.
    /// Kept, that gap would push the relocated entry away from the region whose
    /// table now owns it.
    #[test]
    fn relocated_residue_loses_the_blank_lines_above_it() {
        assert_eq!(trim_leading_blank_lines("\n  \n\tignore = []\n"), "\tignore = []\n");
    }

    /// A dotted key is configuration like any other. Not descending into it
    /// would hide a genuine disagreement, and the hand-written value would be
    /// deleted in favour of the managed one instead of being reported.
    #[test]
    fn a_differing_dotted_key_is_reported_as_a_conflict() {
        let adoption = adopt_unmanaged_toml_tables(
            "[lints]\nrust.unsafe_code = \"forbid\"\n",
            "[lints]\nrust.unsafe_code = \"allow\"\n",
            SYN,
        );

        assert_eq!(
            adoption,
            TomlAdoption::Conflict {
                table: "lints".to_owned(),
                key: "rust.unsafe_code".to_owned(),
                managed: "\"allow\"".to_owned(),
                hand_written: "\"forbid\"".to_owned(),
            },
            "the dotted key is compared rather than skipped"
        );
    }

    /// `[workspace.package]` declares table `package`, not a key of
    /// `[workspace]`. Folding its values into the parent's would make the
    /// managed table look as though it already declared `package.edition`, and
    /// the hand-written copy of that key would be deleted instead of kept.
    #[test]
    fn a_nested_headed_table_is_not_part_of_the_table_that_declares_it() {
        let text = "[workspace]\nmembers = []\npackage.edition = \"2024\"\n";
        let body = "[workspace]\nmembers = []\n\n[workspace.package]\nedition = \"2024\"\n";
        let (adopted, residue) = adopted_with_residue(text, body);

        assert_eq!(residue, "package.edition = \"2024\"\n", "the hand-written dotted key is kept");
        assert_eq!(adopted, "", "the hand-written table is adopted:\n{adopted}");
    }

    /// The same distinction seen from the host: a nested headed table is not an
    /// entry of the table above it. Counted as one, it would be relocated out
    /// from under its own header and become a setting of a different table.
    #[test]
    fn a_nested_headed_table_is_not_an_entry_of_the_table_above_it() {
        let text = "[workspace]\nmembers = []\n\n[workspace.package]\nedition = \"2024\"\n";
        let adopted = adopted_text(text, "[workspace]\nmembers = []\n");

        assert_eq!(
            adopted, "[workspace.package]\nedition = \"2024\"\n",
            "the nested table stays where it was written:\n{adopted}"
        );
    }

    /// The refusal check masks every managed region except the one being
    /// introduced, which has to stay readable for the check to judge it. The
    /// blanking keeps every line break, so the parser reports the same spans
    /// against the copy as against the original.
    #[test]
    fn masking_keeps_the_named_region_and_every_line_break() {
        let text = "[advisories]\n\
                    # >>> anvil-managed: a\n\
                    yanked = \"deny\"\n\
                    # <<< anvil-managed: a\n\
                    # >>> anvil-managed: b\n\
                    unmaintained = \"warn\"\n\
                    # <<< anvil-managed: b\n";
        let masked = mask_other_managed_regions(text, SYN, "b");

        assert_eq!(masked.len(), text.len(), "every byte offset is where it was");
        assert_eq!(
            masked.matches('\n').count(),
            text.matches('\n').count(),
            "the line breaks survive the blanking:\n{masked}"
        );
        assert!(
            masked.contains("unmaintained = \"warn\""),
            "the region under consideration is left readable:\n{masked}"
        );
        assert!(!masked.contains("yanked = \"deny\""), "the other region is blanked:\n{masked}");
    }
}
