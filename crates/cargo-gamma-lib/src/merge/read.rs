// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Reading a report document back from disk.

use std::collections::BTreeMap;
#[cfg(test)]
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Result as IoResult};
use std::time::{SystemTime, UNIX_EPOCH};

use camino::Utf8Path;
use serde_json::Value;

use super::incoming::Incoming;
use super::merged::MAX_SHARDS;
use super::status::scoring;
use crate::Result;
use crate::elements::Report;
use crate::error::error;

/// The largest report a merge will read.
///
/// The same argument as [`MAX_SHARDS`], applied to the one input dimension the decode leaves
/// unbounded: nesting depth is capped by the JSON decoder and the rotation size is capped
/// explicitly, so a file's length is what is left. The read holds three live copies of the document
/// at its peak — the text, the decoded shape, and the report built from it — and the realistic route
/// to an enormous one is a directory of `.json` files that happens to contain something else
/// entirely, since `merge` expands a directory to everything in it. A quarter of a gigabyte is two
/// orders of magnitude above any report this tool writes for a real workspace, so the bound turns an
/// unexplained kill by the OOM killer into a message naming the file.
const MAX_BYTES: u64 = 256 * 1024 * 1024;

/// How far ahead of this machine's clock an input's start time may sit.
///
/// A start time is not a decoration: it is the primary key the merge ranks verdicts by, so a report
/// claiming a far-future run wins every mutant it mentions, supplies the population that decides
/// what has been withdrawn, and supplies the source text the merged document renders. It also
/// defeats the freshness accounting, since the age of a verdict from the future saturates to zero
/// and it is reported as current however old it is. One skewed runner can therefore decide a whole
/// rotation.
///
/// An hour is far more skew than a synchronized fleet ever shows and far less than the days a
/// rotation spans, so it separates a clock that is drifting from one that is wrong, without
/// refusing a report over the second or two of skew that is normal between machines.
const MAX_SKEW: u64 = 3_600;

/// Reads a report from a file.
///
/// The document is decoded through [`Incoming`], the shape the interchange schema actually
/// requires, so a conforming report from another producer is accepted rather than refused over
/// metadata the merge never reads. Everything the schema makes mandatory stays mandatory, and a
/// document missing one of those fields, or stating one in the wrong shape, is still not a mutation
/// report.
///
/// Every number the merge will later count up to, index into, or rank by is checked here, because
/// this is where they arrive from outside. Refusing an impossible one at the file it came from costs
/// a comparison and names the input; letting it through costs an allocation proportional to a number
/// an input chose, a rotation coverage figure that can read 100% with a shard missing, or a score
/// computed from a word nobody can interpret.
#[cfg(test)]
fn read(path: &Utf8Path) -> Result<Report> {
    read_limited(path, MAX_BYTES).map(|input| input.report)
}

/// The report and input bytes retained by a caller that collects several reports.
pub(crate) struct ReadReport {
    pub(crate) report: Report,
    pub(crate) bytes: u64,
}

