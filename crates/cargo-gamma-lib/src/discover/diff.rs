// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Restricting a run to the lines a unified diff touches.

#[cfg(test)]
use std::fs;
use std::fs::File;
use std::io::{Read, stdin};

use camino::{Utf8Component, Utf8Path, Utf8PathBuf};

use super::input;
use crate::error::error;
use crate::{HashMap, HashSet, Result};

/// The lines each file gained in a diff, as a set of line numbers in the new file.
#[derive(Debug, Default)]
pub struct Diff {
    touched: HashMap<Utf8PathBuf, Vec<u32>>,

    /// For each key, the path exactly as the diff spelled it.
    ///
    /// Kept because prefix stripping is a guess, and [`Diff::resolve`] needs the unaltered text to
    /// fall back on when the guess turns out not to name anything in the workspace.
    sources: HashMap<Utf8PathBuf, Utf8PathBuf>,
}

impl Diff {
    /// Reads a unified diff from a path, or from standard input when the path is `-`.
    ///
    /// # Errors
    ///
    /// Returns an error if the diff cannot be read.
    pub fn read(path: &Utf8Path) -> Result<Self> {
        Self::read_from(path, stdin())
    }

    /// Reads a unified diff, taking `-` from `input` rather than from the real standard input.
    ///
    /// The seam exists so that the `-` path is an ordinary test rather than something that would
    /// block on a terminal, which is what reading the process's real standard input would do inside
    /// a test binary.
    ///
    /// # Errors
    ///
    /// Returns an error when `path` is `-` and reading `input` fails or its bytes are not UTF-8, or
    /// when another path cannot be opened, read, or decoded as UTF-8. Inputs larger than
    /// [`input::MAX_BYTES`] are refused before they can exhaust the process's memory.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn read_from(path: &Utf8Path, input: impl Read) -> Result<Self> {
        Self::read_from_with(path, input, |path| File::open(path))
    }

    fn read_from_with<R: Read>(path: &Utf8Path, mut input: impl Read, open: impl FnOnce(&Utf8Path) -> std::io::Result<R>) -> Result<Self> {
        // #[gamma::skip(cond.always_true, tag = "timeout", reason = "routing file inputs through standard input blocked waiting for input in the campaign")]
        let text = if path == "-" {
            input::text(&mut input).map_err(|cause| error!("could not read a diff from standard input").caused_by(cause))?
        } else {
            let file = open(path).map_err(|cause| error!("could not read the diff `{path}`").caused_by(cause))?;

            input::text(file).map_err(|cause| error!("could not read the diff `{path}`").caused_by(cause))?
        };

        Self::from_retained(text)
    }

    fn from_retained(text: Option<String>) -> Result<Self> {
        let Some(text) = text else {
            return Err(error!("the diff is larger than the {} bytes cargo-gamma will retain", input::MAX_BYTES).usage());
        };

        Ok(Self::parse(&text))
    }

    /// Parses a unified diff.
    ///
    /// Only added and modified lines count. A deleted line has no position in the new file, so
    /// there is nothing there to mutate, and a context line is by definition unchanged.
    ///
    /// The prefix on each path is worked out from the diff itself rather than assumed to be git's
    /// default `b/`. `diff.mnemonicPrefix` writes `i/`, `w/`, `c/` or `o/`, `--dst-prefix` writes
    /// whatever it was given, and `diff.noprefix` writes none at all — and a prefix that is not
    /// recognized leaves every path naming a file that does not exist, which selects nothing and
    /// reads exactly like a change that touched no code.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut touched: HashMap<Utf8PathBuf, Vec<u32>> = HashMap::default();
        let mut sources: HashMap<Utf8PathBuf, Utf8PathBuf> = HashMap::default();
        let mut current: Option<(Utf8PathBuf, Utf8PathBuf)> = None;
        // #[gamma::skip(literal.int_increment, reason = "every hunk assigns its declared start before any body line can use this initial counter")]
        let mut line_number = 0_u32;

        // What the `diff --git` header said the post-image prefix is, and what the `---` line
        // spelled the pre-image as. Either is enough to recognize the prefix on the `+++` line.
        let mut dst_prefix: Option<String> = None;
        let mut pre_image: Option<String> = None;

        // The pre- and post-image lines the current hunk still expects, taken from its `@@` header.
        // While either is positive the parser is inside the hunk body, where a line that begins like
        // a `+++`/`---` file header is really added or removed source text — a raw `+++ x` is an
        // added `++ x`, a raw `--- y` a removed `-- y` — and must be counted as content rather than
        // consumed as metadata. Only once both are spent is a line read as a header again.
        let mut remaining_old = 0_u32;
        let mut remaining_new = 0_u32;

        for line in text.lines() {
            let marker = line.as_bytes().first().copied();
            let in_body = (remaining_old > 0 || remaining_new > 0) && matches!(marker, Some(b'+' | b'-' | b' ' | b'\\') | None);

            if in_body {
                match marker {
                    Some(b'+') => {
                        if let Some((path, _raw)) = current.as_ref() {
                            touched.entry(path.clone()).or_default().push(line_number);
                        }
                        line_number = line_number.saturating_add(1);
                        remaining_new = remaining_new.saturating_sub(1);
                    }

                    // A deleted line has no place in the post-image, so it advances no numbering; it
                    // does account for one of the pre-image lines the hunk promised.
                    Some(b'-') => remaining_old = remaining_old.saturating_sub(1),

                    // A context line sits in both images and is counted against each.
                    Some(b' ') | None => {
                        line_number = line_number.saturating_add(1);
                        remaining_old = remaining_old.saturating_sub(1);
                        remaining_new = remaining_new.saturating_sub(1);
                    }

                    // `\ No newline at end of file` belongs to neither image and counts for neither.
                    _ => {}
                }

                continue;
            }

            // Outside a hunk body — its declared counts are spent, or this is not a body line — so
            // any hunk in progress is finished and the line is read as metadata.
            remaining_old = 0;
            remaining_new = 0;

            if let Some(rest) = line.strip_prefix("diff --git ") {
                dst_prefix = git_prefix(rest);
                // #[gamma::skip(assign_value.default, reason = "None is exactly the default value of this Option")]
                pre_image = None;
            } else if let Some(rest) = line.strip_prefix("+++ ") {
                current = new_file_path(rest, dst_prefix.as_deref(), pre_image.as_deref());

                if let Some((path, raw)) = current.as_ref() {
                    let _replaced = sources.insert(path.clone(), raw.clone());
                }
            } else if let Some(rest) = line.strip_prefix("--- ") {
                pre_image = header_path(rest).map(ToOwned::to_owned);
            } else if let Some(rest) = line.strip_prefix("@@") {
                if let Some((start, old_count, new_count)) = hunk_header(rest) {
                    line_number = start;
                    remaining_old = old_count;
                    remaining_new = new_count;
                } else {
                    // #[gamma::skip(assign_value.default, reason = "None is exactly the default value of this Option")]
                    current = None;
                }
            }
        }

        sources.retain(|path, _raw| touched.contains_key(path));
        // #[gamma::skip(call_result.default, reason = "normalize_lines returns unit and this mutator still sorts and deduplicates the same lines before replacing unit with unit")]
        normalize_lines(&mut touched);

        Self { touched, sources }
    }

    /// Points every path the diff named at the file in the workspace it refers to.
    ///
    /// A diff is written from wherever the person who produced it happened to be standing, with
    /// whatever prefixes their configuration prefers, so a path in it is a name rather than a
    /// location. Each one is matched against the workspace: the path as spelled, then the path with
    /// leading directories peeled off, then the workspace file it uniquely ends with.
    ///
    /// # Errors
    ///
    /// Returns an error when the diff named a Rust source path that could not be matched, or when
    /// it named paths and not one of them could be matched. Neither is an empty change: an empty
    /// change names nothing. Both mean the diff was not understood, and continuing would run fewer
    /// mutants than the change deserves while reporting a score as though it had run them all.
    pub fn resolve(&mut self, root: &Utf8Path, candidates: &[Utf8PathBuf]) -> Result<()> {
        let named = core::mem::take(&mut self.touched);
        let sources = core::mem::take(&mut self.sources);
        let mut resolved: HashMap<Utf8PathBuf, Vec<u32>> = HashMap::default();
        let mut unresolved: Vec<Utf8PathBuf> = Vec::new();

        // Built once, because every exact and every peeled candidate is looked up in it and the
        // peeling makes that several lookups per path. Scanning the workspace file list instead
        // costs a full pass each time, on the path that is advertised as running on every pull
        // request — where time to first output is the whole point.
        let known: HashSet<&Utf8Path> = candidates.iter().map(Utf8PathBuf::as_path).collect();

        for (path, lines) in named {
            let raw = sources.get(&path).unwrap_or(&path).clone();

            if let Some(found) = locate(&path, &raw, root, &known, candidates) {
                resolved.entry(found).or_default().extend(lines);
            } else {
                unresolved.push(raw);
            }
        }

        unresolved.sort();

        // A path that names Rust source and did not resolve is the dangerous case. `Survey::for_build`
        // retains only the files the diff is believed to touch, so every mutant in an unresolved file
        // is silently never generated: the population shrinks, the score rises, and nothing says so.
        // This is checked before the everything-failed case below because one resolvable `README.md`
        // hunk is enough to make that check pass while a whole source file goes missing.
        //
        // Paths that are not Rust source contribute no mutants whatever happens to them, so an
        // unresolved `README.md` is genuinely nothing to report.
        let rust: Vec<&str> = unresolved
            .iter()
            .filter(|path| path.extension() == Some("rs"))
            .map(|path| path.as_str())
            .collect();

        if !rust.is_empty() {
            let listed = rust.join(", ");

            return Err(error!(
                "the diff names {} of Rust source that this workspace does not contain ({listed}); \
                 continuing would silently generate no mutants for them and report a score for a \
                 population that never included them",
                crate::report::quantity(rust.len(), "path")
            )
            .usage());
        }

        if resolved.is_empty() && !unresolved.is_empty() {
            let listed = unresolved.iter().map(|path| path.as_str()).collect::<Vec<&str>>().join(", ");

            return Err(error!(
                "the diff names {} that this workspace does not contain ({listed}); \
                 it was produced somewhere else, or with path prefixes this workspace cannot resolve",
                crate::report::quantity(unresolved.len(), "path")
            )
            .usage());
        }

        normalize_lines(&mut resolved);
        self.touched = resolved;

        Ok(())
    }

    /// Returns whether a file has any changed line.
    #[must_use]
    pub fn touches_file(&self, path: &Utf8Path) -> bool {
        self.touched.contains_key(path)
    }

    /// Returns whether a region of a file overlaps anything the diff changed.
    ///
    /// A mutation site is matched by its whole extent rather than by its first line, so editing
    /// the middle of a multi-line condition still selects the mutants on it.
    #[must_use]
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn touches(&self, path: &Utf8Path, start: u32, end: u32) -> bool {
        // #[gamma::skip(cond.always_false, reason = "when start exceeds end the partition-point candidate is at least start and therefore cannot satisfy the subsequent end comparison")]
        if start > end {
            return false;
        }

        self.touched.get(path).is_some_and(|lines| {
            let first = lines.partition_point(|line| *line < start);

            lines.get(first).is_some_and(|line| *line <= end)
        })
    }
}

