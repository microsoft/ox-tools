// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Classification, the external-exposure and own-diff floors, and the
//! per-entry version helpers.

use std::cmp::Ordering;

use ohno::{AppError, bail};

use super::Resolver;
use crate::model::{Ambiguity, ClassificationInput, ExposureProbe, ExternalDepChange, PackageFact};
use crate::version::{ChangeType, compare_versions, next_version};

impl Resolver<'_> {
    /// Computes a package's objective classification.
    pub(super) fn classify(&self, fact: &PackageFact) -> Result<super::Classification, AppError> {
        let value = self.request.classification(fact);

        let mut manual_review = fact.proc_macro_only;
        let mut change_type: Option<ChangeType> = None;

        match value {
            Some(ClassificationInput::Simple(spelling)) => {
                change_type = Some(ChangeType::parse(spelling, false)?);
            }
            Some(ClassificationInput::Detailed(obj)) => {
                change_type = Some(ChangeType::parse(&obj.change_type, false)?);
                if let Some(declared) = obj.manual_review
                    && declared != manual_review
                {
                    bail!(
                        "manualReview for '{}' is resolver-owned and must be '{}'.",
                        fact.folder,
                        manual_review
                    );
                }
            }
            None => {}
        }

        if !fact.ever_released {
            return Ok(super::Classification {
                change_type: ChangeType::None,
                manual_review,
            });
        }

        let mut change_type = match change_type {
            Some(change_type) => change_type,
            None => {
                if fact.proc_macro_only {
                    manual_review = true;
                    ChangeType::Patch
                } else {
                    bail!("Missing objective classification for published package '{}'.", fact.folder);
                }
            }
        };

        if let Some(contract) = self.parse_macro_contract(fact)? {
            if value.is_some() && change_type != contract.change_type {
                bail!(
                    "Classification '{}' for proc macro '{}' conflicts with macro-contract verdict '{}'.",
                    change_type.internal_name(),
                    fact.folder,
                    contract.change_type.internal_name()
                );
            }
            change_type = contract.change_type;
            manual_review = true;
        }

        Ok(super::Classification {
            change_type,
            manual_review,
        })
    }

    /// The exposed external dependency changes that force a breaking floor.
    pub(super) fn external_breaking_exposure(fact: &PackageFact) -> Vec<ExternalDepChange> {
        if !fact.ever_released {
            return Vec::new();
        }
        let mut flooring: Vec<ExternalDepChange> = fact
            .external_dep_changes
            .iter()
            .filter(|c| c.breaking && fact.external_exposed_deps.contains(&c.name))
            .cloned()
            .collect();
        flooring.sort_by(|a, b| a.name.cmp(&b.name));
        flooring
    }

    /// The exposed-external-dependency probe list reported in an ambiguity.
    pub(super) fn exposure_probes(changes: &[ExternalDepChange]) -> Vec<ExposureProbe> {
        changes
            .iter()
            .map(|c| ExposureProbe {
                name: c.name.clone(),
                baseline_req: c.baseline_req.clone(),
                current_req: c.current_req.clone(),
            })
            .collect()
    }

    /// A breaking external requirement change on a publicly exposed dependency
    /// forces a breaking floor.
    pub(super) fn enforce_external_exposure(&mut self, folder: &str, change_type: ChangeType) {
        let flooring = Self::external_breaking_exposure(self.fact(folder));
        if flooring.is_empty() || change_type >= ChangeType::Breaking {
            return;
        }
        let key = format!("{folder}|externalExposureUnderclassified");
        let ambiguity = Ambiguity::ExternalExposureUnderclassified {
            package: folder.to_string(),
            classified: change_type.internal_name().to_string(),
            derived_floor: "breaking".to_string(),
            dependencies: Self::exposure_probes(&flooring),
            required_input: format!("classifications.{folder}"),
        };
        self.add_ambiguity(key, ambiguity);
    }

    /// A previously released ordinary library may only claim breaking or
    /// non-breaking on its own account when its packaged Rust source actually
    /// changed.
    pub(super) fn enforce_own_diff_floor(&mut self, folder: &str, change_type: ChangeType) {
        let fact = self.fact(folder);
        if !fact.ever_released
            || fact.proc_macro_only
            || change_type <= ChangeType::Patch
            || fact.rust_implementation_changed
            || !Self::external_breaking_exposure(fact).is_empty()
        {
            return;
        }
        let key = format!("{folder}|ownClassificationUnsupported");
        let ambiguity = Ambiguity::OwnClassificationUnsupported {
            package: folder.to_string(),
            classified: change_type.internal_name().to_string(),
            required_input: format!("classifications.{folder}"),
        };
        self.add_ambiguity(key, ambiguity);
    }

    /// The target version an entry resolves to.
    pub(super) fn entry_target_version(
        &self,
        folder: &str,
        requested_pin: Option<&str>,
        change_type: ChangeType,
    ) -> Result<String, AppError> {
        if let Some(pin) = requested_pin
            && !pin.trim().is_empty()
        {
            return Ok(pin.to_string());
        }
        let fact = self.fact(folder);
        if !fact.ever_released {
            return Ok(fact.version.clone());
        }
        next_version(&fact.version, change_type)
    }

    /// Enforces that an explicit pin is at least the required change type's
    /// target.
    pub(super) fn ensure_pin_satisfies(
        &mut self,
        folder: &str,
        requested_pin: Option<&str>,
        required_change_type: ChangeType,
    ) -> Result<(), AppError> {
        let Some(pin) = requested_pin.filter(|p| !p.trim().is_empty()) else {
            return Ok(());
        };
        let fact = self.fact(folder);
        if !fact.ever_released {
            return Ok(());
        }
        let required_target = next_version(&fact.version, required_change_type)?;
        if compare_versions(pin, &required_target)? != Ordering::Less {
            return Ok(());
        }
        let message = format!(
            "Explicit pin '{pin}' for '{folder}' is below the required '{required_target}' ({}).",
            required_change_type.internal_name()
        );
        if !self.force {
            bail!("{message}");
        }
        self.warnings.push(format!(
            "{message} Force keeps the pin while preserving the stronger change type for further \
             cascade decisions."
        ));
        Ok(())
    }
}