/// Reads one regular report while retaining no more than `limit` input bytes.
///
/// `limit` lets a directory collector enforce its aggregate budget before it retains the decoded
/// report. The per-report ceiling remains in force even when the collector has more room.
pub(crate) fn read_limited(path: &Utf8Path, limit: u64) -> Result<ReadReport> {
    let mut input = open(path).map_err(|cause| error!("could not read `{path}`").caused_by(cause))?;
    let metadata = input
        .metadata()
        .map_err(|cause| error!("could not read `{path}`").caused_by(cause))?;

    #[expect(
        clippy::filetype_is_file,
        reason = "a merge report must be a regular file, not merely something other than a directory"
    )]
    if !metadata.file_type().is_file() {
        return Err(error!("{path} is not a regular file, so it cannot be a merge report").usage());
    }

    let limit = limit.min(MAX_BYTES);
    let size = metadata.len();

    if size > limit {
        return Err(too_large(path, size, limit));
    }

    let (text, bytes_read) = read_contents(&mut input, path, limit)?;

    let document: Value =
        serde_json::from_str(&text).map_err(|cause| error!("{path} is not a mutation report").caused_by(cause).usage())?;

    // Drop the source text now that it has been parsed — the Value owns all needed strings.
    drop(text);

    crate::elements::validate_schema(&document).map_err(|cause| error!("{path} is not a mutation report: {cause}").usage())?;

    let incoming: Incoming =
        serde_json::from_value(document).map_err(|cause| error!("{path} is not a mutation report").caused_by(cause).usage())?;

    if !incoming.has_supported_schema_version() {
        return Err(error!(
            "{path} declares unsupported schema version `{}`; merge accepts only schema versions 1 and 2",
            incoming.schema_version()
        )
        .usage());
    }

    let report = Report::from(incoming);
    let mut ids: BTreeMap<&str, &str> = BTreeMap::new();

    for (file, result) in &report.files {
        for mutant in &result.mutants {
            if let Some(first_file) = ids.insert(mutant.id.as_str(), file.as_str()) {
                return Err(error!(
                    "{path} repeats mutant ID `{}` in `{first_file}` and `{file}`; mutant IDs must be unique across a report",
                    mutant.id
                )
                .usage());
            }
        }
    }

    if let Some(shard) = report.config.as_ref().and_then(|config| config.shard) {
        if shard.count > MAX_SHARDS {
            return Err(error!(
                "{path} claims a rotation of {} shards, more than the {MAX_SHARDS} a merge will account for",
                shard.count
            )
            .usage());
        }

        // The pair is one contract, and half of it enforced is none of it: `shards_seen` is counted
        // against `count` and the missing shards are enumerated from `0..count`, so an index outside
        // the rotation makes both figures meaningless. Two inputs at count 2 with indices 0, 1 and 7
        // report three of two shards seen and 150% of the rotation; indices 0 and 5 report 100% of
        // the rotation on one line and a shard never run on the next.
        if shard.index >= shard.count {
            return Err(error!(
                "{path} claims shard {} of a rotation of {}, which is outside it",
                shard.index, shard.count
            )
            .usage());
        }
    }

    // A status outside the schema's closed set is refused rather than guessed at. The merge scores
    // by status, so a corrupt or misspelled one — `Kiled` for `Killed` — silently became an
    // undetected mutant and moved the score of a rotation the reader had no reason to distrust.
    // The offender is chosen by name rather than by whichever the map yields first, so the same
    // document always produces the same message.
    let undefined = report
        .files
        .iter()
        .flat_map(|(file, result)| result.mutants.iter().map(move |mutant| (file, mutant)))
        .filter(|(_, mutant)| scoring(&mutant.status).is_none())
        .min_by_key(|(file, mutant)| (*file, &mutant.id));

    if let Some((file, mutant)) = undefined {
        return Err(error!(
            "{path} gives mutant `{}` in `{file}` the status `{}`, which a mutation report cannot carry",
            mutant.id, mutant.status
        )
        .usage());
    }

    if let Some(config) = report.config.as_ref()
        // A clock this machine cannot read is not evidence about the input's, so a system time
        // before the epoch skips the comparison rather than refusing every report on the disk.
        && let Some(now) = SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|since| since.as_secs())
        && config.started_at > now.saturating_add(MAX_SKEW)
    {
        return Err(error!(
            "{path} says its run started at {}, {} seconds ahead of this machine's clock and beyond the {MAX_SKEW} a merge allows",
            config.started_at,
            config.started_at.saturating_sub(now)
        )
        .usage());
    }

    Ok(ReadReport { report, bytes: bytes_read })
}

/// Opens without waiting for a FIFO writer, so its file type can be rejected from this handle.
fn open(path: &Utf8Path) -> IoResult<File> {
    let mut options = OpenOptions::new();
    let _read = options.read(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        let _nonblocking = options.custom_flags(libc::O_NONBLOCK);
    }

    options.open(path)
}

fn too_large(path: &Utf8Path, size: u64, limit: u64) -> crate::error::Error {
    error!(
        "{path} is at least {size} bytes, more than the {limit} bytes a merge will retain; \
         use a smaller report or split the inputs into separate merges"
    )
    .usage()
}

