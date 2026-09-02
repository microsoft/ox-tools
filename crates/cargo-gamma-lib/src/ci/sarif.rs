// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The twelve pieces of a SARIF 2.1.0 log document, all serialized together as one wire format.

use camino::Utf8Path;
use serde::Serialize;

use super::finding::{describe, findings, relative};
use super::level::Level;
use super::truncation::Truncation;
use crate::model::Mutant;
use crate::{HashMap, HashSet, Result};

/// A SARIF 2.1.0 log.
#[derive(Debug, Serialize)]
pub(super) struct Log<'findings> {
    pub(super) version: &'static str,
    #[serde(rename = "$schema")]
    pub(super) schema: &'static str,
    pub(super) runs: Vec<Run<'findings>>,
}

/// Borrows its results rather than owning them, so that fitting the log to the byte cap can measure
/// one prefix after another over findings that were built once.
#[derive(Debug, Serialize)]
pub(super) struct Run<'findings> {
    pub(super) tool: Tool,
    pub(super) results: &'findings [Finding],
}

#[derive(Debug, Serialize)]
pub(super) struct Tool {
    pub(super) driver: Driver,
}

#[derive(Debug, Serialize)]
pub(super) struct Driver {
    pub(super) name: &'static str,
    #[serde(rename = "informationUri")]
    pub(super) information_uri: &'static str,
    pub(super) version: &'static str,
    pub(super) rules: Vec<Rule>,
}

#[derive(Debug, Serialize)]
pub(super) struct Rule {
    pub(super) id: String,
    pub(super) name: String,
    #[serde(rename = "shortDescription")]
    pub(super) short_description: Text,
    #[serde(rename = "fullDescription")]
    pub(super) full_description: Text,
    #[serde(rename = "defaultConfiguration")]
    pub(super) default_configuration: Configuration,
}

#[derive(Debug, Serialize)]
pub(super) struct Configuration {
    pub(super) level: &'static str,
}

#[derive(Debug, Serialize)]
pub(super) struct Text {
    pub(super) text: String,
}

#[derive(Debug, Serialize)]
pub(super) struct Finding {
    #[serde(rename = "ruleId")]
    pub(super) rule_id: String,
    pub(super) level: &'static str,
    pub(super) message: Text,
    pub(super) locations: Vec<Location>,
    #[serde(rename = "partialFingerprints")]
    pub(super) partial_fingerprints: HashMap<String, String>,
}

#[derive(Debug, Serialize)]
pub(super) struct Location {
    #[serde(rename = "physicalLocation")]
    pub(super) physical_location: Physical,
}

#[derive(Debug, Serialize)]
pub(super) struct Physical {
    #[serde(rename = "artifactLocation")]
    pub(super) artifact_location: Artifact,
    pub(super) region: Region,
}

#[derive(Debug, Serialize)]
pub(super) struct Artifact {
    pub(super) uri: String,
}

#[derive(Debug, Serialize)]
pub(super) struct Region {
    #[serde(rename = "startLine")]
    pub(super) start_line: usize,
    #[serde(rename = "startColumn")]
    pub(super) start_column: usize,
}

/// GitHub rejects a SARIF upload with more results than this.
///
/// It is a hard limit on their side, not a preference on ours: exceeding it fails the upload
/// outright, which is a worse outcome than a report that says what it left out.
pub(crate) const SARIF_LIMIT: usize = 5_000;

/// GitHub rejects a SARIF upload larger than this, whatever it contains.
///
/// The result count is not a reliable proxy for the size, because a finding carries a message, a
/// path and a fingerprint whose lengths are the code's business rather than ours. A log under the
/// count limit and over the byte limit is rejected just as completely, so both are enforced.
const SARIF_BYTES: usize = 10 * 1024 * 1024;

