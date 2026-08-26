// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Removal of unused `[workspace.dependencies]` entries, preserving the
//! formatting and comments of everything that survives.

use std::collections::BTreeSet;

use toml_edit::{DocumentMut, Item};

/// What a `--fix` did.
pub struct Outcome {
    /// How many entries were removed.
    pub removed: usize,

    /// Comment blocks that moved off a removed entry.
    pub carries: Vec<Carry>,
}

/// Comments that belonged to removed entries and had to go somewhere else.
///
/// Only recorded when comments actually moved, so `from` is never empty.
///
/// Reported so the relocation is visible: the carry-forward cannot tell a group
/// header from a note about one specific dependency, and a note that lands on
/// the next entry reads as if it were about that one.
pub struct Carry {
    /// Entries whose comments were carried, in manifest order.
    pub from: Vec<String>,

    /// The entry the comments landed on, or `None` when the table was emptied
    /// and they were dropped.
    pub onto: Option<String>,

    /// How many comment lines moved.
    pub lines: usize,
}

/// Remove `names` from the catalog.
///
/// Comments attached to a removed entry are carried forward to the next
/// surviving entry, so a group header keeps labeling the group it introduces.
pub fn remove(manifest: &mut DocumentMut, names: &[String]) -> Outcome {
    let table = manifest
        .get_mut("workspace")
        .and_then(Item::as_table_like_mut)
        .and_then(|workspace| workspace.get_mut("dependencies"))
        .and_then(Item::as_table_like_mut)
        .expect("callers only fix a catalog they already read entries from, so the table is present");

    let order: Vec<String> = table.iter().map(|(key, _)| key.to_owned()).collect();
    let doomed: BTreeSet<&str> = names.iter().map(String::as_str).collect();

    let mut outcome = Outcome {
        removed: 0,
        carries: Vec::new(),
    };
    let mut carried = String::new();
    let mut sources: Vec<String> = Vec::new();

    for name in &order {
        let prefix = table
            .key(name)
            .and_then(|key| key.leaf_decor().prefix())
            .and_then(toml_edit::RawString::as_str)
            .unwrap_or_default()
            .to_owned();

        if doomed.contains(name.as_str()) {
            let comments = comments_of(&prefix);
            if !comments.is_empty() {
                sources.push(name.clone());
            }
            carried.push_str(&comments);
            table.remove(name);
            outcome.removed += 1;
        } else if !carried.is_empty() {
            if let Some(mut key) = table.key_mut(name) {
                key.leaf_decor_mut().set_prefix(format!("{carried}{prefix}"));
            }
            outcome.carries.push(Carry {
                from: std::mem::take(&mut sources),
                onto: Some(name.clone()),
                lines: comment_lines(&carried),
            });
            carried.clear();
        }
    }

    if !carried.is_empty() {
        // The removed entries were the last in the table, so there is no
        // following key to carry the comments to. Append them after the final
        // surviving entry's *value* instead: attaching them to that entry's key
        // prefix would hoist them above it and relabel a surviving dependency.
        //
        // When every entry was removed there is no surviving entry at all, and
        // the comments go with the group they introduced.
        let last = table.iter().last().map(|(key, _)| key.to_owned());
        if let Some(last) = last.clone()
            && let Some(value) = table.get_mut(&last).and_then(Item::as_value_mut)
        {
            let suffix = value
                .decor()
                .suffix()
                .and_then(toml_edit::RawString::as_str)
                .unwrap_or_default()
                .to_owned();
            value.decor_mut().set_suffix(format!("{suffix}{carried}"));
        }

        outcome.carries.push(Carry {
            from: std::mem::take(&mut sources),
            onto: last,
            lines: comment_lines(&carried),
        });
    }

    outcome
}

/// The comment-bearing part of a removed entry's decor.
///
/// Blank-line padding is dropped: only comments such as a `# --- group ---`
/// header are worth carrying to another entry.
fn comments_of(prefix: &str) -> String {
    if prefix.lines().any(|line| line.trim_start().starts_with('#')) {
        prefix.to_owned()
    } else {
        String::new()
    }
}

/// How many lines of `decor` are comments.
fn comment_lines(decor: &str) -> usize {
    decor.lines().filter(|line| line.trim_start().starts_with('#')).count()
}
