// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]

//! Validates that the emitted report conforms to the published `mutation-testing-elements` schema.
//!
//! The schema is an artifact of another project, so drift is silent: a renamed field or a newly
//! required one does not break a build, it produces a blank page in someone's browser weeks later.
//! This gate reads the vendored schema document and checks the emitter against it, so the failure
//! arrives here instead.
//!
//! It deliberately checks the constraints that can actually break a report — required fields, the
//! closed status enum and the version pattern — rather than reimplementing JSON Schema. A full
//! validator would cost two hundred transitive dependencies to catch cases the emitter cannot
//! produce.

use std::collections::HashMap;

use cargo_gamma_lib::internals::elements::{
    FileResult, Framework, Location, MutantResult, Position, Report, RunInfo, ShardInfo, Thresholds, to_json,
};
use serde_json::Value;

/// The schema document, vendored beside the viewer it describes.
const SCHEMA: &str = include_str!("../src/vendor/mutation-testing-report-schema.json");

/// Parses the vendored schema.
fn schema() -> Value {
    serde_json::from_str(SCHEMA).expect("the vendored schema is not valid JSON")
}

/// Returns the string entries of an array-valued key.
fn strings(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .expect("the schema has no array at this key")
        .iter()
        .map(|entry| entry.as_str().expect("a non-string entry").to_owned())
        .collect()
}

/// Navigates to the `MutantResult` subschema.
fn mutant_schema(root: &Value) -> &Value {
    &root["properties"]["files"]["additionalProperties"]["properties"]["mutants"]["items"]
}

/// Builds a report exercising every field the emitter can produce.
///
/// Two files and two mutants, deliberately: one mutant carries every optional field and the other
/// carries none of them, so that the golden document pins both what is emitted and what is left
/// out. A single fully-populated mutant would let a `skip_serializing_if` be dropped unnoticed.
fn sample() -> Report {
    let populated = MutantResult {
        id: "abc123abc123".to_owned().into(),
        mutator_name: "relational.lt_to_le".into(),
        location: Location {
            start: Position { line: 2, column: 5 },
            end: Position { line: 2, column: 10 },
        },
        status: "Survived".into(),
        replacement: Some("(a) <= (b)".into()),
        description: Some("replace a < b with (a) <= (b)".to_owned()),
        status_reason: Some("suppressed by comment".to_owned()),
        duration: Some(12.0),
        killed_by: Some(vec!["tests::boundary".to_owned()]),
    };

    let bare = MutantResult {
        id: "def456def456".to_owned().into(),
        mutator_name: "arithmetic.add_to_sub".into(),
        location: Location {
            start: Position { line: 9, column: 1 },
            end: Position { line: 9, column: 6 },
        },
        status: "Killed".into(),
        replacement: None,
        description: None,
        status_reason: None,
        duration: None,
        killed_by: None,
    };

    let mut files = HashMap::new();

    let _ = files.insert(
        "src/lib.rs".to_owned(),
        FileResult {
            source: "fn f() {}\n".to_owned(),
            language: "rust".to_owned(),
            mutants: vec![populated],
        },
    );

    let _ = files.insert(
        "src/other.rs".to_owned(),
        FileResult {
            source: "fn g() -> u8 { 1 + 1 }\n".to_owned(),
            language: "rust".to_owned(),
            mutants: vec![bare],
        },
    );

    Report {
        schema_version: "2".to_owned(),
        thresholds: Thresholds::default(),
        project_root: Some("/work".to_owned()),
        framework: Framework {
            name: "cargo-gamma".to_owned(),
            // A literal rather than `CARGO_PKG_VERSION`, so that the golden document does not have
            // to be re-blessed on every release for a reason that has nothing to do with the shape
            // of the report.
            version: "0.1.0".to_owned(),
        },
        files: files.into_iter().collect(),
        config: Some(RunInfo {
            started_at: 1_700_000_000,
            merged: false,
            shard: Some(ShardInfo { index: 1, count: 4 }),
            tests: Some(37),
            not_built: Some(2),
            dropped_test_packages: vec!["awkward-fixtures".to_owned()],
            merge_provenance: None,
        }),
    }
}

