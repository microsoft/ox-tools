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
    /// `{name}` is the target name as written; `{crate}` is that name with `-`
    /// replaced by `_`, which is what Cargo does when deriving library and
    /// debug-info file names. An executable keeps its hyphen, so `foo-bar` and
    /// `foo_bar` produce distinct executables but a single `foo_bar.pdb`.
    ///
    /// Files whose name derives from another artifact are covered by that
    /// artifact's entry and are deliberately absent: the import library and
    /// export file beside a Windows DLL, and the `<artifact>.cargo-sbom.json`
    /// sidecar. The debug-info file is listed because its name does not derive
    /// that way -- it is `<name>.pdb`, not `<name>.exe.pdb`.
    fn artifacts(self) -> &'static [&'static str] {
        match self {
            Self::Executable => &["{name}[.exe]", "{crate}.pdb"],
            Self::RustLibrary => &["lib{crate}.rlib"],
            Self::SharedLibrary => &["[lib]{crate}[.so|.dll|.dylib]", "{crate}.pdb"],
            Self::StaticLibrary => &["[lib]{crate}[.a|.lib]"],
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

/// A target's kind, flattened into a key. Together with the package and the
/// target name this identifies a declaration: Cargo rejects two targets of one
/// kind sharing a name, but permits a `[lib]` and a `[[bin]]` of one name,
/// even when both point at the same source file.
fn kind_key(target: &Target) -> String {
    target.kind.iter().map(ToString::to_string).collect::<Vec<_>>().join("+")
}
/// The name Cargo derives for a library or debug-info file: hyphens become
/// underscores, because they are not valid in a Rust crate name.
fn crate_name(target: &str) -> String {
    target.replace('-', "_")
}

/// Target kinds Cargo always leaves in `deps/` under a metadata-hashed name,
/// so duplicate names among them can never contend for one file.
fn is_hashed(kind: &TargetKind) -> bool {
    matches!(kind, TargetKind::Test | TargetKind::Bench | TargetKind::CustomBuild)
}

/// One contended file and the targets competing for it.
#[derive(Debug)]
struct Contenders {
    /// The target name every owner spells the same way.
    name: String,
    /// Keyed by package and target identity, so one target counts once even
    /// when several of its crate types land in one family, while two targets in
    /// a single package -- a `[lib]` and a `[[bin]]` of one name, which Cargo
    /// permits, even pointing at the same source file -- still count
    /// separately. Name and kind together identify a declaration: Cargo
    /// rejects two targets of one kind sharing a name.
    owners: BTreeMap<(String, String, String), String>,
}

/// A target name contended by the same set of targets.
///
/// One name can contend for several files at once (an executable collides on
/// both its executable and its debug-info file), so the files are grouped to
/// keep the report to one entry per contended target.
#[derive(Debug)]
pub struct Collision {
    /// The contended target name.
    pub name: String,
    /// Contending targets, each rendered as
    /// `package (kind 'name as that target declares it')`. The declared names
    /// can differ from the contended one: `foo-bar` and `foo_bar` both derive
    /// `foo_bar.pdb`, and a rename is only actionable if the report says which
    /// target spells it which way.
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
            "target '{}' is declared by {} targets: {}\n  they uplift to the same {}: {}",
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

/// Finds every file two or more workspace targets would uplift to.
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
            let crate_name = crate_name(&target.name);
            for family in target.crate_types.iter().filter_map(Family::of) {
                for pattern in family.artifacts() {
                    let file = format!(
                        "{directory}/{}",
                        pattern.replace("{crate}", &crate_name).replace("{name}", &target.name)
                    );
                    owners
                        .entry(file)
                        .or_insert_with(|| Contenders {
                            name: target.name.clone(),
                            owners: BTreeMap::new(),
                        })
                        .owners
                        .insert(
                            (package.name.to_string(), target.name.clone(), kind_key(target)),
                            format!("{} ({} '{}')", package.name, label(target, family), target.name),
                        );
                }
            }
        }
    }

    let mut grouped: BTreeMap<(String, Vec<String>), Vec<String>> = BTreeMap::new();
    for (file, contenders) in owners {
        if contenders.owners.len() < 2 {
            continue;
        }
        let described = contenders.owners.into_values().collect::<Vec<_>>();
        grouped.entry((contenders.name, described)).or_default().push(file);
    }

    grouped
        .into_iter()
        .map(|((name, owners), files)| Collision { name, owners, files })
        .collect()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// Crate types Cargo never uplifts have no family, so a future crate type
    /// is ignored rather than guessed at.
    #[test]
    fn crate_types_that_are_never_uplifted_have_no_family() {
        assert_eq!(Family::of(&CrateType::Unknown("wasm-exotic".to_owned())), None);
    }

    /// Each uplifted crate type maps to the family whose files it emits.
    #[test]
    fn uplifted_crate_types_map_to_their_artifact_family() {
        assert_eq!(Family::of(&CrateType::Bin), Some(Family::Executable));
        assert_eq!(Family::of(&CrateType::Lib), Some(Family::RustLibrary));
        assert_eq!(Family::of(&CrateType::RLib), Some(Family::RustLibrary));
        assert_eq!(Family::of(&CrateType::CDyLib), Some(Family::SharedLibrary));
        assert_eq!(Family::of(&CrateType::DyLib), Some(Family::SharedLibrary));
        assert_eq!(Family::of(&CrateType::ProcMacro), Some(Family::SharedLibrary));
        assert_eq!(Family::of(&CrateType::StaticLib), Some(Family::StaticLibrary));
    }

    /// Only the families that emit a debug-info file claim one, which is what
    /// keeps a binary from contending with an `rlib` of the same name. The
    /// debug-info name is derived, so it uses the `{crate}` placeholder.
    #[test]
    fn only_executables_and_shared_libraries_claim_a_debug_info_file() {
        assert!(Family::Executable.artifacts().contains(&"{crate}.pdb"));
        assert!(Family::SharedLibrary.artifacts().contains(&"{crate}.pdb"));
        assert!(!Family::RustLibrary.artifacts().contains(&"{crate}.pdb"));
        assert!(!Family::StaticLibrary.artifacts().contains(&"{crate}.pdb"));
    }

    /// Hyphens survive in an executable name but not in a derived one.
    #[test]
    fn only_derived_file_names_replace_hyphens() {
        assert_eq!(crate_name("foo-bar"), "foo_bar");
        assert!(Family::Executable.artifacts().contains(&"{name}[.exe]"));
        assert!(Family::RustLibrary.artifacts().contains(&"lib{crate}.rlib"));
    }

    /// The report says "file" for one contended path and "files" for several.
    #[test]
    fn the_diagnostic_agrees_in_number_with_the_contended_files() {
        let one = Collision {
            name: "solo".to_owned(),
            owners: vec!["alpha (library 'solo')".to_owned(), "beta (library 'solo')".to_owned()],
            files: vec!["target/<profile>/libsolo.rlib".to_owned()],
        };
        assert!(one.render().contains("they uplift to the same file: "), "{}", one.render());

        let many = Collision {
            name: "duo".to_owned(),
            owners: vec!["alpha (binary 'duo')".to_owned(), "beta (binary 'duo')".to_owned()],
            files: vec!["target/<profile>/duo[.exe]".to_owned(), "target/<profile>/duo.pdb".to_owned()],
        };
        assert!(many.render().contains("they uplift to the same files: "), "{}", many.render());
    }
}