fn normalize_lines(touched: &mut HashMap<Utf8PathBuf, Vec<u32>>) {
    for lines in touched.values_mut() {
        lines.sort_unstable();
        lines.dedup();
    }
}

/// Extracts the path from a `---` or `+++` header, rejecting the one that means "no such file".
fn header_path(rest: &str) -> Option<&str> {
    // Trailing tab-separated metadata is part of the format, and git writes a timestamp there.
    let path = rest.split('\t').next().unwrap_or(rest).trim();

    if path.is_empty() || path == "/dev/null" { None } else { Some(path) }
}

/// Extracts the path from a `+++` header, paired with the text the diff actually wrote.
///
/// `dst_prefix` is what the `diff --git` header revealed, and `pre_image` is the `---` path this
/// one is paired with. Either identifies the prefix; failing both, git's default `b/` is stripped
/// when it is there, which is what a `diff -u` with no prefixes at all needs left alone.
#[cfg_attr(coverage_nightly, coverage(off))]
fn new_file_path(rest: &str, dst_prefix: Option<&str>, pre_image: Option<&str>) -> Option<(Utf8PathBuf, Utf8PathBuf)> {
    let raw = header_path(rest)?;

    // The header is the best evidence, the `---` line it is paired with is the next best, and
    // git's own default is what is left when a diff carries neither.
    let prefix = match (dst_prefix, pre_image) {
        (Some(prefix), _pre_image) => prefix,
        (None, Some(source)) => derived_prefix(source, raw).unwrap_or("b/"),
        (None, None) => "b/",
    };
    let stripped = raw.strip_prefix(prefix).unwrap_or(raw);

    if stripped.is_empty() {
        return None;
    }

    Some((Utf8PathBuf::from(stripped), Utf8PathBuf::from(raw)))
}

/// Works out the post-image prefix from a `diff --git <src> <dst>` header.
///
/// The two paths differ only in their prefix whenever the file was not renamed, which is what makes
/// the header self-describing: `diff --git i/x.rs w/x.rs` says the post-image prefix is `w/` as
/// plainly as `diff --git a/x.rs b/x.rs` says it is `b/`, and `diff --git x.rs x.rs` says there is
/// none. Returns `None` for a rename, where the two paths carry no common suffix to compare.
#[cfg_attr(coverage_nightly, coverage(off))]
fn git_prefix(rest: &str) -> Option<String> {
    // A path may contain spaces, so the split is found by trying each one and keeping the split
    // whose halves agree once their first segment is removed, rather than by taking two tokens.
    for (index, _matched) in rest.match_indices(' ') {
        // #[gamma::skip(try.propagate_to_unwrap, reason = "match_indices yields an in-bounds UTF-8 boundary for the ASCII space, so this prefix slice always exists")]
        // #[gamma::skip(range.exclusive_to_inclusive, reason = "the extra byte is the matched ASCII space, which trim immediately removes from the prefix")]
        let left = rest.get(..index)?.trim();
        // #[gamma::skip(try.propagate_to_unwrap, reason = "the ASCII space occupies one byte within rest, so the byte immediately after it is an in-bounds UTF-8 boundary")]
        // #[gamma::skip(literal.int_decrement, reason = "starting the suffix at the matched ASCII space instead of after it is identical after trim removes that leading space")]
        let right = rest.get(index.saturating_add(1)..)?.trim();

        // #[gamma::skip(logical.or_remove_left, logical.or_remove_right, logical.or_to_and, reason = "one empty side cannot yield a derived prefix because it has no first segment; all three mutations still skip when both sides are empty")]
        if left.is_empty() || right.is_empty() {
            continue;
        }

        if let Some(prefix) = derived_prefix(left, right) {
            return Some(prefix.to_owned());
        }
    }

    None
}

