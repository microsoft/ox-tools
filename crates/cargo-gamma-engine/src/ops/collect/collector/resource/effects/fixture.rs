// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

fn pure(text: &str) -> usize {
    text.len()
}

#[cfg(not(test))]
fn conditional(text: &str) -> usize {
    text.len()
}

#[cfg(test)]
fn conditional(text: &str) -> usize {
    text.bytes().take_while(|byte| *byte != 0).count()
}

pub(super) fn pure_padding(rows: &mut Vec<String>, target: usize) {
    while rows.len() < target {
        let width = pure("x");
        rows.push(width.to_string());
    }
}

pub(super) fn conditional_padding(rows: &mut Vec<String>, target: usize) {
    while rows.len() < target {
        let width = conditional("x");
        rows.push(width.to_string());
    }
}

fn bounded_read(text: &str, reads: &mut usize) -> usize {
    *reads += 1;
    assert!(*reads <= 2);
    text.len()
}

pub(super) fn volatile_padding(rows: &mut Vec<String>, target: usize, reads: &mut usize) {
    while rows.len() < target {
        let width = bounded_read("x", reads);
        rows.push(width.to_string());
    }
}

#[expect(
    clippy::ptr_arg,
    clippy::needless_pass_by_ref_mut,
    reason = "the finite control preserves the deleted call's original receiver type"
)]
pub(super) fn volatile_deleted(rows: &mut Vec<String>, target: usize, reads: &mut usize) {
    while rows.len() < target {
        let _width = bounded_read("x", reads);
    }
}
