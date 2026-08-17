// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Before/after probe grading shared by regression evidence and macro compile
//! evidence.
//!
//! A measured outcome records which revision was exercised, what it did, and
//! the exit status that proves it. Compile fixtures and behavior probes share
//! this grader so they cannot drift apart on what counts as a usable
//! measurement.

use ohno::{AppError, bail};

use crate::model::{CompileEntry, MeasuredInput, PackageFact, RegressionEntry, RegressionEvidenceOutput, normalize_ident};
use crate::version::ChangeType;

/// The recognized regression-evidence probe kinds.
pub(crate) const REGRESSION_EVIDENCE_KINDS: [&str; 3] = ["consumer-runtime", "consumer-compile", "packaged-artifact"];

/// The graded half of a before/after probe.
#[derive(Debug, Clone)]
pub(crate) struct MeasuredOutcome {
    /// `pass`/`fail` when the measurement is complete, else `None`.
    pub(crate) result: Option<String>,
    /// The exercised revision (trimmed; possibly empty).
    pub(crate) revision: String,
    /// The recorded exit code, when present and integral.
    pub(crate) exit_code: Option<i64>,
    /// Whether the measurement records a usable pass/fail with revision and
    /// exit code.
    pub(crate) complete: bool,
}

/// Grades one measured side of a probe.
pub(crate) fn measured_outcome(input: Option<&MeasuredInput>) -> MeasuredOutcome {
    let Some(input) = input else {
        return MeasuredOutcome {
            result: None,
            revision: String::new(),
            exit_code: None,
            complete: false,
        };
    };
    let result = input.result.trim().to_ascii_lowercase();
    let revision = input.revision.trim().to_string();
    let exit_code = input.exit_code;
    let complete = matches!(result.as_str(), "pass" | "fail") && !revision.is_empty() && exit_code.is_some();
    MeasuredOutcome {
        result: complete.then_some(result),
        revision,
        exit_code,
        complete,
    }
}

/// The parsed regression evidence for one selection decision.
#[derive(Debug, Clone)]
pub(crate) struct RegressionEvidence {
    /// Normalized entries (sorted by kind then probe).
    pub(crate) entries: Vec<RegressionEvidenceOutput>,
    /// De-duplicated, sorted issue messages.
    pub(crate) issues: Vec<String>,
    /// Whether at least one probe demonstrated a fail→pass fix.
    pub(crate) demonstrated: bool,
}

/// Grades regression evidence.
///
/// # Errors
///
/// Returns an error when an entry is a bare string, omits its probe, or uses an
/// unrecognized kind — the malformed-shape cases the resolver rejects.
pub(crate) fn regression_evidence(package: &str, entries: &[RegressionEntry]) -> Result<RegressionEvidence, AppError> {
    let mut graded: Vec<RegressionEvidenceOutput> = Vec::new();
    let mut issues: Vec<String> = Vec::new();
    let mut demonstrated = false;

    for entry in entries {
        let item = match entry {
            RegressionEntry::Text(text) => bail!(
                "Regression evidence in selection decision '{package}' must be an object with kind, \
                 probe, baseline, and current, not the bare string {text:?}."
            ),
            RegressionEntry::Item(item) => item,
        };
        let probe = item.probe.trim();
        if probe.is_empty() {
            bail!("Regression evidence in selection decision '{package}' must name the probe it exercised.");
        }
        let kind = item.kind.trim().to_ascii_lowercase();
        if !REGRESSION_EVIDENCE_KINDS.contains(&kind.as_str()) {
            bail!(
                "Regression evidence '{probe}' in selection decision '{package}' must use kind {}.",
                REGRESSION_EVIDENCE_KINDS.join(", ")
            );
        }

        let baseline = measured_outcome(item.baseline.as_ref());
        let current = measured_outcome(item.current.as_ref());
        for (name, outcome) in [("baseline", &baseline), ("current", &current)] {
            if !outcome.complete {
                issues.push(format!(
                    "Regression evidence '{probe}' in selection decision '{package}' does not record a \
                     {name} pass/fail result with a revision and exit code."
                ));
                continue;
            }
            // An exit code that contradicts the recorded result means the
            // measurement was mis-transcribed; neither half can be trusted.
            if (outcome.result.as_deref() == Some("pass")) != (outcome.exit_code == Some(0)) {
                issues.push(format!(
                    "Regression evidence '{probe}' in selection decision '{package}' records a {name} \
                     result of '{}' with exit code {}.",
                    outcome.result.as_deref().unwrap_or(""),
                    outcome.exit_code.unwrap_or_default()
                ));
            }
        }

        let mut outcome_label: Option<String> = None;
        if baseline.complete
            && current.complete
            && (baseline.result.as_deref() == Some("pass")) == (baseline.exit_code == Some(0))
            && (current.result.as_deref() == Some("pass")) == (current.exit_code == Some(0))
        {
            if baseline.revision == current.revision {
                issues.push(format!(
                    "Regression evidence '{probe}' in selection decision '{package}' measures revision \
                     '{}' on both sides.",
                    baseline.revision
                ));
            } else {
                let base = baseline.result.clone().unwrap_or_default();
                let curr = current.result.clone().unwrap_or_default();
                outcome_label = Some(format!("{base}->{curr}"));
                if base == "fail" && curr == "pass" {
                    demonstrated = true;
                }
            }
        }

        graded.push(RegressionEvidenceOutput {
            kind,
            probe: probe.to_string(),
            outcome: outcome_label.unwrap_or_else(|| "inconclusive".to_string()),
        });
    }

    graded.sort_by(|a, b| (a.kind.as_str(), a.probe.as_str()).cmp(&(b.kind.as_str(), b.probe.as_str())));
    issues.sort();
    issues.dedup();
    Ok(RegressionEvidence {
        entries: graded,
        issues,
        demonstrated,
    })
}