/// The prefix on `dst`, given a `src` that names the same file with a prefix of its own.
///
/// An empty prefix is a real answer: `diff.noprefix` writes the path twice, unadorned.
fn derived_prefix<'a>(src: &str, dst: &'a str) -> Option<&'a str> {
    if src == dst {
        return Some("");
    }

    let (_src_prefix, src_rest) = split_first_segment(src)?;
    let (dst_prefix, dst_rest) = split_first_segment(dst)?;

    (src_rest == dst_rest && !dst_rest.is_empty()).then_some(dst_prefix)
}

/// Splits a path into its first segment, separator included, and the rest.
fn split_first_segment(path: &str) -> Option<(&str, &str)> {
    let slash = path.find('/')?;
    // #[gamma::skip(try.propagate_to_unwrap, reason = "a slash found inside a string has an index smaller than its length, so adding one cannot overflow usize")]
    let split = slash.checked_add(1)?;

    // #[gamma::skip(try.propagate_to_unwrap, reason = "the split is immediately after an in-bounds ASCII slash, so both slices use a valid in-bounds UTF-8 boundary")]
    Some((path.get(..split)?, path.get(split..)?))
}

/// Finds the workspace file a diff path refers to, if any.
#[cfg_attr(coverage_nightly, coverage(off))]
fn locate(path: &Utf8Path, raw: &Utf8Path, root: &Utf8Path, known: &HashSet<&Utf8Path>, candidates: &[Utf8PathBuf]) -> Option<Utf8PathBuf> {
    locate_with(path, raw, root, known, candidates, Utf8Path::is_file)
}

fn locate_with(
    path: &Utf8Path,
    raw: &Utf8Path,
    root: &Utf8Path,
    known: &HashSet<&Utf8Path>,
    candidates: &[Utf8PathBuf],
    mut is_file: impl FnMut(&Utf8Path) -> bool,
) -> Option<Utf8PathBuf> {
    for candidate in [path, raw] {
        if candidate.is_absolute() {
            if let Ok(relative) = candidate.strip_prefix(root) {
                if known.contains(relative) {
                    return Some(relative.to_owned());
                }

                if safe_relative(relative) && relative.extension() != Some("rs") && is_file(&root.join(relative)) {
                    return Some(relative.to_owned());
                }
            }

            // #[gamma::skip(loop.delete_continue, reason = "an absolute candidate is never safe_relative, so the following guard independently continues the same loop")]
            continue;
        }

        if !safe_relative(candidate) {
            continue;
        }

        if known.contains(candidate) {
            return Some(candidate.to_owned());
        }

        // A diff produced from a subdirectory, or with a prefix this code did not recognize, names
        // the file with extra leading directories. Peeling them one at a time finds the file
        // without having to know which of the two it was.
        let mut rest = candidate.as_str();

        while let Some((_first, tail)) = split_first_segment(rest) {
            // #[gamma::skip(stmt.delete_assign, tag = "timeout", reason = "the suffix scan must consume a path segment on every iteration")]
            rest = tail;

            if rest.is_empty() {
                break;
            }

            if known.contains(Utf8Path::new(rest)) {
                return Some(Utf8PathBuf::from(rest));
            }
        }
    }

    // Failing all of that, a workspace file the path uniquely ends with is the file meant. Only a
    // single match counts: guessing between two would attribute a change to the wrong file.
    //
    // This one stays a scan, and has to: a suffix match is not a lookup, and there is no key to
    // hash. It is reached only when every exact spelling has already failed.
    let mut matched = candidates.iter().filter(|file| {
        // #[gamma::skip(collection.reverse_array, reason = "any over these two pure path predicates is independent of evaluation order")]
        [path, raw]
            .into_iter()
            .any(|candidate| (candidate.has_root() || safe_relative(candidate)) && ends_with_path(candidate, file))
    });

    match (matched.next(), matched.next()) {
        (Some(found), None) => Some(found.clone()),
        (Some(_ambiguous), Some(_)) => None,
        (None, _) => [path, raw]
            .into_iter()
            .find(|candidate| safe_relative(candidate) && candidate.extension() != Some("rs") && is_file(&root.join(candidate)))
            .map(ToOwned::to_owned),
    }
}

/// Whether a path can be interpreted relative to the workspace without escaping it.
fn safe_relative(path: &Utf8Path) -> bool {
    path.components()
        .all(|component| matches!(component, Utf8Component::CurDir | Utf8Component::Normal(_)))
}

/// Whether `path` ends with `suffix` at a segment boundary.
#[cfg_attr(coverage_nightly, coverage(off))]
fn ends_with_path(path: &Utf8Path, suffix: &Utf8Path) -> bool {
    let (path, suffix) = (path.as_str(), suffix.as_str());

    if path == suffix {
        return true;
    }

    path.strip_suffix(suffix).is_some_and(|head| head.ends_with('/'))
}

/// Reads a hunk's `@@ -a,b +c,d @@` header into `(new_start, old_count, new_count)`.
///
/// The counts drive the parser's body/metadata decision, so an unreadable header — one missing
/// either range or its leading digits — yields `None` and abandons the hunk rather than guessing.
/// A range with no `,count` covers a single line, per the unified-diff format.
fn hunk_header(rest: &str) -> Option<(u32, u32, u32)> {
    let mut fields = rest.trim_start().split(' ');
    let old = fields.next()?.strip_prefix('-')?;
    let new = fields.next()?.strip_prefix('+')?;

    let (_old_start, old_count) = parse_range(old)?;
    let (new_start, new_count) = parse_range(new)?;

    Some((new_start, old_count, new_count))
}

