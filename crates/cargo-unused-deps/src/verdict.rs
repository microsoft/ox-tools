// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Judging: folding compile evidence into one verdict per declaration.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::detect::{Declared, Section};
use crate::doctests::DoctestEvidence;
use crate::evidence::{Evidence, Scope};

/// What the evidence says about one declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// No unit that could have loaded it did.
    Unused,

    /// Declared as a normal dependency, but only development units use it.
    Misplaced,
}

/// One thing worth telling the user about.
#[derive(Debug, Clone)]
pub struct Finding {
    /// Package that declares it.
    pub package: String,

    /// Manifest that declares it.
    pub manifest_path: PathBuf,

    /// The declaration's key.
    pub name: String,

    /// Which table declared it.
    pub section: Section,

    /// Target-table predicate, or none for an unconditional declaration.
    pub target: Option<String>,

    /// What the evidence says.
    pub verdict: Verdict,
}

/// A package, as far as judging is concerned.
pub struct Package {
    /// Package name, for reporting.
    pub name: String,

    /// Package version, for resolving qualified selectors.
    pub version: String,

    /// Its manifest.
    pub manifest_path: PathBuf,

    /// Everything it declares.
    pub declared: Vec<Declared>,

    /// Package-local source-finding suppressions.
    pub allowed: BTreeSet<String>,

    /// Whether it has a library or proc-macro target, and so can have doctests.
    pub has_doctests: bool,
}

/// Judge every declaration of every package against the evidence.
///
/// Two runs are needed, and they answer different questions. `plain` built
/// default targets only, so a report there can only have come from a target's
/// single, non-`cfg(test)` unit -- which is what "the library itself uses it"
/// means. `all` built everything, and answers "did anything use it".
///
/// `allowed` names are dropped before judging, so an allow-listed dependency
/// produces no finding of any kind.
pub fn judge(
    packages: &[Package],
    selected: &BTreeSet<PathBuf>,
    plain: &Evidence,
    all: &Evidence,
    doctests: &DoctestEvidence,
    allowed: &BTreeSet<String>,
) -> Vec<Finding> {
    let mut findings = Vec::new();

    for package in packages {
        // Transitive workspace dependencies can also produce artifacts. Only
        // roots resolved from the caller's package selectors are judged.
        if !selected.contains(&package.manifest_path) {
            continue;
        }

        let mut declarations_per_scope = BTreeMap::new();
        for declared in &package.declared {
            *declarations_per_scope
                .entry((declared.extern_name(), declared.section == Section::Build))
                .or_insert(0usize) += 1;
        }

        for declared in &package.declared {
            // Rustc reports the extern name, not the manifest declaration that
            // put it in scope. Normal and development declarations share the
            // same runtime extern in development units; build declarations use
            // a separate build-script target and can be judged independently.
            if declarations_per_scope.get(&(declared.extern_name(), declared.section == Section::Build)) != Some(&1) {
                continue;
            }
            if declared.feature_referenced || allowed.contains(&declared.name) || package.allowed.contains(&declared.name) {
                continue;
            }

            if let Some(verdict) = judge_one(package, declared, plain, all, doctests) {
                findings.push(Finding {
                    package: package.name.clone(),
                    manifest_path: package.manifest_path.clone(),
                    name: declared.name.clone(),
                    section: declared.section,
                    target: declared.target.clone(),
                    verdict,
                });
            }
        }
    }

    findings
}

/// Judge one declaration.
fn judge_one(package: &Package, declared: &Declared, plain: &Evidence, all: &Evidence, doctests: &DoctestEvidence) -> Option<Verdict> {
    let name = declared.extern_name();
    let manifest = package.manifest_path.as_path();
    let used_by_doctest = doctests.used(&package.manifest_path, &name);

    match declared.section {
        Section::Normal => {
            if plain.used_by_plain_unit(manifest, &name) {
                None
            } else if used_by_doctest || all.used_by_any_unit(manifest, &name, Scope::Always) {
                Some(Verdict::Misplaced)
            } else {
                Some(Verdict::Unused)
            }
        }

        Section::Development => (used_by_doctest || all.used_by_any_unit(manifest, &name, Scope::DevelopmentOnly))
            .then_some(())
            .map_or(Some(Verdict::Unused), |()| None),

        Section::Build => all
            .used_by_build_script(manifest, &name)
            .then_some(())
            .map_or(Some(Verdict::Unused), |()| None),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn declaration(name: &str, section: Section) -> Declared {
        Declared {
            name: name.to_owned(),
            section,
            target: None,
            feature_referenced: false,
        }
    }

    fn package(name: &str, declared: Vec<Declared>) -> Package {
        Package {
            name: name.to_owned(),
            version: "1.0.0".to_owned(),
            manifest_path: PathBuf::from(name).join("Cargo.toml"),
            declared,
            allowed: BTreeSet::new(),
            has_doctests: false,
        }
    }

    #[test]
    fn skipped_packages_and_declarations_never_stop_later_findings() {
        let unselected = package("unselected", vec![declaration("first", Section::Normal)]);
        let selected = package(
            "selected",
            vec![
                declaration("ambiguous", Section::Normal),
                declaration("ambiguous", Section::Development),
                declaration("last", Section::Normal),
            ],
        );
        let selection = BTreeSet::from([selected.manifest_path.clone()]);
        let findings = judge(
            &[unselected, selected],
            &selection,
            &Evidence::default(),
            &Evidence::default(),
            &DoctestEvidence::default(),
            &BTreeSet::new(),
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].package, "selected");
        assert_eq!(findings[0].name, "last");
        assert_eq!(findings[0].verdict, Verdict::Unused);
    }

    #[test]
    fn each_exemption_skips_only_its_own_declaration() {
        for exemption in ["feature", "workspace", "package"] {
            let mut fixture = package(
                "fixture",
                vec![declaration("exempt", Section::Normal), declaration("last", Section::Normal)],
            );
            let mut allowed = BTreeSet::new();
            match exemption {
                "feature" => fixture.declared[0].feature_referenced = true,
                "workspace" => {
                    allowed.insert("exempt".to_owned());
                }
                "package" => {
                    fixture.allowed.insert("exempt".to_owned());
                }
                _ => unreachable!("the exemption table contains only supported exemptions"),
            }
            let selection = BTreeSet::from([fixture.manifest_path.clone()]);
            let findings = judge(
                &[fixture],
                &selection,
                &Evidence::default(),
                &Evidence::default(),
                &DoctestEvidence::default(),
                &allowed,
            );
            assert_eq!(findings.len(), 1, "{exemption}");
            assert_eq!(findings[0].name, "last", "{exemption}");
        }
    }
}
