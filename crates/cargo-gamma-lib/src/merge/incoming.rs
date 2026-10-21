// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A report in the shape the interchange schema requires, rather than the shape this tool writes.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use crate::elements::{FileResult, Framework, Report, RunInfo, Thresholds};

/// What a producer that did not name itself, or did not name its version, is recorded as.
///
/// [`Report`] has to carry something in both fields, because every document this tool writes has
/// them and the merged document is written through the same type. Naming this tool would be a
/// forgery — the input was read, not produced, here — and copying a name over from another input
/// would attribute one producer's report to another. Saying the metadata is unknown is the only
/// claim that stays true, and it is a claim a viewer can render.
const UNKNOWN: &str = "unknown";

/// A report as read back from a file.
///
/// `merge` is the one place this tool reads a document it did not necessarily write. The
/// interchange format is a published schema, so a report another producer emitted against it is a
/// legitimate input, and the schema requires only `schemaVersion`, `thresholds` and `files` at the
/// top level. Those three stay mandatory here, exactly as the schema has them; `framework` is
/// optional, and its `version` is optional even when the object is present.
///
/// [`Report`] makes both mandatory, because everything this tool writes has both, and decoding
/// an input straight into it turned a conforming document into "not a mutation report" over two
/// fields the merge never reads. This shape is the read path's own, so the writer's type keeps
/// stating what the writer guarantees while the reader accepts what the schema allows.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Incoming {
    /// The schema version the document claims to conform to.
    schema_version: String,

    /// The score bands the viewer colors by.
    thresholds: Thresholds,

    /// Absolute path the file keys are relative to.
    project_root: Option<String>,

    /// What produced the report, where the report says.
    framework: Option<IncomingFramework>,

    /// One entry per mutated file, keyed by workspace-relative path.
    files: BTreeMap<String, FileResult>,

    /// Free-form producer metadata.
    ///
    /// The interchange schema intentionally does not prescribe this object. It is decoded as JSON
    /// first, then interpreted as [`RunInfo`] only when it has cargo-gamma's shape.
    config: Option<Value>,
}

/// What produced a report, with the version the schema leaves optional.
#[derive(Debug, Deserialize)]
struct IncomingFramework {
    /// The tool name, which the schema requires of any framework object that is present.
    name: String,

    /// The tool version, which the schema does not require.
    version: Option<String>,
}

impl From<Incoming> for Report {
    fn from(incoming: Incoming) -> Self {
        // Absence is recorded rather than papered over: a merged document that named this tool as
        // the producer of an input it only read would be a lie that outlives the merge, since the
        // document is what a viewer shows.
        let framework = incoming.framework.map_or_else(
            || Framework {
                name: UNKNOWN.to_owned(),
                version: UNKNOWN.to_owned(),
            },
            |framework| Framework {
                name: framework.name,
                version: framework.version.unwrap_or_else(|| UNKNOWN.to_owned()),
            },
        );
        let config = incoming.config.and_then(|config| serde_json::from_value::<RunInfo>(config).ok());

        Self {
            schema_version: incoming.schema_version,
            thresholds: incoming.thresholds,
            project_root: incoming.project_root,
            framework,
            files: incoming.files,
            config,
        }
    }
}

impl Incoming {
    /// Whether this document names a schema version the interchange format supports.
    ///
    /// This is kept on the input shape so a rejected version never becomes a `Report`: a
    /// `Report` is a document this crate is willing to render or write.
    pub(super) fn has_supported_schema_version(&self) -> bool {
        crate::elements::supported_schema_version(&self.schema_version)
    }

    pub(super) fn schema_version(&self) -> &str {
        &self.schema_version
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod survivor_tests {
    use super::*;

    #[test]
    fn survivor_incoming_schema_accessors_preserve_the_version_and_the_validators_closed_major_set() {
        for version in ["1", "1.0", "1.99.123", "2", "2.0.0", "3", "2.01", "2.0.0.0", ""] {
            let document = serde_json::json!({
                "schemaVersion": version,
                "thresholds": { "high": 100, "low": 0 },
                "files": {}
            });
            let incoming: Incoming = serde_json::from_value(document.clone()).unwrap();
            assert_eq!(incoming.schema_version(), version);
            let supported = matches!(version, "1" | "1.0" | "1.99.123" | "2" | "2.0.0");
            assert_eq!(incoming.has_supported_schema_version(), supported);
            assert_eq!(crate::elements::validate_schema(&document).is_ok(), supported);
        }
    }

    #[test]
    fn survivor_schema_valid_large_positions_expose_only_the_native_usize_deserialization_boundary() {
        let mut document = serde_json::json!({
            "schemaVersion": "2",
            "thresholds": { "high": 100, "low": 0 },
            "files": {
                "src/lib.rs": {
                    "source": "",
                    "language": "rust",
                    "mutants": [{
                        "id": "deadbeefcafe",
                        "mutatorName": "external",
                        "status": "Survived",
                        "location": {
                            "start": { "line": u64::MAX, "column": 1 },
                            "end": { "line": u64::MAX, "column": 1 }
                        }
                    }]
                }
            }
        });
        crate::elements::validate_schema(&document).unwrap();
        #[cfg(target_pointer_width = "64")]
        {
            let incoming: Incoming = serde_json::from_value(document.clone()).unwrap();
            assert_eq!(incoming.files["src/lib.rs"].mutants[0].location.start.line, usize::MAX);
        }
        #[cfg(target_pointer_width = "32")]
        serde_json::from_value::<Incoming>(document.clone()).expect_err("a schema-valid u64 position can exceed native usize");
        document["files"]["src/lib.rs"]["mutants"][0]["location"]["start"]["line"] = serde_json::json!(1);
        document["files"]["src/lib.rs"]["mutants"][0]["location"]["end"]["line"] = serde_json::json!(1);
        let document: Incoming = serde_json::from_value(document).unwrap();
        assert_eq!(document.files["src/lib.rs"].mutants[0].location.start.line, 1);
    }
}
