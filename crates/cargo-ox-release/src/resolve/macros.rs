// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Proc-macro contract parsing, review-scope computation, and the
//! require/register flow that turns a supplied contract into a checked verdict.

use std::collections::BTreeMap;

use ohno::{AppError, bail};

use super::{Decision, MacroContract, Resolver};
use crate::model::{Ambiguity, MacroContractInput, PackageFact, clean_string_list, normalize_ident};
use crate::resolve::evidence::{compile_fixture_key, macro_compile_evidence};
use crate::version::ChangeType;

const CHANNEL_STATES: [&str; 3] = ["unchanged", "changed", "notapplicable"];

impl Resolver<'_> {
    /// Parses and validates a supplied macro contract. Returns `None` for a non-macro package or a package
    /// with no supplied contract.
    pub(super) fn parse_macro_contract(&self, fact: &PackageFact) -> Result<Option<MacroContract>, AppError> {
        if !fact.proc_macro_only {
            return Ok(None);
        }
        let Some(input) = self.request.macro_contract(fact) else {
            return Ok(None);
        };

        // The verdict is read first; a string contract carries only a verdict
        // and then fails the required-property check below.
        let (verdict_raw, detailed) = match input {
            MacroContractInput::Verdict(spelling) => (spelling.trim(), None),
            MacroContractInput::Detailed(obj) => (obj.verdict.trim(), Some(obj.as_ref())),
        };
        let change_type = match verdict_raw.to_ascii_lowercase().as_str() {
            "compatible" => ChangeType::Patch,
            "nonbreaking" | "non-breaking" => ChangeType::NonBreaking,
            "breaking" => ChangeType::Breaking,
            _ => bail!("Unknown macro-contract verdict '{verdict_raw}' for '{}'.", fact.folder),
        };

        let Some(obj) = detailed else {
            bail!(
                "Macro contract '{}' must include reviewedPackages, channels, and evidence.",
                fact.folder
            );
        };
        let (Some(reviewed_raw), Some(channels), Some(evidence_raw)) =
            (obj.reviewed_packages.as_ref(), obj.channels.as_ref(), obj.evidence.as_ref())
        else {
            bail!(
                "Macro contract '{}' must include reviewedPackages, channels, and evidence.",
                fact.folder
            );
        };

        let reviewed_packages = clean_string_list(reviewed_raw);
        if reviewed_packages.is_empty() {
            bail!("Macro contract '{}' must include at least one reviewed package.", fact.folder);
        }

        let channel_values = [
            ("exportedMacros", &channels.exported_macros),
            ("acceptedSyntax", &channels.accepted_syntax),
            ("compileBehavior", &channels.compile_behavior),
            ("generatedApi", &channels.generated_api),
            ("generatedRuntimePaths", &channels.generated_runtime_paths),
            ("hygiene", &channels.hygiene),
        ];
        let mut channel_map: BTreeMap<String, String> = BTreeMap::new();
        for (name, raw) in channel_values {
            let state = raw.trim().to_ascii_lowercase();
            if !CHANNEL_STATES.contains(&state.as_str()) {
                bail!(
                    "Macro contract '{}' must classify channel '{name}' as unchanged, changed, or \
                     notApplicable.",
                    fact.folder
                );
            }
            channel_map.insert(name.to_string(), state);
        }

        let evidence = clean_string_list(evidence_raw);
        if evidence.is_empty() {
            bail!("Macro contract '{}' must include evidence.", fact.folder);
        }

        let compile = macro_compile_evidence(fact, &obj.compile_evidence)?;

        Ok(Some(MacroContract {
            change_type,
            reviewed_packages,
            channels: channel_map,
            evidence,
            compile_evidence: compile.entries,
            evidence_issues: compile.issues,
            derived_floor: compile.derived_floor,
            deciding_fixtures: compile.deciding,
        }))
    }

    /// The canonical review scope: self plus every modified implementation-
    /// closure member and modified runtime partner, plus an optional trigger.
    pub(super) fn macro_review_scope(&self, fact: &PackageFact, trigger_fact: Option<&PackageFact>) -> Vec<String> {
        let mut scope: Vec<String> = vec![fact.name.clone()];
        for candidate in self.facts {
            let normalized = candidate.normalized_name();
            if candidate.workspace_modified
                && (fact.macro_implementation_closure.contains(&normalized) || fact.macro_runtime_partners.contains(&normalized))
            {
                scope.push(candidate.name.clone());
            }
        }
        if let Some(trigger) = trigger_fact {
            scope.push(trigger.name.clone());
        }
        scope.sort();
        scope.dedup();
        scope
    }

    /// The trigger-independent review scope emitted in the plan.
    pub(super) fn emitted_review_scope(&self, folder: &str) -> Vec<String> {
        self.folder_index
            .get(folder)
            .map_or_else(Vec::new, |&index| self.macro_review_scope(&self.facts[index], None))
    }

    /// Whether a supplied contract covers every package in the scope.
    fn contract_covers_scope(contract: &MacroContract, scope: &[String]) -> bool {
        let reviewed: Vec<String> = contract.reviewed_packages.iter().map(|p| normalize_ident(p)).collect();
        scope.iter().all(|id| reviewed.contains(&normalize_ident(id)))
    }

    /// Whether any member of a macro's implementation closure or runtime
    /// partner set was modified.
    pub(super) fn macro_scope_modified(&self, fact: &PackageFact) -> bool {
        self.facts.iter().any(|candidate| {
            candidate.workspace_modified
                && (fact.macro_implementation_closure.contains(&candidate.normalized_name())
                    || fact.macro_runtime_partners.contains(&candidate.normalized_name()))
        })
    }

    /// Checks a supplied contract against its measured obligations and records
    /// it. Returns `None` when the contract
    /// is blocked by an ambiguity (it is still recorded for echoing).
    ///
    /// # Errors
    ///
    /// Returns an error when a derived floor contradicts the package's
    /// selection decision — the hard conflicts the resolver rejects.
    pub(super) fn record_macro_contract(
        &mut self,
        fact: &PackageFact,
        contract: MacroContract,
        trigger: &str,
    ) -> Result<Option<MacroContract>, AppError> {
        let folder = fact.folder.clone();
        let mut blocked = false;

        let obligations = &fact.macro_compile_fixture_changes;
        if !obligations.is_empty() {
            let evidence_keys: std::collections::HashSet<String> = contract
                .compile_evidence
                .iter()
                .map(|e| format!("{}|{}", e.owner_package, e.key))
                .collect();
            let mut missing: Vec<String> = obligations
                .iter()
                .filter_map(|o| {
                    let owner = normalize_ident(&o.owner_package);
                    let key = compile_fixture_key(&o.path);
                    (!evidence_keys.contains(&format!("{owner}|{key}"))).then(|| o.path.clone())
                })
                .collect();
            missing.sort();
            missing.dedup();
            if !missing.is_empty() {
                let key = format!("{folder}|macroCompileFixtureUnevidenced");
                let ambiguity = Ambiguity::MacroCompileFixtureUnevidenced {
                    package: folder.clone(),
                    trigger: trigger.to_string(),
                    fixtures: missing,
                    required_input: format!("macroContracts.{folder}.compileEvidence"),
                };
                self.add_ambiguity(key, ambiguity);
                blocked = true;
            }
        }

        if !contract.evidence_issues.is_empty() {
            let key = format!("{folder}|macroCompileEvidenceInconclusive");
            let ambiguity = Ambiguity::MacroCompileEvidenceInconclusive {
                package: folder.clone(),
                trigger: trigger.to_string(),
                issues: contract.evidence_issues.clone(),
                required_input: format!("macroContracts.{folder}.compileEvidence"),
            };
            self.add_ambiguity(key, ambiguity);
            blocked = true;
        }

        // The verdict is a checked assertion. Measured outcomes set a floor; a
        // declared verdict may sit at or above it, never below.
        if contract.change_type < contract.derived_floor {
            let key = format!("{folder}|macroVerdictUnderclassified");
            let ambiguity = Ambiguity::MacroVerdictUnderclassified {
                package: folder.clone(),
                trigger: trigger.to_string(),
                declared_verdict: contract.change_type.macro_verdict_name().to_string(),
                derived_verdict: contract.derived_floor.macro_verdict_name().to_string(),
                deciding_fixtures: contract.deciding_fixtures.clone(),
                required_input: format!("macroContracts.{folder}.verdict"),
            };
            self.add_ambiguity(key, ambiguity);
            blocked = true;
        } else if let Some(decision) = self.selection_decisions.get(&folder) {
            // A measured compile-contract change also has to be the reason the
            // package was selected, so a "behavior fix" cannot carry a break.
            let required_reasons: &[&str] = match contract.derived_floor {
                ChangeType::Breaking => &["breaking"],
                ChangeType::NonBreaking => &["breaking", "nonbreaking-api", "behavior-fix"],
                ChangeType::None | ChangeType::Patch => &[],
            };
            if !required_reasons.is_empty() {
                let derived_name = contract.derived_floor.macro_verdict_name();
                if decision.decision != Decision::Accept {
                    bail!(
                        "Selection decision '{folder}' declines a package whose compile evidence \
                         derives a '{derived_name}' macro contract."
                    );
                }
                if !required_reasons.contains(&decision.reason.as_str()) {
                    bail!(
                        "Selection reason '{}' for '{folder}' conflicts with the '{derived_name}' macro \
                         contract derived from its compile evidence. Use {}.",
                        decision.reason,
                        required_reasons.join(" or ")
                    );
                }
            }
        }

        self.used_macro_contracts.insert(folder, contract.clone());
        if blocked {
            return Ok(None);
        }
        Ok(Some(contract))
    }

    /// Requires a reviewed, scope-covering contract. Returns `None` when the contract is missing,
    /// incomplete, or otherwise blocked.
    ///
    /// # Errors
    ///
    /// Propagates the hard conflicts from [`Self::record_macro_contract`].
    pub(super) fn ensure_macro_contract(
        &mut self,
        fact: &PackageFact,
        trigger_fact: Option<&PackageFact>,
        trigger: &str,
    ) -> Result<Option<MacroContract>, AppError> {
        let scope = self.macro_review_scope(fact, trigger_fact);
        let contract = self.parse_macro_contract(fact)?;
        let trigger_folder = trigger_fact.map_or(String::new(), |t| t.folder.clone());
        let key = format!("{}|{trigger}|{trigger_folder}", fact.folder);

        let Some(contract) = contract else {
            let ambiguity = Ambiguity::MacroContractUnreviewed {
                package: fact.folder.clone(),
                trigger: trigger.to_string(),
                review_scope: scope,
                required_input: format!("macroContracts.{}", fact.folder),
            };
            self.add_ambiguity(key, ambiguity);
            return Ok(None);
        };

        if !Self::contract_covers_scope(&contract, &scope) {
            let ambiguity = Ambiguity::MacroContractIncomplete {
                package: fact.folder.clone(),
                trigger: trigger.to_string(),
                review_scope: scope,
                reviewed: contract.reviewed_packages,
                required_input: format!("macroContracts.{}.reviewedPackages", fact.folder),
            };
            self.add_ambiguity(key, ambiguity);
            return Ok(None);
        }

        let runtime_partners_present = fact.macro_runtime_partners.iter().any(|p| !p.trim().is_empty());
        if contract.channel("generatedRuntimePaths") == "changed" && !runtime_partners_present {
            let runtime_key = format!("{}|macroRuntimeUnknown", fact.folder);
            let ambiguity = Ambiguity::MacroRuntimeUnknown {
                package: fact.folder.clone(),
                trigger: trigger.to_string(),
                review_scope: scope,
                required_input: "Expose the macro from its runtime facade, emit a literal workspace \
                 path, or declare package.metadata.oxidizer_release.macro_runtime."
                    .to_string(),
            };
            self.add_ambiguity(runtime_key, ambiguity);
            return Ok(None);
        }

        self.record_macro_contract(fact, contract, trigger)
    }
}
