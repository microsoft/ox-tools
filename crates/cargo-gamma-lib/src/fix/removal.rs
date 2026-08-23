// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Deleting whole directive lines from a file's text.

use std::collections::BTreeSet;

/// Deletes whole lines from a file's text, by one-based line number.
///
/// The counterpart of [`super::apply`], and deliberately the dumbest thing that can work: a directive is
/// only ever removed when it is the entire content of its own line, so removal is a line delete and
/// nothing else. See [`removable`] for what makes that true, and why anything else is left alone.
#[must_use]
pub fn remove(text: &str, lines: &BTreeSet<usize>) -> String {
    text.split_inclusive('\n')
        .enumerate()
        .filter(|(index, _)| !lines.contains(&(index + 1)))
        .map(|(_, line)| line)
        .collect()
}

/// Whether a line holds a skip directive and nothing else, so that deleting the line deletes it.
///
/// Removal has to be conservative in a way that adding does not. A directive can be attached to a
/// line of code, wrapped in a `cfg_attr`, or spread over several lines, and in each of those cases
/// there is no line whose deletion removes the directive and only the directive. Editing *within* a
/// line to take one attribute out of a list is a different and much less safe operation, so those
/// are reported and left for a person.
#[must_use]
pub fn removable(line: &str) -> bool {
    let trimmed = line.trim();

    let body = if let Some(comment) = trimmed.strip_prefix("//") {
        let comment = comment.trim_start();
        let Some(body) = comment.strip_prefix("#[").and_then(|rest| rest.strip_suffix(']')) else {
            return false;
        };
        body
    } else {
        trimmed
            .strip_prefix("#[")
            .and_then(|rest| rest.strip_suffix(']'))
            .unwrap_or(trimmed)
    };

    if !body.starts_with("gamma::skip") {
        return false;
    }

    // An attribute whose arguments run onto the next line leaves its parentheses open here, and
    // deleting the first line of it would leave the rest behind as a syntax error.
    let opened = body.matches('(').count();

    opened == body.matches(')').count() && !body.contains("//")
}
