// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The shapes cargo's JSON output is read back as.

use std::borrow::Cow;
use std::fs;

use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;

use crate::{HashMap, HashSet};

/// One line of cargo's `--message-format=json` stream, decoded to the fields this tool reads.
///
/// A borrowed decode rather than a `serde_json::Value`: the stream runs to megabytes, four separate
/// consumers walk it, and between them they read eight fields. Decoding into a shape that names
/// those fields lets serde skip everything else without building it, and lets the strings that are
/// only ever compared point into the line instead of onto the heap.
///
/// `Cow` rather than `&str` because a rendered diagnostic is full of escapes, and a borrowed `&str`
/// cannot represent a JSON string that needed unescaping. Every field is optional or defaulted:
/// cargo emits several kinds of message on this stream, most lines carry only some of these, and a
/// line this tool has no interest in must be skipped rather than fail the parse.
#[derive(Deserialize)]
pub(super) struct CargoMessage<'line> {
    /// What kind of message this is: `compiler-message`, `compiler-artifact`, and others.
    #[serde(borrow, default)]
    pub(super) reason: Cow<'line, str>,

    /// The diagnostic itself, on a `compiler-message`.
    #[serde(borrow, default)]
    pub(super) message: Option<CompilerMessage<'line>>,

    /// The artifacts produced, on a `compiler-artifact`.
    #[serde(borrow, default)]
    pub(super) filenames: Vec<Cow<'line, str>>,

    /// The manifest of the package being built, used to put the caller's own errors first.
    #[serde(borrow, default)]
    pub(super) manifest_path: Option<Cow<'line, str>>,
}

/// A compiler diagnostic, and the children that carry the rest of what it knows.
#[derive(Deserialize)]
pub(super) struct CompilerMessage<'line> {
    /// `error`, `warning`, and the rest of rustc's levels.
    #[serde(borrow, default)]
    pub(super) level: Cow<'line, str>,

    /// The compiler's own rendering, snippet and underlines and all.
    #[serde(borrow, default)]
    pub(super) rendered: Option<Cow<'line, str>>,

    /// The error code, when the diagnostic has one.
    #[serde(borrow, default)]
    pub(super) code: Option<DiagnosticCode<'line>>,

    /// Where in the source the diagnostic points.
    #[serde(borrow, default)]
    pub(super) spans: Vec<Span<'line>>,

    /// The notes and helps hung off this diagnostic, which carry spans of their own.
    #[serde(borrow, default)]
    pub(super) children: Vec<Self>,
}

/// The `E0382`-style code of a diagnostic, which arrives wrapped in an object of its own.
#[derive(Deserialize)]
pub(super) struct DiagnosticCode<'line> {
    /// The code itself.
    #[serde(borrow, default)]
    pub(super) code: Cow<'line, str>,
}

/// One region of source a diagnostic points at.
///
/// The line and column numbers are `u64` rather than `u32` because they are read from JSON that
/// this tool does not produce: a number too large for a `u32` must clamp, exactly as the field-by-
/// field reading it replaced did, rather than fail the whole message and lose the diagnostic.
#[derive(Deserialize)]
pub(super) struct Span<'line> {
    /// The file the span is in, as the compiler spelled it.
    #[serde(borrow, default)]
    pub(super) file_name: Option<Cow<'line, str>>,

    /// First line of the region, 1-based.
    #[serde(default)]
    pub(super) line_start: Option<u64>,

    /// Last line of the region, 1-based.
    #[serde(default)]
    pub(super) line_end: Option<u64>,

    /// First column of the region.
    #[serde(default)]
    pub(super) column_start: Option<u64>,

    /// Last column of the region.
    #[serde(default)]
    pub(super) column_end: Option<u64>,

    /// Whether this is the span the diagnostic is really about.
    #[serde(default)]
    pub(super) is_primary: bool,
}

/// Decodes one line of cargo's JSON stream, or `None` when it is not one.
///
/// Cargo interleaves its own prose on this stream when it feels like it, and a line that does not
/// decode is one this tool has nothing to say about rather than an error.
pub(super) fn cargo_message(line: &str) -> Option<CargoMessage<'_>> {
    serde_json::from_str(line).ok()
}

