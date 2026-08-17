// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Selection-decision validation and evidence grading — the deterministic
//! selection table that decides which candidates release and why.

use ohno::{AppError, bail};

use super::{Decision, Resolver, SelectionDecision};
use crate::model::{Ambiguity, PackageFact, SelectionDecisionInput, clean_string_list};
use crate::resolve::evidence::regression_evidence;
use crate::version::ChangeType;

const ACCEPTED_REASONS: [&str; 7] = [
    "breaking",
    "nonbreaking-api",
    "behavior-fix",
    "authored-doc-fix",
    "runtime-manifest-change",
    "first-release",
    "explicit-release",
];
const DECLINED_REASONS: [&str; 7] = [
    "test-only",
    "benchmark-only",
    "dev-dependency-only",
    "release-metadata-only",
    "generated-artifact-only",
    "internal-only",
    "unchanged",
];

/// The generated artifacts that a crate owns (its own README/CHANGELOG).
const GENERATED_ARTIFACTS: [&str; 2] = ["README.md", "CHANGELOG.md"];
/// The ignorable release-metadata files under a crate's own prefix.
const IGNORABLE_METADATA: [&str; 3] = ["Cargo.toml", "README.md", "CHANGELOG.md"];

fn normalized_modified_files(fact: &PackageFact) -> Vec<String> {
    fact.modified_files.iter().map(|f| f.replace('\\', "/")).collect()
}

