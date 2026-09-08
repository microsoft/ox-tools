// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The `mutation-testing-elements` report document.
//!
//! The schema is a published artifact of another project, so the mapping is spelled out rather
//! than left implicit: drift is silent and shows up as a blank page in someone's browser rather
//! than as a failing build.

use core::fmt::{Display, Write as _};
use std::collections::BTreeMap;
use std::{fs, io};

use camino::Utf8Path;
use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::discover::Plan;
use crate::error::error;
use crate::model::{Mutant, Outcome};
use crate::parse::SourceFile;
use crate::{HashMap, HashSet, Result};

/// The schema version we emit.
///
/// The version string validates against `^([1-2])(\.(([1-9]\d*)|0)){0,2}$` — major 1 and 2 only —
/// even though the npm package that defines it is at 3.x. Emitting "3" fails validation for a
/// reason that looks like version skew and is not.
const SCHEMA_VERSION: &str = "2";

/// The highest schema major version a cross-run reader will act on.
///
/// Kept separate from [`SCHEMA_VERSION`] because the two answer different questions: one is what we
/// write, the other is what we are willing to read. A build that starts emitting a newer version
/// still has to read the reports its predecessors wrote.
pub(super) const SUPPORTED_SCHEMA_MAJOR: u32 = 2;

/// The `framework.name` this tool writes, and the only one a cross-run reader accepts.
pub(super) const FRAMEWORK_NAME: &str = "cargo-gamma";

type SchemaResult<T> = core::result::Result<T, String>;

/// A whole mutation test result, the root of the report document.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    /// The schema version this document claims to conform to.
    pub schema_version: String,

    /// The score bands the viewer colors by.
    pub thresholds: Thresholds,

    /// Absolute path the file keys are relative to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_root: Option<String>,

    /// What produced the report.
    pub framework: Framework,

    /// One entry per mutated file, keyed by workspace-relative path.
    ///
    /// Ordered rather than hashed because this is the one map whose iteration order is observable:
    /// the document is written straight from these types, so what the map yields is what the file
    /// contains. Sorting at the write would answer the same question once per publication and leave
    /// every other traversal — merging and digesting — free to differ between runs for no reason.
    pub files: BTreeMap<String, FileResult>,

    /// Free-form run metadata.
    ///
    /// The schema declares this "free-format", which is what makes it the right home for the shard
    /// identity and the run time. `merge` needs both, and inventing a sidecar file for them would
    /// mean a report artifact that is only half the story.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<RunInfo>,
}

/// What `merge` needs to know about the run that produced a report.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunInfo {
    /// When the run started, in seconds since the Unix epoch.
    ///
    /// Seconds rather than a formatted timestamp because every use is arithmetic — freshness,
    /// ordering, windowing — and parsing a date format back is a step that can only lose.
    pub started_at: u64,

    /// The mutant-identity scheme used by every ID in this report.
    ///
    /// Reports written before this field existed omit it and are treated as the current scheme by
    /// the merger. Keeping it with run metadata lets a shard rotation reject identities from a
    /// different namespace instead of counting the same logical mutant twice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mutant_id_version: Option<u32>,

    /// Whether this document combines reports rather than describing one run.
    ///
    /// A merged document is not a complete population snapshot even though it has no shard
    /// identity: it can combine an incomplete rotation and reports from different revisions.
    #[serde(default, skip_serializing_if = "is_false")]
    pub merged: bool,

    /// The shard this run covered, when it was sharded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shard: Option<ShardInfo>,

    /// How many tests the baseline ran, when a harness announced a count.
    ///
    /// Carried in the report because it is the figure that says whether the suite ran at all, and
    /// a score computed over a suite that ran nothing is the most dangerous number this tool can
    /// produce. The console shows it too, but the console is not there in CI.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tests: Option<usize>,

    /// How many mutants belonged to a build this run gave up on.
    ///
    /// These export as `Ignored`, which they share with deliberate suppressions, because the
    /// statuses that would name them better — `NoCoverage`, `RuntimeError` — sit outside the
    /// schema's denominator and would make the viewer's score disagree with the printed one. That
    /// is the disagreement the whole `NotBuilt` outcome exists to remove, so the distinction is
    /// carried here instead of being pushed into the status.
    ///
    /// A reader wanting to know which *particular* mutants they were has `NOT_BUILT_PREFIX`; this
    /// is so that they can be counted without walking every mutant in the report. Filled in by
    /// [`build`], never by the caller, so it cannot drift from the mutants beside it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_built: Option<usize>,

    /// Test packages the preflight dropped so that the tree would check at all.
    ///
    /// Empty in the ordinary run, and omitted from the document when it is. When it is not, some
    /// package the caller never asked to mutate does not compile, and rather than refuse to run,
    /// the tool narrowed the build to the packages being mutated and went ahead.
    ///
    /// It is in the report because it changes how the report reads. Those packages' test targets
    /// were neither built nor run, so a mutant one of them would have killed appears here as a
    /// survivor — a gap in this run's oracle wearing the clothes of a gap in the suite. Anyone
    /// comparing this run against one taken over the whole workspace needs to know that before
    /// they compare the scores.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped_test_packages: Vec<String>,

    /// The original verdict and source generations retained by a merged report.
    ///
    /// A merged report can carry verdicts from several runs. Keeping only its own publication time
    /// would make a later merge treat every one of them as equally fresh, so this free-form config
    /// extension preserves the information the merger needs without extending the interchange
    /// schema's mutant objects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_provenance: Option<MergeProvenance>,
}

/// Per-item provenance retained by a merged report.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeProvenance {
    /// The report that supplied each rendered file's source and language.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sources: BTreeMap<String, SourceProvenance>,

    /// The run that established each retained verdict.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub verdicts: BTreeMap<String, VerdictProvenance>,
}

/// The source generation a merged file renders.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceProvenance {
    /// When the source report started.
    pub started_at: u64,

    /// The source report's stable merge tie-breaker.
    pub origin: String,

    /// A deterministic identity for this source generation.
    ///
    /// Older merged reports omit this field. The merger derives an identity for those documents
    /// from their rendered source so they remain readable.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub lineage: String,
}

/// The run generation that established a merged mutant's status.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerdictProvenance {
    /// When the verdict's run started.
    pub started_at: u64,

    /// The verdict report's stable merge tie-breaker.
    pub origin: String,

    /// A deterministic identity for this verdict generation.
    ///
    /// Older merged reports omit this field. The merger derives an identity for those documents
    /// from their retained mutant result so they remain readable.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub lineage: String,
}

/// Identifies one shard of a rotation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShardInfo {
    /// Which shard this was, from zero.
    pub index: u32,

    /// How many shards the population was divided into.
    pub count: u32,
}

#[expect(clippy::trivially_copy_pass_by_ref, reason = "serde requires a predicate over a reference")]
const fn is_false(value: &bool) -> bool {
    !*value
}

/// The score bands the viewer colors by.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Thresholds {
    /// At or above this score the viewer shows green.
    pub high: u32,

    /// Below this score the viewer shows red.
    pub low: u32,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self { high: 80, low: 60 }
    }
}

/// Identifies the tool, which the viewer shows in its header.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Framework {
    /// The tool name.
    pub name: String,

    /// The tool version.
    pub version: String,
}

/// One mutated file: its full source, and every mutant generated in it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileResult {
    /// The complete source text. The viewer renders this with the mutants overlaid, which is why
    /// the report is self-contained and can be opened without the repository.
    pub source: String,

    /// The language, used to pick syntax highlighting.
    pub language: String,

    /// Every mutant in this file.
    pub mutants: Vec<MutantResult>,
}

/// One mutant, in the schema's vocabulary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MutantResult {
    /// Our content-addressed identity.
    ///
    /// Stryker uses a within-run integer here; the field is a free-form string, and an identity
    /// that survives edits elsewhere in the file is strictly more useful in a report someone may
    /// compare against last week's.
    pub id: CompactString,

    /// The registry name of the mutator, such as `relational.lt_to_le`.
    ///
    /// The viewer groups and filters by this string, so the naming scheme becomes the UI's facet
    /// list at no extra cost.
    pub mutator_name: CompactString,

    /// Where the mutated construct is.
    pub location: Location,

    /// The verdict, in the schema's closed `PascalCase` vocabulary.
    ///
    /// Owned rather than `&'static str` so a written report can be read back — which is what
    /// `merge` does. The closed-enum guarantee is kept at the only place it can be violated, the
    /// mapping in `status_of`, and asserted against the vendored schema by a conformance test.
    pub status: CompactString,

    /// The replacement source text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement: Option<CompactString>,

    /// A human sentence describing the change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Why the status is what it is: a suppression reason, or the test that killed it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,

    /// Wall time spent on this mutant, in milliseconds.
    ///
    /// The interchange schema permits any JSON number, rather than only whole milliseconds.
    /// Keeping that precision is necessary when a report produced by another implementation is
    /// merged and written again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,

    /// The test that killed it, when one did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub killed_by: Option<Vec<String>>,
}

