// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Neutral starting values for the record types the unit tests build by hand.
//!
//! [`Mutant`] has twenty-one fields and [`Report`] has six, but a test is usually about two
//! or three of them. Writing the literal out in full at every site buries those under the
//! filler and means a new field has to be added to every `mod tests` in the crate.
//!
//! Each function here returns a value whose fields are all neutral, to be adjusted with struct
//! update syntax so a test states only what it varies:
//!
//! ```ignore
//! let mutant = Mutant { line: 7, outcome: Outcome::Survived, ..fixtures::mutant() };
//! ```
//!
//! Gated on `cfg(test)` rather than living in [`crate::testing`]: that module is `pub`, so anything
//! added to it joins the crate's API, and these are only ever wanted from a `mod tests` block.

use std::collections::HashMap;

use camino::Utf8PathBuf;

use crate::elements::{FileResult, Framework, Location, MutantResult, Position, Report, RunInfo, ShardInfo, Thresholds};
use crate::model::{Mutant, Outcome};
use crate::ops::collect::Shape;

pub(crate) const UNRESOLVED_LINK_SOURCE: &str = "unsafe extern \"C\" {\n\
    \x20\x20\x20\x20fn gamma_absent_symbol() -> i32;\n\
    }\n\n\
    pub fn less(a: i32, b: i32) -> bool { a < b }\n\n\
    pub fn touch() -> i32 { unsafe { gamma_absent_symbol() } }\n\n\
    #[test]\n\
    fn calls_it() {\n\
    \x20\x20\x20\x20assert!(less(1, 2));\n\
    \x20\x20\x20\x20assert_eq!(touch(), 0);\n\
    }\n";

/// A killed `relational.lt_to_le` mutant of `a` to `b`, at line 1 of `src/lib.rs` in `subject`.
pub(crate) fn mutant() -> Mutant {
    Mutant {
        id: "m1".to_owned().into(),
        ordinal: 1,
        file: Utf8PathBuf::from("src/lib.rs").into(),
        package: "subject".to_owned().into(),
        span: 0..1,
        line: 1,
        end_line: 1,
        column: 1,
        mutator: "relational.lt_to_le".to_owned().into(),
        item_path: "f".to_owned().into(),
        occurrence: 0,
        replacement_index: 0,
        original: "a".to_owned().into(),
        replacement: "b".to_owned().into(),
        shape: Shape::Expr,
        outcome: Outcome::Killed,
        suppression: None,
        expectation: None,
        test_timeout_multiplier: None,
        elapsed_ms: 0,
        killed_by: None,
        note: None,
    }
}

pub(crate) fn mutant_at(id: &str, file: &str, line: usize, mutator: &str, outcome: Outcome) -> Mutant {
    Mutant {
        id: id.to_owned().into(),
        file: Utf8PathBuf::from(file).into(),
        line,
        mutator: mutator.to_owned().into(),
        outcome,
        ..mutant()
    }
}

/// The reported form of [`mutant`], spanning columns 1 to 9 of line 1.
pub(crate) fn mutant_result() -> MutantResult {
    MutantResult {
        id: "m1".to_owned().into(),
        mutator_name: "relational.lt_to_le".into(),
        location: Location {
            start: Position { line: 1, column: 1 },
            end: Position { line: 1, column: 9 },
        },
        status: "Killed".into(),
        replacement: None,
        description: None,
        status_reason: None,
        duration: None,
        killed_by: None,
    }
}

pub(crate) fn mutant_result_at(id: &str, line: usize, status: &str) -> MutantResult {
    MutantResult {
        id: id.to_owned().into(),
        location: Location {
            start: Position { line, column: 1 },
            end: Position { line, column: 9 },
        },
        status: status.into(),
        ..mutant_result()
    }
}

/// An empty schema-2 report from this tool, with no run info and no files.
pub(crate) fn report() -> Report {
    Report {
        schema_version: "2".to_owned(),
        thresholds: Thresholds::default(),
        project_root: None,
        framework: Framework {
            name: "cargo-gamma".to_owned(),
            version: "0.1.0".to_owned(),
        },
        files: HashMap::default(),
        config: None,
    }
}

pub(crate) fn report_with(shard: Option<(u32, u32)>, started_at: u64, mutants: Vec<MutantResult>) -> Report {
    let mut files = HashMap::default();

    let _ = files.insert(
        "src/lib.rs".to_owned(),
        FileResult {
            source: "fn f() {}\n".to_owned(),
            language: "rust".to_owned(),
            mutants,
        },
    );

    Report {
        files,
        config: Some(RunInfo {
            started_at,
            merged: false,
            shard: shard.map(|(index, count)| ShardInfo { index, count }),
            tests: None,
            not_built: None,
            dropped_test_packages: Vec::new(),
            merge_provenance: None,
        }),
        ..report()
    }
}

pub(crate) fn crate_dir(name: &str, source: &str) -> (tempfile::TempDir, Utf8PathBuf) {
    let dir = crate::testing::workdir(name);
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");

    std::fs::create_dir(root.join("src")).expect("src");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"subject\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
    )
    .expect("manifest");
    std::fs::write(root.join("src/lib.rs"), source).expect("lib");

    (dir, root)
}