/// Reads one already checked handle without ever allocating more than the caller's cap.
fn read_contents(input: &mut File, path: &Utf8Path, limit: u64) -> Result<(String, u64)> {
    let mut bytes = Vec::new();
    let mut capped = input.by_ref().take(limit.saturating_add(1));

    let _read = capped
        .read_to_end(&mut bytes)
        .map_err(|cause| error!("could not read `{path}`").caused_by(cause))?;

    let bytes_read = u64::try_from(bytes.len()).map_err(|cause| error!("could not read `{path}`").caused_by(cause))?;

    if bytes_read > limit {
        return Err(too_large(path, bytes_read, limit));
    }

    let text = String::from_utf8(bytes).map_err(|cause| error!("could not read `{path}`").caused_by(cause))?;

    Ok((text, bytes_read))
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use camino::Utf8PathBuf;

    use super::*;
    use crate::testing::workdir;

    /// A whole document, with every optional field present, as this tool writes one.
    const COMPLETE: &str = r#"{
        "schemaVersion": "2",
        "thresholds": { "high": 80, "low": 60 },
        "projectRoot": "/w",
        "framework": { "name": "other-tool", "version": "9.9" },
        "files": {
            "src/lib.rs": {
                "source": "fn f() {}\n",
                "language": "rust",
                "mutants": [
                    { "id": "aaa", "mutatorName": "m", "status": "Killed",
                      "location": { "start": { "line": 1, "column": 1 }, "end": { "line": 1, "column": 2 } } }
                ]
            }
        },
        "config": { "startedAt": 100, "shard": { "index": 0, "count": 4 }, "tests": 7, "notBuilt": 1, "droppedTestPackages": ["helper"] }
    }"#;

    fn written(text: &str) -> (tempfile::TempDir, Utf8PathBuf) {
        let directory = workdir("merge-read-");
        let path = Utf8PathBuf::from_path_buf(directory.path().join("report.json")).expect("a utf-8 path");

        fs::write(&path, text).expect("the report should be writable");
        (directory, path)
    }

    fn read_text(text: &str) -> Result<Report> {
        let (_directory, path) = written(text);

        read(&path)
    }

    /// The schema requires `schemaVersion`, `thresholds` and `files`, and nothing else at the top
    /// level, so a document carrying only those three is a mutation report.
    #[test]
    fn a_report_without_a_framework_is_still_a_mutation_report() {
        let report = read_text(
            r#"{
                "schemaVersion": "2",
                "thresholds": { "high": 80, "low": 60 },
                "files": {
                    "src/lib.rs": {
                        "source": "fn f() {}\n",
                        "language": "rust",
                        "mutants": [
                            { "id": "aaa", "mutatorName": "m", "status": "Killed",
                              "location": { "start": { "line": 1, "column": 1 }, "end": { "line": 1, "column": 2 } } }
                        ]
                    }
                }
            }"#,
        )
        .expect("a schema-conforming report");

        assert_eq!(report.files["src/lib.rs"].mutants[0].id, "aaa");

        // Absence is recorded as absence: nothing here claims this tool, or any other, produced it.
        assert_eq!(report.framework.name, "unknown");
        assert_eq!(report.framework.version, "unknown");
    }

    /// `version` is optional even when the framework object is present, and the name that *is*
    /// there survives the read.
    #[test]
    fn a_framework_without_a_version_keeps_its_name() {
        let report = read_text(
            r#"{
                "schemaVersion": "2",
                "thresholds": { "high": 80, "low": 60 },
                "framework": { "name": "stryker" },
                "files": {}
            }"#,
        )
        .expect("a schema-conforming report");

        assert_eq!(report.framework.name, "stryker");
        assert_eq!(report.framework.version, "unknown");
    }

    #[test]
    fn unrelated_free_form_config_does_not_make_a_report_invalid() {
        let report = read_text(
            r#"{
                "schemaVersion": "2",
                "thresholds": { "high": 80, "low": 60 },
                "config": {
                    "producer": "other-tool",
                    "run": { "opaque": true }
                },
                "files": {}
            }"#,
        )
        .expect("a schema-conforming report with another producer's config");

        assert!(report.config.is_none(), "only cargo-gamma run metadata is retained");
    }

    #[test]
    fn repeated_ids_are_rejected_within_and_across_files() {
        for files in [
            r#""a.rs":{"source":"","language":"rust","mutants":[
                {"id":"same","mutatorName":"m","status":"Killed","location":{"start":{"line":1,"column":1},"end":{"line":1,"column":2}}},
                {"id":"same","mutatorName":"m","status":"Survived","location":{"start":{"line":2,"column":1},"end":{"line":2,"column":2}}}
            ]}"#,
            r#""a.rs":{"source":"","language":"rust","mutants":[
                {"id":"same","mutatorName":"m","status":"Killed","location":{"start":{"line":1,"column":1},"end":{"line":1,"column":2}}}
            ]},"b.rs":{"source":"","language":"rust","mutants":[
                {"id":"same","mutatorName":"m","status":"Survived","location":{"start":{"line":1,"column":1},"end":{"line":1,"column":2}}}
            ]}"#,
        ] {
            let error = read_text(&format!(
                r#"{{"schemaVersion":"2","thresholds":{{"high":80,"low":60}},"files":{{{files}}}}}"#
            ))
            .expect_err("duplicate ID")
            .to_string();

            assert!(error.contains("same"), "{error}");
            assert!(error.contains("unique across a report"), "{error}");
        }
    }

    #[test]
    fn supported_schema_versions_are_accepted() {
        for version in ["1", "1.0", "1.42.7", "2", "2.0", "2.999.1"] {
            let report = read_text(&format!(
                r#"{{
                    "schemaVersion": "{version}",
                    "thresholds": {{ "high": 80, "low": 60 }},
                    "files": {{}}
                }}"#
            ))
            .expect(version);

            assert_eq!(report.schema_version, version);
        }
    }

    #[test]
    fn unsupported_or_malformed_schema_versions_are_refused() {
        for version in ["0", "3", "01", "1.", "1.0.0.0", "2.01", "2.0-beta", "two"] {
            let error = read_text(&format!(
                r#"{{
                    "schemaVersion": "{version}",
                    "thresholds": {{ "high": 80, "low": 60 }},
                    "files": {{}}
                }}"#
            ))
            .expect_err(version)
            .to_string();

            assert!(error.contains(version), "{error}");
            assert!(error.contains("schema version"), "{error}");
        }
    }

    #[test]
    fn fractional_mutant_durations_survive_schema_valid_ingress() {
        let report = read_text(
            r#"{
                "schemaVersion": "2",
                "thresholds": { "high": 80, "low": 60 },
                "files": {
                    "src/lib.rs": {
                        "source": "fn f() {}\n",
                        "language": "rust",
                        "mutants": [
                            { "id": "fraction", "mutatorName": "m", "status": "Killed", "duration": 1.25,
                              "location": { "start": { "line": 1, "column": 1 }, "end": { "line": 1, "column": 2 } } }
                        ]
                    }
                }
            }"#,
        )
        .expect("a fractional schema duration is valid");

        assert_eq!(report.files["src/lib.rs"].mutants[0].duration, Some(1.25));
    }

    #[test]
    fn schema_bounds_are_enforced_before_a_report_is_merged() {
        let invalid = [
            (
                "high threshold",
                r#"{ "schemaVersion": "2", "thresholds": { "high": 101, "low": 60 }, "files": {} }"#,
            ),
            (
                "low threshold",
                r#"{ "schemaVersion": "2", "thresholds": { "high": 80, "low": 101 }, "files": {} }"#,
            ),
            (
                "zero line",
                r#"{
                    "schemaVersion": "2", "thresholds": { "high": 80, "low": 60 },
                    "files": { "src/lib.rs": { "source": "", "language": "rust", "mutants": [
                        { "id": "m", "mutatorName": "m", "status": "Killed",
                          "location": { "start": { "line": 0, "column": 1 }, "end": { "line": 1, "column": 1 } } }
                    ] } }
                }"#,
            ),
            (
                "zero column",
                r#"{
                    "schemaVersion": "2", "thresholds": { "high": 80, "low": 60 },
                    "files": { "src/lib.rs": { "source": "", "language": "rust", "mutants": [
                        { "id": "m", "mutatorName": "m", "status": "Killed",
                          "location": { "start": { "line": 1, "column": 1 }, "end": { "line": 1, "column": 0 } } }
                    ] } }
                }"#,
            ),
        ];

        for (what, document) in invalid {
            let error = read_text(document).expect_err(what).to_string();

            assert!(error.contains("not a mutation report"), "{what}: {error}");
        }
    }

    #[test]
    fn invalid_optional_schema_fields_are_not_silently_ignored() {
        let error = read_text(
            r#"{
                "schemaVersion": "2",
                "thresholds": { "high": 80, "low": 60 },
                "performance": { "setup": 1, "initialRun": 2, "mutation": "three" },
                "files": {}
            }"#,
        )
        .expect_err("a malformed known optional field")
        .to_string();

        assert!(error.contains("performance"), "{error}");
    }

    /// Leniency about the optional fields is not leniency about the required ones: each of these
    /// omits or corrupts something the schema demands, and each is still not a mutation report.
    #[test]
    fn a_document_missing_or_corrupting_a_required_field_is_not_a_report() {
        let refused = [
            ("no files", r#"{ "schemaVersion": "2", "thresholds": { "high": 80, "low": 60 } }"#),
            ("no schema version", r#"{ "thresholds": { "high": 80, "low": 60 }, "files": {} }"#),
            ("no thresholds", r#"{ "schemaVersion": "2", "files": {} }"#),
            (
                "half a threshold",
                r#"{ "schemaVersion": "2", "thresholds": { "high": 80 }, "files": {} }"#,
            ),
            (
                "a threshold that is not a number",
                r#"{ "schemaVersion": "2", "thresholds": { "high": "high", "low": 60 }, "files": {} }"#,
            ),
            (
                "a schema version that is not a string",
                r#"{ "schemaVersion": 2, "thresholds": { "high": 80, "low": 60 }, "files": {} }"#,
            ),
            (
                "a framework with no name",
                r#"{ "schemaVersion": "2", "thresholds": { "high": 80, "low": 60 }, "framework": { "version": "1" }, "files": {} }"#,
            ),
            (
                "a file with no source",
                r#"{ "schemaVersion": "2", "thresholds": { "high": 80, "low": 60 },
                     "files": { "src/lib.rs": { "language": "rust", "mutants": [] } } }"#,
            ),
            (
                "a mutant with no id",
                r#"{ "schemaVersion": "2", "thresholds": { "high": 80, "low": 60 },
                     "files": { "src/lib.rs": { "source": "", "language": "rust", "mutants": [
                        { "mutatorName": "m", "status": "Killed",
                          "location": { "start": { "line": 1, "column": 1 }, "end": { "line": 1, "column": 2 } } }
                     ] } } }"#,
            ),
            (
                "a mutant with no location",
                r#"{ "schemaVersion": "2", "thresholds": { "high": 80, "low": 60 },
                     "files": { "src/lib.rs": { "source": "", "language": "rust", "mutants": [
                        { "id": "aaa", "mutatorName": "m", "status": "Killed" }
                     ] } } }"#,
            ),
            ("not an object at all", "[]"),
            ("not json at all", "{"),
        ];

        for (what, text) in refused {
            let error = read_text(text).expect_err(what).to_string();

            assert!(error.contains("is not a mutation report"), "{what}: {error}");
        }
    }

    /// Nothing the writer puts in a document is lost on the way back in: reading a complete report
    /// and writing it out again reproduces it field for field.
    ///
    /// The read path decodes through a shape of its own, and a shape of its own can drift from the
    /// one the writer emits — a field added to the report and not to the reader would be dropped
    /// silently, and every other test here would still pass.
    #[test]
    fn a_complete_document_survives_the_read_unchanged() {
        let report = read_text(COMPLETE).expect("a complete report");
        let written: Value = serde_json::from_str(&crate::elements::to_json(&report).expect("serializes")).expect("json");
        let original: Value = serde_json::from_str(COMPLETE).expect("json");

        assert_eq!(written, original);
    }

    /// A count near `u32::MAX` asks the merge to account for four billion shards. It is refused
    /// where it arrives, by a comparison rather than an allocation, and the message names both the
    /// file that carried it and the bound it broke.
    #[test]
    fn an_impossible_shard_count_is_refused_at_the_file_that_carried_it() {
        let text = format!(
            r#"{{
                "schemaVersion": "2",
                "thresholds": {{ "high": 80, "low": 60 }},
                "files": {{}},
                "config": {{ "startedAt": 100, "shard": {{ "index": 0, "count": {} }} }}
            }}"#,
            u32::MAX
        );

        let (_directory, path) = written(&text);
        let error = read(&path).expect_err("an impossible rotation").to_string();

        assert!(error.contains(&u32::MAX.to_string()), "{error}");
        assert!(error.contains(&MAX_SHARDS.to_string()), "{error}");
        assert!(error.contains(path.as_str()), "{error}");
    }

    /// The bound guards against a corrupt number rather than limiting how anyone shards: a rotation
    /// at the largest supported count reads back exactly as it was written.
    #[test]
    fn a_rotation_at_the_largest_supported_count_is_still_read() {
        let text = format!(
            r#"{{
                "schemaVersion": "2",
                "thresholds": {{ "high": 80, "low": 60 }},
                "files": {{}},
                "config": {{ "startedAt": 100, "shard": {{ "index": 3, "count": {MAX_SHARDS} }} }}
            }}"#
        );

        let report = read_text(&text).expect("a rotation at the bound");
        let shard = report.config.expect("run info").shard.expect("a shard");

        assert_eq!(shard.count, MAX_SHARDS);
        assert_eq!(shard.index, 3);
    }

    /// An index outside its own rotation is refused where it arrives, naming the file and both
    /// numbers.
    ///
    /// The pair is one contract. Downstream, `shards_seen` is counted against `count` and the
    /// missing shards are enumerated from `0..count`, so an index outside the range makes rotation
    /// coverage read above 100%, or read 100% while the next line names a shard that never ran —
    /// and a coverage figure that can say a rotation is complete while it is not is worse than none.
    #[test]
    fn a_shard_outside_its_own_rotation_is_refused() {
        for (index, count) in [(5_u32, 2_u32), (2, 2), (0, 0)] {
            let text = format!(
                r#"{{
                    "schemaVersion": "2",
                    "thresholds": {{ "high": 80, "low": 60 }},
                    "files": {{}},
                    "config": {{ "startedAt": 100, "shard": {{ "index": {index}, "count": {count} }} }}
                }}"#
            );

            let (_directory, path) = written(&text);
            let error = read(&path).expect_err("a shard outside its rotation").to_string();

            assert!(error.contains(&index.to_string()), "{error}");
            assert!(error.contains(&count.to_string()), "{error}");
            assert!(error.contains(path.as_str()), "{error}");
        }
    }

    /// A status the schema does not define is refused rather than scored.
    ///
    /// The merge scores by status, so a misspelling — or any corruption of the field — silently
    /// became an undetected mutant and moved the score of a rotation nobody had reason to distrust.
    /// The message names the mutant, so the file can be repaired rather than merely discarded.
    #[test]
    fn a_status_the_schema_does_not_define_is_refused() {
        let text = r#"{
            "schemaVersion": "2",
            "thresholds": { "high": 80, "low": 60 },
            "files": {
                "src/lib.rs": {
                    "source": "fn f() {}\n",
                    "language": "rust",
                    "mutants": [
                        { "id": "aaa", "mutatorName": "m", "status": "Killed",
                          "location": { "start": { "line": 1, "column": 1 }, "end": { "line": 1, "column": 2 } } },
                        { "id": "bbb", "mutatorName": "m", "status": "Kiled",
                          "location": { "start": { "line": 2, "column": 1 }, "end": { "line": 2, "column": 2 } } }
                    ]
                }
            }
        }"#;

        let (_directory, path) = written(text);
        let error = read(&path).expect_err("an undefined status").to_string();

        assert!(error.contains("Kiled"), "{error}");
        assert!(error.contains("bbb"), "{error}");
        assert!(error.contains(path.as_str()), "{error}");
    }

    /// `RuntimeError` is a status the schema defines, so a document carrying one is still a report.
    ///
    /// The refusal above is about words the schema has no meaning for, not about verdicts this tool
    /// does not itself write; another producer's report is a legitimate input, which is the whole
    /// reason the read path decodes through a shape of its own.
    #[test]
    fn a_status_this_tool_never_writes_is_still_a_status_the_schema_defines() {
        let report = read_text(
            r#"{
                "schemaVersion": "2",
                "thresholds": { "high": 80, "low": 60 },
                "files": {
                    "src/lib.rs": {
                        "source": "fn f() {}\n",
                        "language": "rust",
                        "mutants": [
                            { "id": "aaa", "mutatorName": "m", "status": "RuntimeError",
                              "location": { "start": { "line": 1, "column": 1 }, "end": { "line": 1, "column": 2 } } }
                        ]
                    }
                }
            }"#,
        )
        .expect("a schema-conforming report");

        assert_eq!(report.files["src/lib.rs"].mutants[0].status, "RuntimeError");
    }

    /// A run that has not happened yet is refused, because the merge ranks by that timestamp.
    ///
    /// A report from the future outranks every genuine one for every mutant it mentions, supplies
    /// the population that decides what has been withdrawn, and is classified fresh however old it
    /// really is — the age of a verdict from the future saturates to zero. One runner with a broken
    /// clock can decide a whole rotation, so the input is refused at the file that carried it.
    #[test]
    fn a_report_from_the_future_is_refused_at_the_file_that_carried_it() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("this machine's clock is after the epoch")
            .as_secs();
        let ahead = now + 10 * 86_400;

        let text = format!(
            r#"{{
                "schemaVersion": "2",
                "thresholds": {{ "high": 80, "low": 60 }},
                "files": {{}},
                "config": {{ "startedAt": {ahead} }}
            }}"#
        );

        let (_directory, path) = written(&text);
        let error = read(&path).expect_err("a run that has not happened").to_string();

        assert!(error.contains(&ahead.to_string()), "{error}");
        assert!(error.contains(path.as_str()), "{error}");

        // The tolerance is what separates a drifting clock from a broken one: a report a few
        // seconds ahead is the normal state of two machines and is still a report.
        let text = format!(
            r#"{{
                "schemaVersion": "2",
                "thresholds": {{ "high": 80, "low": 60 }},
                "files": {{}},
                "config": {{ "startedAt": {} }}
            }}"#,
            now + 5
        );

        let _accepted = read_text(&text).expect("a report from a clock a few seconds ahead");
    }

    /// A file larger than the ceiling is refused before it is read, not after.
    ///
    /// The read holds three live copies of a document at its peak, and `merge` expands a directory
    /// to every `.json` in it, so the file whose size nobody chose is exactly the one that arrives
    /// this way. Refusing it costs a `stat`; letting it through costs an OOM kill with no message.
    #[test]
    fn a_file_over_the_ceiling_is_refused_by_its_size_alone() {
        let directory = workdir("merge-read-");
        let path = Utf8PathBuf::from_path_buf(directory.path().join("report.json")).expect("a utf-8 path");

        // Sized rather than written: the point is that nothing reads the contents, and a quarter of
        // a gigabyte of real bytes would make the test cost what the bound exists to avoid.
        File::create(&path)
            .expect("the report should be creatable")
            .set_len(MAX_BYTES + 1)
            .expect("the report should be sizable");

        let error = read(&path).expect_err("a file over the ceiling").to_string();

        assert!(error.contains(&(MAX_BYTES + 1).to_string()), "{error}");
        assert!(error.contains(&MAX_BYTES.to_string()), "{error}");
        assert!(error.contains(path.as_str()), "{error}");
    }

    #[test]
    fn a_report_that_grows_after_its_handle_is_checked_still_hits_the_read_cap() {
        let (_directory, path) = written("{}");
        let mut input = open(&path).expect("open");

        fs::write(&path, "12345").expect("grow the opened file");

        let error = read_contents(&mut input, &path, 4)
            .expect_err("the grown report exceeds the cap")
            .to_string();

        assert!(error.contains("5 bytes"), "{error}");
        assert!(error.contains("4 bytes"), "{error}");
    }

    #[test]
    fn replacing_a_checked_path_cannot_change_the_capped_handle() {
        let (_directory, path) = written("12345");
        let replacement = path.with_file_name("replacement.json");
        let mut input = open(&path).expect("open");

        fs::write(&replacement, "{}").expect("replacement");
        fs::rename(&replacement, &path).expect("replace the path");

        let error = read_contents(&mut input, &path, 4)
            .expect_err("the original opened report still exceeds the cap")
            .to_string();

        assert!(error.contains("5 bytes"), "{error}");
        assert!(error.contains("4 bytes"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_is_refused_without_waiting_for_a_writer() {
        use std::process::Command;

        let directory = workdir("merge-read-fifo-");
        let path = Utf8PathBuf::from_path_buf(directory.path().join("report.json")).expect("a utf-8 path");
        let created = Command::new("mkfifo").arg(&path).status().expect("mkfifo is available");
        assert!(created.success(), "mkfifo exited with {created}");

        let error = read(&path).expect_err("a FIFO is not a report").to_string();

        assert!(error.contains("not a regular file"), "{error}");
        assert!(error.contains(path.as_str()), "{error}");
    }
}
