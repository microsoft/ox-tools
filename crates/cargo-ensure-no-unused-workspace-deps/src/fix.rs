// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Removal of unused `[workspace.dependencies]` entries, preserving the
//! formatting and comments of everything that survives.

use std::collections::BTreeSet;

use toml_edit::{DocumentMut, Item, TableLike};

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

    /// The entry the comments landed on, or `None` when they could not be
    /// placed and were dropped: nothing survived the removal, or the only
    /// surviving anchor was a value the comments cannot attach to.
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
        if doomed.contains(name.as_str()) {
            let comments = comments_of(&leading_comments(table, name));
            if !comments.is_empty() {
                sources.push(name.clone());
            }
            carried.push_str(&comments);
            table.remove(name);
            outcome.removed += 1;
        } else if !carried.is_empty() {
            // `onto` reports where the comments actually landed. Claiming a
            // move that did not happen sends the reviewer of the `--fix` diff
            // hunting for text that is not there, which is the failure this
            // reporting exists to prevent.
            let onto = prepend_comments(table, name, &carried);

            outcome.carries.push(Carry {
                from: std::mem::take(&mut sources),
                onto,
                lines: comment_lines(&carried),
            });
            carried.clear();
        }
    }

    if !carried.is_empty() {
        // The removed entries were the last in the table, so there is no
        // following entry to carry the comments to. Append them after the final
        // surviving entry's *value* instead: attaching them ahead of that entry
        // would hoist them above it and relabel a surviving dependency.
        //
        // Only a plain value has a suffix to append to. When the last survivor
        // is a sub-table, and when every entry was removed and nothing survives
        // at all, the comments go with the group they introduced -- reported as
        // a drop, because that is what happened.
        let last = table.iter().last().map(|(key, _)| key.to_owned());
        let onto = last.and_then(|last| {
            let appended = table.get_mut(&last).and_then(Item::as_value_mut).map(|value| {
                let suffix = value
                    .decor()
                    .suffix()
                    .and_then(toml_edit::RawString::as_str)
                    .unwrap_or_default()
                    .to_owned();
                value.decor_mut().set_suffix(format!("{suffix}{carried}"));
            });

            appended.map(|()| last)
        });

        outcome.carries.push(Carry {
            from: std::mem::take(&mut sources),
            onto,
            lines: comment_lines(&carried),
        });
    }

    outcome
}

/// The name of the inner key that renders first inside a dotted entry.
///
/// `dep.version = "1"` is a dotted table whose leading comments hang off the
/// inner `version` key, not off `dep`.
fn dotted_leaf(table: &dyn TableLike, name: &str) -> Option<String> {
    let nested = table.get(name).and_then(Item::as_table).filter(|nested| nested.is_dotted())?;

    nested.iter().next().map(|(inner, _)| inner.to_owned())
}

/// The decor preceding one entry, read from whichever slot renders it.
///
/// `toml_edit` keeps an entry's leading comments in one of three places: on the
/// key for a plain value, on the table for a `[workspace.dependencies.name]`
/// sub-table, and on the first inner key for a dotted `name.version = "1"`.
/// Reading only the key lets the other two disappear unremarked.
fn leading_comments(table: &dyn TableLike, name: &str) -> String {
    if let Some(leaf) = dotted_leaf(table, name) {
        return table
            .get(name)
            .and_then(Item::as_table_like)
            .and_then(|nested| nested.key(&leaf))
            .and_then(|key| key.leaf_decor().prefix())
            .and_then(toml_edit::RawString::as_str)
            .unwrap_or_default()
            .to_owned();
    }

    if let Some(nested) = table.get(name).and_then(Item::as_table) {
        return nested
            .decor()
            .prefix()
            .and_then(toml_edit::RawString::as_str)
            .unwrap_or_default()
            .to_owned();
    }

    table
        .key(name)
        .and_then(|key| key.leaf_decor().prefix())
        .and_then(toml_edit::RawString::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Prepend `carried` to an entry's leading comments, in the same slot
/// [`leading_comments`] reads from.
///
/// Writing to a different slot than the one that holds the entry's own decor
/// would render both, duplicating text the user never wrote. Returns the entry
/// name when the comments were placed, and `None` when the entry offers no such
/// slot, so the caller can report a drop instead of an imagined move.
fn prepend_comments(table: &mut dyn TableLike, name: &str, carried: &str) -> Option<String> {
    let existing = leading_comments(table, name);
    let combined = format!("{carried}{existing}");

    if let Some(leaf) = dotted_leaf(table, name) {
        return table
            .get_mut(name)
            .and_then(Item::as_table_like_mut)
            .and_then(|nested| nested.key_mut(&leaf))
            .map(|mut key| {
                key.leaf_decor_mut().set_prefix(combined);
                name.to_owned()
            });
    }

    if let Some(nested) = table.get_mut(name).and_then(Item::as_table_mut) {
        nested.decor_mut().set_prefix(combined);
        return Some(name.to_owned());
    }

    table.key_mut(name).map(|mut key| {
        key.leaf_decor_mut().set_prefix(combined);
        name.to_owned()
    })
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