/// Splits a `start` or `start,count` range into its two numbers, defaulting a missing count to 1.
fn parse_range(field: &str) -> Option<(u32, u32)> {
    let (start, count) = field.split_once(',').unwrap_or((field, "1"));

    Some((start.parse().ok()?, count.parse().ok()?))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod survivor_tests {
    use super::*;

    #[test]
    fn survivor_malformed_hunk_ranges_are_ignored_without_panicking() {
        for input in ["", "-", "-1", "-1 x2", "-x +2", "-1 +x", "-1,wat +2", "-1 +2,wat", "-1  +2"] {
            assert_eq!(hunk_header(input), None, "{input:?}");
        }
        assert_eq!(hunk_header("-0,0 +12,2 @@ heading"), Some((12, 0, 2)));
        assert_eq!(hunk_header("-1 +9 @@"), Some((9, 1, 1)));
        for input in ["", "x", "1,x", "x,1", "4294967296", "1,4294967296"] {
            assert_eq!(parse_range(input), None, "{input:?}");
        }
    }

    #[test]
    fn survivor_raw_path_fallback_is_checked_after_an_unsafe_stripped_candidate() {
        let candidates = [Utf8PathBuf::from("src/lib.rs")];
        let known = candidates.iter().map(Utf8PathBuf::as_path).collect();
        assert_eq!(
            locate(
                Utf8Path::new("../unsafe.rs"),
                Utf8Path::new("prefix/src/lib.rs"),
                Utf8Path::new("."),
                &known,
                &candidates
            ),
            Some(Utf8PathBuf::from("src/lib.rs"))
        );
        let mut diff = Diff::parse(
            "--- missing_a.txt\n+++ missing_a.txt\n@@ -1 +1 @@\n+new\n--- missing_b.txt\n+++ missing_b.txt\n@@ -1 +1 @@\n+new\n",
        );
        let failure = diff.resolve(Utf8Path::new("."), &[]).unwrap_err();
        assert!(failure.to_string().contains("missing_a.txt, missing_b.txt"));
    }
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod fuzz {
    use camino::Utf8Path;

    use super::Diff;
    use crate::testing::token;

    /// Arbitrary text is parsed without panicking, and never claims a line that is not a line.
    ///
    /// A diff arrives from whatever tooling the user has: a mail client that rewrapped it, a
    /// review tool with its own header, or a file that is not a diff at all. Line numbers are
    /// accumulated with `u32` arithmetic driven entirely by the input, so a hunk header claiming a
    /// start near the maximum is a real input rather than a hypothetical one.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn arbitrary_text_is_parsed_without_panicking() {
        bolero::check!().with_type::<String>().for_each(|text| {
            let diff = Diff::parse(text);

            for (path, lines) in &diff.touched {
                assert!(diff.touches_file(path), "a touched file is not reported as touched: {path}");
                assert!(!lines.is_empty(), "a file was recorded with no changed line: {path}");
            }
        });
    }

    /// An added line is still found when the patch is surrounded by text that is not a diff.
    ///
    /// Prose above and below a patch is the normal case, not the strange one: it is what an email,
    /// a pull-request body and a `git format-patch` cover letter all look like. Losing the hunk to
    /// it would silently narrow a `--in-diff` run to nothing and report a perfect score for a
    /// population nobody looked at.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn an_added_line_survives_surrounding_prose() {
        bolero::check!()
            .with_type::<(Vec<String>, String, u16)>()
            .for_each(|(noise, name, start)| {
                let name = token(name);
                let start = u32::from(*start).max(1);

                // Nothing generated may look like a header, or it would open a hunk of its own and
                // the assertion below would be about a different file than the one it names.
                if noise.iter().any(|line| {
                    line.lines()
                        .any(|line| line.starts_with(['+', '-', ' ']) || line.starts_with("@@") || line.starts_with("diff --git"))
                }) {
                    return;
                }

                let patch = format!("diff --git a/{name}.rs b/{name}.rs\n--- a/{name}.rs\n+++ b/{name}.rs\n@@ -1 +{start} @@\n+let x = 1;");
                let text = format!("{}\n{patch}\n{}", noise.join("\n"), noise.join("\n"));

                let diff = Diff::parse(&text);
                let touched = Utf8Path::new(name.as_str()).with_extension("rs");

                assert!(diff.touches(&touched, start, start), "the added line was lost in {text:?}");
            });
    }
}