impl Resolver<'_> {
    /// Validates one selection decision.
    #[expect(
        clippy::too_many_lines,
        reason = "the selection table's rules form one cohesive validator; splitting it would scatter tightly-coupled checks"
    )]
    pub(super) fn parse_selection(&self, folder: &str, input: &SelectionDecisionInput) -> Result<SelectionDecision, AppError> {
        let fact = self.fact(folder).clone();

        let decision = match input.decision.to_ascii_lowercase().as_str() {
            "accept" => Decision::Accept,
            "decline" => Decision::Decline,
            _ => bail!("Selection decision '{folder}' must be accept or decline."),
        };

        let reason = input.reason.to_ascii_lowercase();
        let allowed: &[&str] = match decision {
            Decision::Accept => &ACCEPTED_REASONS,
            Decision::Decline => &DECLINED_REASONS,
        };
        if !allowed.contains(&reason.as_str()) {
            bail!("Selection decision '{folder}' has invalid {} reason '{reason}'.", decision.as_str());
        }

        if reason == "explicit-release" && (self.mode != super::Mode::All || fact.modified) {
            bail!("Selection reason 'explicit-release' is only valid for an unchanged package in all mode.");
        }

        let scopes = &fact.manifest_dependency_scopes;
        let has_runtime_dependency_change = scopes.iter().any(|s| s == "normal" || s == "build" || s == "features");
        if reason == "runtime-manifest-change" && !has_runtime_dependency_change {
            bail!(
                "Selection reason 'runtime-manifest-change' for '{folder}' requires a changed \
                 normal/build dependency or package feature declaration."
            );
        }
        if decision == Decision::Decline && has_runtime_dependency_change {
            bail!(
                "Selection decision '{folder}' cannot decline a changed normal/build dependency or \
                 package feature declaration."
            );
        }
        // A published manifest dependency change owns the reason: `authored-doc-fix`
        // cannot be paired with a normal/build/features change.
        if reason == "authored-doc-fix" && has_runtime_dependency_change {
            bail!(
                "Selection reason 'authored-doc-fix' for '{folder}' cannot be used alongside a \
                 normal/build dependency or package feature change; use 'runtime-manifest-change'."
            );
        }

        let package_prefix = format!("crates/{folder}/");
        let other_files: Vec<String> = normalized_modified_files(&fact)
            .into_iter()
            .filter(|path| {
                let Some(relative) = path.strip_prefix(&package_prefix) else {
                    return true;
                };
                !IGNORABLE_METADATA.contains(&relative)
            })
            .collect();

        let pure_dev = scopes.len() == 1 && scopes[0] == "dev" && !fact.manifest_other_changed && other_files.is_empty();
        if decision == Decision::Accept && pure_dev {
            bail!("Selection decision '{folder}' cannot accept a dev-dependency-only manifest change.");
        }
        if decision == Decision::Decline && pure_dev && reason != "dev-dependency-only" {
            bail!(
                "Selection decision '{folder}' must classify a pure dev dependency manifest edit as \
                 'dev-dependency-only'."
            );
        }
        if reason == "dev-dependency-only" {
            if !scopes.iter().any(|s| s == "dev") || has_runtime_dependency_change || fact.manifest_other_changed {
                bail!(
                    "Selection reason 'dev-dependency-only' for '{folder}' requires only changed dev \
                     dependency declarations and ignorable release metadata."
                );
            }
            if !other_files.is_empty() {
                bail!(
                    "Selection reason 'dev-dependency-only' for '{folder}' cannot ignore changed \
                     source, tests, benchmarks, or authored documentation."
                );
            }
        }

        // A generated artifact is exactly this crate's own README or CHANGELOG.
        let changed_files = normalized_modified_files(&fact);
        let is_generated_only = !changed_files.is_empty()
            && changed_files.iter().all(|path| {
                path.strip_prefix(&package_prefix)
                    .is_some_and(|relative| GENERATED_ARTIFACTS.contains(&relative))
            });
        if reason == "generated-artifact-only" && !is_generated_only {
            bail!(
                "Selection reason 'generated-artifact-only' for '{folder}' requires that only this \
                 crate's generated README.md or CHANGELOG.md changed; a Cargo.toml or other edit is \
                 'release-metadata-only'."
            );
        }
        if reason == "release-metadata-only" && is_generated_only {
            bail!(
                "Selection reason 'release-metadata-only' for '{folder}' cannot classify a change to \
                 only a generated README.md or CHANGELOG.md; use 'generated-artifact-only'."
            );
        }

        // A rustdoc-visible doc comment changed with no implementation change:
        // the crate's own diff is consumer-visible documentation. With no
        // runtime-manifest change and no exposed breaking external dependency,
        // the one canonical outcome is accept `authored-doc-fix`.
        let has_exposed_breaking_external = fact
            .external_dep_changes
            .iter()
            .any(|c| c.breaking && fact.external_exposed_deps.contains(&c.name));
        if fact.ever_released
            && !fact.proc_macro_only
            && fact.doc_comment_changed
            && !fact.rust_implementation_changed
            && !has_runtime_dependency_change
            && !has_exposed_breaking_external
            && !(decision == Decision::Accept && reason == "authored-doc-fix")
        {
            bail!(
                "Selection decision '{folder}' changes a rustdoc-visible doc comment with no \
                 implementation change; a consumer-visible doc change must be accepted as \
                 'authored-doc-fix', not '{reason}'."
            );
        }

        if decision == Decision::Accept && !fact.ever_released {
            if reason != "first-release" {
                bail!("Never-released package '{folder}' must use selection reason 'first-release'.");
            }
            let release_worthy = normalized_modified_files(&fact).into_iter().any(|path| {
                let Some(relative) = path.strip_prefix(&package_prefix) else {
                    return false;
                };
                relative.starts_with("src/")
                    || relative.starts_with("examples/")
                    || (relative.starts_with("docs/") && relative.to_ascii_lowercase().ends_with(".md"))
                    || relative == "build.rs"
            });
            if !release_worthy {
                bail!(
                    "Selection reason 'first-release' for '{folder}' requires a changed packaged file \
                     outside tests, benchmarks, and generated artifacts."
                );
            }
        }

        let evidence = clean_string_list(&input.evidence);
        if evidence.is_empty() {
            bail!("Selection decision '{folder}' must include evidence.");
        }

        let regression = regression_evidence(folder, &input.regression_evidence)?;

        Ok(SelectionDecision {
            folder: folder.to_string(),
            decision,
            reason,
            evidence,
            regression_evidence: regression.entries,
            evidence_issues: regression.issues,
            regression_shown: regression.demonstrated,
        })
    }

    /// Grades a selection decision's evidence before any token is expanded.
    pub(super) fn grade_selection_evidence(&mut self, folder: &str) -> Result<(), AppError> {
        let decision = self.selection_decisions[folder].clone();
        let fact = self.fact(folder).clone();

        let exposure_flooring = Self::external_breaking_exposure(&fact);
        if !exposure_flooring.is_empty() && decision.reason != "breaking" {
            let key = format!("{folder}|externalExposureUnderselected");
            let ambiguity = Ambiguity::ExternalExposureUnderselected {
                package: folder.to_string(),
                decision: decision.decision.as_str().to_string(),
                reason: decision.reason.clone(),
                derived_floor: "breaking".to_string(),
                dependencies: Self::exposure_probes(&exposure_flooring),
                required_input: format!("selectionDecisions.{folder}.reason"),
            };
            self.add_ambiguity(key, ambiguity);
        }

        if decision.reason == "breaking" {
            let classification = self.classify(&fact)?;
            if classification.change_type != ChangeType::Breaking {
                let key = format!("{folder}|breakingSelectionUnderclassified");
                let ambiguity = Ambiguity::BreakingSelectionUnderclassified {
                    package: folder.to_string(),
                    reason: decision.reason.clone(),
                    objective_classification: classification.change_type.macro_verdict_name().to_string(),
                    required_input: format!("selectionDecisions.{folder}.reason"),
                };
                self.add_ambiguity(key, ambiguity);
            }
        }

        if decision.reason != "behavior-fix" {
            return Ok(());
        }

        if !decision.evidence_issues.is_empty() {
            let key = format!("{folder}|behaviorEvidenceInconclusive");
            let ambiguity = Ambiguity::BehaviorEvidenceInconclusive {
                package: folder.to_string(),
                reason: decision.reason.clone(),
                issues: decision.evidence_issues.clone(),
                required_input: format!("selectionDecisions.{folder}.regressionEvidence"),
            };
            self.add_ambiguity(key, ambiguity);
        }

        if !decision.regression_shown {
            let key = format!("{folder}|behaviorFixUndemonstrated");
            let ambiguity = Ambiguity::BehaviorFixUndemonstrated {
                package: folder.to_string(),
                reason: decision.reason.clone(),
                probes: decision.regression_evidence,
                required_input: format!("selectionDecisions.{folder}.regressionEvidence"),
            };
            self.add_ambiguity(key, ambiguity);
        }

        Ok(())
    }
}