/// Names every source file the compiler actually read, according to cargo's dep-info.
///
/// Returns `None` when no dep-info could be read at all, which has to mean "do not draw any
/// conclusion" rather than "nothing was compiled": treating an unreadable scratch tree as an empty
/// set would condemn every mutant in the run as unbuilt.
///
/// Cargo writes a `.d` file beside each artifact listing the sources that went into it, in the
/// makefile format `target: dep dep dep`. That list is the compiler's own account of what it read,
/// which is the only thing that answers the question honestly. Evaluating `#[cfg]` predicates
/// ourselves would mean reimplementing feature resolution, target detection and every other
/// predicate cargo and rustc already agreed on, and being subtly wrong about it in the cases that
/// matter most.
///
/// Which `.d` files belong to *this* build is decided from the build's own artifact messages
/// rather than by looking at what is on disk. The scratch target directory is deliberately kept
/// between runs so that builds are incremental, so it accumulates dep-info from every earlier run
/// as well — including runs with a different feature set. Reading all of it would union today's
/// answer with a previous one and quietly conclude that everything was compiled, which is the
/// wrong answer in exactly the case this is here to catch.
pub(super) fn compiled_sources(stdout: &str, root: &Utf8Path) -> Option<HashSet<Utf8PathBuf>> {
    let mut compiled: HashSet<Utf8PathBuf> = HashSet::default();
    let mut read_any = false;

    for dep_file in dep_files(stdout) {
        let Ok(text) = fs::read_to_string(dep_file.as_std_path()) else {
            continue;
        };

        read_any = true;

        for line in text.lines() {
            let Some((_artifact, list)) = line.split_once(": ") else {
                continue;
            };

            for path in dependencies(list) {
                let path = Utf8Path::new(&path);
                let relative = path.strip_prefix(root).unwrap_or(path);

                let _added = compiled.insert(Utf8PathBuf::from(normalize_separators(relative.as_str())));
            }
        }
    }

    read_any.then_some(compiled)
}

/// Splits the dependency half of a dep-info line into the paths it names.
///
/// The `.d` file is a makefile fragment, so the separator is unescaped whitespace and a path that
/// contains a space is written with the space escaped: `src/my\ file.rs` is one dependency, not
/// two. Splitting on whitespace alone yields two fragments, neither of which is a path the survey
/// knows about, and the file then looks as though the compiler never read it — which is how every
/// mutant inside it comes to be excused as unbuilt.
///
/// Only whitespace is treated as escapable, rather than "a backslash escapes whatever follows".
/// Both emitters — rustc's `escape_dep_filename` and cargo's — escape the space and nothing else,
/// so a backslash before anything other than whitespace is a Windows path separator and has to
/// survive to reach [`normalize_separators`]. A general makefile unescape would eat those
/// separators and turn `C:\src\lib.rs` into `C:srclib.rs`, trading a rare defect for a universal
/// one on that platform.
fn dependencies(list: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut path = String::new();
    let mut characters = list.chars().peekable();

    while let Some(character) = characters.next() {
        match character {
            '\\' if characters.peek().is_some_and(|next| next.is_whitespace()) => {
                if let Some(escaped) = characters.next() {
                    path.push(escaped);
                }
            }

            character if character.is_whitespace() => {
                if !path.is_empty() {
                    paths.push(core::mem::take(&mut path));
                }
            }

            character => path.push(character),
        }
    }

    if !path.is_empty() {
        paths.push(path);
    }

    paths
}

/// Names the dep-info file for every unit in one build, from cargo's JSON artifact messages.
///
/// Cargo does not list the `.d` file among an artifact's `filenames`, but it does name it after
/// the same unit hash, so an artifact at `deps/libfoo-9a3f.rmeta` is described by `deps/foo-9a3f.d`.
/// Deriving the name from the hash rather than from the file stem avoids having to reproduce
/// cargo's own rules about which artifact kinds carry a `lib` prefix.
///
/// Uplifted copies such as `debug/libfoo.rlib` are skipped: they carry no hash, so their dep-info
/// is overwritten by whichever run last built that package under any feature set, which is the
/// staleness this is avoiding.
pub(super) fn dep_files(stdout: &str) -> Vec<Utf8PathBuf> {
    let mut wanted: HashMap<Utf8PathBuf, HashSet<String>> = HashMap::default();

    for line in stdout.lines() {
        let Some(message) = cargo_message(line) else {
            continue;
        };

        if message.reason != "compiler-artifact" {
            continue;
        }

        for filename in &message.filenames {
            let path = Utf8Path::new(filename.as_ref());

            let Some((directory, stem)) = path.parent().zip(path.file_stem()) else {
                continue;
            };

            let Some((_name, hash)) = stem.rsplit_once('-') else {
                continue;
            };

            let _added = wanted.entry(directory.to_owned()).or_default().insert(hash.to_owned());
        }
    }

    let mut found = Vec::new();

    for (directory, hashes) in wanted {
        let Ok(entries) = fs::read_dir(directory.as_std_path()) else {
            continue;
        };

        for entry in entries.flatten() {
            let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) else {
                continue;
            };

            if path.extension() != Some("d") {
                continue;
            }

            let matched = path
                .file_stem()
                .and_then(|stem| stem.rsplit_once('-'))
                .is_some_and(|(_name, hash)| hashes.contains(hash));

            if matched {
                found.push(path);
            }
        }
    }

    found
}

/// Rewrites `\` to `/` so that a dep-info path compares equal to a discovered one on Windows.
pub(super) fn normalize_separators(path: &str) -> String {
    path.replace('\\', "/")
}
