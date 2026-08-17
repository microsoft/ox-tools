// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Token expansion, the type- and macro-contract-aware cascade, topological
//! ordering, and the resolved-plan output.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};

use ohno::{AppError, bail};

use super::{CascadeReason, Decision, MacroContract, PlanEntry, Resolver};
use crate::model::{CascadeReasonOutput, PackageFact, Plan, PlanStatus, ReleaseOutput, ReleaseSource};
use crate::version::{ChangeType, compare_versions, is_breaking_change};

impl Resolver<'_> {
    /// Expands one accepted release token into a seeded plan entry.
    ///
    /// Returns `Ok(())` without seeding an entry when a proc-macro token is
    /// blocked by an unresolved contract (it is skipped rather than seeded).
    ///
    /// # Errors
    ///
    /// Returns an error for the hard token failures: an unpublishable or
    /// duplicate token, a non-candidate token, a contradictory pin, or a
    /// requested change type exceeding a macro's contract verdict.
    #[expect(
        clippy::too_many_lines,
        reason = "the token-expansion rules form one cohesive step; splitting it would scatter tightly-coupled validation"
    )]
    pub(super) fn process_token(&mut self, token: &str) -> Result<(), AppError> {
        let (identifier, spec) = match token.split_once('@') {
            Some((left, right)) => (left, Some(right)),
            None => (token, None),
        };
        let index = self.find_package_fact(identifier)?;
        let fact = self.facts[index].clone();
        if !fact.published {
            bail!("Package '{}' is not publishable.", fact.folder);
        }
        if !self.token_folders.insert(fact.folder.clone()) {
            bail!("Package '{}' appears more than once in the release tokens.", fact.folder);
        }
        if self.mode.requires_selection() && !self.selection_decisions.contains_key(&fact.folder) {
            bail!("Release token '{}' is not a candidate in {} mode.", fact.folder, self.mode.as_str());
        }
        if self.mode.requires_selection() && self.selection_decisions[&fact.folder].decision != Decision::Accept {
            bail!("Release token '{}' conflicts with its decline selection decision.", fact.folder);
        }

        let classification = self.classify(&fact)?;
        self.enforce_external_exposure(&fact.folder, classification.change_type);
        self.enforce_own_diff_floor(&fact.folder, classification.change_type);

        let mut requested_change_type = ChangeType::None;
        let mut requested_pin: Option<String> = None;
        if let Some(spec) = spec {
            if let Ok(ct) = ChangeType::parse(spec, false) {
                requested_change_type = ct;
            } else {
                // Not a change type; it must be an explicit version pin.
                crate::version::validate_version(spec)?;
                if compare_versions(spec, &fact.version)? != Ordering::Greater {
                    bail!(
                        "Explicit pin '{spec}' for '{}' must be strictly greater than '{}'.",
                        fact.folder,
                        fact.version
                    );
                }
                requested_pin = Some(spec.to_string());
            }
        }

        let mut effective = classification.change_type.max(requested_change_type);
        if let Some(pin) = &requested_pin {
            let pin_change_type = crate::version::change_type_from_versions(&fact.version, pin)?;
            effective = effective.max(pin_change_type);
        }
        if effective == ChangeType::None {
            effective = ChangeType::Patch;
        }

        let mut macro_contract = self.parse_macro_contract(&fact)?;
        if fact.proc_macro_only {
            let scope_modified = self.macro_scope_modified(&fact);
            let needs_review = fact.modified
                || scope_modified
                || classification.change_type != ChangeType::Patch
                || matches!(requested_change_type, ChangeType::NonBreaking | ChangeType::Breaking)
                || requested_pin.is_some();
            if needs_review {
                let trigger = if fact.modified {
                    "macroPackageModified"
                } else if scope_modified {
                    "implementationClosureModified"
                } else {
                    "macroContractChangeRequested"
                };
                macro_contract = self.ensure_macro_contract(&fact, None, trigger)?;
                if macro_contract.is_none() {
                    return Ok(());
                }
            } else if let Some(contract) = macro_contract {
                macro_contract = self.record_macro_contract(&fact, contract, "macroContractSupplied")?;
                if macro_contract.is_none() {
                    return Ok(());
                }
            }
            if let Some(contract) = &macro_contract
                && requested_change_type > contract.change_type
            {
                bail!(
                    "Requested change '{}' for proc macro '{}' conflicts with its '{}' contract \
                         verdict. Use an exact version pin for a compatible version-line change.",
                    requested_change_type.internal_name(),
                    fact.folder,
                    contract.change_type.internal_name()
                );
            }
        }

        let contract_breaking = fact.proc_macro_only
            && match &macro_contract {
                Some(c) => c.change_type == ChangeType::Breaking,
                None => classification.change_type == ChangeType::Breaking || requested_change_type == ChangeType::Breaking,
            };

        self.ensure_pin_satisfies(&fact.folder, requested_pin.as_deref(), effective)?;
        let target_version = self.entry_target_version(&fact.folder, requested_pin.as_deref(), effective)?;
        let entry = PlanEntry {
            source: "user".to_string(),
            requested_pin,
            effective_change_type: effective,
            target_version,
            manual_review: classification.manual_review,
            macro_contract_reviewed: macro_contract.is_some(),
            contract_breaking,
            reasons: BTreeMap::new(),
        };
        self.plan.insert(fact.folder.clone(), entry);
        self.queue.push_back(fact.folder);
        Ok(())
    }

    /// Requires the contract for a declined proc macro whose fixtures changed,
    /// so a decline stays honest without forcing a release.
    ///
    /// # Errors
    ///
    /// Propagates the hard conflicts from macro-contract registration.
    pub(super) fn require_declined_macro_obligations(&mut self) -> Result<(), AppError> {
        let candidates: Vec<String> = self
            .selection_decisions
            .iter()
            .filter(|(folder, decision)| {
                decision.decision == Decision::Decline
                    && self
                        .folder_index
                        .get(*folder)
                        .is_some_and(|&i| self.facts[i].proc_macro_only && !self.facts[i].macro_compile_fixture_changes.is_empty())
            })
            .map(|(folder, _)| folder.clone())
            .collect();
        for folder in candidates {
            let fact = self.fact(&folder).clone();
            self.ensure_macro_contract(&fact, None, "macroCompileFixtureChanged")?;
        }
        Ok(())
    }

    /// Runs the cascade until the queue drains, propagating each release to its
    /// dependents.
    ///
    /// # Errors
    ///
    /// Propagates hard conflicts from pin reconciliation and macro-contract
    /// registration.
    pub(super) fn run_cascade(&mut self) -> Result<(), AppError> {
        while let Some(dependency_folder) = self.queue.pop_front() {
            let dep_fact = self.fact(&dependency_folder).clone();
            let dep_entry = self.plan[&dependency_folder].clone();
            let dependency_name = dep_fact.normalized_name();

            let dependency_version_breaking = dep_fact.ever_released
                && compare_versions(&dep_entry.target_version, &dep_fact.version)? != Ordering::Equal
                && is_breaking_change(&dep_fact.version, dep_entry.effective_change_type)?;
            let dependency_contract_breaking = dep_fact.proc_macro_only && dep_entry.contract_breaking;

            let dependents = self.cascade_dependents(
                &dep_fact,
                &dependency_name,
                dependency_version_breaking,
                dependency_contract_breaking,
            );

            for dependent_fact in dependents {
                self.propagate_to_dependent(
                    &dep_fact,
                    &dep_entry,
                    &dependency_name,
                    dependency_version_breaking,
                    dependency_contract_breaking,
                    &dependent_fact,
                )?;
            }

            if !dep_fact.proc_macro_only && dependency_version_breaking {
                self.propagate_runtime_macros(&dep_fact, &dep_entry, &dependency_name)?;
            }
        }
        Ok(())
    }

    /// The dependents that this dependency reaches on this pass.
    fn cascade_dependents(
        &self,
        dep_fact: &PackageFact,
        dependency_name: &str,
        dependency_version_breaking: bool,
        dependency_contract_breaking: bool,
    ) -> Vec<PackageFact> {
        let mut dependents: Vec<PackageFact> = self
            .facts
            .iter()
            .filter(|f| {
                f.published
                    && f.ever_released
                    && f.folder != dep_fact.folder
                    && (f.deps.iter().any(|d| d == dependency_name)
                        || (!dep_fact.proc_macro_only
                            && dependency_version_breaking
                            && f.exposed_deps.iter().any(|d| d == dependency_name))
                        || (dep_fact.proc_macro_only
                            && dependency_contract_breaking
                            && f.macro_public_deps.iter().any(|d| d == dependency_name)))
            })
            .cloned()
            .collect();
        dependents.sort_by(|a, b| a.folder.cmp(&b.folder));
        dependents
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the dependent-propagation rules form one cohesive step; splitting it would scatter tightly-coupled state updates"
    )]
    fn propagate_to_dependent(
        &mut self,
        dep_fact: &PackageFact,
        dep_entry: &PlanEntry,
        dependency_name: &str,
        dependency_version_breaking: bool,
        dependency_contract_breaking: bool,
        dependent_fact: &PackageFact,
    ) -> Result<(), AppError> {
        let classification = self.classify(dependent_fact)?;
        self.enforce_external_exposure(&dependent_fact.folder, classification.change_type);
        self.enforce_own_diff_floor(&dependent_fact.folder, classification.change_type);

        let mut macro_contract: Option<MacroContract> = None;
        if dependent_fact.proc_macro_only {
            let scope_modified = self.macro_scope_modified(dependent_fact);
            let needs_review =
                dependency_version_breaking || dependent_fact.modified || scope_modified || classification.change_type != ChangeType::Patch;
            if needs_review {
                macro_contract = self.ensure_macro_contract(dependent_fact, Some(dep_fact), "implementationDependencyChanged")?;
                if macro_contract.is_none() {
                    return Ok(());
                }
            } else if let Some(contract) = self.parse_macro_contract(dependent_fact)? {
                macro_contract = self.record_macro_contract(dependent_fact, contract, "macroContractSupplied")?;
            }
        }

        let is_direct_dependent = dependent_fact.deps.iter().any(|d| d == dependency_name);
        let (edge_class, edge_breaking, judgment, judgment_source) = if dependent_fact.proc_macro_only {
            let breaking = macro_contract.as_ref().is_some_and(|c| c.change_type == ChangeType::Breaking);
            let judgment = if breaking {
                "contractBreaking"
            } else if macro_contract.is_some() {
                "contractCompatible"
            } else {
                "patchFloor"
            };
            let source = if macro_contract.is_some() {
                "macroContracts"
            } else {
                "dependencyRequirement"
            };
            ("macroImplementation", breaking, judgment, source)
        } else if dep_fact.proc_macro_only {
            let macro_is_public = dependent_fact.macro_public_deps.iter().any(|d| d == dependency_name);
            let edge_class = if macro_is_public { "macroPublic" } else { "macroPrivate" };
            let breaking = dependency_contract_breaking && macro_is_public;
            let judgment = if breaking {
                "contractBreaking"
            } else if macro_is_public && dep_entry.macro_contract_reviewed {
                "contractCompatible"
            } else if macro_is_public {
                "patchFloor"
            } else {
                "privateDependency"
            };
            let source = if dep_entry.macro_contract_reviewed {
                "macroContracts"
            } else {
                "dependencyRequirement"
            };
            (edge_class, breaking, judgment, source)
        } else {
            let exposes_dependency = (is_direct_dependent && dependent_fact.exposure_unknown)
                || dependent_fact.exposed_deps.iter().any(|d| d == dependency_name);
            let breaking = dependency_version_breaking && exposes_dependency;
            let judgment = if breaking { "typeExposed" } else { "encapsulated" };
            ("type", breaking, judgment, "releaseFacts")
        };

        let cascade_change_type = if edge_breaking {
            ChangeType::Breaking
        } else {
            ChangeType::Patch.max(classification.change_type)
        };

        let is_new = !self.plan.contains_key(&dependent_fact.folder);
        if is_new {
            let target_version = self.entry_target_version(&dependent_fact.folder, None, cascade_change_type)?;
            let contract_breaking =
                dependent_fact.proc_macro_only && macro_contract.as_ref().is_some_and(|c| c.change_type == ChangeType::Breaking);
            let entry = PlanEntry {
                source: "cascade".to_string(),
                requested_pin: None,
                effective_change_type: cascade_change_type,
                target_version,
                manual_review: classification.manual_review,
                macro_contract_reviewed: macro_contract.is_some(),
                contract_breaking,
                reasons: BTreeMap::new(),
            };
            self.plan.insert(dependent_fact.folder.clone(), entry);
        }

        let reason = CascadeReason {
            target: dep_fact.name.clone(),
            version: dep_entry.target_version.clone(),
            breaking: edge_breaking,
            edge_class: edge_class.to_string(),
            judgment: judgment.to_string(),
            judgment_source: judgment_source.to_string(),
        };
        let current = self
            .plan
            .get(&dependent_fact.folder)
            .expect("dependent entry exists after the is_new insert above")
            .effective_change_type;
        let stronger = current.max(cascade_change_type);
        let strengthened = stronger != current;

        if strengthened {
            let requested_pin = self.plan[&dependent_fact.folder].requested_pin.clone();
            self.ensure_pin_satisfies(&dependent_fact.folder, requested_pin.as_deref(), stronger)?;
            let target_version = self.entry_target_version(&dependent_fact.folder, requested_pin.as_deref(), stronger)?;
            let macro_break =
                dependent_fact.proc_macro_only && macro_contract.as_ref().is_some_and(|c| c.change_type == ChangeType::Breaking);
            let entry = self
                .plan
                .get_mut(&dependent_fact.folder)
                .expect("dependent entry exists after the is_new insert above");
            entry.effective_change_type = stronger;
            entry.target_version = target_version;
            if macro_break {
                entry.contract_breaking = true;
            }
        }

        // Record the edge after the strengthen step so the entry always exists.
        self.plan
            .get_mut(&dependent_fact.folder)
            .expect("dependent entry exists after the is_new insert above")
            .reasons
            .insert(dep_fact.folder.clone(), reason);

        if is_new || strengthened {
            self.queue.push_back(dependent_fact.folder.clone());
        }
        Ok(())
    }

    /// Propagates a breaking ordinary dependency into the proc macros whose
    /// generated runtime paths name it.
    fn propagate_runtime_macros(&mut self, dep_fact: &PackageFact, dep_entry: &PlanEntry, dependency_name: &str) -> Result<(), AppError> {
        let runtime_macros: Vec<PackageFact> = {
            let mut macros: Vec<PackageFact> = self
                .facts
                .iter()
                .filter(|f| {
                    f.published && f.ever_released && f.proc_macro_only && f.macro_runtime_partners.iter().any(|p| p == dependency_name)
                })
                .cloned()
                .collect();
            macros.sort_by(|a, b| a.folder.cmp(&b.folder));
            macros
        };

        for macro_fact in runtime_macros {
            let contract = self.ensure_macro_contract(&macro_fact, Some(dep_fact), "generatedRuntimeChanged")?;
            let Some(contract) = contract.filter(|c| c.change_type != ChangeType::Patch) else {
                continue;
            };

            let classification = self.classify(&macro_fact)?;
            self.enforce_external_exposure(&macro_fact.folder, classification.change_type);
            let cascade_change_type = classification.change_type.max(contract.change_type);
            let breaking = contract.change_type == ChangeType::Breaking;

            let is_new = !self.plan.contains_key(&macro_fact.folder);
            if is_new {
                let target_version = self.entry_target_version(&macro_fact.folder, None, cascade_change_type)?;
                let entry = PlanEntry {
                    source: "cascade".to_string(),
                    requested_pin: None,
                    effective_change_type: cascade_change_type,
                    target_version,
                    manual_review: true,
                    macro_contract_reviewed: true,
                    contract_breaking: breaking,
                    reasons: BTreeMap::new(),
                };
                self.plan.insert(macro_fact.folder.clone(), entry);
            }

            let reason = CascadeReason {
                target: dep_fact.name.clone(),
                version: dep_entry.target_version.clone(),
                breaking,
                edge_class: "macroRuntime".to_string(),
                judgment: if breaking { "contractBreaking" } else { "contractNonbreaking" }.to_string(),
                judgment_source: "macroContracts".to_string(),
            };

            let current = self
                .plan
                .get(&macro_fact.folder)
                .expect("macro entry exists after the is_new insert above")
                .effective_change_type;
            let stronger = current.max(cascade_change_type);
            let strengthened = stronger != current;
            if strengthened {
                let target_version = self.entry_target_version(&macro_fact.folder, None, stronger)?;
                let entry = self
                    .plan
                    .get_mut(&macro_fact.folder)
                    .expect("macro entry exists after the is_new insert above");
                entry.effective_change_type = stronger;
                entry.target_version = target_version;
                entry.contract_breaking = breaking;
            }

            self.plan
                .get_mut(&macro_fact.folder)
                .expect("macro entry exists after the is_new insert above")
                .reasons
                .insert(dep_fact.folder.clone(), reason);

            if is_new || strengthened {
                self.queue.push_back(macro_fact.folder.clone());
            }
        }
        Ok(())
    }

    /// Topologically orders the plan (dependencies before dependents, ties
    /// broken by folder) and emits the resolved plan.
    ///
    /// # Errors
    ///
    /// Returns an error when the release set contains a dependency cycle.
    pub(super) fn resolved_plan(&self) -> Result<Plan, AppError> {
        let ordered = self.topological_order()?;
        let releases = ordered
            .iter()
            .map(|folder| {
                let entry = &self.plan[folder];
                let fact = self.fact(folder);
                let mut reasons: Vec<&CascadeReason> = entry.reasons.values().collect();
                reasons.sort_by(|a, b| a.target.cmp(&b.target));
                ReleaseOutput {
                    folder: fact.folder.clone(),
                    name: fact.name.clone(),
                    from: fact.version.clone(),
                    to: entry.target_version.clone(),
                    change_type: entry.effective_change_type,
                    source: if entry.source == "user" {
                        ReleaseSource::User
                    } else {
                        ReleaseSource::Cascade
                    },
                    manual_review: entry.manual_review,
                    contract_breaking: entry.contract_breaking,
                    cascade_reasons: reasons
                        .into_iter()
                        .map(|r| CascadeReasonOutput {
                            target: r.target.clone(),
                            version: r.version.clone(),
                            breaking: r.breaking,
                            edge_class: r.edge_class.clone(),
                            judgment: r.judgment.clone(),
                            judgment_source: r.judgment_source.clone(),
                        })
                        .collect(),
                }
            })
            .collect();

        Ok(Plan {
            status: PlanStatus::Resolved,
            mode: self.mode.as_str().to_string(),
            selection_decisions: self.selection_decision_output(),
            releases,
            macro_contracts: self.macro_contract_output(),
            ambiguities: Vec::new(),
            warnings: self.warnings.clone(),
        })
    }

    fn topological_order(&self) -> Result<Vec<String>, AppError> {
        // A plan package's in-plan dependencies, by folder.
        let name_to_folder: HashMap<String, String> = self.plan.keys().map(|f| (self.fact(f).normalized_name(), f.clone())).collect();

        let mut indegree: HashMap<String, usize> = self.plan.keys().map(|f| (f.clone(), 0)).collect();
        for folder in self.plan.keys() {
            let unique_deps: HashSet<&str> = self.fact(folder).deps.iter().map(String::as_str).collect();
            let in_plan = unique_deps.iter().filter(|dep| name_to_folder.contains_key(**dep)).count();
            *indegree.get_mut(folder).expect("folder seeded above") += in_plan;
        }

        let mut ordered: Vec<String> = Vec::new();
        let mut ordered_set: HashSet<String> = HashSet::new();
        while ordered.len() < self.plan.len() {
            let mut ready: Vec<String> = indegree
                .iter()
                .filter(|(folder, deg)| **deg == 0 && !ordered_set.contains(*folder))
                .map(|(folder, _)| folder.clone())
                .collect();
            ready.sort();
            if ready.is_empty() {
                bail!("The release set contains a dependency cycle and cannot be topologically ordered.");
            }
            for folder in ready {
                let released_name = self.fact(&folder).normalized_name();
                ordered_set.insert(folder.clone());
                ordered.push(folder);
                for candidate in self.plan.keys() {
                    if ordered_set.contains(candidate) {
                        continue;
                    }
                    if self.fact(candidate).deps.iter().any(|d| d == &released_name) {
                        *indegree.get_mut(candidate).expect("candidate seeded above") -= 1;
                    }
                }
            }
        }
        Ok(ordered)
    }
}