/// A half-open source range, in the schema's one-based line and column terms.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Location {
    /// Inclusive start.
    pub start: Position,

    /// Exclusive end.
    pub end: Position,
}

/// A one-based line and column.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Position {
    /// One-based line.
    pub line: usize,

    /// One-based column.
    pub column: usize,
}

/// Opens the `statusReason` of a mutant its memory ceiling stopped.
///
/// The schema has no out-of-memory status, so the verdict is exported as `Survived` and this
/// prefix preserves the resource outcome. `statusReason` is a free-form string in the schema, so
/// carrying the distinction here costs the schema nothing, where a field of our own invention
/// would be an extension a strict validator could reject.
pub(super) const OUT_OF_MEMORY_PREFIX: &str = "out of memory: ";

/// Marks a timed-out mutant exported as schema `Survived`.
///
/// The schema treats its `Timeout` status as detected. Gamma does not: a resource limit observed
/// the mutant, but no assertion rejected it. The prefix preserves the distinct verdict while the
/// `Survived` status keeps schema consumers' score aligned with Gamma's.
pub(super) const TIMEOUT_PREFIX: &str = "timed out: ";

/// Marks a mutant the build gave up on in `statusReason`, for the same reason
/// [`OUT_OF_MEMORY_PREFIX`] exists.
///
/// `NotBuilt` exports as `Ignored`, which is also what a deliberate suppression exports as, and the
/// two ask a reader to do opposite things: one is work somebody chose not to do, the other is work
/// this run could not do. The per-mutant count is in `config.notBuilt`; this is what lets a reader
/// tell which of the two *one* mutant was, and what stops incremental execution from freezing a mutant nobody
/// ever judged into every later run as though it had been suppressed.
pub(super) const NOT_BUILT_PREFIX: &str = "not built: ";

/// Marks a flake in `statusReason`, for the same reason [`OUT_OF_MEMORY_PREFIX`] exists.
///
/// A flake exports as `Ignored`, which it shares with suppressions and with mutants the build never
/// compiled. Those three ask the reader to do quite different things, and this prefix is what tells
/// them apart once the report has left the machine that produced it.
pub(super) const FLAKY_PREFIX: &str = "flaky: ";

/// Maps a verdict onto the schema's closed status enum.
///
/// The schema treats `Timeout` as detected, so Gamma exports both resource-exhaustion outcomes as
/// `Survived` with reason prefixes. They remain in the denominator without entering the numerator,
/// and the prefixes preserve the verdicts the schema cannot represent directly.
const fn status_of(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Pending => "Pending",
        Outcome::Killed => "Killed",
        Outcome::Survived | Outcome::Timeout | Outcome::OutOfMemory => "Survived",
        Outcome::CompileError => "CompileError",
        // A mutant the build never compiled is `Ignored` rather than `NoCoverage`: both are real
        // options here, but `NoCoverage` is in the schema's denominator and would lower the score
        // the viewer shows relative to the printed one, which is the disagreement this whole
        // outcome exists to remove.
        // A flake exports as `Ignored` for the same reason `NotBuilt` does: the run established
        // nothing about this mutant, and every other status the schema offers would state something
        // it did not find. The reason string carries the test that failed both ways.
        Outcome::Ignored | Outcome::NotBuilt | Outcome::Flaky => "Ignored",
        Outcome::NoCoverage => "NoCoverage",
    }
}

/// Explains a verdict in one sentence, for the viewer's detail pane.
fn reason_for(mutant: &Mutant) -> Option<String> {
    if let Some(suppression) = mutant.suppression.as_ref() {
        let mut text = format!("suppressed by {}", suppression.channel.as_str());

        if let Some(reason) = suppression.reason.as_ref() {
            text.push_str(": ");
            text.push_str(reason);
        }

        if let Some(tag) = suppression.tag.as_ref() {
            let _ = write!(text, " [#{tag}]");
        }

        return Some(text);
    }

    match mutant.outcome {
        Outcome::Killed => mutant
            .killed_by
            .as_ref()
            .map(|test| format!("failed `{test}`"))
            .or_else(|| mutant.note.clone()),
        Outcome::CompileError => Some("the mutant does not compile".to_owned()),
        // `NotBuilt` and a suppression both export as `Ignored`, so without this the reader cannot
        // tell a mutant the run gave up on from one that was deliberately skipped.
        Outcome::NotBuilt => Some(format!(
            "{NOT_BUILT_PREFIX}{}",
            mutant
                .note
                .clone()
                .unwrap_or_else(|| "the build this mutant belonged to could not be converged".to_owned())
        )),
        Outcome::Timeout => Some(format!(
            "{TIMEOUT_PREFIX}{}",
            mutant.note.clone().unwrap_or_else(|| "the test run exceeded its budget".to_owned())
        )),
        // Exports as `Ignored`, like a suppression and like `NotBuilt`, so without the note a
        // reader of the report cannot tell a flake from a mutant somebody deliberately skipped —
        // and the note is where the test to fix is named.
        Outcome::Flaky => Some(format!(
            "{FLAKY_PREFIX}{}",
            mutant
                .note
                .clone()
                .unwrap_or_else(|| "a test failed with no mutant active as well as with one".to_owned())
        )),
        // The sweep already built a note saying how far past the ceiling the run went, and dropping
        // it here would leave the reader a bare `Survived` with nothing to explain the distinct
        // resource outcome that was reached.
        Outcome::OutOfMemory => Some(format!(
            "{OUT_OF_MEMORY_PREFIX}{}",
            mutant
                .note
                .clone()
                .unwrap_or_else(|| "the test run exceeded the memory this run allowed it".to_owned())
        )),
        Outcome::NoCoverage => Some(
            "no selected runtime test reached this mutation site; coverage reports may exclude this code or include other configurations"
                .to_owned(),
        ),
        _ => None,
    }
}

/// Builds the report document for a completed plan.
///
/// Every mutated file's full source is embedded, because a report that needs the repository beside
/// it to be readable cannot be attached to a CI run or mailed to someone.
///
/// # Errors
///
/// Returns an error if a mutated file's source cannot be read back from disk or parsed for
/// source-location rendering, which embedding it requires. The read and parse happen after the run
/// rather than during it, so a file the repository deleted, made unreadable, made invalid Rust, or
/// nested beyond the supported parser limit in the meantime fails here.
pub fn build(plan: &Plan, thresholds: Thresholds, run: Option<RunInfo>) -> Result<Report> {
    let mut files: BTreeMap<String, FileResult> = BTreeMap::new();

    // Grouped once rather than rescanned per file: a workspace with many files has many mutants
    // too, so the pairing is quadratic in exactly the case it needs not to be.
    let mut grouped: HashMap<&Utf8Path, Vec<&Mutant>> = HashMap::default();

    for mutant in &plan.mutants {
        grouped.entry(&*mutant.file).or_default().push(mutant);
    }

    for file in &plan.files {
        let Some(mutants) = grouped.get(file.path.as_path()) else {
            continue;
        };

        let original = fs::read_to_string(file.absolute.as_std_path())
            .map_err(|cause| error!("could not read `{}`", file.absolute).caused_by(cause))?;
        let has_bom = original.starts_with('\u{feff}');
        let source = SourceFile::parse(file.absolute.clone(), original.clone())?;

        if let Some(expected) = plan.digests.get(&file.path)
            && crate::discover::digest(source.text().as_bytes()) != *expected
        {
            return Err(error!(
                "`{}` changed after its mutants were discovered; rerun cargo-gamma so discovery, verdicts, and report source use the same generation",
                file.path
            ));
        }

        let rendered = mutants
            .iter()
            .map(|mutant| render_with_first_line_offset(mutant, &source, usize::from(has_bom)))
            .collect();

        let _ = files.insert(
            file.path.to_string(),
            FileResult {
                source: original,
                language: "rust".to_owned(),
                mutants: rendered,
            },
        );
    }

    let not_built = plan.mutants.iter().filter(|mutant| mutant.outcome == Outcome::NotBuilt).count();

    Ok(Report {
        schema_version: SCHEMA_VERSION.to_owned(),
        thresholds,
        project_root: Some(plan.root.to_string()),
        framework: Framework {
            name: FRAMEWORK_NAME.to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        },
        files,
        config: run.map(|run| RunInfo {
            not_built: (not_built > 0).then_some(not_built),
            mutant_id_version: Some(crate::model::MUTANT_ID_VERSION),
            ..run
        }),
    })
}

/// Whether a schema version has the form and major version this crate supports.
#[must_use]
pub(crate) fn supported_schema_version(version: &str) -> bool {
    let mut parts = version.split('.');
    let Some(major) = parts.next() else {
        return false;
    };

    if !matches!(major, "1" | "2") {
        return false;
    }

    let mut components = 1;

    for part in parts {
        components += 1;

        if components > 3 || part.is_empty() || (part.len() > 1 && part.starts_with('0')) || !part.bytes().all(|byte| byte.is_ascii_digit())
        {
            return false;
        }
    }

    true
}

