// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

//! Exercises every exported attribute macro from an external consuming crate.
//!
//! A doctest inside this crate proves an expansion compiles; it does not prove the annotated item
//! survived intact, because a valid doctest's example is never called. These tests call the item
//! each macro annotates, so a shim that discarded the item, swapped its delegation for an
//! unrelated validator, or otherwise mangled a valid expansion fails here even when every doctest
//! still compiles.

use std::env::current_exe;
use std::process::Command;

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
    assert_eq!(killed(b"abc"), 96_354);
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

#[gamma::resource("cargo-subprocess")]
#[test]
fn resource_leaves_the_annotated_test_callable() {
    assert_eq!(2 + 2, 4);
}

#[gamma::resource("cargo-subprocess")]
mod alpha_resource_tests {
    #[test]
    fn callable() {}
}

#[gamma::resource("cargo-subprocess")]
mod beta_resource_tests {
    #[test]
    fn callable() {}
}

#[test]
#[cfg_attr(miri, ignore = "spawns the current test executable to inspect harness listing")]
fn resource_marker_is_exposed_to_harness_listing() {
    let executable = current_exe().expect("the test harness has a current executable");
    let output = Command::new(executable)
        .args(["--list", "--format", "terse"])
        .output()
        .expect("the test harness can list its tests");
    assert!(
        output.status.success(),
        "test listing failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let listing = String::from_utf8(output.stdout).expect("libtest emits UTF-8 test names");

    // The function marker is the protocol prefix plus lowercase hexadecimal UTF-8 bytes for the
    // resource, `_test_`, the function name encoded the same way, and libtest's `: test` suffix.
    assert!(
        listing.contains(
            "__cargo_gamma_resource_636172676f2d73756270726f63657373_test_\
             7265736f757263655f6c65617665735f7468655f616e6e6f74617465645f746573745f63616c6c61626c65: test"
        ),
        "{listing}"
    );
    for module in ["alpha_resource_tests", "beta_resource_tests"] {
        assert!(
            listing.contains(&format!(
                "{module}::__cargo_gamma_resource_636172676f2d73756270726f63657373_binary: test"
            )),
            "{listing}"
        );
    }
}
