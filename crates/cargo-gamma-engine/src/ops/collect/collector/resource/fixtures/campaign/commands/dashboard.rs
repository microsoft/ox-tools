// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

fn visible_width(text: &str) -> usize {
    crate::report::unstyled_width(text)
}

fn equalize_panels(left: &mut Vec<String>, right: &mut Vec<String>) {
    let target = left.len().max(right.len());
    while left.len() < target {
        let width = visible_width(&left[0]);
        left.insert(left.len() - 1, format!("│{}│", " ".repeat(width.saturating_sub(2))));
    }
    while right.len() < target {
        let width = visible_width(&right[0]);
        right.insert(right.len() - 1, format!("│{}│", " ".repeat(width.saturating_sub(2))));
    }
}