/// Validates a document against the adopted mutation-testing-elements schema.
///
/// The schema permits extension fields, so this checks every field it defines and deliberately
/// leaves unknown fields alone. That lets an external producer add metadata without letting a
/// malformed known field silently become valid merely because this crate does not use it.
pub(crate) fn validate_schema(document: &Value) -> SchemaResult<()> {
    let report = object(document, "report")?;

    validate_schema_version(required(report, "schemaVersion", "report")?, "report.schemaVersion")?;
    validate_thresholds(required(report, "thresholds", "report")?, "report.thresholds")?;
    validate_files(required(report, "files", "report")?, "report.files")?;

    if let Some(config) = report.get("config") {
        let _config = object(config, "report.config")?;
    }

    if let Some(root) = report.get("projectRoot") {
        let _ = string(root, "report.projectRoot")?;
    }

    if let Some(framework) = report.get("framework") {
        validate_framework(framework, "report.framework")?;
    }

    if let Some(performance) = report.get("performance") {
        validate_performance(performance, "report.performance")?;
    }

    if let Some(test_files) = report.get("testFiles") {
        validate_test_files(test_files, "report.testFiles")?;
    }

    if let Some(system) = report.get("system") {
        validate_system(system, "report.system")?;
    }

    Ok(())
}

fn object(value: &Value, path: impl Display) -> SchemaResult<&serde_json::Map<String, Value>> {
    value.as_object().ok_or_else(|| format!("{path} must be an object"))
}

fn array(value: &Value, path: impl Display) -> SchemaResult<&Vec<Value>> {
    value.as_array().ok_or_else(|| format!("{path} must be an array"))
}

fn required<'value>(object: &'value serde_json::Map<String, Value>, name: &str, path: impl Display) -> SchemaResult<&'value Value> {
    object.get(name).ok_or_else(|| format!("{path} is missing required field `{name}`"))
}

fn string(value: &Value, path: impl Display) -> SchemaResult<&str> {
    value.as_str().ok_or_else(|| format!("{path} must be a string"))
}

fn number(value: &Value, path: impl Display) -> SchemaResult<()> {
    if value.is_number() {
        Ok(())
    } else {
        Err(format!("{path} must be a number"))
    }
}

fn boolean(value: &Value, path: impl Display) -> SchemaResult<()> {
    if value.is_boolean() {
        Ok(())
    } else {
        Err(format!("{path} must be a boolean"))
    }
}

fn integer(value: &Value, path: impl Display) -> SchemaResult<u64> {
    value.as_u64().ok_or_else(|| format!("{path} must be a non-negative integer"))
}

fn validate_schema_version(value: &Value, path: &str) -> SchemaResult<()> {
    let version = string(value, path)?;

    if supported_schema_version(version) {
        Ok(())
    } else {
        Err(format!("schema version `{version}` at {path} must match the supported pattern"))
    }
}

fn validate_thresholds(value: &Value, path: &str) -> SchemaResult<()> {
    let thresholds = object(value, path)?;

    for name in ["high", "low"] {
        let field = format!("{path}.{name}");
        let threshold = integer(required(thresholds, name, path)?, &field)?;

        if threshold > 100 {
            return Err(format!("{field} must be at most 100"));
        }
    }

    Ok(())
}

fn validate_files(value: &Value, path: &str) -> SchemaResult<()> {
    for (name, file) in object(value, path)? {
        validate_file(file, &format!("{path}[{name:?}]"))?;
    }

    Ok(())
}

fn validate_file(value: &Value, path: &str) -> SchemaResult<()> {
    let file = object(value, path)?;

    let _ = string(required(file, "language", path)?, format_args!("{path}.language"))?;
    let _ = string(required(file, "source", path)?, format_args!("{path}.source"))?;
    validate_mutants(required(file, "mutants", path)?, &format!("{path}.mutants"))
}

fn validate_mutants(value: &Value, path: &str) -> SchemaResult<()> {
    let mutants = array(value, path)?;
    let mut unique = HashSet::default();
    let mut encoded = Vec::new();

    for (index, mutant) in mutants.iter().enumerate() {
        let path = format!("{path}[{index}]");
        encoded.clear();
        serde_json::to_writer(&mut encoded, mutant).map_err(|cause| format!("{path} could not be compared: {cause}"))?;

        if !unique.insert(blake3::hash(&encoded)) {
            return Err(format!("{path} duplicates another mutant"));
        }

        validate_mutant(mutant, &path)?;
    }

    Ok(())
}

fn validate_mutant(value: &Value, path: &str) -> SchemaResult<()> {
    let mutant = object(value, path)?;

    let id = string(required(mutant, "id", path)?, format_args!("{path}.id"))?;
    let _ = string(required(mutant, "mutatorName", path)?, format_args!("{path}.mutatorName"))?;
    validate_location(required(mutant, "location", path)?, format_args!("{path}.location"), false)?;

    let status = string(required(mutant, "status", path)?, format_args!("{path}.status"))?;

    if !matches!(
        status,
        "Killed" | "Survived" | "NoCoverage" | "CompileError" | "RuntimeError" | "Timeout" | "Ignored" | "Pending"
    ) {
        return Err(format!("{path} mutant `{id}` has unknown schema status `{status}`"));
    }

    for name in ["description", "replacement", "statusReason"] {
        if let Some(value) = mutant.get(name) {
            let _ = string(value, format_args!("{path}.{name}"))?;
        }
    }

    for name in ["duration", "testsCompleted"] {
        if let Some(value) = mutant.get(name) {
            number(value, format_args!("{path}.{name}"))?;
        }
    }

    for name in ["coveredBy", "killedBy"] {
        if let Some(value) = mutant.get(name) {
            validate_strings(value, format_args!("{path}.{name}"))?;
        }
    }

    if let Some(value) = mutant.get("static") {
        boolean(value, format_args!("{path}.static"))?;
    }

    Ok(())
}

fn validate_strings(value: &Value, path: impl Display + Copy) -> SchemaResult<()> {
    for (index, value) in array(value, path)?.iter().enumerate() {
        let _ = string(value, format_args!("{path}[{index}]"))?;
    }

    Ok(())
}

fn validate_location(value: &Value, path: impl Display + Copy, open_end: bool) -> SchemaResult<()> {
    let location = object(value, path)?;
    validate_position(required(location, "start", path)?, format_args!("{path}.start"))?;

    if open_end {
        if let Some(end) = location.get("end") {
            validate_position(end, format_args!("{path}.end"))?;
        }
    } else {
        validate_position(required(location, "end", path)?, format_args!("{path}.end"))?;
    }

    Ok(())
}

fn validate_position(value: &Value, path: impl Display + Copy) -> SchemaResult<()> {
    let position = object(value, path)?;

    for name in ["line", "column"] {
        if integer(required(position, name, path)?, format_args!("{path}.{name}"))? == 0 {
            return Err(format!("{path}.{name} must be at least 1"));
        }
    }

    Ok(())
}

fn validate_test_files(value: &Value, path: &str) -> SchemaResult<()> {
    for (name, file) in object(value, path)? {
        let file_path = format!("{path}[{name:?}]");
        let file = object(file, &file_path)?;

        if let Some(source) = file.get("source") {
            let _ = string(source, format_args!("{file_path}.source"))?;
        }

        for (index, test) in array(required(file, "tests", &file_path)?, format_args!("{file_path}.tests"))?
            .iter()
            .enumerate()
        {
            let test_path = format!("{file_path}.tests[{index}]");
            let test = object(test, &test_path)?;

            let _ = string(required(test, "id", &test_path)?, format_args!("{test_path}.id"))?;
            let _ = string(required(test, "name", &test_path)?, format_args!("{test_path}.name"))?;

            if let Some(location) = test.get("location") {
                validate_location(location, format_args!("{test_path}.location"), true)?;
            }
        }
    }

    Ok(())
}

fn validate_performance(value: &Value, path: &str) -> SchemaResult<()> {
    let performance = object(value, path)?;

    for name in ["setup", "initialRun", "mutation"] {
        number(required(performance, name, path)?, format_args!("{path}.{name}"))?;
    }

    Ok(())
}

fn validate_framework(value: &Value, path: &str) -> SchemaResult<()> {
    let framework = object(value, path)?;

    let _ = string(required(framework, "name", path)?, format_args!("{path}.name"))?;

    if let Some(version) = framework.get("version") {
        let _ = string(version, format_args!("{path}.version"))?;
    }

    if let Some(branding) = framework.get("branding") {
        let branding_path = format!("{path}.branding");
        let branding = object(branding, &branding_path)?;
        let homepage = string(
            required(branding, "homepageUrl", &branding_path)?,
            format_args!("{branding_path}.homepageUrl"),
        )?;

        if !is_uri(homepage) {
            return Err(format!("{branding_path}.homepageUrl must be a URI"));
        }

        if let Some(image) = branding.get("imageUrl") {
            let _ = string(image, format_args!("{branding_path}.imageUrl"))?;
        }
    }

    if let Some(dependencies) = framework.get("dependencies") {
        let dependencies_path = format!("{path}.dependencies");

        for (name, version) in object(dependencies, &dependencies_path)? {
            let _ = string(version, format_args!("{dependencies_path}[{name:?}]"))?;
        }
    }

    Ok(())
}