/// Serializes the sample report as a JSON value.
fn emitted() -> Value {
    serde_json::from_str(&to_json(&sample()).expect("serializes")).expect("emits valid JSON")
}

#[test]
fn the_document_has_every_top_level_required_field() {
    let root = schema();
    let document = emitted();

    for field in strings(&root, "required") {
        assert!(document.get(&field).is_some(), "the report is missing `{field}`");
    }
}

#[test]
fn every_file_entry_has_the_required_fields() {
    let root = schema();
    let document = emitted();
    let required = strings(&root["properties"]["files"]["additionalProperties"], "required");

    for (path, file) in document["files"].as_object().expect("files is an object") {
        for field in &required {
            assert!(file.get(field).is_some(), "`{path}` is missing `{field}`");
        }
    }
}

#[test]
fn every_mutant_has_the_required_fields() {
    let root = schema();
    let document = emitted();
    let required = strings(mutant_schema(&root), "required");

    for file in document["files"].as_object().expect("files is an object").values() {
        for mutant in file["mutants"].as_array().expect("mutants is an array") {
            for field in &required {
                assert!(mutant.get(field).is_some(), "a mutant is missing `{field}`: {mutant}");
            }
        }
    }
}

#[test]
fn every_status_we_emit_is_in_the_schemas_closed_enum() {
    // `MutantStatus` has no room for invention. A value outside this list is rejected by the
    // viewer, which renders as an empty report rather than as an error anyone can read.
    let root = schema();
    let allowed = strings(&mutant_schema(&root)["properties"]["status"], "enum");

    for outcome in ["Pending", "Killed", "Survived", "Timeout", "CompileError", "Ignored", "NoCoverage"] {
        assert!(
            allowed.contains(&outcome.to_owned()),
            "`{outcome}` is not in the schema's status enum: {allowed:?}"
        );
    }
}

#[test]
fn the_emitted_schema_version_matches_the_schemas_own_pattern() {
    // The npm package is at 3.x while the schema accepts major 1 and 2 only. Emitting "3" fails
    // validation for a reason that looks exactly like a version-skew bug and is not.
    let root = schema();
    let pattern = root["properties"]["schemaVersion"]["pattern"]
        .as_str()
        .expect("the schema has no version pattern");

    assert_eq!(pattern, r"^([1-2])(\.(([1-9]\d*)|0)){0,2}$");

    let emitted = emitted();
    let version = emitted["schemaVersion"].as_str().expect("a version is emitted");

    assert!(
        matches!(version, "1" | "2") || version.starts_with("1.") || version.starts_with("2."),
        "`{version}` does not satisfy `{pattern}`"
    );
}

#[test]
fn the_optional_fields_we_emit_are_ones_the_schema_knows() {
    // Extension fields are allowed — no object in the schema sets `additionalProperties: false` —
    // but a *misspelled* known field would be silently accepted and silently ignored, which is the
    // failure this catches.
    let root = schema();
    let known = mutant_schema(&root)["properties"]
        .as_object()
        .expect("the mutant schema has properties");
    let document = emitted();

    for file in document["files"].as_object().expect("files is an object").values() {
        for mutant in file["mutants"].as_array().expect("mutants is an array") {
            for field in mutant.as_object().expect("a mutant is an object").keys() {
                assert!(known.contains_key(field), "`{field}` is not a schema field");
            }
        }
    }
}

/// A committed document, reviewed by hand, that the emitter had no part in producing.
///
/// Everything above validates the emitter against a schema vendored beside it, which cannot catch a
/// rename made in both at once, and checks presence rather than shape, which cannot catch `duration`
/// turning from a number into a string. This is the non-circular half: the bytes below were read by
/// a person and committed, so any change to a field's name, type, value or presence fails here until
/// somebody deliberately re-blesses them.
const GOLDEN: &str = include_str!("fixtures/report.golden.json");

#[test]
fn the_emitted_document_matches_the_committed_one_field_for_field() {
    let expected: Value = serde_json::from_str(GOLDEN).expect("the golden report is not valid JSON");
    let actual = emitted();

    assert_eq!(
        actual,
        expected,
        "the report no longer matches `tests/fixtures/report.golden.json`.\n\
         If the change is intended, replace that file with:\n{}",
        to_json(&sample()).expect("serializes")
    );
}