#[cfg(all(test, not(miri)))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::io;

    use super::*;

    #[test]
    fn header_pair_without_git_metadata_derives_nonstandard_prefix_from_preimage() {
        let diff = Diff::parse("--- old/source.rs\n+++ new/source.rs\n@@ -1 +1 @@\n+changed\n");
        assert!(diff.touches(Utf8Path::new("source.rs"), 1, 1));
        assert!(!diff.touches_file(Utf8Path::new("new/source.rs")));
    }

    #[test]
    fn locate_non_source_matrix_requires_safe_existing_files_and_prefers_normalized_path() {
        let directory = crate::testing::workdir("diff-nonsource-matrix-");
        let base = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("fixture is UTF-8");
        let root = base.join("workspace");
        std::fs::create_dir(&root).expect("workspace directory");
        for name in ["first.toml", "second.toml", "unselected.rs"] {
            std::fs::write(root.join(name), "").expect("fixture file");
        }
        std::fs::write(base.join("outside.toml"), "").expect("external file remains inside fixture");
        std::fs::create_dir(root.join("directory")).expect("nonfile fixture");
        let known = HashSet::default();
        for (path, expected) in [
            (root.join("first.toml"), Some(Utf8PathBuf::from("first.toml"))),
            (root.join("missing.toml"), None),
            (root.join("directory"), None),
            (root.join("unselected.rs"), None),
            (Utf8PathBuf::from("../outside.toml"), None),
            (Utf8PathBuf::from("first.toml"), Some(Utf8PathBuf::from("first.toml"))),
            (Utf8PathBuf::from("missing.toml"), None),
            (Utf8PathBuf::from("directory"), None),
            (Utf8PathBuf::from("unselected.rs"), None),
        ] {
            assert_eq!(locate(&path, &path, &root, &known, &[]), expected, "{path}");
        }
        assert_eq!(
            locate(Utf8Path::new("first.toml"), Utf8Path::new("second.toml"), &root, &known, &[]),
            Some(Utf8PathBuf::from("first.toml"))
        );
        let selected = [Utf8PathBuf::from("selected.rs")];
        let known = selected.iter().map(Utf8PathBuf::as_path).collect();
        assert_eq!(
            locate(Utf8Path::new("prefix/"), Utf8Path::new("selected.rs"), &root, &known, &selected),
            Some(Utf8PathBuf::from("selected.rs"))
        );
        let ambiguous = [Utf8PathBuf::from("src/a.rs"), Utf8PathBuf::from("a.rs")];
        let known = ambiguous.iter().map(Utf8PathBuf::as_path).collect();
        assert_eq!(
            locate(&root.join("missing.rs"), Utf8Path::new("src/a.rs"), &root, &known, &ambiguous),
            Some(Utf8PathBuf::from("src/a.rs")),
            "an unresolved absolute normalized path must not stop the exact raw-path lookup before ambiguous suffix fallback"
        );
    }

    const SAMPLE: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1234567..89abcde 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -10,6 +10,8 @@ fn existing() {
 context one
 context two
+added at twelve
+added at thirteen
 context three
-removed line
 context four
";

    #[test]
    fn diff_header_helpers_preserve_empty_paths_prefixes_and_segment_boundaries() {
        for (text, expected) in [
            ("", None),
            (" \tstamp", None),
            ("/dev/null", None),
            ("src/a.rs\tstamp", Some("src/a.rs")),
        ] {
            assert_eq!(header_path(text), expected, "{text:?}");
        }
        for (source, destination, expected) in [
            ("a/a.rs", "b/a.rs", Some("b/")),
            ("a/", "b/", None),
            ("a/a.rs", "a.rs", None),
            ("a.rs", "a.rs", Some("")),
        ] {
            assert_eq!(derived_prefix(source, destination), expected, "{source:?} -> {destination:?}");
        }
        for (path, suffix, expected) in [
            ("a.rs", "a.rs", true),
            ("src/a.rs", "a.rs", true),
            ("othera.rs", "a.rs", false),
            ("src/b.rs", "a.rs", false),
        ] {
            assert_eq!(ends_with_path(Utf8Path::new(path), Utf8Path::new(suffix)), expected);
        }
        assert_eq!(git_prefix(" "), None);
        assert_eq!(git_prefix(" a/file.rs b/file.rs "), Some("b/".to_owned()));
        assert_eq!(new_file_path("b/", None, None), None);
        assert_eq!(
            new_file_path("after/a.rs", None, Some("before/a.rs")),
            Some((Utf8PathBuf::from("a.rs"), Utf8PathBuf::from("after/a.rs")))
        );
    }

    #[test]
    fn parsed_hunk_lines_are_sorted_deduplicated_and_not_confused_with_headers() {
        let patch = "\
--- old/src/a.rs
+++ new/src/a.rs
@@ -8,4 +20,4 @@
-gone
--- deleted source
 keep
+added
 keep
+added again
@@ -0,0 +5,1 @@
+early
@@ -0,0 +5,1 @@
+same line again
@@ -0,0 +9,1 @@
++ source beginning with two pluses
+++ b/untouched.rs
@@ -1 +1 @@
 keep
";
        let diff = Diff::parse(patch);
        assert_eq!(diff.touched[Utf8Path::new("src/a.rs")], [5, 9, 21, 23]);
        assert_eq!(diff.sources[Utf8Path::new("src/a.rs")], Utf8Path::new("new/src/a.rs"));
        assert_eq!(diff.touched.len(), 1);
        assert_eq!(diff.sources.len(), 1);
        for (start, end, expected) in [
            (4, 4, false),
            (5, 5, true),
            (6, 8, false),
            (9, 21, true),
            (24, 25, false),
            (21, 20, false),
        ] {
            assert_eq!(diff.touches(Utf8Path::new("src/a.rs"), start, end), expected, "{start}..={end}");
        }
    }

    #[test]
    fn malformed_hunks_do_not_reuse_the_previous_file_and_file_headers_reset_prefixes() {
        let diff = Diff::parse(
            "\
diff --git before/a.rs after/a.rs
--- before/a.rs
+++ after/a.rs
@@ -0,0 +1,1 @@
+one
@@ invalid @@
+not a valid hunk
@@ -0,0 +2,1 @@
+orphan after malformed header
diff --git before/old.rs next/a.rs
+++ next/a.rs
@@ -0,0 +3,1 @@
+two
",
        );
        assert_eq!(diff.touched[Utf8Path::new("a.rs")], [1]);
        assert_eq!(diff.touched[Utf8Path::new("next/a.rs")], [3]);
        assert_eq!(diff.touched.len(), 2);
    }

    #[test]
    fn deletion_and_context_counts_keep_header_like_source_inside_the_hunk() {
        for body in [
            "@@ -1,2 +0,0 @@\n-gone\n--- removed source\n",
            "@@ -1,2 +20,1 @@\n keep\n--- removed source\n",
        ] {
            let patch = format!("--- old/same.rs\n+++ new/same.rs\n{body}+++ new/same.rs\n@@ -0,0 +3,1 @@\n+added\n");
            let diff = Diff::parse(&patch);
            assert_eq!(diff.touched.len(), 1, "{patch}");
            assert_eq!(diff.touched[Utf8Path::new("same.rs")], [3], "{patch}");
            assert_eq!(diff.sources[Utf8Path::new("same.rs")], Utf8Path::new("new/same.rs"), "{patch}");
        }
    }

    #[test]
    fn resolution_normalizes_merged_aliases_and_reports_sorted_missing_sources() {
        let mut diff = Diff::parse(
            "\
--- one/src/a.rs
+++ one/src/a.rs
@@ -0,0 +2,1 @@
+two
@@ -0,0 +8,1 @@
+eight
--- two/src/a.rs
+++ two/src/a.rs
@@ -0,0 +4,1 @@
+four
@@ -0,0 +8,1 @@
+eight again
",
        );
        diff.resolve(Utf8Path::new("workspace"), &[Utf8PathBuf::from("src/a.rs")])
            .expect("both aliases peel to the same known source");
        assert_eq!(diff.touched[Utf8Path::new("src/a.rs")], [2, 4, 8]);
        assert_eq!(diff.touched.len(), 1);

        let mut missing = Diff::parse(
            "\
+++ z.rs
@@ -0,0 +1,1 @@
+z
+++ a.rs
@@ -0,0 +1,1 @@
+a
",
        );
        let cause = missing
            .resolve(Utf8Path::new("workspace"), &[])
            .expect_err("neither source is in the supplied workspace inventory");
        assert!(cause.to_string().contains("(a.rs, z.rs)"), "{cause}");
    }

    #[test]
    fn path_resolution_prefers_exact_then_peeled_names_and_never_peels_traversals() {
        for (path, raw, candidates, expected) in [
            ("src/a.rs", "b/src/a.rs", vec!["src/a.rs", "b/src/a.rs"], Some("src/a.rs")),
            (
                "extra/long/src/a.rs",
                "extra/long/src/a.rs",
                vec!["src/a.rs", "long/src/a.rs"],
                Some("long/src/a.rs"),
            ),
            ("a.rs", "/elsewhere/src/a.rs", vec!["src/a.rs"], Some("src/a.rs")),
            ("../a.rs", "../a.rs", vec!["a.rs"], None),
        ] {
            let candidates: Vec<_> = candidates.into_iter().map(Utf8PathBuf::from).collect();
            let known = candidates.iter().map(Utf8PathBuf::as_path).collect();
            assert_eq!(
                locate(
                    Utf8Path::new(path),
                    Utf8Path::new(raw),
                    Utf8Path::new("workspace"),
                    &known,
                    &candidates
                ),
                expected.map(Utf8PathBuf::from),
                "{path:?} / {raw:?}"
            );
        }
    }

    #[test]
    fn non_body_metadata_abandons_remaining_hunk_counts_before_the_next_file() {
        for range in ["-1,2 +0,0", "-0,0 +1,2"] {
            let patch =
                format!("--- a/old.rs\n+++ b/old.rs\n@@ {range} @@\nmetadata outside the hunk\n+++ b/new.rs\n@@ -0,0 +4,1 @@\n+new\n");
            let diff = Diff::parse(&patch);
            assert_eq!(diff.touched.len(), 1, "{patch}");
            assert_eq!(diff.touched[Utf8Path::new("new.rs")], [4], "{patch}");
            assert!(!diff.touches_file(Utf8Path::new("old.rs")), "{patch}");
        }
    }

    #[test]
    fn added_lines_are_numbered_in_the_new_file() {
        let diff = Diff::parse(SAMPLE);
        let path = Utf8Path::new("src/lib.rs");

        assert!(diff.touches(path, 12, 12));
        assert!(diff.touches(path, 13, 13));
        assert!(!diff.touches(path, 11, 11));
        assert!(!diff.touches(path, 14, 20));
    }

    #[test]
    fn added_text_before_any_file_header_is_not_attributed_to_an_empty_path() {
        let diff = Diff::parse("@@ -0,0 +1 @@\n+orphan\n");

        assert!(!diff.touches_file(Utf8Path::new("")));
    }

    #[test]
    fn the_git_prefix_is_stripped() {
        assert!(Diff::parse(SAMPLE).touches_file(Utf8Path::new("src/lib.rs")));
    }

    // `diff.mnemonicPrefix` replaces `a/` and `b/` with letters that say which side is which, so a
    // parser that knows only `b/` resolves nothing, selects nothing, and reports a perfect score
    // for a change it never looked at. The header names both prefixes, so nothing has to be assumed.
    #[test]
    fn a_mnemonic_prefix_is_taken_from_the_header() {
        let text = "diff --git i/x.rs w/x.rs\n--- i/x.rs\n+++ w/x.rs\n@@ -1 +1,2 @@\n one\n+two\n";
        let diff = Diff::parse(text);

        assert!(diff.touches_file(Utf8Path::new("x.rs")), "{diff:?}");
        assert!(diff.touches(Utf8Path::new("x.rs"), 2, 2));
    }

    // `--src-prefix` and `--dst-prefix` take arbitrary text, and a multi-segment path behind one of
    // them must lose only the prefix rather than its own leading directory.
    #[test]
    fn an_arbitrary_prefix_is_taken_from_the_header() {
        let text = "diff --git old/src/lib.rs new/src/lib.rs\n--- old/src/lib.rs\n+++ new/src/lib.rs\n@@ -1 +1,2 @@\n one\n+two\n";
        let diff = Diff::parse(text);

        assert!(diff.touches_file(Utf8Path::new("src/lib.rs")), "{diff:?}");
    }

    // `diff.noprefix` writes the path unadorned on both sides, so nothing may be stripped — a file
    // that genuinely lives in `b/` would otherwise be renamed out of existence.
    #[test]
    fn a_diff_written_without_prefixes_keeps_its_leading_directory() {
        let text = "diff --git b/lib.rs b/lib.rs\n--- b/lib.rs\n+++ b/lib.rs\n@@ -1 +1,2 @@\n one\n+two\n";
        let diff = Diff::parse(text);

        assert!(diff.touches_file(Utf8Path::new("b/lib.rs")), "{diff:?}");
    }

    // A path containing a space cannot be split on whitespace, so the split is found by the halves
    // agreeing about which file they name.
    #[test]
    fn a_path_with_a_space_still_yields_its_prefix() {
        let text = "diff --git i/my file.rs w/my file.rs\n+++ w/my file.rs\n@@ -1 +1,2 @@\n one\n+two\n";
        let diff = Diff::parse(text);

        assert!(diff.touches_file(Utf8Path::new("my file.rs")), "{diff:?}");
    }

    // A rename has no common suffix to compare, so the header says nothing; the `---` and `+++`
    // pair still does not agree either, and git's default is the only thing left to try.
    #[test]
    fn a_rename_falls_back_to_the_default_prefix() {
        let text = "diff --git a/old.rs b/new.rs\n--- a/old.rs\n+++ b/new.rs\n@@ -1 +1,2 @@\n one\n+two\n";
        let diff = Diff::parse(text);

        assert!(diff.touches_file(Utf8Path::new("new.rs")), "{diff:?}");
    }

    // A path that is already a workspace file is kept exactly as it is.
    #[test]
    fn a_path_that_names_a_workspace_file_resolves_to_it() {
        let mut diff = Diff::parse(SAMPLE);

        diff.resolve(Utf8Path::new("/nowhere"), &[Utf8PathBuf::from("src/lib.rs")])
            .expect("the path names a workspace file");

        assert!(diff.touches(Utf8Path::new("src/lib.rs"), 12, 12));
    }

    #[test]
    fn a_relative_non_rust_workspace_file_resolves_without_becoming_absolute() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 root");
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"subject\"\n").expect("manifest");

        assert_eq!(
            locate(
                Utf8Path::new("Cargo.toml"),
                Utf8Path::new("Cargo.toml"),
                &root,
                &HashSet::default(),
                &[]
            ),
            Some(Utf8PathBuf::from("Cargo.toml"))
        );
    }

    #[test]
    fn an_absolute_workspace_path_resolves_to_its_relative_candidate() {
        let directory = tempfile::tempdir().expect("could not create a temporary directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is not UTF-8");
        let source = root.join("src/lib.rs");

        fs::create_dir_all(source.parent().expect("source parent")).expect("source directory");
        fs::write(&source, "one\ntwo\n").expect("source");

        let text = format!("--- {source}\n+++ {source}\n@@ -1 +1,2 @@\n one\n+two\n");
        let mut diff = Diff::parse(&text);

        diff.resolve(&root, &[Utf8PathBuf::from("src/lib.rs")])
            .expect("an absolute path inside the workspace resolves");

        assert!(diff.touches(Utf8Path::new("src/lib.rs"), 2, 2));
        assert!(!diff.touches_file(&source), "the absolute spelling must not survive resolution");
    }

    #[test]
    fn an_absolute_path_from_another_checkout_resolves_by_unique_suffix() {
        let text = "--- /other/checkout/src/lib.rs\n+++ /other/checkout/src/lib.rs\n@@ -1 +1,2 @@\n one\n+two\n";
        let mut diff = Diff::parse(text);

        diff.resolve(Utf8Path::new("/this/checkout"), &[Utf8PathBuf::from("src/lib.rs")])
            .expect("the absolute path uniquely names a workspace candidate");

        assert!(diff.touches(Utf8Path::new("src/lib.rs"), 2, 2));
    }

    #[test]
    fn a_traversal_to_an_existing_external_source_is_refused() {
        let directory = tempfile::tempdir().expect("could not create a temporary directory");
        let base = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is not UTF-8");
        let root = base.join("workspace");

        fs::create_dir(&root).expect("workspace");
        fs::write(base.join("outside.rs"), "one\ntwo\n").expect("external source");

        let text = "--- ../outside.rs\n+++ ../outside.rs\n@@ -1 +1,2 @@\n one\n+two\n";
        let mut diff = Diff::parse(text);
        let error = diff
            .resolve(&root, &[Utf8PathBuf::from("src/lib.rs")])
            .expect_err("a path outside the workspace is not a workspace candidate");

        assert!(error.is_usage(), "{error}");
        assert!(error.to_string().contains("../outside.rs"), "{error}");
    }

    #[cfg(windows)]
    #[test]
    fn a_drive_relative_path_is_not_safe_workspace_input() {
        assert!(!safe_relative(Utf8Path::new("C:outside.rs")));
    }

    // A diff produced from a subdirectory, or with a prefix nothing recognized, names the file with
    // leading directories the workspace does not have. The workspace file it uniquely ends with is
    // the file meant, and matching it is the difference between running the change and running
    // nothing.
    #[test]
    fn an_unrecognized_prefix_is_resolved_by_the_file_it_ends_with() {
        let text = "--- q/z/src/lib.rs\n+++ q/z/src/lib.rs\n@@ -1 +1,2 @@\n one\n+two\n";
        let mut diff = Diff::parse(text);

        diff.resolve(Utf8Path::new("/nowhere"), &[Utf8PathBuf::from("src/lib.rs")])
            .expect("the path ends with a workspace file");

        assert!(diff.touches(Utf8Path::new("src/lib.rs"), 2, 2), "{diff:?}");
    }

    // A path that ends with two different workspace files says nothing about which was meant, and
    // attributing the change to the wrong one is worse than not attributing it at all.
    #[test]
    fn an_ambiguous_suffix_resolves_to_nothing() {
        let text = "--- q/lib.rs\n+++ q/lib.rs\n@@ -1 +1,2 @@\n one\n+two\n";
        let mut diff = Diff::parse(text);
        let candidates = [Utf8PathBuf::from("one/lib.rs"), Utf8PathBuf::from("two/lib.rs")];

        let error = diff
            .resolve(Utf8Path::new("/nowhere"), &candidates)
            .expect_err("an ambiguous path resolves to nothing");

        assert!(error.is_usage(), "{error}");
    }

    // The whole reason resolution exists. A diff that names paths and resolves none of them was not
    // understood, and the difference between that and a change that touched no code is the
    // difference between a run that failed and a run that tested nothing and said it was fine.
    #[test]
    fn a_diff_whose_paths_resolve_to_nothing_is_an_error() {
        let text = "diff --git w/src/lib.rs i/src/lib.rs\n--- w/src/lib.rs\n+++ i/src/lib.rs\n@@ -1 +1,2 @@\n one\n+two\n";
        let mut diff = Diff::parse(text);

        let error = diff
            .resolve(Utf8Path::new("/nowhere"), &[Utf8PathBuf::from("other/file.rs")])
            .expect_err("a diff that names nothing this workspace has must not pass for an empty change");

        assert!(error.is_usage(), "{error}");
        assert!(error.to_string().contains("src/lib.rs"), "{error}");
    }

    // A change that named nothing at all is not a misunderstanding, so it stays an empty selection
    // rather than becoming an error.
    #[test]
    fn a_diff_that_names_nothing_resolves_to_nothing_without_failing() {
        let mut diff = Diff::parse("");

        diff.resolve(Utf8Path::new("/nowhere"), &[]).expect("an empty diff is not an error");

        assert!(diff.touched.is_empty());
    }

    // A pull request usually touches a manifest, a changelog and some documentation alongside the
    // code. Those are real paths in the workspace and must count as understood, or every such diff
    // would fail.
    #[test]
    fn a_path_that_is_not_a_source_file_still_counts_as_understood() {
        let directory = tempfile::tempdir().expect("could not create a temporary directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is not UTF-8");

        fs::write(root.join("README.md"), "hello").expect("could not write the file");

        let text = "diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1 +1,2 @@\n one\n+two\n";
        let mut diff = Diff::parse(text);

        diff.resolve(&root, &[]).expect("a file that exists is a file this diff understood");

        assert!(diff.touches_file(Utf8Path::new("README.md")));
    }

    #[test]
    fn a_directory_is_not_treated_as_an_understood_diff_path() {
        let directory = tempfile::tempdir().expect("could not create a temporary directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is not UTF-8");

        fs::create_dir(root.join("docs")).expect("docs directory");

        let text = "--- docs\n+++ docs\n@@ -1 +1,2 @@\n one\n+two\n";
        let mut diff = Diff::parse(text);
        let error = diff.resolve(&root, &[]).expect_err("a directory is not a diff file");

        assert!(error.is_usage(), "{error}");
        assert!(error.to_string().contains("docs"), "{error}");
    }

    #[test]
    fn an_absolute_non_source_path_inside_the_workspace_counts_as_understood() {
        let directory = tempfile::tempdir().expect("could not create a temporary directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is not UTF-8");
        let readme = root.join("README.md");

        fs::write(&readme, "hello").expect("could not write the file");

        let text = format!("--- {readme}\n+++ {readme}\n@@ -1 +1,2 @@\n one\n+two\n");
        let mut diff = Diff::parse(&text);

        diff.resolve(&root, &[])
            .expect("an absolute non-source path inside the workspace is understood");

        assert!(diff.touches_file(Utf8Path::new("README.md")));
    }

    // The partial failure, which is the dangerous one. One resolvable path is enough to satisfy the
    // "resolved nothing" check above, so without this the unresolved source file is dropped
    // silently: `Survey::for_build` keeps only the files the diff is believed to touch, so its
    // mutants are never generated and the score rises because the population shrank.
    #[test]
    fn an_unresolvable_source_path_alongside_a_resolvable_one_is_still_an_error() {
        let directory = tempfile::tempdir().expect("could not create a temporary directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is not UTF-8");

        fs::write(root.join("README.md"), "hello").expect("could not write the file");

        let text = concat!(
            "diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1 +1,2 @@\n one\n+two\n",
            "diff --git w/src/lib.rs i/src/lib.rs\n--- w/src/lib.rs\n+++ i/src/lib.rs\n@@ -1 +1,2 @@\n one\n+two\n",
        );
        let mut diff = Diff::parse(text);

        let error = diff
            .resolve(&root, &[Utf8PathBuf::from("other/file.rs")])
            .expect_err("an unresolved source file must not be hidden by a resolvable README");

        assert!(error.is_usage(), "{error}");
        assert!(error.to_string().contains("src/lib.rs"), "{error}");

        // The README is not the complaint, and naming it would send the reader to the wrong file.
        assert!(!error.to_string().contains("README.md"), "{error}");
    }

    // The other half of the same rule: a non-source path that does not resolve cannot cost a single
    // mutant, so it must not fail a run that otherwise understood the diff.
    #[test]
    fn an_unresolvable_path_that_is_not_source_does_not_fail_a_diff_that_resolved_something() {
        let directory = tempfile::tempdir().expect("could not create a temporary directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is not UTF-8");

        fs::write(root.join("README.md"), "hello").expect("could not write the file");

        let text = concat!(
            "diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1 +1,2 @@\n one\n+two\n",
            "diff --git a/gone.txt b/gone.txt\n--- a/gone.txt\n+++ b/gone.txt\n@@ -1 +1,2 @@\n one\n+two\n",
        );
        let mut diff = Diff::parse(text);

        diff.resolve(&root, &[])
            .expect("an unresolved non-source path is not a misunderstanding");

        assert!(diff.touches_file(Utf8Path::new("README.md")));
        assert!(!diff.touches_file(Utf8Path::new("gone.txt")));
    }

    #[test]
    fn a_deleted_file_contributes_nothing() {
        let diff = Diff::parse("--- a/gone.rs\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-one\n-two\n");

        assert!(diff.touched.is_empty());
    }

    #[test]
    fn a_region_spanning_a_changed_line_is_selected() {
        let diff = Diff::parse(SAMPLE);

        // A mutation site running from line 10 to line 14 encloses the added lines.
        assert!(diff.touches(Utf8Path::new("src/lib.rs"), 10, 14));
    }

    #[test]
    fn a_diff_without_git_prefixes_is_understood() {
        let diff = Diff::parse("--- old.rs\t2020-01-01\n+++ new.rs\t2020-01-02\n@@ -1 +1,2 @@\n one\n+two\n");

        assert!(diff.touches(Utf8Path::new("new.rs"), 2, 2));
    }

    #[test]
    fn several_hunks_in_one_file_all_count() {
        let text = "--- a/x.rs\n+++ b/x.rs\n@@ -1,1 +1,2 @@\n one\n+two\n@@ -50,1 +51,2 @@\n fifty\n+fifty two\n";
        let diff = Diff::parse(text);
        let path = Utf8Path::new("x.rs");

        assert!(diff.touches(path, 2, 2));
        assert!(diff.touches(path, 52, 52));
        assert!(!diff.touches(path, 30, 40));
    }

    #[test]
    fn several_files_are_kept_apart() {
        let text = "--- a/x.rs\n+++ b/x.rs\n@@ -1 +1,2 @@\n one\n+x change\n--- a/y.rs\n+++ b/y.rs\n@@ -1 +1,2 @@\n one\n+y change\n";
        let diff = Diff::parse(text);

        assert!(diff.touches(Utf8Path::new("x.rs"), 2, 2));
        assert!(diff.touches(Utf8Path::new("y.rs"), 2, 2));
        assert!(!diff.touches_file(Utf8Path::new("z.rs")));
    }

    #[test]
    fn an_empty_diff_touches_nothing() {
        assert!(Diff::parse("").touched.is_empty());
    }

    // `--in-diff <PATH>` is how a pull-request job scopes a run, so reading the diff off disk has
    // to produce the same answer as parsing the same bytes in memory.
    #[test]
    fn a_diff_is_read_from_a_file() {
        let dir = tempfile::tempdir().expect("could not create a temporary directory");
        let path = Utf8PathBuf::from_path_buf(dir.path().join("change.patch")).expect("the temporary path is not UTF-8");

        fs::write(&path, SAMPLE).expect("could not write the diff");

        let diff = Diff::read(&path).expect("could not read the diff");

        assert!(!diff.touched.is_empty());
        assert!(diff.touches_file(Utf8Path::new("src/lib.rs")));
    }

    // A diff that does not exist is a usage mistake a caller has to be told about, not an empty
    // selection that would silently mutate nothing and report a perfect score.
    #[test]
    fn a_missing_diff_file_is_an_error_naming_the_path() {
        let error = Diff::read(Utf8Path::new("no/such/change.patch")).expect_err("a missing diff must not parse");

        assert!(error.to_string().contains("no/such/change.patch"), "{error}");
    }

    // `--in-diff -` is the form `git diff | cargo gamma run --in-diff -` uses, and it has to read
    // the whole stream rather than the first line of it.
    //
    // The reader is passed by value here and by `&mut` in the test below, which is the whole point
    // of the generic being taken by value: an owned reader needs no adapter, and a caller that
    // still wants its reader back afterwards passes a mutable borrow of it.
    #[test]
    fn a_diff_is_read_from_standard_input() {
        let diff = Diff::read_from(Utf8Path::new("-"), SAMPLE.as_bytes()).expect("could not read the diff");

        assert!(diff.touches_file(Utf8Path::new("src/lib.rs")));
    }

    // A reader the caller still owns afterwards reaches the same parser through a mutable borrow.
    #[test]
    fn a_borrowed_reader_is_accepted_and_left_with_its_owner() {
        let mut input = SAMPLE.as_bytes();
        let diff = Diff::read_from(Utf8Path::new("-"), &mut input).expect("could not read the diff");

        assert!(diff.touches_file(Utf8Path::new("src/lib.rs")));
        assert!(input.is_empty(), "the borrowed reader was consumed in place rather than copied");
    }

    // A stream that fails half way through must be reported rather than silently truncated into a
    // diff that touches less than the real change did.
    #[test]
    fn a_failing_standard_input_is_an_error() {
        struct Broken;

        impl Read for Broken {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("the pipe broke"))
            }
        }

        let error = Diff::read_from(Utf8Path::new("-"), &mut Broken).expect_err("a broken pipe must not parse");

        assert!(error.to_string().contains("standard input"), "{error}");
    }

    // A hunk header the parser cannot read leaves it with no idea which line the following `+`
    // lines are at, so the file is abandoned rather than credited with invented line numbers.
    #[test]
    fn an_unreadable_hunk_header_abandons_the_file() {
        let text = "--- a/x.rs\n+++ b/x.rs\n@@ nonsense @@\n+added\n";
        let diff = Diff::parse(text);

        assert!(diff.touched.is_empty(), "an unparsable hunk must not contribute lines");
    }

    // Real patches carry lines that belong to neither the pre- nor the post-image. They must not
    // advance the line counter, or every mutant after them would be attributed to the wrong line.
    #[test]
    fn a_line_that_is_not_part_of_the_hunk_does_not_advance_the_count() {
        let text = "--- a/x.rs\n+++ b/x.rs\n@@ -1,2 +1,2 @@\n one\n-old\n\\ No newline at end of file\n+new\n";
        let diff = Diff::parse(text);

        // `one` is line 1, the deletion has no post-image line, the `\` marker is not a line
        // either, so the addition is line 2 rather than line 3.
        assert!(diff.touches(Utf8Path::new("x.rs"), 2, 2), "the addition landed on the wrong line");
    }

    // An added line whose own text begins with `+++ ` renders as a raw `+++ ...` line inside the
    // hunk. Recognizing `+++` as a file header only outside a hunk body keeps it an ordinary
    // addition; treating it as a header would silently drop the change and misread the file path.
    #[test]
    fn an_added_line_that_looks_like_a_file_header_is_still_an_addition() {
        let text = "--- a/x.rs\n+++ b/x.rs\n@@ -1,1 +1,2 @@\n one\n+++ nested;\n";
        let diff = Diff::parse(text);

        // The added source line is `++ nested;`; the leading `+` marks it added, and the parser
        // must stay on `x.rs` at line 2 rather than jumping to a file called `nested;`.
        assert!(
            diff.touches(Utf8Path::new("x.rs"), 2, 2),
            "the addition was mistaken for a `+++` header"
        );
        assert!(
            !diff.touches_file(Utf8Path::new("nested;")),
            "a hunk-body line was read as a file header"
        );
    }
}
#[test]
#[cfg_attr(coverage_nightly, coverage(off))]
fn survivor_diff_returns_file_decoding_and_retention_limit_failures_without_panicking_or_accepting_empty_work() {
    let path = Utf8Path::new("invalid.diff");
    let error = Diff::read_from_with(path, std::io::empty(), |requested| {
        assert_eq!(requested, path);
        Ok(std::io::Cursor::new([0xff]))
    })
    .expect_err("a file-backed invalid encoding is an ordinary read error");
    assert_eq!(error.message(), "could not read the diff `invalid.diff`");
    let cause = std::error::Error::source(&error)
        .and_then(|cause| cause.downcast_ref::<std::io::Error>())
        .expect("invalid file bytes retain the decoding I/O error");
    assert_eq!(cause.kind(), std::io::ErrorKind::InvalidData);
    let error = Diff::from_retained(None).expect_err("the bounded input reader's refusal is not an empty successful diff");
    assert!(error.is_usage());
    assert_eq!(
        error.to_string(),
        format!("the diff is larger than the {} bytes cargo-gamma will retain", input::MAX_BYTES)
    );
}