fn is_uri(value: &str) -> bool {
    let Some((scheme, rest)) = value.split_once(':') else {
        return false;
    };

    if scheme.is_empty()
        || !scheme
            .bytes()
            .enumerate()
            .all(|(index, byte)| byte.is_ascii_alphabetic() || (index > 0 && (byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.'))))
    {
        return false;
    }

    let bytes = rest.as_bytes();
    let mut index = 0;

    while let Some(&byte) = bytes.get(index) {
        if byte == b'%' {
            if !bytes.get(index + 1).is_some_and(u8::is_ascii_hexdigit) || !bytes.get(index + 2).is_some_and(u8::is_ascii_hexdigit) {
                return false;
            }

            index += 3;
        } else {
            if byte.is_ascii_control() || matches!(byte, b' ' | b'"' | b'<' | b'>' | b'\\' | b'^' | b'`' | b'{' | b'|' | b'}') {
                return false;
            }

            index += 1;
        }
    }

    true
}

fn validate_system(value: &Value, path: &str) -> SchemaResult<()> {
    let system = object(value, path)?;
    boolean(required(system, "ci", path)?, format_args!("{path}.ci"))?;

    if let Some(os) = system.get("os") {
        let os_path = format!("{path}.os");
        let os = object(os, &os_path)?;
        let _ = string(required(os, "platform", &os_path)?, format_args!("{os_path}.platform"))?;

        for name in ["description", "version"] {
            if let Some(value) = os.get(name) {
                let _ = string(value, format_args!("{os_path}.{name}"))?;
            }
        }
    }

    if let Some(cpu) = system.get("cpu") {
        let cpu_path = format!("{path}.cpu");
        let cpu = object(cpu, &cpu_path)?;
        number(required(cpu, "logicalCores", &cpu_path)?, format_args!("{cpu_path}.logicalCores"))?;

        if let Some(clock) = cpu.get("baseClock") {
            number(clock, format_args!("{cpu_path}.baseClock"))?;
        }

        if let Some(model) = cpu.get("model") {
            let _ = string(model, format_args!("{cpu_path}.model"))?;
        }
    }

    if let Some(ram) = system.get("ram") {
        let ram_path = format!("{path}.ram");
        let ram = object(ram, &ram_path)?;
        number(required(ram, "total", &ram_path)?, format_args!("{ram_path}.total"))?;
    }

    Ok(())
}

/// Converts one mutant into its schema form.
#[cfg(test)]
fn render(mutant: &Mutant, source: &SourceFile) -> MutantResult {
    render_with_first_line_offset(mutant, source, 0)
}

#[expect(
    clippy::cast_precision_loss,
    reason = "the interchange schema represents durations as JSON numbers, while a run measures milliseconds in u64"
)]
fn render_with_first_line_offset(mutant: &Mutant, source: &SourceFile, first_line_offset: usize) -> MutantResult {
    let (start_line, start_column) = source.location(mutant.span.start);
    let (end_line, end_column) = source.location(mutant.span.end);

    MutantResult {
        id: CompactString::new(&mutant.id),
        mutator_name: CompactString::new(&mutant.mutator),
        location: Location {
            start: Position {
                line: start_line,
                column: start_column + usize::from(start_line == 1) * first_line_offset,
            },
            end: Position {
                line: end_line,
                column: end_column + usize::from(end_line == 1) * first_line_offset,
            },
        },
        status: CompactString::new(status_of(mutant.outcome)),
        replacement: Some(mutant.replacement.clone()),
        description: Some(mutant.summary()),
        status_reason: reason_for(mutant),
        duration: (mutant.elapsed_ms > 0).then_some(mutant.elapsed_ms as f64),
        killed_by: mutant.killed_by.clone().map(|test| vec![test]),
    }
}

/// Serializes the report as pretty-printed JSON.
///
/// # Errors
///
/// Returns an error if the report does not satisfy the schema, or if it cannot be serialized.
pub fn to_json(report: &Report) -> Result<String> {
    validate_report(report).map_err(|cause| error!("could not serialize the report: {cause}"))?;

    serde_json::to_string_pretty(report).map_err(|cause| error!("could not serialize the report").caused_by(cause))
}

/// Writes the report to `path` as pretty-printed JSON.
///
/// Serialized straight from the typed report rather than through an intermediate
/// `serde_json::Value`. That intermediate JSON value was a second complete copy of a document that
/// embeds every mutated file's whole source, built only so that it could be validated and then
/// thrown away; validating the typed report asks the same questions of the same data without the
/// copy. Key order is the declaration order of the types rather than the alphabetical order a
/// `Value` imposed, which is just as deterministic — the field order of a `struct` does not vary
/// between runs — and the one map whose order is observable is ordered for exactly this reason.
///
/// # Errors
///
/// Returns an error if the report does not satisfy the schema, or if it cannot be serialized or
/// written to `path`.
pub fn write_json(report: &Report, path: &Utf8Path) -> Result<()> {
    validate_report(report).map_err(|cause| error!("could not serialize the report: {cause}"))?;

    crate::elements::write_streamed(path, |writer| serde_json::to_writer_pretty(writer, report).map_err(io::Error::from))
}

/// Validates a report this crate built, against the same rules as the untyped document check.
///
/// The typed form makes most of that check unnecessary: schema strings and numbers are represented
/// by the corresponding Rust string and numeric types, and a required field is not an `Option`.
/// What is left is everything the type system cannot state — the version pattern, the bounded
/// thresholds, positions that must be at least one, the closed status vocabulary, and the
/// uniqueness of mutants within a file — and those are checked here.
///
/// [`validate_schema`] stays, and is not implemented in terms of this: it is asked about documents
/// this crate did not write, where the types have not yet been established and the answer must not
/// depend on `serde` having accepted them.
fn validate_report(report: &Report) -> SchemaResult<()> {
    if !supported_schema_version(&report.schema_version) {
        return Err(format!(
            "schema version `{}` at report.schemaVersion must match the supported pattern",
            report.schema_version
        ));
    }

    for (name, threshold) in [("high", report.thresholds.high), ("low", report.thresholds.low)] {
        if threshold > 100 {
            return Err(format!("report.thresholds.{name} must be at most 100"));
        }
    }

    for (name, file) in &report.files {
        validate_file_result(file, &format!("report.files[{name:?}]"))?;
    }

    Ok(())
}

/// Validates one file's mutants, and that no two of them are the same mutant.
fn validate_file_result(file: &FileResult, path: &str) -> SchemaResult<()> {
    let mut unique = HashSet::default();

    for (index, mutant) in file.mutants.iter().enumerate() {
        let path = format!("{path}.mutants[{index}]");

        if !unique.insert(mutant.id.as_str()) {
            return Err(format!("{path} duplicates another mutant"));
        }

        if !matches!(
            mutant.status.as_str(),
            "Killed" | "Survived" | "NoCoverage" | "CompileError" | "RuntimeError" | "Timeout" | "Ignored" | "Pending"
        ) {
            return Err(format!(
                "{path} mutant `{}` has unknown schema status `{}`",
                mutant.id, mutant.status
            ));
        }

        if mutant.duration.is_some_and(|duration| !duration.is_finite()) {
            return Err(format!("{path}.duration must be a finite JSON number"));
        }

        for (corner, position) in [("start", &mutant.location.start), ("end", &mutant.location.end)] {
            for (axis, value) in [("line", position.line), ("column", position.column)] {
                if value == 0 {
                    return Err(format!("{path}.location.{corner}.{axis} must be at least 1"));
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use core::ops::Range;
    use std::borrow::Cow;

    use camino::Utf8PathBuf;

    use super::super::digest::{Digest, settled_mutants, settled_verdict};
    use super::*;
    use crate::discover::TargetFile;
    use crate::fixtures;
    use crate::model::{Channel, Suppression};

    fn mutant(outcome: Outcome, span: Range<usize>) -> Mutant {
        Mutant {
            id: "abc123abc123".to_owned().into(),
            span,
            original: "a < b".to_owned().into(),
            replacement: "(a) <= (b)".to_owned().into(),
            outcome,
            ..fixtures::mutant()
        }
    }

    #[test]
    fn every_verdict_maps_onto_the_closed_enum() {
        // The schema's status list is closed, so a verdict we invent a name for would fail
        // validation in the viewer rather than here.
        const VALID: [&str; 7] = [
            "Killed",
            "Survived",
            "NoCoverage",
            "CompileError",
            "RuntimeError",
            "Timeout",
            "Ignored",
        ];

        // Every non-`Pending` verdict is pinned to the *exact* schema status it exports as, not
        // merely to some valid one: a membership check survives swapping `Killed`→`Survived` or
        // `NoCoverage`→`Ignored`, because the replacement is itself valid. `OutOfMemory`, `Flaky`,
        // and `NotBuilt` have no status of their own and are folded onto the closest one the schema
        // does have; getting any of these wrong silently changes the score a reader sees in the
        // viewer relative to the printed one.
        for (outcome, status) in [
            (Outcome::Killed, "Killed"),
            (Outcome::Survived, "Survived"),
            (Outcome::Timeout, "Survived"),
            (Outcome::OutOfMemory, "Survived"),
            (Outcome::CompileError, "CompileError"),
            (Outcome::NoCoverage, "NoCoverage"),
            (Outcome::Ignored, "Ignored"),
            (Outcome::Flaky, "Ignored"),
            (Outcome::NotBuilt, "Ignored"),
        ] {
            assert_eq!(status_of(outcome), status, "{outcome} maps onto the wrong status");
            assert!(VALID.contains(&status), "{status} is not one of the schema's statuses");
        }

        // A never-judged mutant carries the schema's `Pending`, which sits outside the closed set
        // of resolved statuses above.
        assert_eq!(status_of(Outcome::Pending), "Pending");
    }

    #[test]
    fn the_schema_version_is_in_the_supported_range() {
        // The npm package is at 3.x but the schema only validates major 1 and 2.
        assert_eq!(SCHEMA_VERSION, "2");
    }

    #[test]
    fn branding_homepage_uris_follow_the_schema_format() {
        for uri in [
            "https://example.test/gamma%20report",
            "data:image/png;base64,AAAA",
            "mailto:maintainers@example.test",
        ] {
            assert!(is_uri(uri), "{uri} should be a URI");
        }

        for value in ["not a URI", "https://example.test/a space", "https://example.test/%xz"] {
            assert!(!is_uri(value), "{value} must not be a URI");
        }
    }

    #[test]
    fn a_span_becomes_a_one_based_half_open_location() {
        let source = SourceFile::parse("src/lib.rs", "fn f() {\n    a < b\n}\n".to_owned()).expect("parses");
        let start = source.text().find("a <").expect("present");
        let rendered = render(&mutant(Outcome::Survived, start..start + 5), &source);

        assert_eq!(rendered.location.start.line, 2);
        assert_eq!(rendered.location.start.column, 5);
        assert_eq!(rendered.location.end.line, 2);
        assert_eq!(rendered.location.end.column, 10);
    }

    #[test]
    fn a_bom_embedded_in_report_source_offsets_first_line_columns() {
        let source = SourceFile::parse("src/lib.rs", "\u{feff}fn f() {}".to_owned()).expect("parses");
        let rendered = render_with_first_line_offset(&mutant(Outcome::Survived, 0..2), &source, 1);

        assert_eq!(rendered.location.start.column, 2);
        assert_eq!(rendered.location.end.column, 4);
    }

    #[test]
    fn compact_report_fields_remain_json_strings() {
        let source = SourceFile::parse("src/lib.rs", "fn f() { a < b; }".to_owned()).expect("parses");
        let rendered = render(&mutant(Outcome::Survived, 9..14), &source);
        let json = serde_json::to_value(rendered).expect("serializes");

        assert_eq!(json["id"], "abc123abc123");
        assert_eq!(json["mutatorName"], "relational.lt_to_le");
        assert_eq!(json["status"], "Survived");
        assert_eq!(json["replacement"], "(a) <= (b)");
    }

    #[test]
    fn typed_reports_reject_two_results_for_one_mutant_id() {
        let source = SourceFile::parse("src/lib.rs", "fn f() { a < b; }".to_owned()).expect("parses");
        let first = render(&mutant(Outcome::Survived, 9..14), &source);
        let mut second = first.clone();

        second.status_reason = Some("a different observation".to_owned());

        let file = FileResult {
            source: source.text().to_owned(),
            language: "rust".to_owned(),
            mutants: vec![first, second],
        };

        assert_eq!(
            validate_file_result(&file, "report.files[\"src/lib.rs\"]").unwrap_err(),
            "report.files[\"src/lib.rs\"].mutants[1] duplicates another mutant"
        );
    }

    #[test]
    fn a_killing_test_is_named_in_the_status_reason() {
        let mut subject = mutant(Outcome::Killed, 0..1);

        subject.killed_by = Some("tests::the_boundary".to_owned());

        assert_eq!(reason_for(&subject), Some("failed `tests::the_boundary`".to_owned()));
        assert_eq!(
            render(&subject, &SourceFile::parse("src/lib.rs", "fn f() {}".to_owned()).expect("parses")).killed_by,
            Some(vec!["tests::the_boundary".to_owned()])
        );
    }

    /// A killed mutant whose killer arrived only as a `note` still explains itself.
    ///
    /// A merged or foreign report can carry a kill with no structured `killed_by` — the name of the
    /// failing test survives only in the free-text note. The `.or_else` fallback in `reason_for` is
    /// what keeps that explanation; deleting it leaves the viewer a bare "Killed" with an empty
    /// `statusReason`, and the reader with no test to look at.
    #[test]
    fn a_killed_mutant_without_a_named_test_falls_back_to_its_note() {
        let mut subject = mutant(Outcome::Killed, 0..1);

        subject.killed_by = None;
        subject.note = Some("failed X".to_owned());

        assert_eq!(reason_for(&subject), Some("failed X".to_owned()));
    }

    #[test]
    fn an_uncovered_mutant_explains_why_coverage_can_still_be_complete() {
        let reason = reason_for(&mutant(Outcome::NoCoverage, 0..1)).expect("a reason");

        assert!(reason.contains("no selected runtime test"), "{reason}");
        assert!(reason.contains("coverage reports"), "{reason}");
    }

    #[test]
    fn unviable_and_timeout_mutants_explain_their_status() {
        let mut timed_out = mutant(Outcome::Timeout, 0..1);

        // These verdicts are not self-explanatory in the report viewer, so the reason field
        // distinguishes a compile failure from a budget overrun.
        assert_eq!(
            reason_for(&mutant(Outcome::CompileError, 0..1)),
            Some("the mutant does not compile".to_owned())
        );
        assert_eq!(
            reason_for(&timed_out),
            Some(format!("{TIMEOUT_PREFIX}the test run exceeded its budget"))
        );

        timed_out.note = Some("stalled, last test named was `slow_case`".to_owned());

        assert_eq!(
            reason_for(&timed_out),
            Some(format!("{TIMEOUT_PREFIX}stalled, last test named was `slow_case`"))
        );
    }

    #[test]
    fn a_suppression_carries_its_reason_and_tag_into_the_report() {
        // This is what makes suppressions auditable at a glance in the viewer, rather than a
        // silent hole in the population.
        let mut subject = mutant(Outcome::Ignored, 0..1);

        subject.suppression = Some(Suppression {
            channel: Channel::Comment,
            reason: Some("fixed-point math".to_owned()),
            tag: Some("perf".to_owned()),
            line: Some(4),
        });

        assert_eq!(
            reason_for(&subject),
            Some("suppressed by comment: fixed-point math [#perf]".to_owned())
        );
    }

    #[test]
    fn a_suppression_with_no_reason_or_tag_still_names_its_channel() {
        // A suppression directive is never required to carry a reason or a tag, and the report
        // must not invent either one: naming the channel alone is the honest answer when that is
        // all the directive said.
        let mut subject = mutant(Outcome::Ignored, 0..1);

        subject.suppression = Some(Suppression {
            channel: Channel::Comment,
            reason: None,
            tag: None,
            line: Some(4),
        });

        assert_eq!(reason_for(&subject), Some("suppressed by comment".to_owned()));
    }

    #[test]
    fn an_untimed_mutant_omits_its_duration() {
        // Emitting `"duration": 0` for a mutant that never ran would show up in the viewer as a
        // suspiciously fast result rather than as no result.
        let source = SourceFile::parse("src/lib.rs", "fn f() {}".to_owned()).expect("parses");

        assert_eq!(render(&mutant(Outcome::Ignored, 0..1), &source).duration, None);

        let mut timed = mutant(Outcome::Killed, 0..1);

        timed.elapsed_ms = 12;

        assert_eq!(render(&timed, &source).duration, Some(12.0));
    }

    #[test]
    fn only_settled_mutants_are_carried_forward() {
        // A survivor has to be retried, because the next run's tests may kill it. A killed mutant
        // never will be, so rerunning it is pure cost.
        let text = r#"{
            "schemaVersion": "2",
            "thresholds": { "high": 80, "low": 60 },
            "framework": { "name": "cargo-gamma", "version": "0.1.0" },
            "files": {
                "src/lib.rs": {
                    "language": "rust",
                    "source": "src/lib.rs",
                    "mutants": [
                        { "id": "a", "mutatorName": "m", "status": "Killed", "location": { "start": { "line": 1, "column": 1 }, "end": { "line": 1, "column": 2 } } },
                        { "id": "b", "mutatorName": "m", "status": "Survived", "location": { "start": { "line": 1, "column": 1 }, "end": { "line": 1, "column": 2 } } },
                        { "id": "c", "mutatorName": "m", "status": "Timeout", "statusReason": "the test run exceeded its budget", "location": { "start": { "line": 1, "column": 1 }, "end": { "line": 1, "column": 2 } } },
                        { "id": "d", "mutatorName": "m", "status": "NoCoverage", "location": { "start": { "line": 1, "column": 1 }, "end": { "line": 1, "column": 2 } } }
                    ]
                }
            }
        }"#;

        let settled = settled_mutants(text).expect("parses");
        let mut ids: Vec<&str> = settled.iter().map(String::as_str).collect();

        ids.sort_unstable();
        assert_eq!(ids, vec!["a", "c"]);
    }

    /// A status whose meaning lives in a reason the document does not carry settles nothing.
    ///
    /// `Ignored` is written for a suppression, for a flake, and for a mutant the build gave up on;
    /// `Timeout` is written for both the clock and the memory ceiling. The reason is the only thing
    /// that tells them apart, so a verdict without one has not said which it is — and settling it
    /// would be choosing whichever reading happens to close the mutant.
    #[test]
    fn a_status_with_no_reason_to_disambiguate_it_is_not_settled() {
        assert_eq!(settled_verdict("Ignored", None), None);
        assert_eq!(settled_verdict("Timeout", None), None);

        assert_eq!(settled_verdict("Ignored", Some("suppressed by attribute")), Some(Outcome::Ignored));
        assert_eq!(
            settled_verdict("Timeout", Some("the test run exceeded its budget")),
            Some(Outcome::Timeout)
        );

        // Every `Ignored` and `Timeout` this tool writes carries a reason, so the rule costs a
        // report of our own nothing.
        for outcome in [Outcome::Ignored, Outcome::Timeout, Outcome::NotBuilt, Outcome::Flaky] {
            let mut subject = mutant(outcome, 0..1);

            if outcome == Outcome::Ignored {
                subject.suppression = Some(Suppression {
                    channel: Channel::Attribute,
                    reason: None,
                    tag: None,
                    line: None,
                });
            }

            assert!(
                reason_for(&subject).is_some(),
                "{outcome:?} exports with no reason to disambiguate it"
            );
        }
    }

    /// A report another tool wrote, or one from a schema this build predates, is refused.
    ///
    /// The format is shared, so a foreign document parses into [`Digest`] perfectly well and its
    /// statuses are then read with *this* tool's meanings — meanings that rest on a `statusReason`
    /// convention no schema imposes.
    #[test]
    fn a_report_this_tool_did_not_write_settles_nothing() {
        let document = |framework: &str, version: &str| {
            format!(
                r#"{{
                    "schemaVersion": "{version}",
                    "thresholds": {{ "high": 80, "low": 60 }},
                    "framework": {{ "name": "{framework}", "version": "0.1.0" }},
                    "files": {{
                        "src/lib.rs": {{
                            "language": "rust",
                            "source": "fn f() {{}}",
                            "mutants": [
                                {{ "id": "a", "mutatorName": "m", "status": "Killed", "location": {{ "start": {{ "line": 1, "column": 1 }}, "end": {{ "line": 1, "column": 2 }} }} }}
                            ]
                        }}
                    }}
                }}"#
            )
        };

        let foreign = settled_mutants(&document("stryker-js", SCHEMA_VERSION)).expect_err("a foreign producer");
        assert!(foreign.contains("stryker-js"), "{foreign}");

        let ahead = settled_mutants(&document(FRAMEWORK_NAME, "3")).expect_err("an unsupported schema");
        assert!(ahead.contains('3'), "{ahead}");

        for version in ["later", "2.bad", "2.0.0.0"] {
            let unreadable = settled_mutants(&document(FRAMEWORK_NAME, version)).expect_err("an unreadable schema");
            assert!(unreadable.contains(version), "{unreadable}");
        }

        // A minor and a patch component are additive, so our own future reports still read.
        let ours = settled_mutants(&document(FRAMEWORK_NAME, "2.1.3")).expect("our own report");
        assert_eq!(ours.len(), 1);
    }

    /// A report missing the fields that identify it is refused rather than trusted by default.
    #[test]
    fn a_report_that_does_not_identify_itself_settles_nothing() {
        let text = r#"{
            "files": {
                "src/lib.rs": {
                    "language": "rust",
                    "source": "fn f() {}",
                    "mutants": [
                        { "id": "a", "mutatorName": "m", "status": "Killed", "location": { "start": { "line": 1, "column": 1 }, "end": { "line": 1, "column": 2 } } }
                    ]
                }
            }
        }"#;

        let _cause = settled_mutants(text).expect_err("a document that names neither its schema nor its producer");
    }

    /// A cross-run reader borrows what it needs and never copies the source a report embeds.
    ///
    /// The embedded source is the overwhelming majority of a report — the complete text of every
    /// mutated file, kept so a viewer can render it without the repository — and both readers that
    /// carry knowledge between runs want only the verdicts. This is the assertion that keeps it that
    /// way: decoding through [`Report`] again would compile and pass every other test, while
    /// silently allocating the whole document.
    #[test]
    fn a_digest_borrows_its_verdicts_and_leaves_the_embedded_source_alone() {
        let text = r#"{
            "schemaVersion": "2",
            "thresholds": { "high": 80, "low": 60 },
            "framework": { "name": "cargo-gamma", "version": "0" },
            "files": {
                "src/lib.rs": {
                    "source": "fn f() {}",
                    "language": "rust",
                    "mutants": [
                        { "id": "a", "mutatorName": "m", "status": "Killed",
                          "location": { "start": { "line": 1, "column": 1 }, "end": { "line": 1, "column": 2 } },
                          "killedBy": ["tests::one"] }
                    ]
                }
            }
        }"#;

        let digest: Digest<'_> = serde_json::from_str(text).expect("parses");
        let file = digest.files.get("src/lib.rs").expect("the file");
        let mutant = file.mutants.first().expect("the mutant");

        assert!(
            matches!(mutant.id, Cow::Borrowed(_)),
            "an unescaped id was copied out of the document"
        );
        assert_eq!(mutant.settled_outcome(), Some(Outcome::Killed));
    }

    #[test]
    fn an_out_of_memory_mutant_keeps_its_note_in_the_status_reason() {
        // `Survived` keeps the schema's score aligned with Gamma's, while the reason preserves the
        // resource outcome the schema cannot represent.
        let mut starved = mutant(Outcome::OutOfMemory, 0..1);
        let source = SourceFile::parse("src/lib.rs", "fn f() {}".to_owned()).expect("parses");

        starved.note = Some("`unit-abc` reached 300.0 MB, past the 256.0 MB this run allowed it".to_owned());

        let rendered = render(&starved, &source);
        let json = serde_json::to_string(&rendered).expect("serializes");
        let read: MutantResult = serde_json::from_str(&json).expect("round-trips");
        let reason = read.status_reason.clone().expect("a reason");

        assert_eq!(read.status, "Survived");
        assert!(reason.starts_with(OUT_OF_MEMORY_PREFIX), "{reason}");
        assert!(reason.contains("256.0 MB"), "{reason}");
        assert_eq!(
            settled_verdict(&read.status, read.status_reason.as_deref()),
            None,
            "a mutant the ceiling stopped was never judged, so a rerun could change it"
        );
    }

    /// A flake keeps the test to fix in its status reason, and is told apart from a suppression.
    ///
    /// The schema has no flaky status, so it exports as `Ignored` — the same status a deliberate
    /// skip gets. Without the prefix and the note, a reader of the report would see a mutant
    /// somebody chose to ignore, when in fact a test in their suite is unreliable.
    #[test]
    fn a_flaky_mutant_keeps_the_test_to_fix_in_its_status_reason() {
        let mut flake = mutant(Outcome::Flaky, 0..1);
        let source = SourceFile::parse("src/lib.rs", "fn f() {}".to_owned()).expect("parses");

        flake.note = Some("test `a::b` in `unit-abc` fails with no mutant active as well as with one".to_owned());

        let rendered = render(&flake, &source);
        let json = serde_json::to_string(&rendered).expect("serializes");
        let read: MutantResult = serde_json::from_str(&json).expect("round-trips");
        let reason = read.status_reason.clone().expect("a reason");

        assert_eq!(read.status, "Ignored");
        assert!(reason.starts_with(FLAKY_PREFIX), "{reason}");
        assert!(reason.contains("test `a::b`"), "{reason}");
        assert_eq!(
            settled_verdict(&read.status, read.status_reason.as_deref()),
            None,
            "a flake was never judged, so a rerun could change it"
        );
    }

    /// A flake is re-tested by the next run rather than carried forward as settled.
    ///
    /// `Ignored` is otherwise the most settled status there is, so without this one unreliable test
    /// would permanently exclude a mutant that was never judged at all — and the next run is
    /// exactly the thing that might judge it. A genuine suppression stays settled.
    #[test]
    fn a_flake_is_not_settled_but_a_real_suppression_is() {
        let mut flake = mutant(Outcome::Flaky, 0..1);
        let source = SourceFile::parse("src/lib.rs", "fn f() {}".to_owned()).expect("parses");

        flake.note = Some("test `a::b` in `unit-abc` fails with no mutant active as well as with one".to_owned());

        let read: MutantResult =
            serde_json::from_str(&serde_json::to_string(&render(&flake, &source)).expect("serializes")).expect("round-trips");

        assert_eq!(
            settled_verdict(&read.status, read.status_reason.as_deref()),
            None,
            "a flake was never judged, so a rerun could change it"
        );

        let skipped = MutantResult {
            status_reason: Some("suppressed by an attribute".to_owned()),
            ..read
        };

        assert_eq!(
            settled_verdict(&skipped.status, skipped.status_reason.as_deref()),
            Some(Outcome::Ignored)
        );
    }

    /// A mutant the build gave up on is neither counted as suppressed nor carried forward as one.
    ///
    /// `Ignored` covers both "we chose not to test this" and "we could not", and the two ask a
    /// reader to do opposite things. Without the distinction, incremental execution would freeze a mutant
    /// nobody ever judged into every later run as though somebody had decided to skip it.
    #[test]
    fn a_mutant_the_build_gave_up_on_is_not_settled_but_a_real_suppression_is() {
        let mut abandoned = mutant(Outcome::NotBuilt, 0..1);
        let source = SourceFile::parse("src/lib.rs", "fn f() {}".to_owned()).expect("parses");

        abandoned.note = Some("the build this mutant belonged to could not be converged".to_owned());

        let rendered = render(&abandoned, &source);

        assert_eq!(rendered.status, "Ignored", "the status must stay in the schema's denominator");

        let read: MutantResult = serde_json::from_str(&serde_json::to_string(&rendered).expect("serializes")).expect("round-trips");

        assert!(
            read.status_reason
                .as_deref()
                .is_some_and(|reason| reason.starts_with(NOT_BUILT_PREFIX)),
            "{:?}",
            read.status_reason
        );
        assert_eq!(
            settled_verdict(&read.status, read.status_reason.as_deref()),
            None,
            "nothing was established about this mutant"
        );

        let skipped = MutantResult {
            status_reason: Some("suppressed by an attribute".to_owned()),
            ..read
        };

        assert_eq!(
            settled_verdict(&skipped.status, skipped.status_reason.as_deref()),
            Some(Outcome::Ignored)
        );
    }

    /// A reader can count the mutants a run gave up on without walking every mutant in the report.
    #[test]
    fn a_report_counts_the_mutants_its_run_never_built() {
        let source = "pub fn f(a: i32, b: i32) -> bool { a < b }
";
        let dir = crate::testing::workdir("elements-not-built-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8");

        fs::write(root.join("lib.rs").as_std_path(), source).expect("source");

        let plan = Plan {
            skipped: Vec::new(),
            digests: HashMap::default(),
            root: root.clone(),
            files: vec![TargetFile {
                path: Utf8PathBuf::from("lib.rs"),
                absolute: root.join("lib.rs"),
                package: "subject".to_owned(),
            }],
            mutants: vec![
                Mutant {
                    file: (Utf8PathBuf::from("lib.rs")).into(),
                    ..mutant(Outcome::NotBuilt, 35..40)
                },
                Mutant {
                    id: "def456def456".to_owned().into(),
                    file: (Utf8PathBuf::from("lib.rs")).into(),
                    ..mutant(Outcome::Killed, 35..40)
                },
            ],
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            reach: HashMap::default(),
            specs: HashMap::default(),
        };
        let info = RunInfo {
            started_at: 0,
            mutant_id_version: None,
            merged: false,
            shard: None,
            tests: None,
            not_built: None,
            dropped_test_packages: Vec::new(),
            merge_provenance: None,
        };

        let report = build(&plan, Thresholds::default(), Some(info)).expect("a report");

        assert_eq!(report.config.expect("config").not_built, Some(1));

        // Nothing to report is left out rather than written as a zero, so an ordinary run's report
        // is unchanged.
        let clean = Plan {
            mutants: vec![Mutant {
                file: (Utf8PathBuf::from("lib.rs")).into(),
                ..mutant(Outcome::Killed, 35..40)
            }],
            ..plan
        };
        let report = build(
            &clean,
            Thresholds::default(),
            Some(RunInfo {
                started_at: 0,
                mutant_id_version: None,
                merged: false,
                shard: None,
                tests: None,
                not_built: None,
                dropped_test_packages: Vec::new(),
                merge_provenance: None,
            }),
        )
        .expect("a report");

        assert_eq!(report.config.expect("config").not_built, None);
    }

    #[test]
    fn an_out_of_memory_mutant_with_no_note_still_says_what_stopped_it() {
        let reason = reason_for(&mutant(Outcome::OutOfMemory, 0..1)).expect("a reason");

        assert!(reason.starts_with(OUT_OF_MEMORY_PREFIX), "{reason}");
        assert!(reason.len() > OUT_OF_MEMORY_PREFIX.len(), "{reason}");
    }

    #[test]
    fn a_timeout_exports_as_undetected_without_losing_its_verdict() {
        let source = SourceFile::parse("src/lib.rs", "fn f() {}".to_owned()).expect("parses");
        let rendered = render(&mutant(Outcome::Timeout, 0..1), &source);

        assert_eq!(rendered.status, "Survived");
        assert!(
            rendered
                .status_reason
                .as_deref()
                .is_some_and(|reason| reason.starts_with(TIMEOUT_PREFIX))
        );
        assert_eq!(
            settled_verdict(&rendered.status, rendered.status_reason.as_deref()),
            Some(Outcome::Timeout)
        );
    }

    #[test]
    fn an_out_of_memory_verdict_is_not_settled_but_a_real_timeout_is() {
        // The ceiling is inferred from a sampled peak rather than observed, so a spurious verdict
        // must not become permanent. A stall is observed, so it stays settled.
        let text = r#"{
            "schemaVersion": "2",
            "thresholds": { "high": 80, "low": 60 },
            "framework": { "name": "cargo-gamma", "version": "0.1.0" },
            "files": {
                "src/lib.rs": {
                    "language": "rust",
                    "source": "src/lib.rs",
                    "mutants": [
                        { "id": "stalled", "mutatorName": "m", "status": "Timeout", "statusReason": "stalled, last test named was `slow_case`", "location": { "start": { "line": 1, "column": 1 }, "end": { "line": 1, "column": 2 } } },
                        { "id": "starved", "mutatorName": "m", "status": "Timeout", "statusReason": "out of memory: `unit-abc` reached 300.0 MB, past the 256.0 MB this run allowed it", "location": { "start": { "line": 1, "column": 1 }, "end": { "line": 1, "column": 2 } } }
                    ]
                }
            }
        }"#;

        let settled = settled_mutants(text).expect("parses");

        assert!(settled.contains("stalled"), "{settled:?}");
        assert!(!settled.contains("starved"), "{settled:?}");
    }

    #[test]
    fn a_file_without_mutants_is_left_out_of_the_report() {
        let plan = Plan {
            skipped: Vec::new(),
            digests: HashMap::default(),
            root: Utf8PathBuf::from("/w"),
            files: vec![TargetFile {
                path: Utf8PathBuf::from("src/lib.rs"),
                absolute: Utf8PathBuf::from("/w/src/lib.rs"),
                package: "subject".to_owned(),
            }],
            mutants: Vec::new(),
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            reach: HashMap::default(),
            specs: HashMap::default(),
        };

        let report = build(&plan, Thresholds::default(), None).expect("report builds");

        // Embedding every selected file would bloat empty reports and try to read files that have
        // no result to show.
        assert!(report.files.is_empty());
    }

    #[test]
    fn an_unparsable_prior_report_is_reported_rather_than_ignored() {
        let _cause = settled_mutants("not json").unwrap_err();
    }

    #[test]
    fn the_document_serializes_with_the_schema_field_names() {
        let report = Report {
            schema_version: SCHEMA_VERSION.to_owned(),
            thresholds: Thresholds::default(),
            project_root: None,
            framework: Framework {
                name: "cargo-gamma".to_owned(),
                version: "0.1.0".to_owned(),
            },
            files: BTreeMap::new(),
            config: None,
        };
        let json = to_json(&report).expect("serializes");

        assert!(json.contains("\"schemaVersion\": \"2\""), "{json}");
        assert!(json.contains("\"thresholds\""), "{json}");
        assert!(json.contains("\"files\""), "{json}");
        assert!(!json.contains("projectRoot"), "{json}");
    }

    #[test]
    fn serialization_refuses_reports_outside_the_adopted_schema() {
        let mut threshold = fixtures::report();
        threshold.thresholds.high = 101;

        assert!(to_json(&threshold).is_err(), "an out-of-range threshold must not be emitted");

        let mut position = fixtures::report();
        let mut mutant = fixtures::mutant_result();
        mutant.location.start.line = 0;
        let _ = position.files.insert(
            "src/lib.rs".to_owned(),
            FileResult {
                source: "fn f() {}\n".to_owned(),
                language: "rust".to_owned(),
                mutants: vec![mutant],
            },
        );

        assert!(to_json(&position).is_err(), "a zero source position must not be emitted");
    }

    #[test]
    fn serialization_refuses_non_finite_durations_before_publishing() {
        let directory = crate::testing::workdir("elements-non-finite-duration-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the scratch path is UTF-8");

        for (index, duration) in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY].into_iter().enumerate() {
            let mut mutant = fixtures::mutant_result();
            mutant.duration = Some(duration);
            let report = fixtures::report_with(None, 100, vec![mutant]);
            let error = to_json(&report).expect_err("non-finite duration must not serialize");

            assert!(error.to_string().contains("duration must be a finite JSON number"), "{error}");

            let path = root.join(format!("report-{index}.json"));
            let error = write_json(&report, &path).expect_err("non-finite duration must not be published");

            assert!(error.to_string().contains("duration must be a finite JSON number"), "{error}");
            assert!(!path.exists(), "a rejected report must leave no file behind");
        }
    }

    /// The streamed JSON writer emits exactly the bytes the string form produces and publishes them
    /// atomically, so moving the report path to streaming cannot change the document a reader gets.
    #[test]
    fn streamed_json_matches_the_string_form() {
        let report = fixtures::report();
        let directory = crate::testing::workdir("elements-stream-json-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the scratch path is UTF-8");
        let path = root.join("report.json");

        write_json(&report, &path).expect("streams the report");

        let expected = to_json(&report).expect("serializes");
        assert_eq!(fs::read_to_string(path.as_std_path()).expect("published bytes"), expected);
    }

    /// A report the schema rejects is refused before a byte reaches the destination, matching the
    /// string form's validation and leaving no partial file behind.
    #[test]
    fn streamed_json_refuses_an_out_of_schema_report_without_writing() {
        let mut threshold = fixtures::report();
        threshold.thresholds.high = 101;
        let directory = crate::testing::workdir("elements-stream-invalid-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the scratch path is UTF-8");
        let path = root.join("report.json");

        assert!(
            write_json(&threshold, &path).is_err(),
            "an out-of-range threshold must not be emitted"
        );
        assert!(!path.exists(), "a rejected report must leave no file behind");
    }

    /// Every discovered mutant has to reach the report, because the score's denominator is the
    /// population the report holds.
    ///
    /// `build` groups mutants by file and then walks the plan's file list, skipping any group whose
    /// file the plan does not list. Nothing else asserts that what came out equals what went in, so
    /// emitting one mutant per file — or breaking out of the loop after the first — passes every
    /// other test in this module, and the failure it hides is the one the design says must never
    /// happen quietly: a smaller denominator reads as a better score.
    #[test]
    fn every_discovered_mutant_reaches_the_report() {
        let directory = crate::testing::workdir("elements-conservation");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the scratch path is UTF-8");

        let sources = [
            ("a.rs", "fn a() { x(a < b); }"),
            ("b.rs", "fn b() { y(a < b); }"),
            ("c.rs", "fn c() { z(a < b); }"),
        ];

        for (name, text) in sources {
            fs::write(root.join(name).as_std_path(), text).expect("source");
        }

        let files: Vec<TargetFile> = sources
            .iter()
            .map(|(name, _text)| TargetFile {
                path: Utf8PathBuf::from(*name),
                absolute: root.join(*name),
                package: "subject".to_owned(),
            })
            .collect();

        // Two mutants in one file, one in each of the others, so that a loop emitting a single
        // mutant per file and a loop stopping after the first file both fail.
        let placements = [("a.rs", "m1"), ("a.rs", "m2"), ("b.rs", "m3"), ("c.rs", "m4")];

        let plan = Plan {
            skipped: Vec::new(),
            digests: HashMap::default(),
            root,
            files,
            mutants: placements
                .iter()
                .map(|(file, id)| Mutant {
                    id: (*id).to_owned().into(),
                    file: (Utf8PathBuf::from(*file)).into(),
                    ..mutant(Outcome::Killed, 11..16)
                })
                .collect(),
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            reach: HashMap::default(),
            specs: HashMap::default(),
        };

        let report = build(&plan, Thresholds::default(), None).expect("a report");

        let mut emitted: Vec<(String, String)> = report
            .files
            .iter()
            .flat_map(|(path, file)| file.mutants.iter().map(move |result| (path.clone(), result.id.to_string())))
            .collect();

        emitted.sort();

        let mut expected: Vec<(String, String)> = placements.iter().map(|(file, id)| ((*file).to_owned(), (*id).to_owned())).collect();

        expected.sort();

        assert_eq!(emitted, expected);
    }

    #[test]
    fn a_report_refuses_source_changed_after_discovery() {
        let directory = crate::testing::workdir("elements-source-generation");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the scratch path is UTF-8");
        let path = Utf8PathBuf::from("lib.rs");
        let absolute = root.join(&path);
        let discovered = "fn f() { a < b; }\n";

        fs::write(&absolute, discovered).expect("discovered source");

        let mut digests = HashMap::default();
        let _previous = digests.insert(path.clone(), crate::discover::digest(discovered.as_bytes()));
        let plan = Plan {
            skipped: Vec::new(),
            digests,
            root,
            files: vec![TargetFile {
                path: path.clone(),
                absolute: absolute.clone(),
                package: "subject".to_owned(),
            }],
            mutants: vec![Mutant {
                file: path.into(),
                ..mutant(Outcome::Killed, 9..14)
            }],
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            reach: HashMap::default(),
            specs: HashMap::default(),
        };

        fs::write(&absolute, "fn f() { a > b; }\n").expect("edited source");

        let error = build(&plan, Thresholds::default(), None).expect_err("mixed source generations must be refused");

        assert!(
            error
                .to_string()
                .contains("rerun cargo-gamma so discovery, verdicts, and report source use the same generation"),
            "{error}"
        );
    }

    /// A mutant in a file the plan does not list would vanish from the denominator without a word.
    ///
    /// The skip is deliberate — a report cannot embed the source of a file the survey never
    /// recorded — but it is exactly the shape of the loss the test above guards against, so the
    /// case is pinned rather than left to be discovered as a wrong score.
    #[test]
    fn a_mutant_in_an_unlisted_file_is_the_only_thing_the_report_drops() {
        let directory = crate::testing::workdir("elements-unlisted");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the scratch path is UTF-8");

        fs::write(root.join("a.rs").as_std_path(), "fn a() { x(a < b); }").expect("source");

        let plan = Plan {
            skipped: Vec::new(),
            digests: HashMap::default(),
            root: root.clone(),
            files: vec![TargetFile {
                path: Utf8PathBuf::from("a.rs"),
                absolute: root.join("a.rs"),
                package: "subject".to_owned(),
            }],
            mutants: vec![
                Mutant {
                    id: "listed".to_owned().into(),
                    file: (Utf8PathBuf::from("a.rs")).into(),
                    ..mutant(Outcome::Killed, 11..16)
                },
                Mutant {
                    id: "unlisted".to_owned().into(),
                    file: (Utf8PathBuf::from("gone.rs")).into(),
                    ..mutant(Outcome::Killed, 11..16)
                },
            ],
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            reach: HashMap::default(),
            specs: HashMap::default(),
        };

        let report = build(&plan, Thresholds::default(), None).expect("a report");
        let emitted: Vec<&str> = report
            .files
            .values()
            .flat_map(|file| file.mutants.iter().map(|result| result.id.as_str()))
            .collect();

        assert_eq!(emitted, vec!["listed"]);
        assert!(!report.files.contains_key("gone.rs"));
    }

    /// Streaming `write_json` produces the same bytes as the in-memory `to_json`.
    #[test]
    fn write_json_matches_to_json_output() {
        let report = fixtures::report_with(Some((0, 2)), 100, vec![fixtures::mutant_result()]);
        let expected = to_json(&report).expect("to_json succeeds");

        let dir = crate::testing::workdir("write_json_matches");
        let path = Utf8PathBuf::from_path_buf(dir.path().join("report.json")).expect("utf8");

        write_json(&report, &path).expect("write_json succeeds");

        let written = fs::read_to_string(&path).expect("read back");
        assert_eq!(written, expected, "streaming write must produce identical bytes");
    }
}
