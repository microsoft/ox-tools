// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! What a build failure quotes back to the reader.

use camino::Utf8Path;

use super::invoke::is_progress;
use super::messages::{cargo_message, normalize_separators};
use crate::HashSet;
use crate::discover::Plan;

/// How many compiler errors a build failure quotes before it starts counting instead.
///
/// A tree that does not compile usually does so for one reason, restated by every crate that
/// depended on it, so quoting everything buries the first error under its own consequences.
pub(super) const DIAGNOSTIC_LIMIT: usize = 5;

/// Keeps the part of cargo's stderr that explains a failure.
///
/// Cargo narrates its progress on the same stream it reports failures on, and a cold build narrates
/// thousands of lines. Handing all of that to someone whose build just failed buries the two lines
/// that matter, so the progress verbs are dropped and everything else is kept — including the
/// indented `Caused by` blocks and build-script output, which is usually where the real cause is.
///
/// Segments are split on carriage returns as well as newlines, and the progress bar is recognised
/// by `is_progress` rather than by a list of its own. Cargo redraws the bar by returning to the
/// start of the line, so a whole build's worth of redraws is a single newline-terminated line;
/// splitting only on newlines would hand the reader that entire bar as one enormous complaint.
pub(super) fn complaints(stderr: &str) -> String {
    /// The verbs cargo uses to narrate work it is doing rather than trouble it has hit.
    const PROGRESS: [&str; 11] = [
        "Compiling",
        "Building",
        "Checking",
        "Downloading",
        "Downloaded",
        "Updating",
        "Locking",
        "Adding",
        "Finished",
        "Fresh",
        "Running",
    ];

    let mut kept = String::new();

    // A carriage return separates redraws of the same line, so its segments are lines in their own
    // right; the empty ones a redraw leaves behind are an artifact of the drawing rather than
    // something cargo said, and only a genuinely blank line is worth keeping.
    let segments = stderr
        .lines()
        .flat_map(|line| line.split('\r').filter(move |segment| !segment.is_empty() || line.is_empty()));

    for line in segments {
        let trimmed = line.trim_start();

        if is_progress(line) {
            continue;
        }

        if PROGRESS.iter().any(|verb| {
            trimmed
                .strip_prefix(verb)
                .is_some_and(|rest| rest.starts_with(' ') || rest.is_empty())
        }) {
            continue;
        }

        if trimmed.is_empty() && kept.is_empty() {
            continue;
        }

        kept.push_str(line);
        kept.push('\n');
    }

    if kept.trim().is_empty() {
        return "cargo said nothing on stderr either.".to_owned();
    }

    kept
}

/// One error-level compiler message, kept with the package it came from.
///
/// The package matters because a failing preflight quotes only the first few errors, and the ones
/// worth quoting are those in the code the caller chose to mutate. A reverse dependency dragged in
/// because its tests form part of the oracle can produce dozens of errors that are consequences of
/// a single problem in the caller's own crate, and quoting those instead sends them to fix a
/// package they never mentioned.
pub(super) struct Diagnostic {
    /// The manifest of the package the compiler was building, when cargo said.
    pub(super) manifest: Option<String>,

    /// The compiler's own rendering, snippet and underlines and all.
    pub(super) rendered: String,
}

/// Extracts the human-readable compiler diagnostics from cargo's JSON output.
///
/// With `--message-format=json` the diagnostics arrive on stdout as structured messages and
/// stderr carries only a summary, so a failure report built from stderr would not say what went wrong.
pub(super) fn diagnostics(stdout: &str) -> Vec<Diagnostic> {
    let mut rendered = Vec::new();

    for line in stdout.lines() {
        let Some(message) = cargo_message(line) else {
            continue;
        };

        if message.reason != "compiler-message" {
            continue;
        }

        let manifest = message.manifest_path.as_deref().map(normalize_separators);

        let Some(diagnostic) = message.message else {
            continue;
        };

        if diagnostic.level != "error" {
            continue;
        }

        if let Some(text) = diagnostic.rendered {
            rendered.push(Diagnostic {
                manifest,
                rendered: text.into_owned(),
            });
        }
    }

    rendered
}

/// Moves the diagnostics from `manifests` to the front, keeping each group in the order the
/// compiler emitted it.
///
/// A stable partition rather than a filter: the other packages' errors are still worth having when
/// there is room for them, and when the caller's own package turns out to be clean they are the
/// whole story.
pub(super) fn prioritize(found: &mut [Diagnostic], manifests: &HashSet<String>) {
    found.sort_by_key(|diagnostic| !diagnostic.manifest.as_ref().is_some_and(|path| manifests.contains(path)));
}

/// The manifest paths of the named packages inside the copied tree.
pub(super) fn manifests_of(plan: &Plan, root: &Utf8Path, packages: &[String]) -> HashSet<String> {
    packages
        .iter()
        .filter_map(|package| plan.directory_of(package))
        .map(|directory| {
            let absolute = if directory.as_str().is_empty() {
                root.to_owned()
            } else {
                root.join(directory)
            };

            normalize_separators(absolute.join("Cargo.toml").as_str())
        })
        .collect()
}

/// Renders the first few diagnostics whole, saying how many were left out.
///
/// From the front, and by whole diagnostics rather than by line. A compiler's first error is the
/// one to fix — the rest are frequently consequences of it — and a limit measured in lines cuts
/// one in half, which produces a report that opens partway through a snippet with no error line
/// above it to say what is being pointed at.
pub(super) fn leading(found: &[Diagnostic], limit: usize) -> String {
    let shown: String = found.iter().take(limit).map(|diagnostic| diagnostic.rendered.as_str()).collect();
    let omitted = found.len().saturating_sub(limit);

    if omitted == 0 {
        return shown;
    }

    format!("{shown}\n(and {} not shown)\n", crate::report::quantity(omitted, "further error"))
}
