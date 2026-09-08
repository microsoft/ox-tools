// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]

//! The complete SARIF 2.1.0 document, against a committed expected-output fixture (also called a
//! golden file) that was reviewed by a person.
//!
//! SARIF is a wire format for external tools. Every field whose wire name differs from its Rust
//! identifier has a hand-written serde rename — `information_uri` is `informationUri`, `rule_id`
//! is `ruleId` — and dropping one of those attributes produces a document that is still valid
//! JSON, still passes every assertion that inspects a field it happens to name, and is rejected or
//! silently misread by GitHub. The unit tests beside the emitter check the fields they are about;
//! nothing checked the shape as a whole.
//!
//! No SARIF schema document is vendored in this repository, and fetching one at test time would
//! make the suite depend on a network and on whatever is at the other end of it. The alternative
//! this takes is the same one the report emitter uses: a complete document, committed, reviewed by
//! hand, and compared field for field. Any rename, omission, addition, retype or value change fails
//! here until a maintainer intentionally updates the fixture after validating external-consumer
//! compatibility.

use camino::Utf8Path;
use cargo_gamma_lib::internals::ci::{Level, sarif};
use cargo_gamma_lib::internals::model::{MUTANT_ID_HEX_LEN, MUTANT_ID_VERSION, Mutant, Outcome, SiteIndex, mutant_id, normalize_site_text};
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

/// One log covering repeated rules, representative message forms, and every wire field.
fn emitted() -> Value {
    let mut arithmetic = mutant("/w/src/b.rs", 12, "arith.add_to_sub", Outcome::NoCoverage);
    arithmetic.original = "a + b".into();
    arithmetic.replacement = "a - b".into();

    let mut mutants = vec![
        mutant("/w/src/a.rs", 7, "relational.gt_to_ge", Outcome::Survived),
        arithmetic,
        mutant("/w/src/c.rs", 3, "relational.gt_to_ge", Outcome::Timeout),
    ];

    for mutant in &mut mutants {
        assign_production_identity(mutant);
    }

    let (text, truncation) = sarif(&mutants, &root(), Level::Warning).expect("the log serializes");

    assert!(truncation.is_none(), "the fixture is not near either cap");

    serde_json::from_str(&text).expect("the emitted log is not valid JSON")
}

/// Assigns the same content-addressed identity that discovery gives a production mutant.
fn assign_production_identity(mutant: &mut Mutant) {
    let file = Utf8Path::new(mutant.file.as_str())
        .strip_prefix(root())
        .expect("the SARIF fixture file is under its project root");
    let normalized = normalize_site_text(&mutant.original);
    let site = SiteIndex::new(mutant.occurrence, mutant.replacement_index);

    mutant.id = mutant_id(file, &mutant.item_path, &mutant.mutator, &normalized, site);
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
         intentionally update that fixture after checking that external consumers still accept it, \
         because every name in it is a wire-format contract."
    );
}

#[test]
fn fingerprint_values_have_the_production_identity_shape() {
    let log = emitted();
    let results = log["runs"][0]["results"]
        .as_array()
        .expect("the emitted SARIF log has a result array");

    for result in results {
        let fingerprint = result["partialFingerprints"]["gammaMutantId/v5"]
            .as_str()
            .expect("each result has a mutant fingerprint");

        assert_eq!(fingerprint.len(), MUTANT_ID_HEX_LEN);
        assert!(
            fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
    }
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