/// A `.stderr`/`.stdout` sibling collapses onto the `.rs` fixture it records, so
/// one measurement of that fixture discharges the whole group.
pub(crate) fn compile_fixture_key(path: &str) -> String {
    let normalized = path.trim().replace('\\', "/");
    for extension in [".stderr", ".stdout"] {
        if let Some(stripped) = strip_suffix_ignore_ascii_case(&normalized, extension) {
            return format!("{stripped}.rs");
        }
    }
    normalized
}

fn strip_suffix_ignore_ascii_case(value: &str, suffix: &str) -> Option<String> {
    if value.len() < suffix.len() {
        return None;
    }
    let (head, tail) = value.split_at(value.len() - suffix.len());
    tail.eq_ignore_ascii_case(suffix).then(|| head.to_string())
}

/// The mechanical reading of a compile fixture: what the same consumer program
/// did before the change versus after it.
fn compile_evidence_outcome(baseline: &str, current: &str) -> ChangeType {
    match (baseline, current) {
        ("pass", "fail") => ChangeType::Breaking,
        ("fail", "pass") => ChangeType::NonBreaking,
        _ => ChangeType::Patch,
    }
}

/// One recorded compile-evidence measurement, keyed for obligation discharge.
#[derive(Debug, Clone)]
pub(crate) struct CompileEvidenceEntry {
    /// The fixture owner, normalized (`-`→`_`).
    pub(crate) owner_package: String,
    /// The fixture key (`.rs` form).
    pub(crate) key: String,
}

/// The parsed compile evidence for one macro contract.
#[derive(Debug, Clone)]
pub(crate) struct MacroCompileEvidence {
    /// `owner|key` pairs actually recorded, used to check that every obligation
    /// was evidenced. Recorded for every well-formed entry, even one whose
    /// measurement is incomplete — an incomplete measurement is a separate
    /// "inconclusive" issue, not an "unevidenced" one.
    pub(crate) entries: Vec<CompileEvidenceEntry>,
    /// De-duplicated, sorted issue messages.
    pub(crate) issues: Vec<String>,
    /// The verdict floor derived from the measured outcomes.
    pub(crate) derived_floor: ChangeType,
    /// The fixtures that decided the floor (for reporting).
    pub(crate) deciding: Vec<String>,
}

/// Grades macro compile evidence.
///
/// # Errors
///
/// Returns an error when an entry is a bare string or omits its owner/path.
pub(crate) fn macro_compile_evidence(fact: &PackageFact, entries: &[CompileEntry]) -> Result<MacroCompileEvidence, AppError> {
    let mut recorded: Vec<CompileEvidenceEntry> = Vec::new();
    let mut issues: Vec<String> = Vec::new();
    let mut floor = ChangeType::Patch;
    let mut deciding: Vec<String> = Vec::new();

    // A fixture owned by a published implementation dependency is a consumer
    // program for that crate, not for this macro; it carries its own release
    // classification and must not set this macro's floor.
    let mut non_floor_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    for obligation in &fact.macro_compile_fixture_changes {
        if obligation.scope_role == "implementationClosure" && obligation.owner_published {
            let owner = normalize_ident(&obligation.owner_package);
            non_floor_keys.insert(format!("{owner}|{}", compile_fixture_key(&obligation.path)));
        }
    }

    for entry in entries {
        let item = match entry {
            CompileEntry::Text(text) => bail!(
                "Compile evidence in macro contract '{}' must be an object with ownerPackage, path, \
                 baseline, and current, not the bare string {text:?}.",
                fact.folder
            ),
            CompileEntry::Item(item) => item,
        };
        let owner_package = item.owner_package.trim();
        let path = item.path.trim();
        if owner_package.is_empty() || path.is_empty() {
            bail!(
                "Compile evidence in macro contract '{}' must name ownerPackage and path.",
                fact.folder
            );
        }

        let baseline = measured_outcome(item.baseline.as_ref());
        let current = measured_outcome(item.current.as_ref());
        for (side, outcome) in [("baseline", &baseline), ("current", &current)] {
            if !outcome.complete {
                issues.push(format!(
                    "Compile evidence for '{path}' in macro contract '{}' does not record a {side} \
                     pass/fail result with a revision and exit code.",
                    fact.folder
                ));
            }
        }

        let owner_normalized = normalize_ident(owner_package);
        let key = compile_fixture_key(path);
        // Record the entry for every well-formed item so the obligation is
        // discharged; only a *complete* measurement moves the verdict floor.
        if let (Some(base), Some(curr)) = (baseline.result.as_deref(), current.result.as_deref()) {
            let outcome = compile_evidence_outcome(base, curr);
            if !non_floor_keys.contains(&format!("{owner_normalized}|{key}")) {
                let stronger = floor.max(outcome);
                if stronger != floor {
                    floor = stronger;
                    deciding.clear();
                }
                if outcome == floor && outcome != ChangeType::Patch {
                    deciding.push(path.to_string());
                }
            }
        }
        recorded.push(CompileEvidenceEntry {
            owner_package: owner_normalized,
            key,
        });
    }

    issues.sort();
    issues.dedup();
    deciding.sort();
    deciding.dedup();
    Ok(MacroCompileEvidence {
        entries: recorded,
        issues,
        derived_floor: floor,
        deciding,
    })
}
