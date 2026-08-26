// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Removal of unused `[workspace.dependencies]` entries, preserving the
//! formatting and comments of everything that survives.

use std::collections::BTreeSet;

use toml_edit::{DocumentMut, Item};

/// Remove `names` from the catalog and return how many entries went away.
///
/// Comments attached to a removed entry are carried forward to the next
/// surviving entry, so a group header keeps labeling the group it introduces.
pub fn remove(manifest: &mut DocumentMut, names: &[String]) -> usize {
    let table = manifest
        .get_mut("workspace")
        .and_then(Item::as_table_like_mut)
        .and_then(|workspace| workspace.get_mut("dependencies"))
        .and_then(Item::as_table_like_mut)
        .expect("callers only fix a catalog they already read entries from, so the table is present");

    let order: Vec<String> = table.iter().map(|(key, _)| key.to_owned()).collect();
    let doomed: BTreeSet<&str> = names.iter().map(String::as_str).collect();

    let mut removed = 0;
    let mut carried = String::new();

    for name in &order {
        let prefix = table
            .key(name)
            .and_then(|key| key.leaf_decor().prefix())
            .and_then(toml_edit::RawString::as_str)
            .unwrap_or_default()
            .to_owned();

        if doomed.contains(name.as_str()) {
            carried.push_str(&comments_of(&prefix));
            table.remove(name);
            removed += 1;
        } else if !carried.is_empty() {
            if let Some(mut key) = table.key_mut(name) {
                key.leaf_decor_mut().set_prefix(format!("{carried}{prefix}"));
            }
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
        if let Some(last) = table.iter().last().map(|(key, _)| key.to_owned())
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
    }

    removed
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
