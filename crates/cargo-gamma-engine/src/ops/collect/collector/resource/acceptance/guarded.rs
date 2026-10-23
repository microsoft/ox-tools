// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[expect(
    clippy::needless_pass_by_value,
    reason = "the compiler control preserves the owned-vector collector input"
)]
pub(super) fn scan(characters: Vec<char>, mut index: usize) -> usize {
    while index < characters.len() {
        match characters[index] {
            'x' if {
                index += 1;
                true
            } =>
            {
                index += 1;
            }
            _ => {
                index += 1;
            }
        }
    }
    index
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the finite replacement differs only by its deleted assignment"
)]
pub(super) fn deleted(characters: Vec<char>, mut index: usize) -> usize {
    while index < characters.len() {
        match characters[index] {
            'x' if {
                index += 1;
                true
            } => {}
            _ => {
                index += 1;
            }
        }
    }
    index
}

pub(super) fn finite_growth(limit: usize) -> usize {
    let mut items: Vec<char> = Vec::new();
    let mut cursor: usize = 0;
    while cursor < limit {
        if items.len() >= 2 {
            break;
        }
        items.push('x');
        cursor += 1;
    }
    cursor
}

#[expect(
    clippy::while_immutable_condition,
    reason = "the finite control preserves the deleted cursor update"
)]
pub(super) fn growth_deleted(limit: usize) -> usize {
    let mut items: Vec<char> = Vec::new();
    let cursor: usize = 0;
    while cursor < limit {
        if items.len() >= 2 {
            break;
        }
        items.push('x');
    }
    cursor
}

pub(super) fn pad_right(right: &mut Vec<String>, target: usize) {
    while right.len() < target {
        right.insert(0, String::new());
    }
}

pub(super) fn negated_padding(right: &mut Vec<String>, target: usize) {
    while right.len() >= target {
        right.insert(0, String::new());
    }
}
