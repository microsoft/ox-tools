// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Judging: folding compile evidence into one verdict per declaration.

use std::collections::BTreeSet;
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

    /// What the evidence says.
    pub verdict: Verdict,
}

/// A package, as far as judging is concerned.
pub struct Package {
    /// Package name, for reporting.
    pub name: String,

    /// Its manifest.
    pub manifest_path: PathBuf,

    /// Everything it declares.
    pub declared: Vec<Declared>,

    /// Whether it has a library target, and so can have doctests at all.
    pub has_library: bool,
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
    plain: &Evidence,
    all: &Evidence,
    doctests: &DoctestEvidence,
    allowed: &BTreeSet<String>,
) -> Vec<Finding> {
    let mut findings = Vec::new();

    for package in packages {
        // A package nothing was compiled for was not part of this selection;
        // silence about it is absence of evidence, not evidence of absence.
        if !all.saw_package(&package.manifest_path) {
            continue;
        }

        for declared in &package.declared {
            if allowed.contains(&declared.name) {
                continue;
            }

            if let Some(verdict) = judge_one(package, declared, plain, all, doctests) {
                findings.push(Finding {
                    package: package.name.clone(),
                    manifest_path: package.manifest_path.clone(),
                    name: declared.name.clone(),
                    section: declared.section,
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
    let used_by_doctest = doctests.used(&package.name, &name);

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
