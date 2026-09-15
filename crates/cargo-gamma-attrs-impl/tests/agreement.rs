// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(feature = "agreement")]

use cargo_gamma_attrs_impl::test_support;

#[test]
fn agreement_support_exposes_the_parser_contract() {
    let stream = "value.field".parse().expect("the fixture tokenizes");

    assert!(!test_support::exceeds_nesting_limit(&stream, test_support::NESTING_LIMIT));
    const {
        assert!(test_support::CHAIN_FACTOR > 0);
    }
    assert!(test_support::MOST_FACTOR.is_finite());

    let at_limit = format!(
        "{}value{}",
        "(".repeat(test_support::NESTING_LIMIT),
        ")".repeat(test_support::NESTING_LIMIT)
    );
    let over_limit = format!(
        "{}value{}",
        "(".repeat(test_support::NESTING_LIMIT + 1),
        ")".repeat(test_support::NESTING_LIMIT + 1)
    );
    assert!(!test_support::exceeds_nesting_limit(
        &at_limit.parse().expect("the boundary fixture tokenizes"),
        test_support::NESTING_LIMIT
    ));
    assert!(test_support::exceeds_nesting_limit(
        &over_limit.parse().expect("the over-limit fixture tokenizes"),
        test_support::NESTING_LIMIT
    ));
}
