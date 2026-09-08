// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Exercises every exported attribute macro from an external consuming crate.
//!
//! A doctest inside this crate proves an expansion compiles; it does not prove the annotated item
//! survived intact, because a valid doctest's example is never called. These tests call the item
//! each macro annotates, so a shim that discarded the item, swapped its delegation for an
//! unrelated validator, or otherwise mangled a valid expansion fails here even when every doctest
//! still compiles.

use gamma::gamma;

#[gamma::skip]
fn skipped(x: u32) -> u32 {
    x + 1
}

#[test]
fn skip_leaves_the_annotated_item_callable() {
    assert_eq!(skipped(41), 42);
}

#[gamma::expect_survived(literal, reason = "consumer test fixture")]
fn survived(n: usize) -> String {
    format!("{n} items")
}

#[test]
fn expect_survived_leaves_the_annotated_item_callable() {
    assert_eq!(survived(3), "3 items");
}

#[gamma::expect_killed]
fn killed(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .fold(0_u32, |acc, byte| acc.wrapping_mul(31).wrapping_add(u32::from(*byte)))
}

#[test]
fn expect_killed_leaves_the_annotated_item_callable() {
    assert_eq!(killed(b"abc"), killed(b"abc"));
    assert_ne!(killed(b"abc"), killed(b"abd"));
}

#[gamma::value(u32::MAX)]
fn valued() -> u32 {
    7
}

#[test]
fn value_leaves_the_annotated_item_callable() {
    assert_eq!(valued(), 7);
}

#[gamma::test_timeout_multiplier(2.5)]
fn multiplied(data: &[u8]) -> usize {
    data.len() * 2
}

#[test]
fn test_timeout_multiplier_leaves_the_annotated_item_callable() {
    assert_eq!(multiplied(b"abc"), 6);
}

#[gamma::timeout_multiplier(2.5)]
fn aliased_multiplied(data: &[u8]) -> usize {
    data.len() * 3
}

#[test]
fn timeout_multiplier_leaves_the_annotated_item_callable() {
    assert_eq!(aliased_multiplied(b"ab"), 6);
}

#[gamma(test_timeout_multiplier = 2.0)]
fn generic_gamma(n: usize) -> usize {
    n * 2
}

#[test]
fn gamma_leaves_the_annotated_item_callable() {
    assert_eq!(generic_gamma(4), 8);
}
