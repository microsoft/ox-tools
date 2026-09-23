// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The collision rule: which files Cargo uplifts, and who owns each of them.

use std::collections::BTreeMap;

use cargo_metadata::{CrateType, Metadata, Target, TargetKind};

/// The family of build artifacts a crate type emits.
///
/// Crate types are grouped by the files they produce rather than by their own
/// identity, because the relationship runs both ways: `cdylib`, `dylib`, and
/// `proc-macro` all emit one platform shared library, while an `rlib` and a
/// `staticlib` of the same name emit different files and do not contend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Family {
    Executable,
    RustLibrary,
    SharedLibrary,
    StaticLibrary,
}

impl Family {
    /// The family a crate type belongs to, or `None` when Cargo never uplifts
    /// it. `Lib` and `RLib` are the same family; `cargo metadata` reports an
    /// ordinary library as `lib`, so both spellings must be handled.
    fn of(crate_type: &CrateType) -> Option<Self> {
        match crate_type {
            CrateType::Bin => Some(Self::Executable),
            CrateType::Lib | CrateType::RLib => Some(Self::RustLibrary),
            CrateType::CDyLib | CrateType::DyLib | CrateType::ProcMacro => Some(Self::SharedLibrary),
            CrateType::StaticLib => Some(Self::StaticLibrary),
            _ => None,
        }
    }

    /// Every uplifted file this family emits, as a display pattern.
    ///
    /// Files whose name derives from another artifact are covered by that
    /// artifact's entry and are deliberately absent: the import library and
    /// export file beside a Windows DLL, and the `<artifact>.cargo-sbom.json`
    /// sidecar. The debug-info file is listed because its name does not derive
    /// that way -- it is `<name>.pdb`, not `<name>.exe.pdb`.
    fn artifacts(self) -> &'static [&'static str] {
        match self {
            Self::Executable => &["{name}[.exe]", "{name}.pdb"],
            Self::RustLibrary => &["lib{name}.rlib"],
            Self::SharedLibrary => &["[lib]{name}[.so|.dll|.dylib]", "{name}.pdb"],
            Self::StaticLibrary => &["[lib]{name}[.a|.lib]"],
        }
    }

    /// How this family is described in a diagnostic.
    fn label(self) -> &'static str {
        match self {
            Self::Executable => "binary",
            Self::RustLibrary => "library",
            Self::SharedLibrary => "shared library",
            Self::StaticLibrary => "static library",
        }
    }
}

/// Target kinds Cargo always leaves in `deps/` under a metadata-hashed name,
/// so duplicate names among them can never contend for one file.
fn is_hashed(kind: &TargetKind) -> bool {
    matches!(kind, TargetKind::Test | TargetKind::Bench | TargetKind::CustomBuild)
}

/// One contended file and the packages competing for it.
#[derive(Debug)]
struct Contenders {
    /// The target name every owner spells the same way.
    name: String,
    /// Maps a package name to how that package spells the target. Keyed by
    /// package so one package owning several crate types of a name counts once.
    owners: BTreeMap<String, &'static str>,
}

/// A target name contended by the same set of packages, and every file they
/// contend for. One name can contend for several files at once (an executable
/// collides on both its executable and its debug-info file), so the files are
/// grouped to keep the report to one entry per contended target.
#[derive(Debug)]
pub struct Collision {
    /// The contended target name.
    pub name: String,
    /// Owning packages, each rendered as `package (how it spells the target)`.
    pub owners: Vec<String>,
    /// Every file the owners contend for.
    pub files: Vec<String>,
}

impl Collision {
    /// The diagnostic for this collision, as the lines the tool prints.
    #[must_use]
    pub fn render(&self) -> String {
        let noun = if self.files.len() == 1 { "file" } else { "files" };
        format!(
            "target '{}' is declared by {} workspace packages: {}\n  they uplift to the same {}: {}",
            self.name,
            self.owners.len(),
            self.owners.join(", "),
            noun,
            self.files.join(", ")
        )
    }
}

/// The directory a target uplifts into, relative to the target directory.
fn directory(target: &Target) -> &'static str {
    if target.kind.iter().any(|kind| matches!(kind, TargetKind::Example)) {
        "target/<profile>/examples"
    } else {
        "target/<profile>"
    }
}

/// How a target is described in a diagnostic: examples are called examples
/// whatever they are built as, because that is how the manifest names them.
fn label(target: &Target, family: Family) -> &'static str {
    if target.kind.iter().any(|kind| matches!(kind, TargetKind::Example)) {
        "example"
    } else {
        family.label()
    }
}

/// Finds every file two or more workspace packages would uplift to.
///
/// Iteration order is that of `BTreeMap`, so the report is byte-ordered and
/// identical on every host, and keys never fold case: two targets whose names
/// differ only in case are distinct to Cargo and stay distinct here.
#[must_use]
pub fn find(metadata: &Metadata) -> Vec<Collision> {
    let mut owners: BTreeMap<String, Contenders> = BTreeMap::new();

    for package in metadata.workspace_packages() {
        for target in &package.targets {
            if target.kind.iter().any(is_hashed) {
                continue;
            }
            let directory = directory(target);
            for family in target.crate_types.iter().filter_map(Family::of) {
                for pattern in family.artifacts() {
                    let file = format!("{directory}/{}", pattern.replace("{name}", &target.name));
                    owners
                        .entry(file)
                        .or_insert_with(|| Contenders {
                            name: target.name.clone(),
                            owners: BTreeMap::new(),
                        })
                        .owners
                        .insert(package.name.to_string(), label(target, family));
                }
            }
        }
    }

    let mut grouped: BTreeMap<(String, Vec<String>), Vec<String>> = BTreeMap::new();
    for (file, contenders) in owners {
        if contenders.owners.len() < 2 {
            continue;
        }
        let described = contenders
            .owners
            .iter()
            .map(|(package, label)| format!("{package} ({label})"))
            .collect::<Vec<_>>();
        grouped.entry((contenders.name, described)).or_default().push(file);
    }

    grouped
        .into_iter()
        .map(|((name, owners), files)| Collision { name, owners, files })
        .collect()
}