/// Renders survivors as a SARIF 2.1.0 log, and says what it had to leave out.
///
/// Rule identifiers are our stable mutator names, which is what makes GitHub's alert grouping and
/// dismissal work per operator: a team can permanently dismiss every `literal.int_zero` alert
/// without touching anything else, and that decision keeps applying to code written next year.
///
/// # Errors
///
/// Returns an error if the log cannot be serialized to JSON. Nothing in the document is caller
/// data of a kind serde can refuse — every value is a string, an integer or a fixed-shape struct —
/// so this reports a failure that should not be reachable rather than a condition to handle.
///
/// Exceeding either the result-count or the byte cap is not an error: the log is shortened until it
/// fits and the returned [`Truncation`] says what was dropped, because a CI run that uploads
/// nothing is worse than one that uploads the survivors it had room for.
pub fn sarif(mutants: &[Mutant], root: &Utf8Path, level: Level) -> Result<(String, Option<Truncation>)> {
    let survivors = findings(mutants);
    let found = survivors.len();
    let kept: Vec<&Mutant> = survivors.into_iter().take(SARIF_LIMIT).collect();
    let results = results(&kept, root, level);

    // Shrunk until it fits rather than estimated, because the size of a finding is decided by the
    // length of a path, a message and an identifier, none of which this can predict. Halving
    // converges in a handful of measurements even from the count limit, and the alternative to any
    // of it is an upload GitHub refuses whole.
    //
    // What is measured is not what is built. Each candidate prefix is serialized into a writer that
    // counts bytes and keeps none of them, so a log that is over the cap costs its length in
    // arithmetic rather than in a multi-megabyte `String` that is looked at once and dropped. Only
    // the prefix that fits is rendered, exactly once. The sequence of prefixes is the same one the
    // repeated-render form walked, so the log that comes out is the same log.
    let mut length = results.len();

    loop {
        let rules = rules(&kept[..length], level);
        let log = log(&results[..length], rules);

        if measure(&log)? <= SARIF_BYTES || length == 0 {
            let text = serde_json::to_string_pretty(&log)
                .map_err(|cause| crate::error::error!("could not serialize the SARIF log").caused_by(cause))?;
            let truncation = (found > length).then_some(Truncation { found, written: length });

            return Ok((text, truncation));
        }

        length /= 2;
    }
}

/// The byte length the log would serialize to, without keeping the bytes.
///
/// # Errors
///
/// Returns an error if the log cannot be serialized.
fn measure(log: &Log<'_>) -> Result<usize> {
    let mut counter = Counted::default();

    serde_json::to_writer_pretty(&mut counter, log)
        .map_err(|cause| crate::error::error!("could not serialize the SARIF log").caused_by(cause))?;

    Ok(counter.bytes)
}

/// A writer that remembers how much was written to it and nothing else.
#[derive(Debug, Default)]
struct Counted {
    bytes: usize,
}

impl std::io::Write for Counted {
    #[expect(clippy::renamed_function_params, reason = "`buf` is less clear than `buffer`")]
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes += buffer.len();

        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Assembles the log document around results that were built once and rules that were not.
fn log(results: &[Finding], rules: Vec<Rule>) -> Log<'_> {
    Log {
        version: "2.1.0",
        schema: "https://json.schemastore.org/sarif-2.1.0.json",
        runs: vec![Run {
            tool: Tool {
                driver: Driver {
                    name: "cargo-gamma",
                    information_uri: "https://github.com/microsoft/ox-tools/tree/main/crates/cargo-gamma",
                    version: env!("CARGO_PKG_VERSION"),
                    rules,
                },
            },
            results,
        }],
    }
}

/// The rule table for exactly the findings that are being kept.
///
/// Rebuilt per candidate rather than trimmed, because a rule is shared by every finding that names
/// it and dropping the tail of the results can retire a rule that only they used. There are at most
/// as many rules as there are mutators, so this is small however large the population is.
fn rules(kept: &[&Mutant], level: Level) -> Vec<Rule> {
    let mut seen = HashSet::default();
    let mut rules = Vec::new();

    for mutant in kept {
        if !seen.insert(mutant.mutator.to_string()) {
            continue;
        }

        rules.push(Rule {
            id: mutant.mutator.to_string(),
            name: mutant.mutator.to_string(),
            short_description: Text {
                text: format!("Surviving mutant: {}", mutant.mutator),
            },
            full_description: Text {
                text: format!(
                    "The {} mutation was applied and the test suite still passed, so nothing asserts on the \
                     behavior it changed.",
                    mutant.mutator
                ),
            },
            default_configuration: Configuration { level: level.as_str() },
        });
    }

    rules.sort_by(|left, right| left.id.cmp(&right.id));

    rules
}

