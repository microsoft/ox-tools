// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]

//! The complete SARIF 2.1.0 document, against a golden that was read by a person.
//!
//! SARIF is a wire format for other people's tools. Every field name in it is a hand-written serde
//! rename over a struct whose Rust name is different — `information_uri` is `informationUri`,
//! `rule_id` is `ruleId` — and dropping one of those attributes produces a document that is still
//! valid JSON, still passes every assertion that inspects a field it happens to name, and is
//! rejected or silently misread by GitHub. The unit tests beside the emitter check the fields they
//! are about; nothing checked the shape as a whole.
//!
//! No SARIF schema document is vendored in this repository, and fetching one at test time would
//! make the suite depend on a network and on whatever is at the other end of it. The alternative
//! this takes is the same one the report emitter uses: a complete document, committed, reviewed by
//! hand, and compared field for field. Any rename, omission, addition, retype or value change fails
//! here until somebody deliberately re-blesses the file.

use cargo_gamma_lib::internals::ci::{Level, sarif};
use cargo_gamma_lib::internals::model::{MUTANT_ID_VERSION, Outcome};
use cargo_gamma_lib::testing::ci_fixture::{mutant, root};
use serde_json::Value;

/// A reviewed complete log, committed rather than produced.
const GOLDEN: &str = include_str!("fixtures/sarif.golden.json");

/// What the golden carries where the driver version goes.
///
/// The version is this crate's own and changes at every release, which would make the golden fail
/// for a reason that has nothing to do with the wire shape. It is checked separately, against the
/// value the emitter is supposed to report, so nothing about the field goes unasserted.
const VERSION_PLACEHOLDER: &str = "0.0.0-golden";

/// One log with two rules, three results, three of the four message forms, and every wire field
/// populated.
fn emitted() -> Value {
    let mut arithmetic = mutant("/w/src/b.rs", 12, "arith.add_to_sub", Outcome::NoCoverage);
    arithmetic.original = "a + b".into();
    arithmetic.replacement = "a - b".into();

    let mutants = vec![
        mutant("/w/src/a.rs", 7, "relational.gt_to_ge", Outcome::Survived),
        arithmetic,
        mutant("/w/src/c.rs", 3, "relational.gt_to_ge", Outcome::Timeout),
    ];

    let (text, truncation) = sarif(&mutants, &root(), Level::Warning).expect("the log serializes");

    assert!(truncation.is_none(), "three findings are not near either cap");

    serde_json::from_str(&text).expect("the emitted log is not valid JSON")
}

#[test]
fn the_emitted_log_matches_the_committed_one_field_for_field() {
    let expected: Value = serde_json::from_str(GOLDEN).expect("the golden log is not valid JSON");
    let mut actual = emitted();

    let version = actual["runs"][0]["tool"]["driver"]["version"].take();

    assert_eq!(
        version.as_str(),
        Some(env!("CARGO_PKG_VERSION")),
        "the driver must report this crate's version"
    );

    actual["runs"][0]["tool"]["driver"]["version"] = Value::String(VERSION_PLACEHOLDER.to_owned());

    assert_eq!(
        actual, expected,
        "the SARIF log no longer matches `tests/fixtures/sarif.golden.json`. If the change is intended, \
         re-bless that file — and check first that a consumer will still accept it, because every name \
         in it is a contract with somebody else's tool."
    );
}

/// The fingerprint key carries the identity scheme's version, and the golden spells it out.
///
/// Written literally in the golden so that a change to the scheme fails here rather than passing a
/// comparison that computed both sides from the same constant. This test is what says the literal is
/// the constant, so the two can only drift on purpose.
#[test]
fn the_fingerprint_key_names_the_current_identity_version() {
    let expected = format!("gammaMutantId/v{MUTANT_ID_VERSION}");

    assert!(
        GOLDEN.contains(&format!("\"{expected}\"")),
        "the golden log does not use `{expected}`; the mutant identity version changed, which retires \
         every alert GitHub is currently holding and must be a deliberate act"
    );
}