#[test]
#[cfg_attr(coverage_nightly, coverage(off))]
fn survivor_unresolved_diff_paths_keep_the_sorted_comma_delimited_rust_and_nonrust_diagnostics() {
    let directory = crate::testing::workdir("diff-unresolved-list-");
    let root = Utf8Path::from_path(directory.path()).expect("fixture paths are UTF-8");
    for (first, second) in [("a.rs", "z.rs"), ("a.md", "z.md")] {
        let text = format!("+++ b/{second}\n@@ -0,0 +1 @@\n+new\n+++ b/{first}\n@@ -0,0 +1 @@\n+new\n");
        let error = Diff::parse(&text)
            .resolve(root, &[])
            .expect_err("neither referenced file exists in this actual workspace");
        assert!(error.to_string().contains(&format!("(b/{first}, b/{second})")), "{error}");
    }
}

#[test]
#[cfg_attr(coverage_nightly, coverage(off))]
fn survivor_absolute_nonrust_diff_paths_cannot_escape_through_a_lexical_parent_component() {
    let root = Utf8Path::new(if cfg!(windows) { "C:/workspace" } else { "/workspace" });
    let escaped = root.join("../outside.md");
    let queried = std::cell::Cell::new(false);
    assert_eq!(
        locate_with(&escaped, &escaped, root, &HashSet::default(), &[], |path| {
            assert_eq!(path, escaped);
            queried.set(true);
            true
        }),
        None
    );
    assert!(
        !queried.get(),
        "lexical traversal must be refused before consulting filesystem existence"
    );
    let existing = root.join("inside.md");
    assert_eq!(
        locate_with(&existing, &existing, root, &HashSet::default(), &[], |path| {
            assert_eq!(path, existing);
            true
        }),
        Some(Utf8PathBuf::from("inside.md"))
    );
}