/// Builds one SARIF result per kept mutant.
fn results(kept: &[&Mutant], root: &Utf8Path, level: Level) -> Vec<Finding> {
    kept.iter()
        .map(|mutant| {
            let mut fingerprints = HashMap::default();

            // The mutant id is content-addressed, so an alert follows its code through reformatting
            // and through edits elsewhere in the file instead of being dismissed and resurrected.
            let _previous = fingerprints.insert(format!("gammaMutantId/v{}", crate::model::MUTANT_ID_VERSION), mutant.id.to_string());

            Finding {
                rule_id: mutant.mutator.to_string(),
                level: level.as_str(),
                message: Text { text: describe(mutant) },
                locations: vec![Location {
                    physical_location: Physical {
                        artifact_location: Artifact {
                            uri: relative(&mutant.file, root),
                        },
                        region: Region {
                            start_line: mutant.line,
                            start_column: mutant.column,
                        },
                    },
                }],
                partial_fingerprints: fingerprints,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::model::Outcome;
    use crate::testing::ci_fixture::{mutant, root};

    #[test]
    fn sarif_carries_one_rule_per_mutator() {
        let mutants = vec![
            mutant("/w/src/a.rs", 1, "relational.gt_to_ge", Outcome::Survived),
            mutant("/w/src/a.rs", 2, "relational.gt_to_ge", Outcome::Survived),
            mutant("/w/src/a.rs", 3, "literal.int_zero", Outcome::Survived),
        ];

        let (text, truncation) = sarif(&mutants, &root(), Level::Note).expect("sarif");
        let log: Value = serde_json::from_str(&text).expect("valid json");

        assert_eq!(truncation, None);

        let rules = log["runs"][0]["tool"]["driver"]["rules"].as_array().expect("rules");

        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0]["id"], "literal.int_zero");
        assert_eq!(log["runs"][0]["results"].as_array().expect("results").len(), 3);
    }

    #[test]
    fn a_sarif_result_is_fingerprinted_by_mutant_id() {
        let mutants = vec![mutant("/w/src/a.rs", 7, "relational.gt_to_ge", Outcome::Survived)];
        let (text, _) = sarif(&mutants, &root(), Level::Warning).expect("sarif");
        let log: Value = serde_json::from_str(&text).expect("valid json");
        let result = &log["runs"][0]["results"][0];

        assert_eq!(result["level"], "warning");
        assert_eq!(
            result["partialFingerprints"][format!("gammaMutantId/v{}", crate::model::MUTANT_ID_VERSION)],
            "/w/src/a.rs:7:relational.gt_to_ge"
        );

        let region = &result["locations"][0]["physicalLocation"];

        assert_eq!(region["artifactLocation"]["uri"], "src/a.rs");
        assert_eq!(region["region"]["startLine"], 7);
    }

    /// The count cap only matters at its boundary, and the boundary is the one place nothing
    /// exercised it: the shrink-to-fit test stops at exactly `SARIF_LIMIT`, so neither the
    /// constant nor the `take` that enforces it was ever asked to drop a result. Exceeding
    /// GitHub's cap fails the upload whole, losing every finding rather than the excess.
    #[test]
    fn one_finding_past_the_cap_is_dropped_and_reported() {
        let mutants: Vec<Mutant> = (0..=SARIF_LIMIT)
            .map(|line| mutant("/w/src/a.rs", line, "relational.gt_to_ge", Outcome::Survived))
            .collect();

        assert_eq!(mutants.len(), SARIF_LIMIT + 1);

        let (text, truncation) = sarif(&mutants, &root(), Level::Warning).expect("sarif");
        let log: Value = serde_json::from_str(&text).expect("valid json");
        let truncation = truncation.expect("one finding past the cap must be reported as truncated");

        // The rendered log is what GitHub sees, so the cap is asserted there and not only on the
        // bookkeeping that describes it.
        assert_eq!(log["runs"][0]["results"].as_array().expect("results").len(), SARIF_LIMIT);
        assert_eq!(truncation.found, SARIF_LIMIT + 1);
        assert_eq!(truncation.written, SARIF_LIMIT);
        assert_eq!(truncation.found - truncation.written, 1);
    }

    #[test]
    #[cfg(not(miri))]
    fn a_log_too_large_to_upload_is_shrunk_until_it_fits() {
        // The count limit is not a size limit: a finding's size is decided by the length of a path
        // and a message, and a log GitHub refuses is worth nothing however many results it holds.
        let deep = format!("/w/src/{}/a.rs", "nested/".repeat(500));
        let mutants: Vec<Mutant> = (0..SARIF_LIMIT)
            .map(|line| mutant(&deep, line, "relational.gt_to_ge", Outcome::Survived))
            .collect();

        let (text, truncation) = sarif(&mutants, &root(), Level::Warning).expect("sarif");
        let truncation = truncation.expect("a log this large cannot have been written whole");

        assert!(text.len() <= SARIF_BYTES, "{} bytes", text.len());
        assert_eq!(truncation.found, SARIF_LIMIT);
        assert!(truncation.written < SARIF_LIMIT, "{}", truncation.written);
    }
}
