// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Released-surface filtering for cargo-llvm-cov LCOV reports.
//!
//! Cargo target roots establish whether a source file belongs to a released
//! library/binary surface or only to tests, examples, benches, and build
//! scripts. Out-of-line modules are followed with Rust's module-resolution
//! rules. Within released files, syntax ranges guarded exclusively by test
//! configuration are removed from every line-bearing LCOV record. Opaque
//! production macros and measured source-graph gaps limit exclusions to file-local proofs, retaining
//! counters whose indirect source contexts cannot be established.

use std::borrow::Cow;
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::{env, fs};

use cargo_metadata::{Edition as CargoEdition, Metadata, TargetKind};
use ra_ap_syntax::ast::{self, HasAttrs, HasName, LiteralKind};
use ra_ap_syntax::{AstNode, AstToken, Edition, SourceFile, SyntaxKind, SyntaxNode, TextRange};

use crate::CoverageGateError;
use crate::error::{
    AmbiguousModuleSourceError, CompilerSourceDiscoveryError, ConditionalModulePathError, ConflictingReleasedSourceAttributionError,
    ConflictingSourceEditionError, CoverageLineOutOfRangeError, InspectCoverageSourceError, InvalidModulePathAttributeError,
    InvalidTestAttributePathError, InvalidTestAttributesValueError, NormalizedSourceChecksumError, PackageScopedTestAttributesError,
    ParseCoverageSourceError, ParseLcovError, ReadCoverageSourceError, ResolveCoverageSourceError, UnattributableReleasedSourceError,
    UnownedCoverageSourceError, UnsupportedSourceEditionError,
};

const LINE_RELEASED: u8 = 1;
const LINE_TEST_ONLY: u8 = 1 << 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Surface {
    Released,
    NonReleased,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SourceRole {
    CrateRoot,
    PlainModule,
    PathModule,
    IncludedSource,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct MemberDirectory {
    canonical: PathBuf,
    reported: PathBuf,
}

/// One traversal of a canonical source file through a particular declaration
/// path and cfg guard.
///
/// Canonical paths identify contents and ownership; resolution paths preserve
/// the declared file location used to resolve child modules.
#[derive(Debug, Clone)]
struct PendingSource {
    canonical_path: PathBuf,
    resolution_path: PathBuf,
    edition: Edition,
    surface: Surface,
    role: SourceRole,
    guard: CfgFormula,
    owning_member: MemberDirectory,
}

/// A discovered child source together with the guard under which it exists.
#[derive(Debug, Clone)]
struct ModuleDeclaration {
    path: PathBuf,
    role: SourceRole,
    guard: CfgFormula,
}

/// Parsed source facts shared by every visit to one canonical file.
///
/// Test-only included fragments intentionally have no standalone Rust syntax;
/// a later ordinary module visit promotes them to parsed analysis.
#[derive(Debug)]
struct SourceAnalysis {
    syntax: Option<ast::SourceFile>,
    line_count: usize,
    line_starts: Vec<usize>,
    test_only_lines: BTreeSet<u32>,
}

impl SourceAnalysis {
    fn syntax(&self) -> &ast::SourceFile {
        self.syntax
            .as_ref()
            .expect("only ordinary Rust source analysis requires syntax; included-source leaves return before traversal")
    }
}

/// Canonical source analysis plus every distinct traversal context.
///
/// Test-only line sets from released visits are intersected so a line remains
/// measured whenever any released visit can enable it.
#[derive(Debug)]
struct IndexedSource {
    analysis: SourceAnalysis,
    edition: Edition,
    released_path: Option<PathBuf>,
    test_only_lines: BTreeSet<u32>,
    visit_contexts: BTreeSet<(Surface, SourceRole, PathBuf, PathBuf, CfgFormula)>,
}

impl IndexedSource {
    fn has_visit(&self, pending: &PendingSource) -> bool {
        self.visit_contexts.contains(&(
            pending.surface,
            pending.role,
            pending.resolution_path.clone(),
            pending.owning_member.canonical.clone(),
            pending.guard.clone(),
        ))
    }

    fn add_visit(&mut self, pending: &PendingSource, released_path: Option<PathBuf>) -> Result<(), CoverageGateError> {
        if let (Some(first), Some(second)) = (&self.released_path, &released_path)
            && first != second
        {
            return Err(ConflictingReleasedSourceAttributionError::new(
                diagnostic_path(&pending.canonical_path),
                diagnostic_path(first),
                diagnostic_path(second),
            )
            .into());
        }
        let _ = self.visit_contexts.insert((
            pending.surface,
            pending.role,
            pending.resolution_path.clone(),
            pending.owning_member.canonical.clone(),
            pending.guard.clone(),
        ));
        if let Some(path) = released_path {
            let _ = self.released_path.get_or_insert(path);
        }
        Ok(())
    }

    fn merge_released_test_only_lines(&mut self, current: BTreeSet<u32>) {
        if self.released_path.is_some() {
            self.test_only_lines.retain(|line| current.contains(line));
        } else {
            self.test_only_lines = current;
        }
    }
}

#[derive(Debug)]
struct SourceIndex {
    workspace_root: PathBuf,
    files: BTreeMap<PathBuf, IndexedSource>,
    member_dirs: MemberDirectories,
    uncertain_sources: BTreeSet<PathBuf>,
}

#[derive(Debug)]
struct MemberDirectories {
    reported_by_canonical: BTreeMap<PathBuf, PathBuf>,
}

fn configured_test_attributes(metadata: &Metadata) -> Result<BTreeSet<String>, CoverageGateError> {
    reject_package_test_attributes(
        metadata
            .workspace_packages()
            .into_iter()
            .map(|package| (package.name.as_str(), &package.metadata)),
    )?;

    parse_configured_test_attributes(test_attributes_value(&metadata.workspace_metadata))
}

fn reject_package_test_attributes<'a>(
    packages: impl IntoIterator<Item = (&'a str, &'a serde_json::Value)>,
) -> Result<(), CoverageGateError> {
    for (name, metadata) in packages {
        if package_test_attributes_value(metadata).is_some() {
            return Err(PackageScopedTestAttributesError::new(name.to_owned()).into());
        }
    }
    Ok(())
}

fn test_attributes_value(metadata: &serde_json::Value) -> Option<&serde_json::Value> {
    metadata.get("coverage-gate").and_then(|gate| gate.get("test-attributes"))
}

fn package_test_attributes_value(metadata: &serde_json::Value) -> Option<&serde_json::Value> {
    let gate = metadata.get("coverage-gate")?;
    gate.get("test-attributes").or_else(|| {
        gate.get("target")?
            .as_object()?
            .values()
            .find_map(|policy| policy.get("test-attributes"))
    })
}

fn parse_configured_test_attributes(raw: Option<&serde_json::Value>) -> Result<BTreeSet<String>, CoverageGateError> {
    let Some(raw) = raw else {
        return Ok(BTreeSet::new());
    };
    let values = raw
        .as_array()
        .ok_or_else(|| InvalidTestAttributesValueError::new("workspace".to_owned(), raw.clone()))?;
    values
        .iter()
        .map(|value| {
            let path = value
                .as_str()
                .ok_or_else(|| InvalidTestAttributesValueError::new("workspace".to_owned(), value.clone()))?;
            parse_configured_test_attribute(path)
                .ok_or_else(|| InvalidTestAttributePathError::new(crate::package_glob::diagnostic(path)).into())
        })
        .collect()
}

fn parse_configured_test_attribute(value: &str) -> Option<String> {
    let expected_attribute = format!("#[{value}]");
    let source = format!("{expected_attribute}\nfn __coverage_gate_test_attribute() {{}}\n");

    // #[gamma::skip(collection.reverse_array, reason = "Every accepted edition yields the same normalized path; reversing edition order preserves the acceptance union.", tag = "equivalent")]
    [
        Edition::Edition2024,
        Edition::Edition2021,
        Edition::Edition2018,
        Edition::Edition2015,
    ]
    .into_iter()
    .find_map(|edition| parse_configured_test_attribute_for_edition(&source, &expected_attribute, edition))
}

// Validates exactly one path attribute in one edition; the caller owns cross-edition acceptance.
fn parse_configured_test_attribute_for_edition(source: &str, expected_attribute: &str, edition: Edition) -> Option<String> {
    let parse = SourceFile::parse(source, edition);
    if !parse.errors().is_empty() {
        return None;
    }
    let tree = parse.tree();
    let attribute = tree.syntax().descendants().find_map(ast::Attr::cast)?;
    if attribute.syntax().text() != expected_attribute {
        return None;
    }
    let ast::Meta::PathMeta(meta) =
    // #[gamma::skip(try.propagate_to_unwrap, reason = "An error-free complete attribute matching the expected text necessarily contains its Meta node.", tag = "equivalent")]
    attribute.meta()? else {
        return None;
    };
    meta.path().map(|path| normalize_attribute_path(&path))
}

impl SourceIndex {
    #[ohno::enrich_err("failed to load Cargo target ownership for coverage filtering")]
    fn from_metadata(metadata: &Metadata) -> Result<Self, CoverageGateError> {
        Self::from_metadata_with_source_resolver(metadata, &mut resolve_source_path)
    }

    fn from_metadata_with_source_resolver(
        metadata: &Metadata,
        resolve_source: &mut impl FnMut(&Path) -> Result<PathBuf, CoverageGateError>,
    ) -> Result<Self, CoverageGateError> {
        Self::from_metadata_with_source_reader(metadata, resolve_source, &mut |path| fs::read_to_string(path))
    }

    fn from_metadata_with_source_reader(
        metadata: &Metadata,
        resolve_source: &mut impl FnMut(&Path) -> Result<PathBuf, CoverageGateError>,
        read_source: &mut impl FnMut(&Path) -> std::io::Result<String>,
    ) -> Result<Self, CoverageGateError> {
        let test_attributes = configured_test_attributes(metadata)?;
        let workspace_root = resolve_source(metadata.workspace_root.as_std_path())?;
        let (member_dirs, mut queue) = source_roots(metadata, resolve_source)?;

        let mut files: BTreeMap<PathBuf, IndexedSource> = BTreeMap::new();
        while let Some(pending) = queue.pop_front() {
            if let Some(indexed) = files.get_mut(&pending.canonical_path) {
                let Some(declarations) =
                    Self::revisit_indexed_source_with_reader(indexed, &pending, &member_dirs, &test_attributes, read_source)?
                else {
                    continue;
                };
                enqueue_declarations_with_source_resolver(
                    &mut queue,
                    declarations,
                    pending.edition,
                    pending.surface,
                    &pending.owning_member,
                    resolve_source,
                )?;
                continue;
            }

            let analysis = if pending.role == SourceRole::IncludedSource {
                analyze_included_source_with_reader(&pending.canonical_path, read_source)?
            } else {
                analyze_source_with_reader(&pending.canonical_path, pending.edition, &test_attributes, read_source)?
            };
            let declarations = source_declarations(&pending.resolution_path, &analysis, pending.role, &pending.guard, &test_attributes)?;
            let released_path = released_source_attributed_path(&member_dirs, &pending)?;
            let test_only_lines = if released_path.is_some() {
                test_only_lines_for_guard(&analysis, &pending.guard, &test_attributes)
            } else {
                BTreeSet::new()
            };
            let mut indexed = IndexedSource {
                analysis,
                edition: pending.edition,
                released_path: None,
                test_only_lines,
                visit_contexts: BTreeSet::new(),
            };

            // #[gamma::skip(try.propagate_to_unwrap, reason = "The initial visit has no released path; conflicts require two distinct Some paths.", tag = "equivalent")]
            indexed.add_visit(&pending, released_path)?;
            let _ = files.insert(pending.canonical_path.clone(), indexed);
            enqueue_declarations_with_source_resolver(
                &mut queue,
                declarations,
                pending.edition,
                pending.surface,
                &pending.owning_member,
                resolve_source,
            )?;
        }

        let uncertain_sources = files
            .iter()
            .filter(|(_, indexed)| {
                indexed.analysis.syntax.as_ref().is_some_and(|syntax| {
                    indexed.visit_contexts.iter().any(|(surface, role, _, _, guard)| {
                        *role != SourceRole::IncludedSource && source_has_opaque_source_macro(syntax, *surface, guard, &test_attributes)
                    })
                })
            })
            .map(|(path, _)| path.clone())
            .collect();
        Ok(Self {
            workspace_root,
            files,
            member_dirs,
            uncertain_sources,
        })
    }

    fn index_reported_member_sources(&mut self, reports: &[lcov::Report], metadata: &Metadata) -> Result<(), CoverageGateError> {
        let test_attributes = configured_test_attributes(metadata)?;
        for report in reports {
            for key in report.sections.keys() {
                let (canonical, source) = self.source_for_lcov(&key.source_file)?;
                if source.is_some() {
                    continue;
                }
                let Some(reported_member) = canonical
                    .ancestors()
                    .find_map(|ancestor| self.member_dirs.reported_by_canonical.get(ancestor))
                else {
                    continue;
                };
                let package = metadata
                    .packages
                    .iter()
                    .filter(|package| metadata.workspace_members.contains(&package.id))
                    .find(|package| {
                        package
                            .manifest_path
                            .parent()
                            .is_some_and(|parent| parent.as_std_path() == reported_member)
                    })
                    .expect("member directories originate from the same Cargo metadata snapshot");
                let edition = measured_member_edition(package, &canonical)?;
                let analysis = analyze_source(&canonical, edition, &test_attributes)?;
                let _ = self.uncertain_sources.insert(canonical.clone());
                let _ = self.files.insert(
                    canonical,
                    IndexedSource {
                        analysis,
                        edition,
                        released_path: None,
                        test_only_lines: BTreeSet::new(),
                        visit_contexts: BTreeSet::new(),
                    },
                );
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn revisit_indexed_source(
        indexed: &mut IndexedSource,
        pending: &PendingSource,
        member_dirs: &MemberDirectories,
        test_attributes: &BTreeSet<String>,
    ) -> Result<Option<Vec<ModuleDeclaration>>, CoverageGateError> {
        Self::revisit_indexed_source_with_reader(indexed, pending, member_dirs, test_attributes, &mut |path| fs::read_to_string(path))
    }

    fn revisit_indexed_source_with_reader(
        indexed: &mut IndexedSource,
        pending: &PendingSource,
        member_dirs: &MemberDirectories,
        test_attributes: &BTreeSet<String>,
        read_source: &mut impl FnMut(&Path) -> std::io::Result<String>,
    ) -> Result<Option<Vec<ModuleDeclaration>>, CoverageGateError> {
        if indexed.edition != pending.edition {
            return Err(ConflictingSourceEditionError::new(
                diagnostic_path(&pending.canonical_path),
                format!("{:?}", indexed.edition),
                format!("{:?}", pending.edition),
            )
            .into());
        }
        if indexed.has_visit(pending) {
            return Ok(None);
        }
        if pending.role != SourceRole::IncludedSource && indexed.analysis.syntax.is_none() {
            indexed.analysis = analyze_source_with_reader(&pending.canonical_path, pending.edition, test_attributes, read_source)?;
        }
        let released_path = released_source_attributed_path(member_dirs, pending)?;
        if released_path.is_some() {
            let current = test_only_lines_for_guard(&indexed.analysis, &pending.guard, test_attributes);
            indexed.merge_released_test_only_lines(current);
        }
        indexed.add_visit(pending, released_path)?;
        source_declarations(
            &pending.resolution_path,
            &indexed.analysis,
            pending.role,
            &pending.guard,
            test_attributes,
        )
        .map(Some)
    }

    fn source_for_lcov(&self, path: &Path) -> Result<(PathBuf, Option<&IndexedSource>), CoverageGateError> {
        self.source_for_lcov_with_resolver(path, resolve_source_path)
    }

    fn source_for_lcov_with_resolver(
        &self,
        path: &Path,
        resolve_source: impl FnOnce(&Path) -> Result<PathBuf, CoverageGateError>,
    ) -> Result<(PathBuf, Option<&IndexedSource>), CoverageGateError> {
        let candidate = self.workspace_root.join(path);
        let resolved = resolve_source(&candidate)?;
        let source = self.files.get(&resolved);
        Ok((resolved, source))
    }

    fn retained_source_path(&self, canonical: &Path, source: &IndexedSource) -> Result<PathBuf, CoverageGateError> {
        if let Some(path) = &source.released_path {
            return Ok(path.clone());
        }
        if let Some(path) = attributed_path(&self.member_dirs, canonical) {
            return Ok(path);
        }

        let mut retained: Option<PathBuf> = None;
        for (_, _, _, member, _) in &source.visit_contexts {
            let reported = self
                .member_dirs
                .reported_by_canonical
                .get(member)
                .expect("every source visit originates from an indexed Cargo member");
            let relative =
                relative_path(member, canonical).ok_or_else(|| UnattributableReleasedSourceError::new(diagnostic_path(canonical)))?;
            let candidate = reported.join(relative);
            if let Some(first) = &retained
                && first != &candidate
            {
                return Err(ConflictingReleasedSourceAttributionError::new(
                    diagnostic_path(canonical),
                    diagnostic_path(first),
                    diagnostic_path(&candidate),
                )
                .into());
            }
            retained = Some(candidate);
        }
        retained.ok_or_else(|| UnownedCoverageSourceError::new(diagnostic_path(canonical)).into())
    }

    fn conservative_warning(&self) -> Option<String> {
        self.uncertain_sources.first().map(|path| {
            format!(
                "warning: coverage-gate: opaque production macro or incomplete source graph at `{}`; \
                 excluding only file-local test-only ranges and retaining ambiguous source counters. \
                 Filtered LCOV is conservative, not complete production-only classification; \
                 use `#[cfg_attr(coverage_nightly, coverage(off))]` for exceptional code only when appropriate",
                diagnostic_path(path)
            )
        })
    }
}

fn measured_member_edition(package: &cargo_metadata::Package, source: &Path) -> Result<Edition, CoverageGateError> {
    let mut editions = package.targets.iter().map(|target| syntax_edition(target.edition, source));
    let first = editions.next().unwrap_or_else(|| syntax_edition(package.edition, source))?;
    for edition in editions {
        let edition = edition?;
        if first != edition {
            return Err(ConflictingSourceEditionError::new(diagnostic_path(source), format!("{first:?}"), format!("{edition:?}")).into());
        }
    }
    Ok(first)
}

fn source_roots(
    metadata: &Metadata,
    resolve_source: &mut impl FnMut(&Path) -> Result<PathBuf, CoverageGateError>,
) -> Result<(MemberDirectories, VecDeque<PendingSource>), CoverageGateError> {
    let mut reported_by_canonical = BTreeMap::new();
    let workspace_packages = metadata.workspace_packages();
    let mut owning_members = Vec::with_capacity(workspace_packages.len());
    for package in &workspace_packages {
        let directory = package
            .manifest_path
            .parent()
            .expect("cargo metadata always reports a manifest path with a parent directory");
        let canonical = resolve_source(directory.as_std_path())?;
        let _ = reported_by_canonical
            .entry(canonical.clone())
            .or_insert_with(|| directory.as_std_path().to_path_buf());
        owning_members.push(MemberDirectory {
            canonical,
            reported: directory.as_std_path().to_path_buf(),
        });
    }
    let member_dirs = MemberDirectories { reported_by_canonical };

    let mut queue = VecDeque::new();
    for (package, owning_member) in workspace_packages.into_iter().zip(owning_members) {
        for target in &package.targets {
            let edition =
            // #[gamma::skip(try.propagate_to_unwrap, reason = "Every constructible cargo_metadata edition is handled; this fallback is reserved for future dependency variants.", tag = "equivalent")]
            syntax_edition(target.edition, target.src_path.as_std_path())?;
            let canonical_path = resolve_source(target.src_path.as_std_path())?;
            let resolution_path = resolve_module_resolution_path_with_resolver(target.src_path.as_std_path(), resolve_source)?;
            queue.push_back(PendingSource {
                canonical_path,
                resolution_path,
                edition,
                surface: if target_is_released(&target.kind) {
                    Surface::Released
                } else {
                    Surface::NonReleased
                },
                role: SourceRole::CrateRoot,
                guard: target_required_features_guard(&target.required_features),
                owning_member: owning_member.clone(),
            });
        }
    }
    Ok((member_dirs, queue))
}

fn attributed_path(member_dirs: &MemberDirectories, path: &Path) -> Option<PathBuf> {
    path.ancestors().find_map(|ancestor| {
        let reported = member_dirs.reported_by_canonical.get(ancestor)?;
        let relative = path.strip_prefix(ancestor).expect("path ancestors are always valid strip prefixes");
        Some(reported.join(relative))
    })
}

fn target_required_features_guard(required_features: &[String]) -> CfgFormula {
    CfgFormula::all(required_features.iter().filter_map(|selector| {
        // Dependency-feature availability does not prove which local feature activated it.
        if selector.starts_with("dep:") || selector.contains('/') {
            return None;
        }
        Some(CfgFormula::Atom(format!("feature={selector}")))
    }))
}

fn relative_path(from: &Path, to: &Path) -> Option<PathBuf> {
    let from = from.components().collect::<Vec<_>>();
    let to = to.components().collect::<Vec<_>>();
    let common = from.iter().zip(&to).take_while(|(left, right)| left == right).count();
    if common == 0 {
        return None;
    }

    let mut relative = PathBuf::new();
    for component in &from[common..] {
        match component {
            Component::Normal(_) => relative.push(".."),
            Component::CurDir => {}
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => return None,
        }
    }
    for component in &to[common..] {
        relative.push(component.as_os_str());
    }
    Some(relative)
}

fn owning_member_attributed_path(member: &MemberDirectory, path: &Path) -> Option<PathBuf> {
    relative_path(&member.canonical, path).map(|relative| member.reported.join(relative))
}

fn released_source_attributed_path(member_dirs: &MemberDirectories, pending: &PendingSource) -> Result<Option<PathBuf>, CoverageGateError> {
    match pending.surface {
        Surface::NonReleased => Ok(None),
        Surface::Released => attributed_path(member_dirs, &pending.canonical_path)
            .or_else(|| owning_member_attributed_path(&pending.owning_member, &pending.canonical_path))
            .map(Some)
            .ok_or_else(|| UnattributableReleasedSourceError::new(diagnostic_path(&pending.canonical_path)).into()),
    }
}

fn enqueue_declarations_with_source_resolver(
    queue: &mut VecDeque<PendingSource>,
    declarations: Vec<ModuleDeclaration>,
    edition: Edition,
    parent_surface: Surface,
    owning_member: &MemberDirectory,
    resolve_source: &mut impl FnMut(&Path) -> Result<PathBuf, CoverageGateError>,
) -> Result<(), CoverageGateError> {
    for declaration in declarations {
        let canonical_path = resolve_source(&declaration.path)?;
        let resolution_path = resolve_module_resolution_path_with_resolver(&declaration.path, resolve_source)?;
        queue.push_back(PendingSource {
            canonical_path,
            resolution_path,
            edition,
            surface: if parent_surface == Surface::Released && cfg_formula_possibilities_without_test(&declaration.guard).can_be_true {
                Surface::Released
            } else {
                Surface::NonReleased
            },
            role: declaration.role,
            guard: declaration.guard,
            owning_member: owning_member.clone(),
        });
    }
    Ok(())
}

#[cfg(test)]
fn resolve_module_resolution_path(path: &Path) -> Result<PathBuf, CoverageGateError> {
    resolve_module_resolution_path_with_resolver(path, &mut resolve_source_path)
}

fn resolve_module_resolution_path_with_resolver(
    path: &Path,
    resolve_source: &mut impl FnMut(&Path) -> Result<PathBuf, CoverageGateError>,
) -> Result<PathBuf, CoverageGateError> {
    // Preserve a file symlink's declared name while collapsing directory aliases so visit contexts remain bounded.
    let parent = path
        .parent()
        .expect("Cargo target and resolved module source paths always have a parent directory");
    let file_name = path
        .file_name()
        .expect("Cargo target and resolved module source paths always name a source file");
    resolve_source(parent).map(|canonical_parent| canonical_parent.join(file_name))
}

fn source_declarations(
    path: &Path,
    analysis: &SourceAnalysis,
    role: SourceRole,
    guard: &CfgFormula,
    test_attributes: &BTreeSet<String>,
) -> Result<Vec<ModuleDeclaration>, CoverageGateError> {
    if role == SourceRole::IncludedSource {
        return Ok(Vec::new());
    }
    let mut is_file = source_path_is_file;
    module_declarations(path, analysis.syntax(), role, guard, test_attributes, &mut is_file)
}

fn source_path_is_file(path: &Path) -> Result<bool, CoverageGateError> {
    classify_source_path_metadata(path, fs::metadata(path))
}

fn classify_source_path_metadata(path: &Path, metadata: std::io::Result<fs::Metadata>) -> Result<bool, CoverageGateError> {
    match metadata {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => Ok(false),
        Err(error) => Err(InspectCoverageSourceError::caused_by(diagnostic_path(path), error).into()),
    }
}

fn target_is_released(kinds: &[TargetKind]) -> bool {
    !kinds.is_empty()
        && kinds.iter().all(|kind| {
            matches!(
                kind,
                TargetKind::Lib
                    | TargetKind::RLib
                    | TargetKind::DyLib
                    | TargetKind::CDyLib
                    | TargetKind::StaticLib
                    | TargetKind::ProcMacro
                    | TargetKind::Bin
            )
        })
}

// cargo_metadata::Edition is non-exhaustive, while every currently
// constructible variant is covered by direct unit tests.
#[cfg_attr(coverage_nightly, coverage(off))]
fn syntax_edition(edition: CargoEdition, path: &Path) -> Result<Edition, CoverageGateError> {
    match edition {
        CargoEdition::E2015 => Ok(Edition::Edition2015),
        CargoEdition::E2018 => Ok(Edition::Edition2018),
        CargoEdition::E2021 => Ok(Edition::Edition2021),
        CargoEdition::E2024 => Ok(Edition::Edition2024),
        _ => Err(UnsupportedSourceEditionError::new(
            // #[gamma::skip(call.replace_with_default, call_result.default, reason = "Every constructible cargo_metadata edition is handled; this fallback is reserved for future dependency variants.", tag = "equivalent")]
            diagnostic_path(path),
            format!("{edition:?}"),
        )
        .into()),
    }
}

fn diagnostic_path(path: &Path) -> String {
    crate::package_glob::diagnostic(&path.display().to_string())
}

fn resolve_source_path(path: &Path) -> Result<PathBuf, CoverageGateError> {
    fs::canonicalize(path).map_err(|error| ResolveCoverageSourceError::caused_by(diagnostic_path(path), error).into())
}

fn analyze_source(path: &Path, edition: Edition, test_attributes: &BTreeSet<String>) -> Result<SourceAnalysis, CoverageGateError> {
    analyze_source_with_reader(path, edition, test_attributes, &mut |path| fs::read_to_string(path))
}

fn analyze_source_with_reader(
    path: &Path,
    edition: Edition,
    test_attributes: &BTreeSet<String>,
    read_source: &mut impl FnMut(&Path) -> std::io::Result<String>,
) -> Result<SourceAnalysis, CoverageGateError> {
    let source = read_source(path).map_err(|error| ReadCoverageSourceError::caused_by(diagnostic_path(path), error))?;
    analyze_source_text(path, &source, edition, test_attributes)
}

fn analyze_included_source_with_reader(
    path: &Path,
    read_source: &mut impl FnMut(&Path) -> std::io::Result<String>,
) -> Result<SourceAnalysis, CoverageGateError> {
    let source = read_source(path).map_err(|error| ReadCoverageSourceError::caused_by(diagnostic_path(path), error))?;
    Ok(SourceAnalysis {
        syntax: None,
        line_count: source.lines().count(),
        line_starts: line_starts(&source),
        test_only_lines: BTreeSet::new(),
    })
}

fn analyze_source_text(
    path: &Path,
    source: &str,
    edition: Edition,
    test_attributes: &BTreeSet<String>,
) -> Result<SourceAnalysis, CoverageGateError> {
    // rust-analyzer attaches leading comments differently around CRLF blank lines.
    // #[gamma::skip(literal.str_to_empty, reason = "Always replacing CRLF preserves the same bytes; only the no-CRLF borrowed fast path changes.", tag = "perf")]
    let has_crlf = source.contains("\r\n");
    // #[gamma::skip(cond.always_true, reason = "Always replacing CRLF preserves the same bytes; only the no-CRLF borrowed fast path changes.", tag = "perf")]
    let source = if has_crlf {
        Cow::Owned(source.replace("\r\n", "\n"))
    } else {
        Cow::Borrowed(source)
    };
    let source = source.as_ref();
    let parse = SourceFile::parse(source, edition);
    let line_starts = line_starts(source);
    if !parse.errors().is_empty() {
        let diagnostics = parse
            .errors()
            .iter()
            .map(|error| {
                let offset: usize = error.range().start().into();
                let line_index = offset_to_line(&line_starts, offset);
                let line_start = line_starts[line_index];
                let column = source
                    .get(line_start..offset)
                    .expect("parser error ranges are guaranteed to lie within the parsed source")
                    .chars()
                    .count()
                    + 1;
                format!("{error} at {}:{column}", line_index + 1)
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(ParseCoverageSourceError::new(diagnostic_path(path), diagnostics).into());
    }
    let tree = parse.tree();
    let test_only_lines = classify_test_only_lines(tree.syntax(), &line_starts, test_attributes, &CfgFormula::Value(true));
    Ok(SourceAnalysis {
        syntax: Some(tree),
        line_count: source.lines().count(),
        line_starts,
        test_only_lines,
    })
}

fn test_only_lines_for_guard(analysis: &SourceAnalysis, guard: &CfgFormula, test_attributes: &BTreeSet<String>) -> BTreeSet<u32> {
    if *guard == CfgFormula::Value(true) || analysis.syntax.is_none() {
        return analysis.test_only_lines.clone();
    }
    classify_test_only_lines(analysis.syntax().syntax(), &analysis.line_starts, test_attributes, guard)
}

fn classify_test_only_lines(
    root: &SyntaxNode,
    line_starts: &[usize],
    test_attributes: &BTreeSet<String>,
    inherited_guard: &CfgFormula,
) -> BTreeSet<u32> {
    let test_ranges = collect_test_ranges(root, test_attributes, inherited_guard);
    let test_ranges = merge_text_ranges(test_ranges);
    let mut line_flags = vec![0_u8; line_starts.len()];
    for range in &test_ranges {
        // #[gamma::skip(call_result.default, reason = "This call returns unit; default_after preserves the call and its side effects, then returns the identical unit value.", tag = "equivalent")]
        mark_range_lines(&mut line_flags, line_starts, *range, LINE_TEST_ONLY);
    }
    let mut test_range_cursor = test_ranges.iter().peekable();
    for token in root.descendants_with_tokens().filter_map(ra_ap_syntax::NodeOrToken::into_token) {
        if matches!(token.kind(), SyntaxKind::WHITESPACE | SyntaxKind::COMMENT) {
            continue;
        }
        let token_range = token.text_range();
        while test_range_cursor.peek().is_some_and(|range|
        // #[gamma::skip(relational.le_to_lt, reason = "At equality the previous half-open range cannot contain the token; either cursor position marks it released.", tag = "equivalent")]
        range.end() <= token_range.start())
        {
            // #[gamma::skip(stmt.delete_call, reason = "removing the cursor advance deliberately stalls this loop; classification and cursor boundaries are tested directly", tag = "timeout")]
            test_range_cursor.next().expect("the while guard guarantees a remaining test range");
        }
        if !test_range_cursor.peek().is_some_and(|range| range.contains_range(token_range)) {
            // #[gamma::skip(call_result.default, reason = "This call returns unit; default_after preserves the call and its side effects, then returns the identical unit value.", tag = "equivalent")]
            mark_range_lines(&mut line_flags, line_starts, token.text_range(), LINE_RELEASED);
        }
    }
    line_flags
        .iter()
        .enumerate()
        .filter(|(_, flags)| **flags & LINE_TEST_ONLY != 0 && **flags & LINE_RELEASED == 0)
        .map(|(index, _)| {
            u32::try_from(index + 1)
                .expect("a source with more than u32::MAX lines cannot produce an LCOV line number for the excess lines")
        })
        .collect()
}

fn syntax_cfg_guard(node: &SyntaxNode, test_attributes: &BTreeSet<String>) -> CfgFormula {
    if let Some(owner) = ast::AnyHasAttrs::cast(node.clone()) {
        attributes_guard(ast::attrs_including_inner(&owner), test_attributes)
    } else {
        // MacroExpr has parsed outer attributes but is absent from AnyHasAttrs.
        attributes_guard(node.children().filter_map(ast::Attr::cast), test_attributes)
    }
}

fn collect_test_ranges(root: &SyntaxNode, test_attributes: &BTreeSet<String>, inherited_guard: &CfgFormula) -> Vec<TextRange> {
    fn collect(
        node: &SyntaxNode,
        test_attributes: &BTreeSet<String>,
        inherited_guard: &CfgFormula,
        inherited_possibilities: CfgPossibilities,
        ranges: &mut Vec<TextRange>,
    ) {
        let own_guard = syntax_cfg_guard(node, test_attributes);
        let (guard, possibilities) = extend_cfg_guard(inherited_guard, inherited_possibilities, own_guard);
        if !possibilities.can_be_true {
            ranges.push(syntax_owner_range(node));
            return;
        }
        for child in node.children() {
            // #[gamma::skip(call_result.default, reason = "This call returns unit; default_after preserves the call and its side effects, then returns the identical unit value.", tag = "equivalent")]
            collect(&child, test_attributes, &guard, possibilities, ranges);
        }
    }

    let mut ranges = Vec::new();

    // #[gamma::skip(call_result.default, reason = "This call returns unit; default_after preserves the call and its side effects, then returns the identical unit value.", tag = "equivalent")]
    collect(
        root,
        test_attributes,
        inherited_guard,
        cfg_formula_possibilities_without_test(inherited_guard),
        &mut ranges,
    );
    ranges
}

fn merge_text_ranges(mut ranges: Vec<TextRange>) -> Vec<TextRange> {
    ranges.sort_by_key(|range| (range.start(), range.end()));
    let mut merged: Vec<TextRange> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if let Some(previous) = merged.last_mut()
            && range.start() <= previous.end()
        {
            *previous = TextRange::new(previous.start(), std::cmp::max(previous.end(), range.end()));
            continue;
        }
        merged.push(range);
    }
    merged
}

fn syntax_owner_range(owner: &SyntaxNode) -> TextRange {
    let owner_range = owner.text_range();
    let mut end = if let Some(statement) = owner.parent().and_then(ast::ExprStmt::cast)
        && statement.expr().is_some_and(|expression| expression.syntax() == owner)
        && let Some(semicolon) = statement.semicolon_token()
    {
        semicolon.text_range().end()
    } else {
        owner_range.end()
    };
    let mut next = owner.next_sibling_or_token();
    while let Some(element) = next {
        if matches!(element.kind(), SyntaxKind::WHITESPACE | SyntaxKind::COMMENT) {
            next = element.next_sibling_or_token();
            continue;
        }
        if element.as_token().is_some_and(|token| token.text() == ",") {
            end = element.text_range().end();
        }
        break;
    }
    TextRange::new(owner_range.start(), end)
}

fn line_starts(source: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(
            source
                .char_indices()
                .filter_map(|(index, character)| (character == '\n').then_some(index + 1)),
        )
        .collect()
}

fn mark_range_lines(flags: &mut [u8], starts: &[usize], range: TextRange, flag: u8) {
    let start: usize = range.start().into();
    let end: usize = range.end().into();
    let first = offset_to_line(
        starts,
        // #[gamma::skip(expr.increment, reason = "Owner and nontrivia token ranges never start on LF, so advancing their first byte cannot change the line.", tag = "equivalent")]
        start,
    );
    let last = offset_to_line(
        starts,
        end.checked_sub(1).expect("syntax owner ranges are guaranteed to be non-empty"),
    );
    for line in first..=last {
        let value = flags
            .get_mut(line)
            .expect("line offsets and line flags are derived from the same source");
        *value |= flag;
    }
}

fn offset_to_line(starts: &[usize], offset: usize) -> usize {
    starts.binary_search(&offset).unwrap_or_else(|line| line.saturating_sub(1))
}

#[cfg(test)]
fn attribute_marks_test(attribute: &ast::Attr, test_attributes: &BTreeSet<String>) -> bool {
    !cfg_formula_possibilities_without_test(&attributes_guard(std::iter::once(attribute.clone()), test_attributes)).can_be_true
}

fn attributes_guard(attributes: impl IntoIterator<Item = ast::Attr>, test_attributes: &BTreeSet<String>) -> CfgFormula {
    CfgFormula::all(attributes.into_iter().map(|attribute| {
        let meta = attribute.meta().expect("source parsing succeeds before attributes are classified");
        meta_guard(&meta, test_attributes)
    }))
}

fn meta_guard(meta: &ast::Meta, test_attributes: &BTreeSet<String>) -> CfgFormula {
    match meta {
        ast::Meta::CfgMeta(cfg) => {
            let predicate = cfg
                .cfg_predicate()
                .expect("source parsing succeeds before cfg attributes are classified");
            cfg_formula_from_predicate_or_unknown(&predicate)
        }
        ast::Meta::PathMeta(path_meta) => CfgFormula::Value(!path_meta.path().is_some_and(|path| path_marks_test(&path, test_attributes))),
        ast::Meta::TokenTreeMeta(token_tree) => {
            CfgFormula::Value(!token_tree.path().is_some_and(|path| path_marks_test(&path, test_attributes)))
        }
        ast::Meta::CfgAttrMeta(cfg_attr) => {
            let predicate = cfg_attr
                .cfg_predicate()
                .expect("source parsing succeeds before cfg_attr attributes are classified");
            let predicate = cfg_formula_from_predicate_or_unknown(&predicate);
            let inner = CfgFormula::all(cfg_attr.metas().map(|inner| meta_guard(&inner, test_attributes)));
            CfgFormula::any(
                // #[gamma::skip(collection.reverse_array, reason = "CfgFormula canonicalizes operand order, so reversing this conjunction or disjunction changes nothing.", tag = "equivalent")]
                [CfgFormula::not(predicate), inner],
            )
        }
        ast::Meta::KeyValueMeta(_) | ast::Meta::UnsafeMeta(_) => CfgFormula::Value(true),
    }
}

fn path_marks_test(path: &ast::Path, test_attributes: &BTreeSet<String>) -> bool {
    let normalized = normalize_attribute_path(path);
    matches!(normalized.as_str(), "test" | "bench") || test_attributes.contains(&normalized)
}

fn normalize_attribute_path(path: &ast::Path) -> String {
    let rooted = path.syntax().first_token().is_some_and(|token| token.text() == "::");
    let mut segments = Vec::new();
    let mut current = Some(path.clone());
    while let Some(path) = current {
        let segment = path.segment().expect("a successfully parsed Rust path always has a segment");
        let name = segment
            .name_ref()
            .expect("a successfully parsed Rust path segment always has a name");
        segments.push(name.text().trim_start_matches("r#").to_owned());
        current = path.qualifier();
    }
    segments.reverse();
    let normalized = segments.join("::");
    if rooted { format!("::{normalized}") } else { normalized }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CfgPossibilities {
    can_be_true: bool,
    can_be_false: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
/// A cfg condition before non-test atoms are assigned Boolean indexes.
enum CfgFormula {
    Value(bool),
    Atom(String),
    All(Vec<Self>),
    Any(Vec<Self>),
    Not(Box<Self>),
}

impl CfgFormula {
    fn all(parts: impl IntoIterator<Item = Self>) -> Self {
        let mut flattened = Vec::new();
        for part in parts {
            match part {
                Self::Value(false) => return Self::Value(false),
                Self::Value(true) => {}
                Self::All(parts) => flattened.extend(parts),
                part => flattened.push(part),
            }
        }
        flattened.sort_unstable();
        flattened.dedup();
        match flattened.len() {
            0 => Self::Value(true),
            1 => flattened.pop().expect("guarded by flattened.len() == 1 above"),
            _ => Self::All(flattened),
        }
    }

    fn any(parts: impl IntoIterator<Item = Self>) -> Self {
        let mut flattened = Vec::new();
        for part in parts {
            match part {
                Self::Value(true) => return Self::Value(true),
                Self::Value(false) => {}
                Self::Any(parts) => flattened.extend(parts),
                part => flattened.push(part),
            }
        }
        flattened.sort_unstable();
        flattened.dedup();
        match flattened.len() {
            0 => Self::Value(false),
            1 => flattened.pop().expect("guarded by flattened.len() == 1 above"),
            _ => Self::Any(flattened),
        }
    }

    fn not(part: Self) -> Self {
        match part {
            Self::Value(value) => Self::Value(!value),
            Self::Not(part) => *part,
            part => Self::Not(Box::new(part)),
        }
    }
}

#[derive(Debug)]
/// A compiled cfg formula whose atoms index directly into one assignment.
enum CfgExpression {
    Value(bool),
    Atom(usize),
    All(Vec<Self>),
    Any(Vec<Self>),
    Not(Box<Self>),
}

impl CfgExpression {
    fn evaluate(&self, assignment: &[bool]) -> bool {
        match self {
            Self::Value(value) => *value,
            Self::Atom(index) => assignment[*index],
            Self::All(parts) => parts.iter().all(|part| part.evaluate(assignment)),
            Self::Any(parts) => parts.iter().any(|part| part.evaluate(assignment)),
            Self::Not(part) => !part.evaluate(assignment),
        }
    }
}

impl CfgPossibilities {
    const TRUE: Self = Self {
        can_be_true: true,
        can_be_false: false,
    };
    const FALSE: Self = Self {
        can_be_true: false,
        can_be_false: true,
    };
    const EITHER: Self = Self {
        can_be_true: true,
        can_be_false: true,
    };

    fn not(self) -> Self {
        Self {
            can_be_true: self.can_be_false,
            can_be_false: self.can_be_true,
        }
    }
}

fn extend_cfg_guard(
    inherited_guard: &CfgFormula,
    inherited_possibilities: CfgPossibilities,
    own_guard: CfgFormula,
) -> (CfgFormula, CfgPossibilities) {
    if
    // #[gamma::skip(cond.always_false, reason = "This only avoids recomputing the same pure guard possibilities; either path returns identical evidence.", tag = "perf")]
    own_guard == CfgFormula::Value(true) {
        return (inherited_guard.clone(), inherited_possibilities);
    }
    let guard = CfgFormula::all(
        // #[gamma::skip(collection.reverse_array, reason = "CfgFormula canonicalizes operand order, so reversing this conjunction or disjunction changes nothing.", tag = "equivalent")]
        [inherited_guard.clone(), own_guard],
    );
    let possibilities = if
    // #[gamma::skip(cond.always_false, reason = "This only avoids recomputing the same pure guard possibilities; either path returns identical evidence.", tag = "perf")]
    guard == *inherited_guard {
        inherited_possibilities
    } else {
        cfg_formula_possibilities_without_test(&guard)
    };
    (guard, possibilities)
}

#[cfg(test)]
fn cfg_possibilities_without_test(predicate: &ast::CfgPredicate) -> CfgPossibilities {
    cfg_formula_from_predicate(predicate).map_or(CfgPossibilities::EITHER, |formula| cfg_formula_possibilities_without_test(&formula))
}

fn cfg_formula_possibilities_without_test(formula: &CfgFormula) -> CfgPossibilities {
    const MAX_EXACT_CFG_ATOMS: usize = 12;

    let mut atom_indexes = BTreeMap::new();

    // #[gamma::skip(call_result.default, reason = "This call returns unit; default_after preserves the call and its side effects, then returns the identical unit value.", tag = "equivalent")]
    collect_cfg_atoms(formula, &mut atom_indexes);
    if atom_indexes.len() > MAX_EXACT_CFG_ATOMS {
        return approximate_cfg_possibilities_without_test(formula);
    }
    for (index, atom) in atom_indexes.keys().cloned().collect::<Vec<_>>().into_iter().enumerate() {
        let _ = atom_indexes.insert(atom, index);
    }
    let expression = compile_cfg_expression(formula, &atom_indexes);

    let mut possibilities = CfgPossibilities {
        can_be_true: false,
        can_be_false: false,
    };
    let mut assignment = vec![false; atom_indexes.len()];
    let assignment_count =
    // #[gamma::skip(literal.int_increment, reason = "All binary assignments are already covered; additional iterations only revisit assignments.", tag = "equivalent")]
    2_usize.pow(u32::try_from(atom_indexes.len()).expect("exact cfg atom limit fits u32"));
    for _ in
        // #[gamma::skip(range.exclusive_to_inclusive, reason = "All binary assignments are already covered; additional iterations only revisit assignments.", tag = "equivalent")]
        0..assignment_count
    {
        let value = expression.evaluate(&assignment);
        possibilities.can_be_true |= value;
        possibilities.can_be_false |= !value;
        if
        // #[gamma::skip(cond.always_false, reason = "EITHER is absorbing; scanning additional assignments cannot add another possibility.", tag = "perf")]
        possibilities == CfgPossibilities::EITHER {
            // #[gamma::skip(loop.break_to_continue, reason = "EITHER is absorbing; scanning additional assignments cannot add another possibility.", tag = "perf")]

            // #[gamma::skip(loop.delete_break, reason = "EITHER is absorbing; scanning additional assignments cannot add another possibility.", tag = "perf")]
            break;
        }

        // #[gamma::skip(call_result.default, reason = "This call returns unit; default_after preserves the call and its side effects, then returns the identical unit value.", tag = "equivalent")]
        advance_cfg_assignment(&mut assignment);
    }
    possibilities
}

fn advance_cfg_assignment(assignment: &mut [bool]) {
    for value in assignment {
        *value = !*value;
        if *value {
            break;
        }
    }
}

fn collect_cfg_atoms(formula: &CfgFormula, atoms: &mut BTreeMap<String, usize>) {
    match formula {
        CfgFormula::Value(_) => {}
        CfgFormula::Atom(atom) => {
            let _ = atoms.entry(atom.clone()).or_default();
        }
        CfgFormula::All(parts) | CfgFormula::Any(parts) => {
            for part in parts {
                // #[gamma::skip(call_result.default, reason = "This call returns unit; default_after preserves the call and its side effects, then returns the identical unit value.", tag = "equivalent")]
                collect_cfg_atoms(part, atoms);
            }
        }
        CfgFormula::Not(part) => {
            // #[gamma::skip(call_result.default, reason = "This call returns unit; default_after preserves the call and its side effects, then returns the identical unit value.", tag = "equivalent")]
            collect_cfg_atoms(part, atoms);
        }
    }
}

fn cfg_atom_key(atom: &ast::CfgAtom) -> Option<String> {
    let ident = atom.ident_token()?.text().trim_start_matches("r#").to_owned();
    if atom.eq_token().is_some() {
        let token = atom.string_token()?;
        let string = ast::String::cast(token).expect("string_token returns only STRING tokens, which ast::String::cast accepts");
        let value = string.value().ok()?;
        Some(format!("{ident}={value}"))
    } else {
        Some(ident)
    }
}

fn cfg_formula_from_predicate(predicate: &ast::CfgPredicate) -> Option<CfgFormula> {
    match predicate {
        ast::CfgPredicate::CfgAtom(atom) => {
            if atom.true_token().is_some() {
                return Some(CfgFormula::Value(true));
            }
            if atom.false_token().is_some() {
                return Some(CfgFormula::Value(false));
            }
            if atom.eq_token().is_none()
                && atom
                    .ident_token()
                    .is_some_and(|token| token.text().trim_start_matches("r#") == "test")
            {
                return Some(CfgFormula::Value(false));
            }
            Some(CfgFormula::Atom(cfg_atom_key(atom)?))
        }
        ast::CfgPredicate::CfgComposite(composite) => {
            let keyword =
            // #[gamma::skip(try.propagate_to_unwrap, reason = "The parser creates a composite only after consuming its all/any/not keyword.", tag = "equivalent")]
            composite.keyword()?.text().to_owned();
            let parts = composite
                .cfg_predicates()
                .map(|part| cfg_formula_from_predicate(&part))
                .collect::<Option<Vec<_>>>()?;
            match keyword.as_str() {
                "all" => Some(CfgFormula::all(parts)),
                "any" => Some(CfgFormula::any(parts)),
                "not" if parts.len() == 1 => Some(CfgFormula::not(
                    parts.into_iter().next().expect("guarded by parts.len() == 1 above"),
                )),
                _ => None,
            }
        }
    }
}

fn cfg_formula_from_predicate_or_unknown(predicate: &ast::CfgPredicate) -> CfgFormula {
    cfg_formula_from_predicate(predicate).unwrap_or_else(|| CfgFormula::Atom(format!("?{}", predicate.syntax().text())))
}

fn compile_cfg_expression(formula: &CfgFormula, atom_indexes: &BTreeMap<String, usize>) -> CfgExpression {
    match formula {
        CfgFormula::Value(value) => CfgExpression::Value(*value),
        CfgFormula::Atom(atom) => CfgExpression::Atom(atom_indexes[atom]),
        CfgFormula::All(parts) => CfgExpression::All(parts.iter().map(|part| compile_cfg_expression(part, atom_indexes)).collect()),
        CfgFormula::Any(parts) => CfgExpression::Any(parts.iter().map(|part| compile_cfg_expression(part, atom_indexes)).collect()),
        CfgFormula::Not(part) => CfgExpression::Not(Box::new(compile_cfg_expression(part, atom_indexes))),
    }
}

/// Conservative fallback for formulas with too many distinct atoms.
fn approximate_cfg_possibilities_without_test(formula: &CfgFormula) -> CfgPossibilities {
    match formula {
        CfgFormula::Value(true) => CfgPossibilities::TRUE,
        CfgFormula::Value(false) => CfgPossibilities::FALSE,
        CfgFormula::Atom(_) => CfgPossibilities::EITHER,
        CfgFormula::All(parts) => {
            let parts = parts.iter().map(approximate_cfg_possibilities_without_test).collect::<Vec<_>>();
            CfgPossibilities {
                can_be_true: parts.iter().all(|part| part.can_be_true),
                can_be_false: parts.iter().any(|part| part.can_be_false),
            }
        }
        CfgFormula::Any(parts) => {
            let parts = parts.iter().map(approximate_cfg_possibilities_without_test).collect::<Vec<_>>();
            CfgPossibilities {
                can_be_true: parts.iter().any(|part| part.can_be_true),
                can_be_false: parts.iter().all(|part| part.can_be_false),
            }
        }
        CfgFormula::Not(part) => approximate_cfg_possibilities_without_test(part).not(),
    }
}

fn source_has_opaque_source_macro(
    source: &ast::SourceFile,
    surface: Surface,
    inherited_guard: &CfgFormula,
    test_attributes: &BTreeSet<String>,
) -> bool {
    if surface == Surface::NonReleased {
        return false;
    }
    let (source_guard, source_possibilities) = extend_cfg_guard(
        inherited_guard,
        cfg_formula_possibilities_without_test(inherited_guard),
        attributes_guard(ast::attrs_including_inner(source), test_attributes),
    );
    syntax_contains_opaque_source_macro(source.syntax(), test_attributes, &source_guard, source_possibilities)
}

fn syntax_contains_opaque_source_macro(
    node: &SyntaxNode,
    test_attributes: &BTreeSet<String>,
    inherited_guard: &CfgFormula,
    inherited_possibilities: CfgPossibilities,
) -> bool {
    node.children().any(|child| {
        let own_guard = syntax_cfg_guard(&child, test_attributes);
        let (guard, possibilities) = extend_cfg_guard(inherited_guard, inherited_possibilities, own_guard);
        let test_only = !possibilities.can_be_true;
        (!test_only && ast::MacroCall::can_cast(child.kind()))
            || syntax_contains_opaque_source_macro(&child, test_attributes, &guard, possibilities)
    })
}

fn path_is_builtin_include(path: &ast::Path) -> bool {
    let normalized = normalize_attribute_path(path);
    let normalized = normalized.strip_prefix("::").unwrap_or(&normalized);
    normalized == "include"
        || normalized.strip_suffix("::include").is_some_and(|qualifier| {
            qualifier == "std" || qualifier.starts_with("std::") || qualifier == "core" || qualifier.starts_with("core::")
        })
}

fn static_include_path(macro_call: &ast::MacroCall) -> Option<PathBuf> {
    let token_tree = macro_call.token_tree()?;
    let mut tokens = token_tree
        .syntax()
        .children_with_tokens()
        .filter_map(ra_ap_syntax::NodeOrToken::into_token)
        .filter(|token| {
            !matches!(token.kind(), SyntaxKind::WHITESPACE | SyntaxKind::COMMENT)
                && !matches!(token.text(), "(" | ")" | "{" | "}" | "[" | "]")
        });
    let string = ast::String::cast(tokens.next()?)?;
    if let Some(token) = tokens.next()
        && (token.text() != "," || tokens.next().is_some())
    {
        return None;
    }
    string.value().ok().map(|value| PathBuf::from(value.as_ref()))
}

fn module_declarations(
    path: &Path,
    source: &ast::SourceFile,
    role: SourceRole,
    inherited_guard: &CfgFormula,
    test_attributes: &BTreeSet<String>,
    is_file: &mut impl FnMut(&Path) -> Result<bool, CoverageGateError>,
) -> Result<Vec<ModuleDeclaration>, CoverageGateError> {
    let Some(directory) = owned_directory(path, role) else {
        return Ok(Vec::new());
    };
    let base = path
        .parent()
        .expect("owned_directory returns Some only after confirming that the declaring path has a parent");
    let mut declarations = Vec::new();
    let source_guard = CfgFormula::all(
        // #[gamma::skip(collection.reverse_array, reason = "CfgFormula canonicalizes operand order, so reversing this conjunction or disjunction changes nothing.", tag = "equivalent")]
        [
            inherited_guard.clone(),
            attributes_guard(ast::attrs_including_inner(source), test_attributes),
        ],
    );
    ModuleWalker {
        declaring_file: path,
        test_attributes,
        declarations: &mut declarations,
        is_file,
    }
    .walk_syntax(source.syntax(), &directory, base, &source_guard)?;
    Ok(declarations)
}

fn owned_directory(path: &Path, role: SourceRole) -> Option<PathBuf> {
    let parent = path.parent()?;
    match role {
        SourceRole::CrateRoot | SourceRole::PathModule => Some(parent.to_path_buf()),
        SourceRole::PlainModule => {
            let stem = path.file_stem()?.to_string_lossy();
            if stem == "mod" {
                Some(parent.to_path_buf())
            } else {
                Some(parent.join(&*stem))
            }
        }
        SourceRole::IncludedSource => None,
    }
}

struct ModuleWalker<'a, F> {
    declaring_file: &'a Path,
    test_attributes: &'a BTreeSet<String>,
    declarations: &'a mut Vec<ModuleDeclaration>,
    is_file: &'a mut F,
}

impl<F: FnMut(&Path) -> Result<bool, CoverageGateError>> ModuleWalker<'_, F> {
    fn walk_syntax(
        &mut self,
        node: &SyntaxNode,
        directory: &Path,
        base: &Path,
        inherited_guard: &CfgFormula,
    ) -> Result<(), CoverageGateError> {
        for child in node.children() {
            let own_guard = syntax_cfg_guard(&child, self.test_attributes);
            // #[gamma::skip(collection.reverse_array, reason = "CfgFormula canonicalizes conjunction operand order")]
            let guard = CfgFormula::all([inherited_guard.clone(), own_guard]);
            if let Some(macro_call) = ast::MacroCall::cast(child.clone())
                && macro_call.path().is_some_and(|path| path_is_builtin_include(&path))
                && let Some(relative) = static_include_path(&macro_call)
            {
                let base = self
                    .declaring_file
                    .parent()
                    .expect("resolved source files always have a parent directory");
                self.declarations.push(ModuleDeclaration {
                    path: base.join(relative),
                    role: SourceRole::IncludedSource,
                    guard: guard.clone(),
                });
            }
            if let Some(module) = ast::Module::cast(child.clone()) {
                self.walk_module(&module, directory, base, &guard)?;
            } else {
                self.walk_syntax(&child, directory, base, &guard)?;
            }
        }
        Ok(())
    }

    fn walk_module(&mut self, module: &ast::Module, directory: &Path, base: &Path, guard: &CfgFormula) -> Result<(), CoverageGateError> {
        let name = module
            .name()
            .expect("a parsed module declaration always has a name")
            .text()
            .trim_start_matches("r#")
            .to_owned();
        let path_override = module_path_attribute(module, self.declaring_file, &name)?;

        if let Some(item_list) = module.item_list() {
            let nested = path_override.map_or_else(|| directory.join(&name), |relative| base.join(relative));
            self.walk_syntax(item_list.syntax(), &nested, &nested, guard)?;
            return Ok(());
        }

        let (candidates, role) = path_override.map_or_else(
            || {
                (
                    vec![directory.join(format!("{name}.rs")), directory.join(&name).join("mod.rs")],
                    SourceRole::PlainModule,
                )
            },
            |relative| (vec![base.join(relative)], SourceRole::PathModule),
        );
        let mut existing = Vec::new();
        for candidate in candidates {
            if (self.is_file)(&candidate)? {
                existing.push(candidate);
            }
        }
        if existing.len() > 1 {
            return Err(AmbiguousModuleSourceError::new(
                diagnostic_path(self.declaring_file),
                name,
                diagnostic_path(&existing[0]),
                diagnostic_path(&existing[1]),
            )
            .into());
        }
        if let Some(path) = existing.into_iter().next() {
            self.declarations.push(ModuleDeclaration {
                path,
                role,
                guard: guard.clone(),
            });
        }
        Ok(())
    }
}

fn module_path_attribute(module: &ast::Module, declaring_file: &Path, module_name: &str) -> Result<Option<PathBuf>, CoverageGateError> {
    let mut found = None;
    for (attribute, attribute_meta) in module
        .attrs()
        .filter_map(|attribute| attribute.meta().map(|meta| (attribute, meta)))
    {
        if matches!(attribute_meta, ast::Meta::CfgAttrMeta(_)) && meta_contains_path(&attribute_meta) {
            return Err(ConditionalModulePathError::new(
                diagnostic_path(declaring_file),
                module_name.to_owned(),
                crate::package_glob::diagnostic(&attribute.syntax().text().to_string()),
            )
            .into());
        }
        if let ast::Meta::KeyValueMeta(meta) = attribute_meta
            && meta.path().is_some_and(|path| normalize_attribute_path(&path) == "path")
        {
            let value = match meta.expr() {
                Some(ast::Expr::Literal(literal)) => match literal.kind() {
                    LiteralKind::String(text) => text.value().ok().map(|value| PathBuf::from(value.as_ref())),
                    _ => None,
                },
                _ => None,
            };
            let Some(value) = value else {
                return Err(InvalidModulePathAttributeError::new(
                    diagnostic_path(declaring_file),
                    module_name.to_owned(),
                    crate::package_glob::diagnostic(&attribute.syntax().text().to_string()),
                )
                .into());
            };
            if found.replace(value).is_some() {
                return Err(InvalidModulePathAttributeError::new(
                    diagnostic_path(declaring_file),
                    module_name.to_owned(),
                    "multiple #[path] attributes".to_owned(),
                )
                .into());
            }
        }
    }
    Ok(found)
}

fn meta_contains_path(meta: &ast::Meta) -> bool {
    match meta {
        ast::Meta::KeyValueMeta(meta) => meta.path().is_some_and(|path| normalize_attribute_path(&path) == "path"),
        ast::Meta::CfgAttrMeta(meta) => meta.metas().any(|inner| meta_contains_path(&inner)),
        ast::Meta::CfgMeta(_) | ast::Meta::PathMeta(_) | ast::Meta::TokenTreeMeta(_) | ast::Meta::UnsafeMeta(_) => false,
    }
}

enum SourceDisposition<'a> {
    CompilerLibrary,
    Exclude {
        line_count: usize,
    },
    Include {
        source_file: PathBuf,
        line_count: usize,
        test_only_lines: &'a BTreeSet<u32>,
    },
}

fn filter_report(
    report: lcov::Report,
    sources: &SourceIndex,
    provenance_witness: Option<&Path>,
    is_compiler_source: &mut impl FnMut(&Path) -> Result<bool, CoverageGateError>,
) -> Result<String, CoverageGateError> {
    filter_report_with(report, provenance_witness, |path| {
        let (canonical, source) = sources.source_for_lcov(path)?;
        let Some(source) = source else {
            if attributed_path(&sources.member_dirs, &canonical).is_none() && is_compiler_source(&canonical)? {
                return Ok(SourceDisposition::CompilerLibrary);
            }
            return Err(UnownedCoverageSourceError::new(diagnostic_path(path)).into());
        };
        if !sources.uncertain_sources.is_empty() {
            return Ok(SourceDisposition::Include {
                source_file: sources.retained_source_path(&canonical, source)?,
                line_count: source.analysis.line_count,
                test_only_lines: &source.analysis.test_only_lines,
            });
        }
        let Some(source_file) = &source.released_path else {
            return Ok(SourceDisposition::Exclude {
                line_count: source.analysis.line_count,
            });
        };
        Ok(SourceDisposition::Include {
            source_file: source_file.clone(),
            line_count: source.analysis.line_count,
            test_only_lines: &source.test_only_lines,
        })
    })
}

fn filter_report_with<'a>(
    mut report: lcov::Report,
    provenance_witness: Option<&Path>,
    mut classify: impl FnMut(&Path) -> Result<SourceDisposition<'a>, CoverageGateError>,
) -> Result<String, CoverageGateError> {
    let mut filtered = lcov::report::section::Sections::new();
    for (mut key, mut section) in report.sections {
        let (source_file, line_count, test_only) = match classify(&key.source_file)? {
            SourceDisposition::CompilerLibrary => continue,
            SourceDisposition::Exclude { line_count } => {
                validate_section_lines(&key.source_file, &section, line_count)?;
                continue;
            }
            SourceDisposition::Include {
                source_file,
                line_count,
                test_only_lines,
            } => (source_file, line_count, test_only_lines),
        };
        validate_section_lines(&key.source_file, &section, line_count)?;
        key.source_file = source_file;
        section.lines.retain(|line, _| !test_only.contains(&line.line));
        section.branches.clear();
        section.functions.clear();
        if !section.is_empty() {
            match filtered.entry(key) {
                Entry::Vacant(entry) => {
                    let _ = entry.insert(section);
                }
                Entry::Occupied(mut entry) => {
                    let test_name = crate::package_glob::diagnostic(&entry.key().test_name);
                    let path = diagnostic_path(&entry.key().source_file);
                    for (line, incoming) in section.lines {
                        match entry.get_mut().lines.entry(line) {
                            Entry::Vacant(entry) => _ = entry.insert(incoming),
                            Entry::Occupied(mut entry) => {
                                let existing = entry.get_mut();
                                if existing
                                    .checksum
                                    .as_ref()
                                    .zip(incoming.checksum.as_ref())
                                    .is_some_and(|(left, right)| left != right)
                                {
                                    return Err(NormalizedSourceChecksumError::new(test_name, path, line.line).into());
                                }
                                if incoming.checksum.is_some() {
                                    existing.checksum = incoming.checksum;
                                }
                                existing.count = existing.count.saturating_add(incoming.count);
                            }
                        }
                    }
                }
            }
        }
    }
    if let Some(source_file) = provenance_witness
        && !filtered.keys().any(|key| key.source_file == source_file)
    {
        let _ = filtered.insert(
            lcov::report::section::Key {
                test_name: String::new(),
                source_file: source_file.to_path_buf(),
            },
            lcov::report::section::Value::default(),
        );
    }
    report.sections = filtered;

    let mut output = String::new();
    for record in report.into_records() {
        writeln!(&mut output, "{record}").expect("writing LCOV records to a String cannot fail");
    }
    Ok(output)
}

fn validate_section_lines(path: &Path, section: &lcov::report::section::Value, line_count: usize) -> Result<(), CoverageGateError> {
    for line in section.lines.keys().map(|line| line.line) {
        validate_line(path, line, line_count)?;
    }
    Ok(())
}

fn validate_line(path: &Path, line: u32, line_count: usize) -> Result<(), CoverageGateError> {
    let valid = line != 0 && usize::try_from(line).is_ok_and(|line| line <= line_count);
    if valid {
        Ok(())
    } else {
        Err(CoverageLineOutOfRangeError::new(diagnostic_path(path), line, line_count).into())
    }
}

pub(crate) fn filter_reports_with_metadata(reports: Vec<lcov::Report>, metadata: &Metadata) -> Result<Vec<String>, CoverageGateError> {
    filter_reports_with_compiler_loader(reports, metadata, discover_compiler_library)
}

fn filter_reports_with_compiler_loader(
    reports: Vec<lcov::Report>,
    metadata: &Metadata,
    load_compiler_library: impl FnOnce(&Path) -> Result<Option<PathBuf>, CoverageGateError>,
) -> Result<Vec<String>, CoverageGateError> {
    if reports.is_empty() {
        configured_test_attributes(metadata)?;
        return Ok(Vec::new());
    }
    let mut sources = SourceIndex::from_metadata(metadata)?;
    sources.index_reported_member_sources(&reports, metadata)?;
    // A measured graph-gap source must survive even when all its counters are test-only.
    let provenance_witness = sources
        .files
        .iter()
        .find(|(_, source)| source.visit_contexts.is_empty())
        .map(|(canonical, source)| sources.retained_source_path(canonical, source))
        .transpose()?;
    if let Some(warning) = sources.conservative_warning() {
        eprintln!("{warning}");
    }
    let mut compiler_library: Option<Option<PathBuf>> = None;
    let mut load_compiler_library = Some(load_compiler_library);
    let mut is_compiler_source = |canonical: &Path| {
        if compiler_library.is_none() {
            let load = load_compiler_library
                .take()
                .expect("compiler library discovery is attempted only once per report batch");
            compiler_library = Some(load(metadata.workspace_root.as_std_path())?);
        }
        Ok(compiler_library
            .as_ref()
            .and_then(Option::as_ref)
            .is_some_and(|root| canonical.starts_with(root)))
    };
    reports
        .into_iter()
        .map(|report| filter_report(report, &sources, provenance_witness.as_deref(), &mut is_compiler_source))
        .collect()
}

// Only the real process boundary is excluded; command construction and output
// validation are exercised through the injected helpers below.
#[cfg_attr(coverage_nightly, coverage(off))]
#[mutants::skip]
fn discover_compiler_library(workspace_root: &Path) -> Result<Option<PathBuf>, CoverageGateError> {
    compiler_library_with(
        workspace_root,
        |name| env::var_os(name),
        |command| {
            let operation = format!("{} --print sysroot", diagnostic_path(Path::new(command.get_program())));
            let output = command.output().map_err(|error| {
                CompilerSourceDiscoveryError::caused_by(operation.clone(), "compiler query could not be executed".to_owned(), error)
            })?;
            compiler_sysroot_output(
                &operation,
                output.status.success(),
                &output.status.to_string(),
                &output.stdout,
                &output.stderr,
            )
        },
        canonical_compiler_library,
    )
}

fn compiler_library_with(
    workspace_root: &Path,
    var_os: impl FnOnce(&str) -> Option<OsString>,
    read: impl FnOnce(&mut Command) -> Result<PathBuf, CoverageGateError>,
    resolve: impl FnOnce(&Path) -> Result<Option<PathBuf>, CoverageGateError>,
) -> Result<Option<PathBuf>, CoverageGateError> {
    let rustc = var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
    let mut command = Command::new(rustc);
    command.current_dir(workspace_root).args(["--print", "sysroot"]);
    let sysroot = read(&mut command)?;
    resolve(&sysroot.join("lib").join("rustlib").join("src").join("rust").join("library"))
}

fn compiler_sysroot_output(
    operation: &str,
    success: bool,
    status: &str,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<PathBuf, CoverageGateError> {
    if !success {
        return Err(CompilerSourceDiscoveryError::new(
            operation.to_owned(),
            format!(
                "compiler exited with {status}: {}",
                crate::package_glob::diagnostic(String::from_utf8_lossy(stderr).trim())
            ),
        )
        .into());
    }
    let output = String::from_utf8(stdout.to_vec())
        .map_err(|error| CompilerSourceDiscoveryError::caused_by(operation.to_owned(), "sysroot output is not UTF-8".to_owned(), error))?;
    let path = Path::new(output.trim());
    if output.lines().count() != 1 || !path.is_absolute() {
        return Err(CompilerSourceDiscoveryError::new(
            operation.to_owned(),
            "expected one absolute sysroot path; check the selected RUSTC compiler or proxy".to_owned(),
        )
        .into());
    }
    Ok(path.to_path_buf())
}

fn canonical_compiler_library(path: &Path) -> Result<Option<PathBuf>, CoverageGateError> {
    canonical_compiler_library_with(
        path,
        |path| fs::canonicalize(path),
        |path| fs::metadata(path).map(|metadata| metadata.is_dir()),
    )
}

fn canonical_compiler_library_with(
    path: &Path,
    canonicalize: impl FnOnce(&Path) -> std::io::Result<PathBuf>,
    is_directory: impl FnOnce(&Path) -> std::io::Result<bool>,
) -> Result<Option<PathBuf>, CoverageGateError> {
    let canonical = match canonicalize(path) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ResolveCoverageSourceError::caused_by(diagnostic_path(path), error).into()),
    };
    let is_directory =
        is_directory(&canonical).map_err(|error| InspectCoverageSourceError::caused_by(diagnostic_path(&canonical), error))?;
    if !is_directory {
        return Err(CompilerSourceDiscoveryError::new(
            diagnostic_path(path),
            "the compiler library-source root must be a directory".to_owned(),
        )
        .into());
    }
    Ok(Some(canonical))
}

pub(crate) fn parse_reports(inputs: &[&str]) -> Result<Vec<lcov::Report>, CoverageGateError> {
    inputs.iter().map(|input| parse_report(input)).collect()
}

fn parse_report(input: &str) -> Result<lcov::Report, CoverageGateError> {
    let mut source_sections = BTreeSet::new();
    let mut test_name = String::new();
    let mut source_file = None;
    let reader = lcov::Reader::new(input.as_bytes()).inspect(|record| {
        if let Ok(record) = record {
            match record {
                lcov::Record::TestName { name } => test_name.clone_from(name),
                lcov::Record::SourceFile { path } => source_file = Some(path.clone()),
                lcov::Record::EndOfRecord => {
                    let section_test_name = std::mem::take(&mut test_name);
                    if let Some(source_file) = source_file.take() {
                        let _ = source_sections.insert(lcov::report::section::Key {
                            test_name: section_test_name,
                            source_file,
                        });
                    }
                }
                _ => {}
            }
        }
    });
    let mut report = lcov::Report::from_reader(reader).map_err(|error| CoverageGateError::from(ParseLcovError::from(error)))?;
    // The LCOV parser discards zero-line sections; filtering needs their source-graph provenance.
    for key in source_sections {
        let _ = report.sections.entry(key).or_default();
    }
    Ok(report)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::fmt::Write as _;

    use ra_ap_syntax::ast::HasModuleItem;
    use serde_json::json;

    use super::*;

    const RELEASED_AND_TEST_ONLY_SOURCE: &str = include_str!("../tests/fixtures/released-and-test-only-source.rs");

    struct FixtureSource {
        released: bool,
        analysis: SourceAnalysis,
    }

    fn lexical_normalize(path: &Path) -> PathBuf {
        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    let _ = normalized.pop();
                }
                _ => normalized.push(component.as_os_str()),
            }
        }
        normalized
    }

    fn lcov_section(path: &Path, lines: &[(u32, u64)]) -> String {
        let mut output = format!("TN:\nSF:{}\n", path.display());
        for (line, count) in lines {
            writeln!(&mut output, "DA:{line},{count}").expect("write to String");
        }
        output.push_str("end_of_record\n");
        output
    }

    fn analyze(path: &Path, source: &str) -> SourceAnalysis {
        analyze_source_text(path, source, Edition::Edition2024, &BTreeSet::new()).expect("fixture source parses")
    }

    fn source_index_fixture_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/source-index")
    }

    fn source_index_fixture_metadata(root: &Path) -> Metadata {
        let member = root.join("member");
        let package_id = "path+file:///coverage-source-index#member@0.1.0";
        serde_json::from_value(json!({
            "packages": [{
                "name": "member",
                "version": "0.1.0",
                "authors": [],
                "id": package_id,
                "source": null,
                "description": null,
                "dependencies": [],
                "license": null,
                "license_file": null,
                "targets": [
                    {
                        "kind": ["lib"],
                        "crate_types": ["lib"],
                        "name": "member",
                        "src_path": member.join("src/lib.rs"),
                        "edition": "2024",
                        "doc": true,
                        "doctest": true,
                        "test": true
                    },
                    {
                        "kind": ["test"],
                        "crate_types": ["bin"],
                        "name": "integration",
                        "src_path": member.join("tests/integration.rs"),
                        "edition": "2024",
                        "doc": false,
                        "doctest": false,
                        "test": true
                    }
                ],
                "features": {},
                "manifest_path": member.join("Cargo.toml"),
                "categories": [],
                "keywords": [],
                "readme": null,
                "repository": null,
                "homepage": null,
                "documentation": null,
                "edition": "2024",
                "metadata": {"coverage-gate": {"min-lines-percent": 0}},
                "links": null,
                "publish": null,
                "default_run": null,
                "rust_version": null
            }],
            "workspace_members": [package_id],
            "workspace_default_members": [package_id],
            "resolve": null,
            "workspace_root": root,
            "target_directory": root.join("target"),
            "build_directory": root.join("target"),
            "metadata": {},
            "version": 1
        }))
        .expect("fixture metadata parses")
    }

    fn declarations(
        path: &Path,
        analysis: &SourceAnalysis,
        role: SourceRole,
        existing: &BTreeSet<PathBuf>,
    ) -> Result<Vec<ModuleDeclaration>, CoverageGateError> {
        let mut is_file = |candidate: &Path| Ok(existing.contains(&lexical_normalize(candidate)));
        module_declarations(
            path,
            analysis.syntax(),
            role,
            &CfgFormula::Value(true),
            &BTreeSet::new(),
            &mut is_file,
        )
    }

    fn pending_source(surface: Surface, role: SourceRole, resolution_path: &Path, guard: CfgFormula) -> PendingSource {
        PendingSource {
            canonical_path: resolution_path.to_path_buf(),
            resolution_path: resolution_path.to_path_buf(),
            edition: Edition::Edition2024,
            surface,
            role,
            guard,
            owning_member: MemberDirectory {
                canonical: PathBuf::from("/workspace"),
                reported: PathBuf::from("/workspace"),
            },
        }
    }

    fn pending_source_for_member(
        surface: Surface,
        role: SourceRole,
        resolution_path: &Path,
        guard: CfgFormula,
        member: &Path,
    ) -> PendingSource {
        let mut pending = pending_source(surface, role, resolution_path, guard);
        pending.owning_member = MemberDirectory {
            canonical: fs::canonicalize(member).expect("canonical fixture member"),
            reported: member.to_path_buf(),
        };
        pending
    }

    fn declaration_is_test_only(declaration: &ModuleDeclaration) -> bool {
        !cfg_formula_possibilities_without_test(&declaration.guard).can_be_true
    }

    fn parse_lcov(input: &str) -> lcov::Report {
        parse_report(input).expect("fixture LCOV parses")
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn cfg_semantics_only_exclude_predicates_that_require_test() {
        let source = SourceFile::parse(
            "#[cfg(test)] fn a() {}\n\
             #[cfg(all(test, unix))] fn b() {}\n\
             #[cfg(any(test, feature = \"test-util\"))] fn c() {}\n\
             #[cfg(feature = \"test-util\")] fn d() {}\n\
             #[cfg(not(test))] fn e() {}\n\
             #[tokio::test] fn f() {}\n\
             #[test_case] fn g() {}\n\
             #[cfg(not(not(test)))] fn h() {}\n\
             #[cfg(any(test, false))] fn i() {}\n\
             #[cfg(any(test, true))] fn j() {}\n\
             #[cfg_attr(not(test), cfg(test))] fn k() {}\n\
             #[cfg_attr(test, cfg(unix))] fn l() {}\n\
             #[tokio::test(flavor = \"current_thread\")] fn m() {}\n",
            Edition::Edition2024,
        )
        .tree();
        let marks = source
            .items()
            .map(|item| item.attrs().any(|attribute| attribute_marks_test(&attribute, &BTreeSet::new())))
            .collect::<Vec<_>>();
        assert_eq!(
            marks,
            [true, true, false, false, false, false, false, true, true, false, true, false, false,]
        );

        let configured = BTreeSet::from(["tokio::test".to_owned()]);
        let marks = source
            .items()
            .map(|item| item.attrs().any(|attribute| attribute_marks_test(&attribute, &configured)))
            .collect::<Vec<_>>();
        assert!(marks[5]);
        assert!(marks[12]);
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn configured_test_attributes_are_exact_workspace_paths() {
        assert!(parse_configured_test_attributes(None).expect("absent policy is valid").is_empty());
        assert_eq!(
            parse_configured_test_attributes(Some(&json!(["tokio::test", "::company::async_test"]))).expect("valid paths parse"),
            BTreeSet::from(["::company::async_test".to_owned(), "tokio::test".to_owned()])
        );

        for invalid in [
            json!("tokio::test"),
            json!([7]),
            json!(["tokio::test()"]),
            json!(["my_runtime::test] #[other"]),
        ] {
            assert!(parse_configured_test_attributes(Some(&invalid)).is_err(), "{invalid}");
        }
        assert!(parse_configured_test_attribute("[").is_none());
        let controls = parse_configured_test_attributes(Some(&json!(["bad\n\r\u{1b}[2J"])))
            .expect_err("invalid controlled path must fail")
            .to_string();
        assert!(controls.contains(r"bad\n\r\u{1b}[2J"), "{controls:?}");
        assert!(!controls.contains('\n'), "{controls:?}");
        assert!(!controls.contains('\r'), "{controls:?}");
        assert!(!controls.contains('\u{1b}'), "{controls:?}");

        let package_metadata = json!({"coverage-gate": {"test-attributes": ["company::test"]}});
        let error = reject_package_test_attributes([("alpha", &package_metadata)])
            .expect_err("package policy must be rejected")
            .to_string();
        assert!(error.contains("workspace-scoped"), "{error}");

        let root = source_index_fixture_root();
        let mut metadata = source_index_fixture_metadata(&root);
        metadata.workspace_metadata = json!({"coverage-gate": {"test-attributes": "tokio::test"}});
        let error = crate::evaluate_reports_with_metadata(Vec::new(), &metadata, &[], None)
            .expect_err("empty evaluation must validate workspace filtering policy")
            .to_string();
        assert!(error.contains("array of Rust attribute paths"), "{error}");

        let target_metadata = json!({
            "coverage-gate": {
                "target": {
                    "cfg(unix)": {
                        "min-lines-percent": 100,
                        "test-attributes": ["company::test"]
                    }
                }
            }
        });
        assert!(reject_package_test_attributes([("alpha", &target_metadata)]).is_err());
        assert!(package_test_attributes_value(&json!({"coverage-gate": {"target": "not-an-object"}})).is_none());

        let source = SourceFile::parse(
            "#[company::test] fn named_test() {}\n#[tokio::test] async fn configured_test() {}\n",
            Edition::Edition2024,
        )
        .tree();
        let default_marks = source
            .items()
            .map(|item| item.attrs().any(|attribute| attribute_marks_test(&attribute, &BTreeSet::new())))
            .collect::<Vec<_>>();
        assert_eq!(default_marks, [false, false]);

        let configured = BTreeSet::from(["tokio::test".to_owned()]);
        let configured_marks = source
            .items()
            .map(|item| item.attrs().any(|attribute| attribute_marks_test(&attribute, &configured)))
            .collect::<Vec<_>>();
        assert_eq!(configured_marks, [false, true]);

        let commented = SourceFile::parse(
            "#[company /* formatting trivia */ :: test] fn commented_test() {}\n",
            Edition::Edition2024,
        )
        .tree();
        let configured = BTreeSet::from(["company::test".to_owned()]);
        assert!(
            commented
                .items()
                .next()
                .expect("one item")
                .attrs()
                .any(|attribute| attribute_marks_test(&attribute, &configured))
        );

        let rooted = SourceFile::parse(
            "#[::company::async_test] fn rooted() {}\n\
             #[company::async_test] fn relative() {}\n",
            Edition::Edition2024,
        )
        .tree();
        let configured = BTreeSet::from(["::company::async_test".to_owned()]);
        assert_eq!(
            rooted
                .items()
                .map(|item| item.attrs().any(|attribute| attribute_marks_test(&attribute, &configured)))
                .collect::<Vec<_>>(),
            [true, false]
        );
    }

    #[test]
    fn released_sources_use_member_or_owning_target_attribution() {
        let member_dirs = MemberDirectories {
            reported_by_canonical: BTreeMap::from([(PathBuf::from("/workspace/member"), PathBuf::from("/workspace/member"))]),
        };
        let inside = PendingSource {
            canonical_path: PathBuf::from("/workspace/member/src/lib.rs"),
            resolution_path: PathBuf::from("/workspace/member/src/lib.rs"),
            edition: Edition::Edition2024,
            surface: Surface::Released,
            role: SourceRole::CrateRoot,
            guard: CfgFormula::Value(true),
            owning_member: MemberDirectory {
                canonical: PathBuf::from("/workspace/member"),
                reported: PathBuf::from("/workspace/member"),
            },
        };
        assert_eq!(
            released_source_attributed_path(&member_dirs, &inside).expect("member source is attributable"),
            Some(PathBuf::from("/workspace/member/src/lib.rs"))
        );

        let outside = PendingSource {
            canonical_path: PathBuf::from("/workspace/shared.rs"),
            resolution_path: PathBuf::from("/workspace/shared.rs"),
            edition: Edition::Edition2024,
            surface: Surface::Released,
            role: SourceRole::PathModule,
            guard: CfgFormula::Value(true),
            owning_member: MemberDirectory {
                canonical: PathBuf::from("/workspace/member"),
                reported: PathBuf::from("/workspace/member"),
            },
        };
        assert_eq!(
            released_source_attributed_path(&member_dirs, &outside).expect("external target source is attributable"),
            Some(PathBuf::from("/workspace/member/../shared.rs"))
        );

        let non_release = PendingSource {
            surface: Surface::NonReleased,
            ..outside
        };
        assert_eq!(
            released_source_attributed_path(&member_dirs, &non_release).expect("non-release sources need no package attribution"),
            None
        );
        assert_eq!(
            relative_path(Path::new("/workspace/member"), Path::new("/workspace/shared.rs")),
            Some(PathBuf::from("../shared.rs"))
        );
        assert_eq!(relative_path(Path::new(r"C:\workspace\member"), Path::new(r"D:\shared.rs")), None);
        assert_eq!(relative_path(Path::new("member"), Path::new("shared.rs")), None);
        assert_eq!(
            relative_path(Path::new("/workspace/../member"), Path::new("/workspace/shared.rs")),
            None
        );
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn visit_contexts_are_distinct_and_preserve_the_first_released_path() {
        let cases = [
            (Surface::Released, SourceRole::CrateRoot),
            (Surface::Released, SourceRole::PlainModule),
            (Surface::Released, SourceRole::PathModule),
            (Surface::Released, SourceRole::IncludedSource),
            (Surface::NonReleased, SourceRole::CrateRoot),
            (Surface::NonReleased, SourceRole::PlainModule),
            (Surface::NonReleased, SourceRole::PathModule),
            (Surface::NonReleased, SourceRole::IncludedSource),
        ];
        for (surface, role) in cases {
            let mut indexed = IndexedSource {
                analysis: SourceAnalysis {
                    syntax: None,
                    line_count: 0,
                    line_starts: vec![0],
                    test_only_lines: BTreeSet::new(),
                },
                edition: Edition::Edition2024,
                released_path: None,
                test_only_lines: BTreeSet::new(),
                visit_contexts: BTreeSet::new(),
            };
            let first_context = Path::new("/workspace/first.rs");
            let first = pending_source(surface, role, first_context, CfgFormula::Value(true));
            assert!(!indexed.has_visit(&first));
            let first_path = (surface == Surface::Released).then(|| PathBuf::from("first.rs"));
            indexed.add_visit(&first, first_path.clone()).expect("first visit is valid");
            assert!(indexed.has_visit(&first));
            let second = pending_source(surface, role, Path::new("/workspace/second.rs"), CfgFormula::Value(true));
            assert!(!indexed.has_visit(&second));
            assert_eq!(indexed.released_path, first_path);
            let second_path = first_path.clone();
            indexed.add_visit(&second, second_path).expect("second visit is valid");
            assert!(indexed.has_visit(&second));
            let guarded = pending_source(surface, role, first_context, CfgFormula::Value(false));
            assert!(!indexed.has_visit(&guarded));
            indexed.add_visit(&guarded, None).expect("guarded visit is valid");
            assert!(indexed.has_visit(&guarded));
            assert_eq!(
                indexed.released_path,
                (surface == Surface::Released).then(|| PathBuf::from("first.rs"))
            );
        }
    }

    #[test]
    fn conflicting_released_package_attribution_fails() {
        let mut indexed = IndexedSource {
            analysis: SourceAnalysis {
                syntax: None,
                line_count: 0,
                line_starts: vec![0],
                test_only_lines: BTreeSet::new(),
            },
            edition: Edition::Edition2024,
            released_path: None,
            test_only_lines: BTreeSet::new(),
            visit_contexts: BTreeSet::new(),
        };
        let mut first = pending_source(
            Surface::Released,
            SourceRole::CrateRoot,
            Path::new("/workspace/shared.rs"),
            CfgFormula::Value(true),
        );
        first.owning_member = MemberDirectory {
            canonical: PathBuf::from("/workspace/alpha"),
            reported: PathBuf::from("/workspace/alpha"),
        };
        indexed
            .add_visit(&first, Some(PathBuf::from("/workspace/alpha/../shared.rs")))
            .expect("first attribution is valid");

        let mut second = first.clone();
        second.owning_member = MemberDirectory {
            canonical: PathBuf::from("/workspace/beta"),
            reported: PathBuf::from("/workspace/beta"),
        };
        let error = indexed
            .add_visit(&second, Some(PathBuf::from("/workspace/beta/../shared.rs")))
            .expect_err("one canonical source cannot be attributed to two packages")
            .to_string();
        assert!(error.contains("shared by workspace packages"), "{error}");
    }

    #[test]
    #[cfg_attr(miri, ignore = "canonicalizes checked-in fixture paths, which Miri isolation forbids")]
    fn enqueueing_declarations_propagates_release_semantics() {
        let root = source_index_fixture_root();
        let shared = root.join("member/src/shared.rs");
        let declarations = vec![
            ModuleDeclaration {
                path: shared.clone(),
                role: SourceRole::PlainModule,
                guard: CfgFormula::Value(true),
            },
            ModuleDeclaration {
                path: shared,
                role: SourceRole::PathModule,
                guard: CfgFormula::Value(false),
            },
        ];
        let owning_member = MemberDirectory {
            canonical: root.join("member"),
            reported: root.join("member"),
        };
        let mut queue = VecDeque::new();
        enqueue_declarations_with_source_resolver(
            &mut queue,
            declarations.clone(),
            Edition::Edition2024,
            Surface::Released,
            &owning_member,
            &mut resolve_source_path,
        )
        .expect("released declarations enqueue");
        assert_eq!(queue[0].surface, Surface::Released);
        assert_eq!(queue[0].role, SourceRole::PlainModule);
        assert_eq!(queue[1].surface, Surface::NonReleased);
        assert_eq!(queue[1].role, SourceRole::PathModule);

        queue.clear();
        enqueue_declarations_with_source_resolver(
            &mut queue,
            declarations,
            Edition::Edition2024,
            Surface::NonReleased,
            &owning_member,
            &mut resolve_source_path,
        )
        .expect("non-release declarations enqueue");
        assert!(queue.iter().all(|pending| pending.surface == Surface::NonReleased));
    }

    #[test]
    fn cargo_editions_and_line_offsets_map_exactly() {
        assert_eq!(
            [
                syntax_edition(CargoEdition::E2015, Path::new("2015.rs")).expect("2015"),
                syntax_edition(CargoEdition::E2018, Path::new("2018.rs")).expect("2018"),
                syntax_edition(CargoEdition::E2021, Path::new("2021.rs")).expect("2021"),
                syntax_edition(CargoEdition::E2024, Path::new("2024.rs")).expect("2024"),
            ],
            [
                Edition::Edition2015,
                Edition::Edition2018,
                Edition::Edition2021,
                Edition::Edition2024,
            ]
        );

        let starts = line_starts("one\nαβ\nthree");
        assert_eq!(starts, [0, 4, 9]);
        assert_eq!(offset_to_line(&starts, 0), 0);
        assert_eq!(offset_to_line(&starts, 3), 0);
        assert_eq!(offset_to_line(&starts, 4), 1);
        assert_eq!(offset_to_line(&starts, 8), 1);
        assert_eq!(offset_to_line(&starts, 9), 2);
    }

    #[test]
    fn overlapping_test_ranges_are_merged_in_source_order() {
        assert_eq!(
            merge_text_ranges(vec![
                TextRange::new(10.into(), 20.into()),
                TextRange::new(0.into(), 5.into()),
                TextRange::new(5.into(), 10.into()),
                TextRange::new(32.into(), 40.into()),
                TextRange::new(30.into(), 35.into()),
            ]),
            [TextRange::new(0.into(), 20.into()), TextRange::new(30.into(), 40.into()),]
        );
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn source_line_counts_exclude_eof_sentinels() {
        assert_eq!(analyze(Path::new("empty.rs"), "").line_count, 0);
        assert_eq!(analyze(Path::new("trailing.rs"), "pub fn released() {}\n").line_count, 1);
        assert!(validate_line(Path::new("empty.rs"), 1, 0).is_err());
        assert!(validate_line(Path::new("trailing.rs"), 2, 1).is_err());
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn cfg_possibilities_cover_every_operator_and_literal() {
        fn parsed_predicate(predicate: &str) -> ast::CfgPredicate {
            let source = format!("#[cfg({predicate})] fn fixture() {{}}\n");
            let parsed = SourceFile::parse(&source, Edition::Edition2024).tree();
            parsed
                .syntax()
                .descendants()
                .find_map(ast::CfgPredicate::cast)
                .expect("fixture has a cfg predicate")
        }

        fn possibilities(predicate: &str) -> CfgPossibilities {
            cfg_possibilities_without_test(&parsed_predicate(predicate))
        }

        fn approximate(predicate: &str) -> CfgPossibilities {
            cfg_formula_from_predicate(&parsed_predicate(predicate)).map_or(CfgPossibilities::EITHER, |formula| {
                approximate_cfg_possibilities_without_test(&formula)
            })
        }

        assert_eq!(possibilities("true"), CfgPossibilities::TRUE);
        assert_eq!(possibilities("false"), CfgPossibilities::FALSE);
        assert_eq!(possibilities("test"), CfgPossibilities::FALSE);
        assert_eq!(possibilities("unix"), CfgPossibilities::EITHER);
        assert_eq!(
            possibilities("all(test, unix)"),
            CfgPossibilities {
                can_be_true: false,
                can_be_false: true
            }
        );
        assert_eq!(possibilities("any(test, unix)"), CfgPossibilities::EITHER);
        assert_eq!(possibilities("not(test)"), CfgPossibilities::TRUE);
        assert_eq!(possibilities("not(test, unix)"), CfgPossibilities::EITHER);
        assert_eq!(possibilities("all(not(test, unix), true)"), CfgPossibilities::EITHER);
        assert_eq!(possibilities("any(probe, not(probe))"), CfgPossibilities::TRUE);
        assert_eq!(possibilities("all(probe, not(probe))"), CfgPossibilities::FALSE);
        assert_eq!(
            possibilities("any(feature = \"test-util\", not(feature = \"test-util\"))"),
            CfgPossibilities::TRUE
        );
        assert_eq!(
            possibilities("any(test, all(feature = \"x\", not(feature = r\"x\")))"),
            CfgPossibilities::FALSE
        );
        assert_eq!(
            possibilities("any(test, all(feature = \"x\", not(feature = \"\\x78\")))"),
            CfgPossibilities::FALSE
        );
        assert_eq!(
            possibilities("any(a0, not(a0), a1, a2, a3, a4, a5, a6, a7, a8, a9, a10, a11)"),
            CfgPossibilities::TRUE
        );
        assert_eq!(possibilities("all(a, not(b))"), CfgPossibilities::EITHER);
        assert_eq!(
            possibilities("all(a0, a1, a2, a3, a4, a5, a6, a7, a8, a9, a10, a11, a12, not(a12))"),
            CfgPossibilities::EITHER
        );
        assert_eq!(
            possibilities("any(a0, a1, a2, a3, a4, a5, a6, a7, a8, a9, a10, a11, a12)"),
            CfgPossibilities::EITHER
        );

        assert_eq!(approximate("true"), CfgPossibilities::TRUE);
        assert_eq!(approximate("false"), CfgPossibilities::FALSE);
        assert_eq!(approximate("test"), CfgPossibilities::FALSE);
        assert_eq!(approximate("all(test, unix)"), CfgPossibilities::FALSE);
        assert_eq!(approximate("any(test, unix)"), CfgPossibilities::EITHER);
        assert_eq!(approximate("any(test, false)"), CfgPossibilities::FALSE);
        assert_eq!(approximate("not(test)"), CfgPossibilities::TRUE);
        assert_eq!(approximate("not(test, unix)"), CfgPossibilities::EITHER);
    }

    #[test]
    fn cfg_formulas_are_canonical_and_guard_extension_reuses_results() {
        let a = CfgFormula::Atom("a".to_owned());
        let b = CfgFormula::Atom("b".to_owned());
        assert_eq!(CfgFormula::all([]), CfgFormula::Value(true));
        assert_eq!(CfgFormula::all([a.clone()]), a);
        assert_eq!(
            CfgFormula::all([CfgFormula::Atom("a".to_owned()), b.clone(), CfgFormula::Atom("a".to_owned()),]),
            CfgFormula::all([b.clone(), CfgFormula::Atom("a".to_owned())])
        );
        assert_eq!(CfgFormula::any([]), CfgFormula::Value(false));
        assert_eq!(CfgFormula::any([b.clone()]), b);
        assert_eq!(
            CfgFormula::any([
                CfgFormula::Atom("a".to_owned()),
                CfgFormula::Atom("b".to_owned()),
                CfgFormula::Atom("a".to_owned()),
            ]),
            CfgFormula::any([CfgFormula::Atom("b".to_owned()), CfgFormula::Atom("a".to_owned()),])
        );
        assert_eq!(
            CfgFormula::any([
                CfgFormula::Any(vec![CfgFormula::Atom("a".to_owned()), CfgFormula::Atom("b".to_owned()),]),
                CfgFormula::Atom("c".to_owned()),
            ]),
            CfgFormula::any([
                CfgFormula::Atom("a".to_owned()),
                CfgFormula::Atom("b".to_owned()),
                CfgFormula::Atom("c".to_owned()),
            ])
        );

        let inherited = CfgFormula::Atom("feature=x".to_owned());
        let inherited_possibilities = cfg_formula_possibilities_without_test(&inherited);
        assert_eq!(
            extend_cfg_guard(&inherited, inherited_possibilities, CfgFormula::Value(true),),
            (inherited.clone(), inherited_possibilities)
        );
        assert_eq!(
            extend_cfg_guard(&inherited, inherited_possibilities, inherited.clone(),),
            (inherited, inherited_possibilities)
        );
        assert_eq!(
            extend_cfg_guard(
                &CfgFormula::Value(false),
                CfgPossibilities::FALSE,
                CfgFormula::Atom("feature=y".to_owned()),
            ),
            (CfgFormula::Value(false), CfgPossibilities::FALSE)
        );
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn cfg_attr_marks_tests_only_when_the_predicate_is_unconditional_without_test() {
        let source = SourceFile::parse(
            "#[cfg_attr(false, test)] fn never() {}\n\
             #[cfg_attr(unix, test)] fn conditional() {}\n\
             #[cfg_attr(true, test)] fn always() {}\n\
             #[cfg_attr(any(probe, not(probe)), test)] fn correlated() {}\n\
             #[cfg_attr(not(test, unix), test)] fn unsupported() {}\n",
            Edition::Edition2024,
        )
        .tree();
        assert_eq!(
            source
                .items()
                .map(|item| item.attrs().any(|attribute| attribute_marks_test(&attribute, &BTreeSet::new())))
                .collect::<Vec<_>>(),
            [false, false, true, true, false]
        );
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn mixed_lines_remain_released_while_pure_test_lines_are_excluded() {
        let pure = analyze(Path::new("pure.rs"), "#[cfg(test)] fn test_only() {}\n");
        assert_eq!(pure.test_only_lines, BTreeSet::from([1]));

        let mixed = analyze(Path::new("mixed.rs"), "#[cfg(test)] fn test_only() {} pub fn released() {}\n");
        assert!(mixed.test_only_lines.is_empty());

        let multiline_element = analyze(
            Path::new("multiline.rs"),
            "const VALUES: &[i32] = &[\n    #[cfg(test)]\n    1,\n    2,\n];\n",
        );
        assert_eq!(multiline_element.test_only_lines, BTreeSet::from([2, 3]));

        let shared_line = analyze(
            Path::new("shared-line.rs"),
            "const VALUES: &[i32] = &[\n    #[cfg(test)] 1, 2,\n];\n",
        );
        assert!(shared_line.test_only_lines.is_empty());

        let inner_function = analyze(Path::new("inner-function.rs"), "fn helper() {\n    #![cfg(test)]\n}\n");
        assert_eq!(inner_function.test_only_lines, BTreeSet::from([1, 2, 3]));

        let statement = analyze(
            Path::new("statement.rs"),
            "fn released() {\n\
                 #[cfg(test)] test_helper();\n\
                 released_helper();\n\
             }\n",
        );
        assert_eq!(statement.test_only_lines, BTreeSet::from([2]));

        let stacked = analyze(
            Path::new("stacked.rs"),
            "#[cfg(any(test, feature = \"x\"))]\n\
             #[cfg(any(test, not(feature = \"x\")))]\n\
             fn helper() {}\n",
        );
        assert_eq!(stacked.test_only_lines, BTreeSet::from([1, 2, 3]));

        let nested = analyze(
            Path::new("nested.rs"),
            "#[cfg(any(test, feature = \"x\"))]\n\
             fn outer() {\n\
                 #[cfg(any(test, not(feature = \"x\")))]\n\
                 fn helper() {}\n\
             }\n",
        );
        assert_eq!(nested.test_only_lines, BTreeSet::from([3, 4]));

        let correlated_cfg_attr = analyze(
            Path::new("correlated-cfg-attr.rs"),
            "#[cfg(probe)]\n\
             #[cfg_attr(probe, cfg(test))]\n\
             fn helper() {}\n",
        );
        assert_eq!(correlated_cfg_attr.test_only_lines, BTreeSet::from([1, 2, 3]));

        let complementary_cfg_attrs = analyze(
            Path::new("complementary-cfg-attrs.rs"),
            "#[cfg_attr(probe, cfg(test))]\n\
             #[cfg_attr(not(probe), cfg(test))]\n\
             fn helper() {}\n",
        );
        assert_eq!(complementary_cfg_attrs.test_only_lines, BTreeSet::from([1, 2, 3]));

        let crate_inner = analyze(
            Path::new("crate-inner.rs"),
            "#![cfg(any(test, feature = \"x\"))]\n\
             #![cfg(any(test, not(feature = \"x\")))]\n\
             pub fn helper() {}\n",
        );
        assert_eq!(crate_inner.test_only_lines, BTreeSet::from([1, 2, 3]));
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn checked_in_source_fixture_filters_to_exact_released_lcov() {
        let source_path = Path::new("released-and-test-only-source.rs");
        let analysis = analyze(source_path, RELEASED_AND_TEST_ONLY_SOURCE);
        let filtered = filter_report_with(
            parse_lcov("TN:\nSF:released-and-test-only-source.rs\nDA:4,1\nDA:5,1\nDA:6,1\nend_of_record\n"),
            None,
            |_| {
                Ok(SourceDisposition::Include {
                    source_file: source_path.to_path_buf(),
                    line_count: analysis.line_count,
                    test_only_lines: &analysis.test_only_lines,
                })
            },
        )
        .expect("checked-in fixture filters");
        assert_eq!(
            filtered,
            "TN:\nSF:released-and-test-only-source.rs\nDA:4,1\nLF:1\nLH:1\nend_of_record\n"
        );
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn released_visits_intersect_test_only_lines() {
        let analysis = analyze(
            Path::new("shared.rs"),
            "#[cfg(feature = \"x\")]\n\
             pub fn helper() {}\n",
        );
        let enabled = CfgFormula::Atom("feature=x".to_owned());
        let disabled = CfgFormula::not(enabled.clone());
        let enabled_lines = test_only_lines_for_guard(&analysis, &enabled, &BTreeSet::new());
        let disabled_lines = test_only_lines_for_guard(&analysis, &disabled, &BTreeSet::new());
        assert!(enabled_lines.is_empty());
        assert_eq!(disabled_lines, BTreeSet::from([1, 2]));

        for (first, second) in [(enabled_lines.clone(), disabled_lines.clone()), (disabled_lines, enabled_lines)] {
            let mut indexed = IndexedSource {
                analysis: analyze(Path::new("shared.rs"), "pub fn helper() {}\n"),
                edition: Edition::Edition2024,
                released_path: None,
                test_only_lines: BTreeSet::from([99]),
                visit_contexts: BTreeSet::new(),
            };
            indexed.merge_released_test_only_lines(first.clone());
            assert_eq!(indexed.test_only_lines, first);
            indexed.released_path = Some(PathBuf::from("shared.rs"));
            indexed.merge_released_test_only_lines(second);
            assert!(indexed.test_only_lines.is_empty());
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "canonicalizes checked-in fixture paths, which Miri isolation forbids")]
    fn source_index_keeps_lines_released_by_any_guarded_visit() {
        let root = source_index_fixture_root();
        let mut metadata = source_index_fixture_metadata(&root);
        metadata.packages[0].targets[0].src_path = root.join("member/src/guarded-root.rs").to_string_lossy().into_owned().into();

        let index = SourceIndex::from_metadata(&metadata).expect("guarded source index builds");
        let shared = fs::canonicalize(root.join("member/src/guarded-shared.rs")).expect("canonical guarded source");
        let indexed = index.files.get(&shared).expect("guarded shared source indexed");
        assert_eq!(
            indexed
                .visit_contexts
                .iter()
                .filter(|(surface, role, _, _, _)| *surface == Surface::Released && *role == SourceRole::PathModule)
                .count(),
            2
        );
        assert_eq!(indexed.test_only_lines, BTreeSet::from([7, 8]));

        let filtered = filter_reports_with_metadata(
            vec![parse_lcov(
                "TN:\nSF:member/src/guarded-shared.rs\nDA:4,0\nDA:5,0\nDA:7,0\nDA:8,0\nend_of_record\n",
            )],
            &metadata,
        )
        .expect("guarded report filters");
        let report = parse_lcov(&filtered[0]);
        let (_, section) = report.sections.iter().next().expect("released lines remain");
        assert_eq!(section.lines.keys().map(|line| line.line).collect::<Vec<_>>(), [4, 5]);
    }

    #[test]
    #[cfg_attr(miri, ignore = "canonicalizes checked-in fixture paths, which Miri isolation forbids")]
    fn released_target_roots_outside_the_package_remain_attributable() {
        let root = source_index_fixture_root();
        let mut metadata = source_index_fixture_metadata(&root);
        metadata.packages[0].targets[0].src_path = root.join("external-lib.rs").to_string_lossy().into_owned().into();
        metadata.packages[0].metadata = json!({"coverage-gate": {"min-lines-percent": 100}});

        let index = SourceIndex::from_metadata(&metadata).expect("external target root is attributable");
        let external = fs::canonicalize(root.join("external-lib.rs")).expect("canonical external target");
        let indexed = index.files.get(&external).expect("external target indexed");
        let attributed = root.join("member").join("..").join("external-lib.rs");
        assert_eq!(indexed.released_path.as_deref(), Some(attributed.as_path()));

        let filtered = filter_reports_with_metadata(vec![parse_lcov("TN:\nSF:external-lib.rs\nDA:4,1\nend_of_record\n")], &metadata)
            .expect("external target report filters");
        let report = parse_lcov(&filtered[0]);
        assert_eq!(report.sections.keys().next().expect("one section").source_file, attributed);

        let evaluated = crate::evaluate_reports_with_metadata(
            vec![parse_lcov("TN:\nSF:external-lib.rs\nDA:4,1\nend_of_record\n")],
            &metadata,
            &[],
            None,
        )
        .expect("external target report evaluates");
        assert_eq!(evaluated.verdict(), crate::Verdict::Pass);
    }

    #[test]
    #[cfg_attr(miri, ignore = "canonicalizes checked-in fixture paths, which Miri isolation forbids")]
    fn target_required_features_participate_in_root_guard_classification() {
        let root = source_index_fixture_root();
        let mut metadata = source_index_fixture_metadata(&root);
        metadata.packages[0].targets[0].src_path = root
            .join("member/src/required-feature-root.rs")
            .to_string_lossy()
            .into_owned()
            .into();
        metadata.packages[0].targets[0].required_features = vec!["x".to_owned()];

        let index = SourceIndex::from_metadata(&metadata).expect("required-feature source index builds");
        let source = fs::canonicalize(root.join("member/src/required-feature-root.rs")).expect("canonical required-feature source");
        assert_eq!(index.files[&source].test_only_lines, BTreeSet::from([6, 7]));

        let filtered = filter_reports_with_metadata(
            vec![parse_lcov(
                "TN:\nSF:member/src/required-feature-root.rs\nDA:6,0\nDA:7,0\nend_of_record\n",
            )],
            &metadata,
        )
        .expect("required-feature report filters");
        assert_eq!(filtered, [""]);
    }

    #[test]
    fn dependency_required_feature_guards_do_not_imply_local_activation() {
        for selectors in [vec![], vec!["dep/x"], vec!["alias/x"], vec!["dep?/x"], vec!["dep:dep"]] {
            let selectors = selectors.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert_eq!(target_required_features_guard(&selectors), CfgFormula::Value(true));
        }
        for selectors in [vec!["dep"], vec!["dep/x", "dep"], vec!["dep:dep", "dep"]] {
            let selectors = selectors.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert_eq!(
                target_required_features_guard(&selectors),
                CfgFormula::Atom("feature=dep".to_owned())
            );
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "canonicalizes checked-in fixture paths, which Miri isolation forbids")]
    fn dependency_required_features_retain_alternate_activation_routes() {
        let root = source_index_fixture_root();
        let mut metadata_value = serde_json::to_value(source_index_fixture_metadata(&root)).expect("fixture metadata serializes");
        metadata_value["packages"][0]["dependencies"] = json!([{
            "name": "dep",
            "source": "registry+https://github.com/rust-lang/crates.io-index",
            "req": "*",
            "kind": null,
            "rename": null,
            "optional": true,
            "uses_default_features": true,
            "features": ["x"],
            "target": null,
            "registry": null
        }]);
        metadata_value["packages"][0]["features"] = json!({"dep": ["dep:dep"]});
        let mut metadata: Metadata = serde_json::from_value(metadata_value).expect("dependency fixture metadata parses");
        metadata.packages[0].targets[0].src_path = root
            .join("member/src/dependency-required-feature-root.rs")
            .to_string_lossy()
            .into_owned()
            .into();
        metadata.packages[0].targets[0].required_features = vec!["dep/x".to_owned()];

        let source = fs::canonicalize(root.join("member/src/dependency-required-feature-root.rs"))
            .expect("canonical dependency-required feature source");
        for (optional, renamed, exposes_feature, selector, enables_local) in [
            (true, false, true, "dep/x", false),
            (false, false, true, "dep/x", false),
            (true, true, true, "dep/x", false),
            (true, false, true, "dep?/x", false),
            (true, false, false, "dep:dep", false),
            (true, false, false, "dep/x", false),
            (true, false, true, "dep", true),
        ] {
            metadata.packages[0].dependencies[0].optional = optional;
            metadata.packages[0].dependencies[0].name = if renamed { "original-package" } else { "dep" }.to_owned();
            metadata.packages[0].dependencies[0].rename = renamed.then(|| "dep".to_owned());
            metadata.packages[0].features = if exposes_feature {
                if optional {
                    BTreeMap::from([
                        ("dep".to_owned(), vec!["dep:dep".to_owned()]),
                        ("bundle".to_owned(), vec!["dep:dep".to_owned()]),
                    ])
                } else {
                    BTreeMap::from([("dep".to_owned(), Vec::new())])
                }
            } else {
                BTreeMap::from([("activate".to_owned(), vec!["dep:dep".to_owned()])])
            };
            metadata.packages[0].targets[0].required_features = vec![selector.to_owned()];
            let index = SourceIndex::from_metadata(&metadata).expect("dependency-required feature source index builds");
            let expected_removed = if enables_local { BTreeSet::from([7, 8]) } else { BTreeSet::new() };
            assert_eq!(index.files[&source].test_only_lines, expected_removed, "{selector}");

            let filtered = filter_reports_with_metadata(
                vec![parse_lcov(
                    "TN:\nSF:member/src/dependency-required-feature-root.rs\nDA:4,0\nDA:5,0\nDA:7,0\nDA:8,0\nend_of_record\n",
                )],
                &metadata,
            )
            .expect("dependency-required feature report filters");
            let report = parse_lcov(&filtered[0]);
            let retained = report
                .sections
                .values()
                .next()
                .expect("released section remains")
                .lines
                .keys()
                .map(|line| line.line)
                .collect::<Vec<_>>();
            assert_eq!(retained, if enables_local { vec![4, 5] } else { vec![4, 5, 7, 8] }, "{selector}");
        }
    }

    // Verifies edition fallback independently of source discovery and LCOV filtering.
    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn configured_test_attributes_accept_paths_from_older_editions() {
        let expected_attribute = "#[framework::gen]";
        let source = format!("{expected_attribute}\nfn __coverage_gate_test_attribute() {{}}\n");
        assert!(parse_configured_test_attribute_for_edition(&source, expected_attribute, Edition::Edition2024).is_none());
        assert_eq!(
            parse_configured_test_attribute_for_edition(&source, expected_attribute, Edition::Edition2021).as_deref(),
            Some("framework::gen")
        );
        assert_eq!(parse_configured_test_attribute("framework::gen").as_deref(), Some("framework::gen"));
    }

    // Covers workspace policy, target editions, and exact LCOV output using immutable checked-in files.
    #[test]
    #[cfg_attr(miri, ignore = "canonicalizes checked-in fixture paths, which Miri isolation forbids")]
    fn configured_test_attributes_work_across_target_editions() {
        let root = source_index_fixture_root();
        let mut metadata = source_index_fixture_metadata(&root);
        metadata.workspace_metadata = json!({"coverage-gate": {"test-attributes": ["framework::gen"]}});
        for (target, edition, filename) in [
            (0, CargoEdition::E2021, "edition-2021.rs"),
            (1, CargoEdition::E2024, "edition-2024.rs"),
        ] {
            metadata.packages[0].targets[target].edition = edition;
            metadata.packages[0].targets[target].kind = vec![TargetKind::Bin];
            metadata.packages[0].targets[target].src_path = root.join("member/src").join(filename).to_string_lossy().into_owned().into();
        }

        let index = SourceIndex::from_metadata(&metadata).expect("mixed-edition source index builds");
        for filename in ["edition-2021.rs", "edition-2024.rs"] {
            let source = fs::canonicalize(root.join("member/src").join(filename)).expect("canonical edition fixture");
            assert_eq!(index.files[&source].test_only_lines, BTreeSet::from([4, 5]));
            let report = format!("TN:\nSF:member/src/{filename}\nDA:4,0\nDA:5,0\nDA:7,1\nend_of_record\n");
            let filtered = filter_reports_with_metadata(vec![parse_lcov(&report)], &metadata).expect("mixed-edition report filters");
            let expected_path = root.join("member").join("src").join(filename);
            assert_eq!(
                filtered,
                [format!("TN:\nSF:{}\nDA:7,1\nLF:1\nLH:1\nend_of_record\n", expected_path.display())]
            );
        }
    }

    // Isolates parser attachment and line mapping with controlled LF/CRLF inputs, without file discovery.
    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn configured_test_attribute_classification_is_independent_of_line_endings() {
        let attributes = BTreeSet::from(["framework::gen".to_owned()]);
        for (edition, fixture) in [
            (
                Edition::Edition2021,
                include_str!("../tests/fixtures/source-index/member/src/edition-2021.rs"),
            ),
            (
                Edition::Edition2024,
                include_str!("../tests/fixtures/source-index/member/src/edition-2024.rs"),
            ),
        ] {
            let lf = fixture.replace("\r\n", "\n");
            for source in [&lf, &lf.replace('\n', "\r\n")] {
                let analysis =
                    analyze_source_text(Path::new("edition.rs"), source, edition, &attributes).expect("configured test attribute parses");
                assert_eq!(analysis.test_only_lines, BTreeSet::from([4, 5]));
                assert_eq!(analysis.line_count, 7);
            }
        }
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn source_index_composes_relative_paths_and_release_wins() {
        let root = source_index_fixture_root();
        let metadata = source_index_fixture_metadata(&root);
        let index = SourceIndex::from_metadata(&metadata).expect("source index builds");
        let shared = fs::canonicalize(root.join("member/src/shared.rs")).expect("canonical shared source");
        let indexed = index.files.get(&shared).expect("shared source indexed");
        assert!(
            indexed
                .visit_contexts
                .iter()
                .any(|(surface, role, _, _, _)| *surface == Surface::Released && *role == SourceRole::PathModule)
        );
        assert!(
            indexed
                .visit_contexts
                .iter()
                .any(|(surface, role, _, _, _)| *surface == Surface::NonReleased && *role == SourceRole::PathModule)
        );
        assert_eq!(indexed.released_path.as_deref(), Some(root.join("member/src/shared.rs").as_path()));
        let measured = filter_reports_with_compiler_loader(
            vec![parse_lcov("TN:\nSF:member/src/implementation/root.rs\nDA:4,1\nend_of_record\n")],
            &metadata,
            |_| panic!("measured member source has an established package namespace"),
        )
        .expect("an existing measured member source is conservatively retained");
        assert_eq!(parse_lcov(&measured[0]).sections.len(), 1);

        let filtered = filter_reports_with_metadata(
            vec![parse_lcov(concat!(
                "TN:\nSF:member/src/shared.rs\nDA:4,1\nDA:5,0\nDA:6,0\nend_of_record\n",
                "TN:\nSF:member/tests/integration.rs\nDA:4,1\nDA:5,1\nend_of_record\n"
            ))],
            &metadata,
        )
        .expect("relative report filters");
        assert_eq!(filtered.len(), 1);
        let filtered = parse_lcov(&filtered[0]);
        assert_eq!(filtered.sections.len(), 1);
        let (key, section) = filtered.sections.iter().next().expect("one filtered section");
        assert_eq!(key.test_name, "");
        assert_eq!(key.source_file, root.join("member/src/shared.rs"));
        assert_eq!(section.lines.len(), 1);
        let (line, value) = section.lines.iter().next().expect("one filtered line");
        assert_eq!(line.line, 4);
        assert_eq!(value.count, 1);
        assert_eq!(value.checksum, None);
        assert!(section.functions.is_empty());
        assert!(section.branches.is_empty());

        let mut metadata = source_index_fixture_metadata(&root);
        metadata.packages[0].metadata = json!({"coverage-gate": {"min-lines-percent": 100}});
        let evaluated = crate::evaluate_reports_with_metadata(
            vec![parse_lcov("TN:\nSF:member/src/shared.rs\nDA:4,1\nDA:5,0\nDA:6,0\nend_of_record\n")],
            &metadata,
            &[],
            None,
        )
        .expect("pre-recorded report evaluates");
        assert_eq!(evaluated.verdict(), crate::Verdict::Pass);
    }

    #[test]
    #[cfg_attr(miri, ignore = "canonicalizes checked-in fixture paths, which Miri isolation forbids")]
    fn source_index_preserves_symlinked_module_resolution_context() {
        let root = source_index_fixture_root();
        let alias = root.join("member/src/linked-root.rs");
        let canonical_root = fs::canonicalize(root.join("member/src/implementation/root.rs")).expect("canonical root source");
        let helper = fs::canonicalize(root.join("member/src/helper.rs")).expect("canonical helper source");
        let mut metadata = source_index_fixture_metadata(&root);
        metadata.packages[0].targets[0].src_path = alias.to_string_lossy().into_owned().into();

        let mut resolve_source = |path: &Path| {
            if path == alias {
                Ok(canonical_root.clone())
            } else {
                resolve_source_path(path)
            }
        };
        let index =
            SourceIndex::from_metadata_with_source_resolver(&metadata, &mut resolve_source).expect("simulated symlink source index builds");

        let indexed_helper = index.files.get(&helper).expect("shared helper source indexed");
        let helper_path = root.join("member/src/helper.rs");
        let helper_resolution = resolve_module_resolution_path(&helper_path).expect("helper resolution path");
        assert_eq!(indexed_helper.released_path.as_deref(), Some(helper_path.as_path()));
        assert!(indexed_helper.has_visit(&pending_source_for_member(
            Surface::Released,
            SourceRole::PathModule,
            &helper_resolution,
            CfgFormula::Value(true),
            &root.join("member"),
        )));
        assert!(indexed_helper.has_visit(&pending_source_for_member(
            Surface::NonReleased,
            SourceRole::PathModule,
            &helper_resolution,
            CfgFormula::Value(true),
            &root.join("member"),
        )));
        let indexed_root = index.files.get(&canonical_root).expect("canonical root source indexed");
        let resolution_path = resolve_module_resolution_path(&alias).expect("simulated symlink has a module-resolution path");
        assert!(indexed_root.visit_contexts.contains(&(
            Surface::Released,
            SourceRole::CrateRoot,
            resolution_path,
            fs::canonicalize(root.join("member")).expect("canonical member directory"),
            CfgFormula::Value(true),
        )));
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn released_includes_enable_conservative_filtering_even_when_tests_reach_the_file() {
        let root = source_index_fixture_root();
        let mut metadata = source_index_fixture_metadata(&root);
        metadata.packages[0].targets[0].src_path = root.join("member/src/include-root.rs").to_string_lossy().into_owned().into();

        let index = SourceIndex::from_metadata(&metadata).expect("released includes retain uncertain counters");
        let declaring = fs::canonicalize(root.join("member/src/include-root.rs")).expect("canonical include root");
        assert_eq!(index.uncertain_sources, BTreeSet::from([declaring]));
        let warning = index.conservative_warning().expect("conservative filtering must be visible");
        assert!(warning.contains("retaining ambiguous source counters"));
        assert!(warning.contains("not complete production-only classification"));
        assert!(warning.contains("coverage(off)"));

        let test_only = analyze(
            Path::new("/workspace/lib.rs"),
            "#[cfg(test)] mod tests { include!(\"helper.rs\"); }\n",
        );
        assert!(!source_has_opaque_source_macro(
            test_only.syntax(),
            Surface::Released,
            &CfgFormula::Value(true),
            &BTreeSet::new(),
        ));

        for path in ["include", "::core::include", "std::prelude::v1::include"] {
            let analysis = analyze(Path::new("/workspace/lib.rs"), &format!("{path}!(\"helper.rs\");\n"));
            assert!(source_has_opaque_source_macro(
                analysis.syntax(),
                Surface::Released,
                &CfgFormula::Value(true),
                &BTreeSet::new(),
            ));
            assert!(!source_has_opaque_source_macro(
                analysis.syntax(),
                Surface::NonReleased,
                &CfgFormula::Value(true),
                &BTreeSet::new(),
            ));
        }

        let custom = analyze(Path::new("/workspace/lib.rs"), "company::include!(\"helper.rs\");\n");
        assert!(source_has_opaque_source_macro(
            custom.syntax(),
            Surface::Released,
            &CfgFormula::Value(true),
            &BTreeSet::new(),
        ));

        let dynamic = analyze(
            Path::new("/workspace/lib.rs"),
            "#[cfg(test)] include!(\"first.rs\", \"second.rs\");\n",
        );
        assert!(
            declarations(Path::new("/workspace/lib.rs"), &dynamic, SourceRole::CrateRoot, &BTreeSet::new(),)
                .expect("non-literal include is not indexed")
                .is_empty()
        );

        for invocation in [
            "#[cfg(test)] include!(\"included-test.rs\");\n",
            "#[cfg(test)] include!(\"included-test.rs\",);\n",
            "#[cfg(test)] include! { \"included-test.rs\" }\n",
            "#[cfg(test)] include![\"included-test.rs\"];\n",
        ] {
            let analysis = analyze(Path::new("/workspace/lib.rs"), invocation);
            let declarations = declarations(Path::new("/workspace/lib.rs"), &analysis, SourceRole::CrateRoot, &BTreeSet::new())
                .expect("static test-only include is indexed");
            assert_eq!(declarations.len(), 1, "{invocation}");
            assert_eq!(
                lexical_normalize(&declarations[0].path),
                PathBuf::from("/workspace/included-test.rs"),
                "{invocation}"
            );
            assert_eq!(declarations[0].role, SourceRole::IncludedSource);
            assert!(declaration_is_test_only(&declarations[0]));
        }
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn opaque_includes_exclude_local_test_ranges_but_preserve_shared_production_aliases() {
        let root = source_index_fixture_root();
        let mut metadata = source_index_fixture_metadata(&root);
        metadata.packages[0].targets[0].src_path = root.join("member/src/conservative-root.rs").to_string_lossy().into_owned().into();
        metadata.packages[0].metadata = json!({"coverage-gate": {"min-lines-percent": 100}});
        let root_lines = [
            (6, 2),
            (7, 2),
            (8, 2),
            (17, 1),
            (18, 1),
            (19, 1),
            (22, 0),
            (23, 0),
            (24, 0),
            (28, 0),
            (29, 0),
            (30, 0),
            (33, 1),
            (34, 1),
            (35, 1),
            (38, 2),
            (39, 2),
            (40, 2),
            (43, 1),
            (44, 1),
            (45, 1),
        ];
        let shared_lines = [(4, 1), (5, 1), (6, 1), (8, 0), (9, 0), (10, 0), (15, 1), (16, 1), (17, 1)];
        let raw = format!(
            "{}{}{}",
            lcov_section(&root.join("member/src/conservative-root.rs"), &root_lines),
            lcov_section(&root.join("member/src/conservative-shared.rs"), &shared_lines),
            lcov_section(
                &root.join("member/src/../src/conservative-shared.rs"),
                &[(4, 2), (5, 2), (6, 2), (9, 0)]
            ),
        );
        let filtered = filter_reports_with_metadata(parse_reports(&[&raw]).expect("raw LCOV parses"), &metadata)
            .expect("opaque production does not block authored test filtering");
        let report = parse_lcov(&filtered[0]);
        assert_eq!(report.sections.len(), 2);
        for (key, section) in &report.sections {
            let lines = section
                .lines
                .iter()
                .map(|(line, value)| (line.line, value.count))
                .collect::<BTreeMap<_, _>>();
            if key.source_file == root.join("member/src/conservative-root.rs") {
                assert_eq!(
                    lines,
                    BTreeMap::from([
                        (6, 2),
                        (7, 2),
                        (8, 2),
                        (33, 1),
                        (34, 1),
                        (35, 1),
                        (38, 2),
                        (39, 2),
                        (40, 2),
                        (43, 1),
                        (44, 1),
                        (45, 1),
                    ])
                );
            } else {
                assert_eq!(key.source_file, root.join("member/src/conservative-shared.rs"));
                assert_eq!(lines, BTreeMap::from([(4, 3), (5, 3), (6, 3), (8, 0), (9, 0), (10, 0)]));
            }
            assert!(section.functions.is_empty());
            assert!(section.branches.is_empty());
        }
        let repeated = filter_reports_with_metadata(parse_reports(&[&filtered[0]]).expect("filtered LCOV parses"), &metadata)
            .expect("filtered output is stable");
        assert_eq!(repeated, filtered);
        let evaluated = crate::evaluate_reports_with_metadata(vec![report], &metadata, &[], None).expect("filtered reports evaluate");
        assert_eq!(
            evaluated.verdict(),
            crate::Verdict::Fail,
            "genuine shared production misses must fail the unchanged policy"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "canonicalizes checked-in fixture paths, which Miri isolation forbids")]
    fn opaque_includes_preserve_target_guard_ambiguity_and_validate_inputs() {
        let root = source_index_fixture_root();
        let mut metadata = source_index_fixture_metadata(&root);
        metadata.packages[0].targets[0].src_path = root
            .join("member/src/required-feature-root.rs")
            .to_string_lossy()
            .into_owned()
            .into();
        metadata.packages[0].targets[0].required_features = vec!["x".to_owned()];
        let mut opaque_target = metadata.packages[0].targets[0].clone();
        opaque_target.src_path = root.join("member/src/conservative-root.rs").to_string_lossy().into_owned().into();
        metadata.packages[0].targets.push(opaque_target);
        let index = SourceIndex::from_metadata(&metadata).expect("source index builds");
        let (_, constrained) = index
            .source_for_lcov(&root.join("member/src/required-feature-root.rs"))
            .expect("constrained root is indexed");
        let constrained = constrained.expect("the target root has Cargo ownership");
        assert!(constrained.test_only_lines.contains(&7));
        assert!(!constrained.analysis.test_only_lines.contains(&7));
        let filtered = filter_reports_with_metadata(
            vec![parse_lcov("TN:\nSF:member/src/required-feature-root.rs\nDA:7,0\nend_of_record\n")],
            &metadata,
        )
        .expect("target-context-only exclusion is conservatively retained");
        let report = parse_lcov(&filtered[0]);
        let section = report.sections.values().next().expect("retained source");
        assert_eq!(section.lines.len(), 1);
        assert_eq!(section.lines.values().next().expect("retained miss").count, 0);

        for (source, line) in [("conservative-root.rs", 0), ("conservative-root.rs", 46)] {
            let raw = format!("TN:\nSF:member/src/{source}\nDA:{line},1\nend_of_record\n");
            let error =
                filter_reports_with_metadata(vec![parse_lcov(&raw)], &metadata).expect_err("unrelated input failures remain explicit");
            let causes = std::iter::successors(Some(&error as &(dyn std::error::Error + 'static)), |source| source.source());
            assert!(
                causes
                    .into_iter()
                    .any(<dyn std::error::Error + 'static>::is::<CoverageLineOutOfRangeError>),
                "{error}"
            );
        }
        let conflicting = concat!(
            "TN:\nSF:member/src/conservative-shared.rs\nDA:4,1,first\nend_of_record\n",
            "TN:\nSF:member/src/../src/conservative-shared.rs\nDA:4,1,second\nend_of_record\n"
        );
        let error =
            filter_reports_with_metadata(vec![parse_lcov(conflicting)], &metadata).expect_err("alias checksum conflicts still fail");
        assert!(
            std::iter::successors(Some(&error as &(dyn std::error::Error + 'static)), |source| source.source())
                .any(<dyn std::error::Error + 'static>::is::<NormalizedSourceChecksumError>)
        );
    }

    #[test]
    fn conservative_retention_preserves_external_attribution_errors() {
        let member = PathBuf::from("/workspace/member");
        let other = PathBuf::from("/workspace/other");
        let path = PathBuf::from("/workspace/shared.rs");
        let index = SourceIndex {
            workspace_root: PathBuf::from("/workspace"),
            files: BTreeMap::new(),
            member_dirs: MemberDirectories {
                reported_by_canonical: BTreeMap::from([(member.clone(), member.clone()), (other.clone(), other.clone())]),
            },
            uncertain_sources: BTreeSet::new(),
        };
        assert!(index.conservative_warning().is_none());
        let mut source = IndexedSource {
            analysis: SourceAnalysis {
                syntax: None,
                line_count: 0,
                line_starts: vec![0],
                test_only_lines: BTreeSet::new(),
            },
            edition: Edition::Edition2024,
            released_path: None,
            test_only_lines: BTreeSet::new(),
            visit_contexts: BTreeSet::new(),
        };
        let error = index
            .retained_source_path(&path, &source)
            .expect_err("a source without any visit has no package attribution");
        assert!(
            std::iter::successors(Some(&error as &(dyn std::error::Error + 'static)), |source| source.source())
                .any(<dyn std::error::Error + 'static>::is::<UnownedCoverageSourceError>)
        );
        let pending = PendingSource {
            owning_member: MemberDirectory {
                canonical: member.clone(),
                reported: member.clone(),
            },
            ..pending_source(Surface::NonReleased, SourceRole::PathModule, &path, CfgFormula::Value(true))
        };
        source.add_visit(&pending, None).expect("first owner");
        assert_eq!(
            index.retained_source_path(&path, &source).expect("one owner"),
            member.join("../shared.rs")
        );
        let pending = PendingSource {
            owning_member: MemberDirectory {
                canonical: other.clone(),
                reported: other,
            },
            ..pending
        };
        source
            .add_visit(&pending, None)
            .expect("non-release visits may have distinct owners");
        let error = index
            .retained_source_path(&path, &source)
            .expect_err("retention cannot invent one package namespace");
        assert!(
            std::iter::successors(Some(&error as &(dyn std::error::Error + 'static)), |source| source.source())
                .any(<dyn std::error::Error + 'static>::is::<ConflictingReleasedSourceAttributionError>)
        );
        assert_eq!(
            index
                .retained_source_path(&member.join("inside.rs"), &source)
                .expect("canonical member ownership wins"),
            member.join("inside.rs")
        );
        source.released_path = Some(PathBuf::from("known.rs"));
        assert_eq!(
            index.retained_source_path(&path, &source).expect("known released ownership"),
            PathBuf::from("known.rs")
        );
        source.released_path = None;
        let error = index
            .retained_source_path(Path::new(r"D:\unrelated.rs"), &source)
            .expect_err("cross-filesystem attribution remains unsupported");
        assert!(
            std::iter::successors(Some(&error as &(dyn std::error::Error + 'static)), |source| source.source())
                .any(<dyn std::error::Error + 'static>::is::<UnattributableReleasedSourceError>)
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "canonicalizes checked-in fixture paths, which Miri isolation forbids")]
    fn guarded_production_include_fragments_are_retained_without_standalone_parsing() {
        let root = source_index_fixture_root();
        let mut metadata = source_index_fixture_metadata(&root);
        metadata.packages[0].targets[0].src_path = root
            .join("member/src/conservative-static-root.rs")
            .to_string_lossy()
            .into_owned()
            .into();
        let index = SourceIndex::from_metadata(&metadata).expect("guarded literal inclusion is conservatively indexed");
        let (_, included) = index
            .source_for_lcov(&root.join("member/src/included-expression.rs"))
            .expect("included fragment");
        let included = included.expect("the literal include is indexed");
        assert!(included.analysis.syntax.is_none());
        let filtered = filter_reports_with_metadata(
            vec![parse_lcov("TN:\nSF:member/src/included-expression.rs\nDA:4,0\nend_of_record\n")],
            &metadata,
        )
        .expect("production fragment counters are retained");
        let report = parse_lcov(&filtered[0]);
        assert_eq!(report.sections.len(), 1);
        assert_eq!(
            report
                .sections
                .values()
                .next()
                .expect("fragment")
                .lines
                .values()
                .next()
                .expect("miss")
                .count,
            0
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "canonicalizes immutable fixture paths, which Miri isolation forbids")]
    fn opaque_item_macros_retain_shared_production_even_without_unresolved_records() {
        let root = source_index_fixture_root();
        let mut metadata = source_index_fixture_metadata(&root);
        metadata.packages[0].targets[0].src_path = root.join("member/src/macro-root.rs").to_string_lossy().into_owned().into();
        let index = SourceIndex::from_metadata(&metadata).expect("opaque item macro does not require expansion");
        assert_eq!(
            index.uncertain_sources,
            BTreeSet::from([fs::canonicalize(root.join("member/src/macro-root.rs")).expect("canonical root")])
        );
        let raw = format!(
            "{}{}",
            lcov_section(&root.join("member/src/conservative-shared.rs"), &[(4, 1), (8, 0), (15, 1)]),
            lcov_section(&root.join("member/src/../src/conservative-shared.rs"), &[(4, 2), (9, 0)]),
        );
        let filtered = filter_reports_with_metadata(parse_reports(&[&raw]).expect("raw source"), &metadata)
            .expect("shared released contexts are uncertain, not test-only");
        let report = parse_lcov(&filtered[0]);
        assert_eq!(report.sections.len(), 1);
        let section = report.sections.values().next().expect("shared production");
        let lines = section
            .lines
            .iter()
            .map(|(line, value)| (line.line, value.count))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(lines, BTreeMap::from([(4, 3), (8, 0), (9, 0)]));
    }

    #[test]
    #[cfg_attr(miri, ignore = "canonicalizes immutable fixture paths, which Miri isolation forbids")]
    fn block_local_macros_retain_production_counters_already_indexed_through_test_edges() {
        let root = source_index_fixture_root();
        let mut metadata = source_index_fixture_metadata(&root);
        metadata.packages[0].targets[0].src_path = root.join("member/src/block-macro-root.rs").to_string_lossy().into_owned().into();
        metadata.packages[0].metadata = json!({"coverage-gate": {"min-lines-percent": 100}});
        let index = SourceIndex::from_metadata(&metadata).expect("block-local declaration source parses");
        let (_, shared) = index
            .source_for_lcov(&root.join("member/src/conservative-shared.rs"))
            .expect("known source");
        let shared = shared.expect("the shared source is already indexed");
        assert!(shared.released_path.is_none(), "the ordinary graph contains only the test edge");
        assert!(shared.visit_contexts.iter().all(|(surface, ..)| *surface == Surface::NonReleased));
        let raw = format!(
            "{}{}",
            lcov_section(&root.join("member/src/conservative-shared.rs"), &[(4, 1), (8, 0), (15, 1)]),
            lcov_section(&root.join("member/src/../src/conservative-shared.rs"), &[(4, 2), (9, 0)]),
        );
        let filtered = filter_reports_with_metadata(parse_reports(&[&raw]).expect("actual shared counters"), &metadata)
            .expect("opaque production context prevents whole-file test exclusion");
        let report = parse_lcov(&filtered[0]);
        assert_eq!(report.sections.len(), 1);
        let (key, section) = report.sections.iter().next().expect("shared production counters remain");
        assert_eq!(key.source_file, root.join("member/src/conservative-shared.rs"));
        let lines = section
            .lines
            .iter()
            .map(|(line, value)| (line.line, value.count))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(lines, BTreeMap::from([(4, 3), (8, 0), (9, 0)]));
        assert!(
            index.files.values().all(|source| !source.visit_contexts.is_empty()),
            "no measured graph gap is needed"
        );
        let repeated = filter_reports_with_metadata(parse_reports(&[&filtered[0]]).expect("published counters"), &metadata)
            .expect("block-local source uncertainty survives filtering");
        assert_eq!(repeated, filtered);
        let evaluated = crate::evaluate_reports_with_metadata(vec![report], &metadata, &[], None).expect("production verdict");
        assert_eq!(
            evaluated.verdict(),
            crate::Verdict::Fail,
            "genuine production misses must not disappear"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "canonicalizes immutable fixture paths, which Miri isolation forbids")]
    fn measured_member_source_gaps_use_local_proofs_before_filtering_the_whole_batch() {
        let root = source_index_fixture_root();
        let mut metadata = source_index_fixture_metadata(&root);
        metadata.packages[0].targets[0].src_path = root.join("member/src/macro-root.rs").to_string_lossy().into_owned().into();
        let shared = lcov_section(&root.join("member/src/conservative-shared.rs"), &[(4, 1), (8, 0), (15, 1)]);
        let backend = format!(
            "{}{}",
            lcov_section(
                &root.join("member/src/macro-backend.rs"),
                &[(4, 1), (5, 1), (6, 1), (8, 0), (9, 0), (10, 0), (15, 1)]
            ),
            lcov_section(&root.join("member/src/../src/macro-backend.rs"), &[(4, 2), (9, 0)]),
        );
        let filtered = filter_reports_with_compiler_loader(parse_reports(&[&shared, &backend]).expect("raw source"), &metadata, |_| {
            panic!("existing measured source belongs to a known member namespace")
        })
        .expect("module-graph gaps must not discard measured member source");
        let shared_report = parse_lcov(&filtered[0]);
        let lines = shared_report
            .sections
            .values()
            .next()
            .expect("shared source")
            .lines
            .keys()
            .map(|line| line.line)
            .collect::<Vec<_>>();
        assert_eq!(lines, [4, 8]);
        let backend_report = parse_lcov(&filtered[1]);
        assert_eq!(backend_report.sections.len(), 1);
        let (key, section) = backend_report.sections.iter().next().expect("macro-declared member source");
        assert_eq!(key.source_file, root.join("member/src/macro-backend.rs"));
        let lines = section
            .lines
            .iter()
            .map(|(line, value)| (line.line, value.count))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(lines, BTreeMap::from([(4, 3), (5, 1), (6, 1), (8, 0), (9, 0), (10, 0)]));
        let repeated = filter_reports_with_metadata(
            parse_reports(&filtered.iter().map(String::as_str).collect::<Vec<_>>()).expect("filtered reports"),
            &metadata,
        )
        .expect("published line filtering is stable");
        assert_eq!(repeated, filtered);

        metadata.packages[0].targets[0].src_path = root.join("member/src/test-include-root.rs").to_string_lossy().into_owned().into();
        let shared = lcov_section(&root.join("member/src/included-test.rs"), &[(4, 1)]);
        let filtered = filter_reports_with_metadata(parse_reports(&[&shared, &backend]).expect("raw measured member records"), &metadata)
            .expect("measured graph gaps make earlier whole-file exclusions uncertain");
        let report = parse_lcov(&filtered[0]);
        assert_eq!(report.sections.len(), 2);
        assert!(
            report
                .sections
                .iter()
                .any(|(key, section)| { key.source_file == root.join("member/src/macro-backend.rs") && section.is_empty() })
        );
        assert_eq!(report.sections.values().map(|section| section.lines.len()).sum::<usize>(), 1);
    }

    #[test]
    #[cfg_attr(miri, ignore = "canonicalizes immutable fixture paths, which Miri isolation forbids")]
    fn conservative_provenance_survives_when_every_graph_gap_counter_is_excluded() {
        let root = source_index_fixture_root();
        let mut metadata = source_index_fixture_metadata(&root);
        metadata.packages[0].targets[0].src_path = root.join("member/src/test-include-root.rs").to_string_lossy().into_owned().into();
        metadata.packages[0].metadata = json!({"coverage-gate": {"min-lines-percent": 100}});
        for (count, expected) in [(0, crate::Verdict::Fail), (1, crate::Verdict::Pass)] {
            let raw = format!(
                "{}{}",
                lcov_section(&root.join("member/src/included-test.rs"), &[(4, count)]),
                lcov_section(&root.join("member/src/macro-backend.rs"), &[(15, 1)]),
            );
            let filtered = filter_reports_with_metadata(parse_reports(&[&raw]).expect("raw LCOV"), &metadata)
                .expect("local test ranges are removed without losing conservative provenance");
            let repeated = filter_reports_with_metadata(parse_reports(&[&filtered[0]]).expect("published LCOV"), &metadata)
                .expect("published LCOV may be filtered again");
            assert_eq!(repeated, filtered);

            let report = parse_lcov(&filtered[0]);
            assert_eq!(report.sections.len(), 2);
            let witness = report
                .sections
                .iter()
                .find(|(key, _)| key.source_file == root.join("member/src/macro-backend.rs"))
                .map(|(_, section)| section)
                .expect("the measured source-graph gap remains as a zero-line witness");
            assert!(witness.lines.is_empty());
            assert!(witness.functions.is_empty());
            assert!(witness.branches.is_empty());
            let coverage = crate::lcov_cov::CoverageReport::from_strs(&[&filtered[0]]).expect("filtered line coverage");
            assert_eq!(coverage.files.iter().map(|file| file.lines_total).sum::<u32>(), 1);
            assert_eq!(
                coverage.files.iter().map(|file| file.lines_covered).sum::<u32>(),
                u32::try_from(count).expect("count is zero or one")
            );

            let normal =
                crate::evaluate_reports_with_metadata(parse_reports(&[&filtered[0]]).expect("published LCOV"), &metadata, &[], None)
                    .expect("normal evaluation uses the same published counters");
            assert_eq!(normal.verdict(), expected);
            let workspace = crate::workspace::Workspace::from_metadata(&metadata, None).expect("workspace policy");
            assert_eq!(
                crate::verdict::evaluate(&coverage, &workspace, &[])
                    .expect("already filtered evaluation")
                    .verdict(),
                expected
            );
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "canonicalizes immutable fixture paths, which Miri isolation forbids")]
    fn conservative_provenance_is_carried_by_each_output_of_a_report_batch() {
        let root = source_index_fixture_root();
        let mut metadata = source_index_fixture_metadata(&root);
        metadata.packages[0].targets[0].src_path = root.join("member/src/test-include-root.rs").to_string_lossy().into_owned().into();
        let included = lcov_section(&root.join("member/src/included-test.rs"), &[(4, 0)]);
        let gap = lcov_section(&root.join("member/src/../src/macro-backend.rs"), &[(15, 1)]);
        let raw = [included.as_str(), gap.as_str(), ""];
        let filtered = filter_reports_with_metadata(parse_reports(&raw).expect("raw feature reports"), &metadata).expect("batch filters");
        for text in &filtered {
            let repeated = filter_reports_with_metadata(parse_reports(&[text]).expect("individual published report"), &metadata)
                .expect("each published report retains the batch's conservative proof");
            assert_eq!(repeated.as_slice(), std::slice::from_ref(text));
            let report = parse_lcov(text);
            assert!(
                report
                    .sections
                    .iter()
                    .any(|(key, section)| { key.source_file == root.join("member/src/macro-backend.rs") && section.is_empty() })
            );
        }
        let repeated = filter_reports_with_metadata(
            parse_reports(&filtered.iter().map(String::as_str).collect::<Vec<_>>()).expect("published batch"),
            &metadata,
        )
        .expect("published batch filters again");
        assert_eq!(repeated, filtered);
        let coverage = crate::lcov_cov::CoverageReport::from_strs(&filtered.iter().map(String::as_str).collect::<Vec<_>>())
            .expect("zero-line source witnesses do not add coverage");
        assert_eq!(coverage.files.iter().map(|file| file.lines_total).sum::<u32>(), 1);
        assert_eq!(coverage.files.iter().map(|file| file.lines_covered).sum::<u32>(), 0);
        let reversed = filter_reports_with_metadata(parse_reports(&["", &gap, &included]).expect("reordered batch"), &metadata)
            .expect("batch ordering does not change provenance");
        assert_eq!(reversed, filtered.into_iter().rev().collect::<Vec<_>>());
    }

    #[test]
    fn zero_line_source_records_round_trip_without_contributing_coverage() {
        let input = concat!(
            "TN:discarded-empty-section\nend_of_record\n",
            "TN:discarded\nTN:feature\nSF:member/src/macro-backend.rs\nLF:0\nLH:0\nend_of_record\n",
            "SF:member/src/conservative-shared.rs\nend_of_record\n",
        );
        let report = parse_report(input).expect("valid zero-line LCOV records");
        assert_eq!(report.sections.len(), 2);
        assert!(report.sections.values().all(lcov::report::section::Value::is_empty));
        assert_eq!(
            report.sections.keys().map(|key| key.test_name.as_str()).collect::<Vec<_>>(),
            ["", "feature"]
        );
        let mut serialized = String::new();
        for record in report.clone().into_records() {
            writeln!(&mut serialized, "{record}").expect("writing to String");
        }
        assert_eq!(
            parse_report(&serialized).expect("filter parser preserves zero-line source evidence"),
            report
        );
        assert!(
            lcov::Report::from_reader(lcov::Reader::new(serialized.as_bytes()))
                .expect("the serialized witness is standard LCOV")
                .sections
                .is_empty()
        );
        assert!(
            crate::lcov_cov::CoverageReport::from_str(&serialized)
                .expect("ordinary evaluation parses witness records")
                .files
                .is_empty()
        );
        assert!(!serialized.contains("DA:"));
        parse_report("SF:member/src/macro-backend.rs\nTN:unexpected\nend_of_record\n")
            .expect_err("preserving witnesses does not bypass LCOV grammar validation");
    }

    #[test]
    fn a_provenance_witness_never_replaces_existing_counters_or_test_names() {
        let source = Path::new("/workspace/member/src/macro-backend.rs");
        let raw = "TN:feature\nSF:original.rs\nDA:4,0,checksum\nend_of_record\n";
        let test_only = BTreeSet::new();
        let filtered = filter_report_with(parse_lcov(raw), Some(source), |_| {
            Ok(SourceDisposition::Include {
                source_file: source.to_path_buf(),
                line_count: 4,
                test_only_lines: &test_only,
            })
        })
        .expect("an existing source section already carries the witness");
        let report = parse_lcov(&filtered);
        assert_eq!(report.sections.len(), 1);
        let (key, section) = report.sections.iter().next().expect("original source section");
        assert_eq!(key.test_name, "feature");
        assert_eq!(key.source_file, source);
        let (line, value) = section.lines.iter().next().expect("original production miss");
        assert_eq!(line.line, 4);
        assert_eq!(value.count, 0);
        assert_eq!(value.checksum.as_deref(), Some("checksum"));
        let empty = filter_report_with(parse_lcov(""), None, |_| panic!("empty input has no source sections"))
            .expect("without graph-gap provenance an empty report stays empty");
        assert!(empty.is_empty());
    }

    #[test]
    #[cfg_attr(miri, ignore = "canonicalizes immutable fixture paths, which Miri isolation forbids")]
    fn measured_member_source_gaps_preserve_invalid_source_and_line_failures() {
        let root = source_index_fixture_root();
        let metadata = source_index_fixture_metadata(&root);
        for (path, line) in [
            ("member/src/invalid-source.txt", 1),
            ("member/src/missing-source.rs", 1),
            ("member/src/macro-backend.rs", 0),
            ("member/src/macro-backend.rs", 99),
        ] {
            let error = filter_reports_with_metadata(vec![parse_lcov(&lcov_section(&root.join(path), &[(line, 0)]))], &metadata)
                .expect_err("uncertain ownership does not validate malformed source or stale counters");
            let causes = std::iter::successors(Some(&error as &(dyn std::error::Error + 'static)), |source| source.source());
            assert!(if path == "member/src/invalid-source.txt" {
                causes
                    .into_iter()
                    .any(<dyn std::error::Error + 'static>::is::<ParseCoverageSourceError>)
            } else if path.contains("missing") {
                causes
                    .into_iter()
                    .any(<dyn std::error::Error + 'static>::is::<ResolveCoverageSourceError>)
            } else {
                causes
                    .into_iter()
                    .any(<dyn std::error::Error + 'static>::is::<CoverageLineOutOfRangeError>)
            });
        }
    }

    #[test]
    fn measured_member_source_requires_an_unambiguous_rust_edition() {
        let root = source_index_fixture_root();
        let mut metadata = source_index_fixture_metadata(&root);
        let source = root.join("member/src/macro-backend.rs");
        assert_eq!(
            measured_member_edition(&metadata.packages[0], &source).expect("uniform target edition"),
            Edition::Edition2024
        );
        metadata.packages[0].targets[1].edition = CargoEdition::E2021;
        measured_member_edition(&metadata.packages[0], &source).expect_err("a missing target edge cannot resolve conflicting editions");
        metadata.packages[0].targets.clear();
        metadata.packages[0].edition = CargoEdition::E2018;
        assert_eq!(
            measured_member_edition(&metadata.packages[0], &source).expect("package edition without targets"),
            Edition::Edition2018
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "rowan syntax parsing is not supported by Miri")]
    fn only_potentially_released_source_macros_make_graph_exclusions_uncertain() {
        for source in [
            "opaque! { mod child; }\n",
            "mod inline { opaque! { mod child; } }\n",
            "std::cfg_select! { unix => { mod child; } _ => {} }\n",
            "fn production() { include!(\"fragment.rs\"); }\n",
            "fn production() { declare_shared! {} }\n",
            "fn production() { declare_shared!(); }\n",
            "fn production() { let _value = declare_shared!(); }\n",
            "fn production() { declare_shared!() }\n",
            "fn production() { assert!(true); }\n",
        ] {
            assert!(
                source_has_opaque_source_macro(
                    analyze(Path::new("fixture.rs"), source).syntax(),
                    Surface::Released,
                    &CfgFormula::Value(true),
                    &BTreeSet::new()
                ),
                "{source}"
            );
        }
        for source in [
            "#[cfg(test)] opaque! { mod child; }\n",
            "#[cfg(test)] fn only_tests() { declare_shared! {} }\n",
            "fn production() { #[cfg(test)] declare_shared! {} }\n",
            "macro_rules! unused { () => { mod child; } }\n",
        ] {
            assert!(
                !source_has_opaque_source_macro(
                    analyze(Path::new("fixture.rs"), source).syntax(),
                    Surface::Released,
                    &CfgFormula::Value(true),
                    &BTreeSet::new()
                ),
                "{source}"
            );
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "rowan syntax parsing is not supported by Miri")]
    fn block_local_macro_guards_agree_for_line_filtering_and_source_edges() {
        let path = Path::new("/workspace/lib.rs");
        let source = analyze(
            path,
            "fn production() {\n    #[cfg(test)] include!(\"included-expression.rs\");\n}\n",
        );
        assert_eq!(source.test_only_lines, BTreeSet::from([2]));
        assert!(!source_has_opaque_source_macro(
            source.syntax(),
            Surface::Released,
            &CfgFormula::Value(true),
            &BTreeSet::new()
        ));
        let declarations = declarations(path, &source, SourceRole::CrateRoot, &BTreeSet::new()).expect("guarded include declaration");
        assert_eq!(declarations.len(), 1);
        assert!(declaration_is_test_only(&declarations[0]));
    }

    #[test]
    #[cfg_attr(miri, ignore = "canonicalizes checked-in fixture paths, which Miri isolation forbids")]
    fn test_only_included_sources_are_indexed_without_parsing() {
        let root = source_index_fixture_root();
        let mut metadata = source_index_fixture_metadata(&root);
        metadata.packages[0].targets[0].src_path = root.join("member/src/test-include-root.rs").to_string_lossy().into_owned().into();
        let index = SourceIndex::from_metadata(&metadata).expect("test-only include is indexed");
        assert!(index.uncertain_sources.is_empty());
        assert!(index.conservative_warning().is_none());
        let included = fs::canonicalize(root.join("member/src/included-test.rs")).expect("canonical included test source");
        let indexed = index.files.get(&included).expect("included test source indexed");
        assert_eq!(indexed.released_path, None);
        assert!(indexed.analysis.syntax.is_none());
        assert_eq!(
            indexed
                .visit_contexts
                .iter()
                .filter(|(_, role, _, _, _)| *role == SourceRole::IncludedSource)
                .count(),
            3
        );
        let resolution_path = resolve_module_resolution_path(&included).expect("included source resolution path");
        assert!(indexed.has_visit(&pending_source_for_member(
            Surface::NonReleased,
            SourceRole::IncludedSource,
            &resolution_path,
            CfgFormula::Value(false),
            &root.join("member"),
        )));

        let expression = fs::canonicalize(root.join("member/src/included-expression.rs")).expect("canonical included expression source");
        let indexed_expression = index.files.get(&expression).expect("included expression source indexed");
        assert_eq!(indexed_expression.released_path, None);
        assert!(indexed_expression.analysis.syntax.is_none());
        assert_eq!(indexed_expression.analysis.line_count, 4);
        assert_eq!(
            indexed_expression
                .visit_contexts
                .iter()
                .filter(|(_, role, _, _, _)| *role == SourceRole::IncludedSource)
                .count(),
            1
        );

        let filtered = filter_reports_with_metadata(
            vec![parse_lcov(concat!(
                "TN:\nSF:member/src/included-test.rs\nDA:4,0\nend_of_record\n",
                "TN:\nSF:member/src/included-expression.rs\nDA:4,0\nend_of_record\n",
            ))],
            &metadata,
        )
        .expect("test-only included source filters");
        assert_eq!(filtered, [""]);
    }

    #[test]
    fn compiler_library_query_preserves_proxy_selection_and_workspace() {
        let workspace = source_index_fixture_root();
        let sysroot = workspace.join("toolchain");
        let expected = sysroot.join("lib/rustlib/src/rust/library");
        let resolved = workspace.join("canonical-library");
        let root = compiler_library_with(
            &workspace,
            |name| {
                assert_eq!(name, "RUSTC");
                Some(OsString::from("selected-rustc-proxy"))
            },
            |command| {
                assert_eq!(command.get_program(), "selected-rustc-proxy");
                assert_eq!(command.get_current_dir(), Some(workspace.as_path()));
                assert_eq!(command.get_args().collect::<Vec<_>>(), ["--print", "sysroot"]);
                Ok(sysroot)
            },
            |path| {
                assert_eq!(path, expected);
                Ok(Some(resolved.clone()))
            },
        )
        .expect("selected compiler query");
        assert_eq!(root, Some(resolved));
        let error = compiler_library_with(
            &workspace,
            |_| None,
            |command| {
                assert_eq!(command.get_program(), "rustc");
                Err(CompilerSourceDiscoveryError::new("rustc".to_owned(), "query failed".to_owned()).into())
            },
            |_| panic!("a failed query must not resolve a guessed source root"),
        )
        .expect_err("compiler query failure stays explicit");
        assert!(error.to_string().contains("query failed"));
    }

    #[test]
    fn compiler_sysroot_output_rejects_failed_invalid_or_relative_queries() {
        let root = source_index_fixture_root();
        let stdout = format!("{}\n", root.display());
        assert_eq!(
            compiler_sysroot_output("proxy --print sysroot", true, "0", stdout.as_bytes(), b"").expect("absolute sysroot"),
            root
        );
        let error = compiler_sysroot_output("proxy", false, "exit status: 7", stdout.as_bytes(), b"proxy failed")
            .expect_err("failed compiler must not establish provenance");
        assert!(error.to_string().contains("exit status: 7"));
        assert!(error.to_string().contains("proxy failed"));
        for bytes in [b"".as_slice(), b"relative/path", b"/first\n/second\n", &[0xff]] {
            let error = compiler_sysroot_output("proxy", true, "0", bytes, b"").expect_err("invalid source discovery output");
            assert!(
                std::iter::successors(Some(&error as &(dyn std::error::Error + 'static)), |source| source.source())
                    .any(<dyn std::error::Error + 'static>::is::<CompilerSourceDiscoveryError>)
            );
        }
    }

    #[test]
    fn compiler_library_root_requires_an_existing_directory() {
        let library = Path::new("/compiler/library");
        let canonical = PathBuf::from("/canonical/library");
        assert_eq!(
            canonical_compiler_library_with(
                library,
                |path| {
                    assert_eq!(path, library);
                    Ok(canonical.clone())
                },
                |path| {
                    assert_eq!(path, canonical);
                    Ok(true)
                },
            )
            .expect("an existing directory establishes the compiler source root"),
            Some(canonical.clone())
        );
        assert_eq!(
            canonical_compiler_library_with(
                library,
                |_| Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
                |_| panic!("missing optional sources must not be inspected"),
            )
            .expect("optional sources may be absent"),
            None
        );
        let error =
            canonical_compiler_library_with(library, |_| Ok(canonical), |_| Ok(false)).expect_err("a file is not a source directory");
        let cause = std::iter::successors(Some(&error as &(dyn std::error::Error + 'static)), |source| source.source())
            .find_map(<dyn std::error::Error + 'static>::downcast_ref::<CompilerSourceDiscoveryError>)
            .expect("a non-directory compiler source root has a typed discovery error");
        assert_eq!(cause.operation, "/compiler/library");
        assert_eq!(cause.reason, "the compiler library-source root must be a directory");
    }

    #[test]
    fn compiler_source_inspection_failures_do_not_become_missing_optional_sources() {
        let path = Path::new("/compiler/library");
        let denied = || std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let error = canonical_compiler_library_with(path, |_| Err(denied()), |_| panic!("resolution failed"))
            .expect_err("permission failure is not optional absence");
        assert!(
            std::iter::successors(Some(&error as &(dyn std::error::Error + 'static)), |source| source.source())
                .any(<dyn std::error::Error + 'static>::is::<ResolveCoverageSourceError>)
        );
        let error = canonical_compiler_library_with(path, |path| Ok(path.to_path_buf()), |_| Err(denied()))
            .expect_err("inspection failure remains explicit");
        assert!(
            std::iter::successors(Some(&error as &(dyn std::error::Error + 'static)), |source| source.source())
                .any(<dyn std::error::Error + 'static>::is::<InspectCoverageSourceError>)
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "canonicalizes immutable fixture paths, which Miri isolation forbids")]
    fn verified_compiler_sources_are_removed_once_across_canonical_aliases() {
        let root = source_index_fixture_root();
        let mut metadata = source_index_fixture_metadata(&root);
        metadata.packages[0].targets[0].src_path = root.join("member/src/conservative-root.rs").to_string_lossy().into_owned().into();
        let library = root.join("toolchain/lib/rustlib/src/rust/library");
        assert_eq!(
            canonical_compiler_library(&library).expect("installed source tree"),
            Some(fs::canonicalize(&library).expect("canonical library"))
        );
        assert_eq!(
            canonical_compiler_library(&root.join("absent-rust-src")).expect("optional sources may be absent"),
            None
        );
        let error = canonical_compiler_library(&root.join("external-lib.rs")).expect_err("a file is not a source directory");
        let cause = std::iter::successors(Some(&error as &(dyn std::error::Error + 'static)), |source| source.source())
            .find_map(<dyn std::error::Error + 'static>::downcast_ref::<CompilerSourceDiscoveryError>)
            .expect("the filesystem adapter rejects file roots with a typed discovery error");
        assert_eq!(cause.reason, "the compiler library-source root must be a directory");
        let compiler_file = library.join("std/src/sys/thread_local/native/mod.rs");
        let alias = compiler_file
            .parent()
            .expect("fixture source has a parent")
            .join("../native/mod.rs");
        let alias = PathBuf::from(
            alias
                .to_string_lossy()
                .replace("\\lib\\rustlib\\src\\rust\\library", "\\lib/rustlib/src/rust\\library"),
        );
        let canonical = fs::canonicalize(&compiler_file).expect("canonical source");
        let inputs = [&compiler_file, &alias, &canonical]
            .into_iter()
            .map(|path| {
                format!(
                    "{}{}{}",
                    lcov_section(&root.join("member/src/conservative-root.rs"), &[(6, 2), (7, 2), (8, 2), (17, 1)]),
                    lcov_section(&root.join("member/src/conservative-shared.rs"), &[(4, 1), (8, 0), (15, 1)]),
                    lcov_section(path, &[(4, 0)]),
                )
            })
            .collect::<Vec<_>>();
        let refs = inputs.iter().map(String::as_str).collect::<Vec<_>>();
        let loads = std::cell::Cell::new(0);
        let filtered = filter_reports_with_compiler_loader(parse_reports(&refs).expect("raw input"), &metadata, |workspace| {
            assert_eq!(workspace, root);
            loads.set(loads.get() + 1);
            canonical_compiler_library(&library)
        })
        .expect("verified compiler library records do not invalidate project coverage");
        assert_eq!(loads.get(), 1);
        assert_eq!(filtered[0], filtered[1]);
        assert_eq!(filtered[0], filtered[2]);
        for text in &filtered {
            let report = parse_lcov(text);
            assert_eq!(report.sections.len(), 2);
            for (key, section) in report.sections {
                let lines = section
                    .lines
                    .iter()
                    .map(|(line, value)| (line.line, value.count))
                    .collect::<BTreeMap<_, _>>();
                if key.source_file == root.join("member/src/conservative-root.rs") {
                    assert_eq!(lines, BTreeMap::from([(6, 2), (7, 2), (8, 2)]));
                } else {
                    assert_eq!(key.source_file, root.join("member/src/conservative-shared.rs"));
                    assert_eq!(lines, BTreeMap::from([(4, 1), (8, 0)]));
                }
            }
        }
        let refs = filtered.iter().map(String::as_str).collect::<Vec<_>>();
        let repeated = filter_reports_with_compiler_loader(parse_reports(&refs).expect("filtered input"), &metadata, |_| {
            panic!("fully owned published reports must not query a compiler")
        })
        .expect("filtered output stays byte-stable");
        assert_eq!(repeated, filtered);
    }

    #[test]
    #[cfg_attr(miri, ignore = "canonicalizes immutable fixture paths, which Miri isolation forbids")]
    fn compiler_provenance_does_not_hide_unknown_or_missing_project_sources() {
        let root = source_index_fixture_root();
        let metadata = source_index_fixture_metadata(&root);
        let library = root.join("toolchain/lib/rustlib/src/rust/library");
        let lookalike = root.join("toolchain/lib/rustlib/src/rust/library-other/std.rs");
        let error = filter_reports_with_compiler_loader(vec![parse_lcov(&lcov_section(&lookalike, &[(4, 0)]))], &metadata, |_| {
            canonical_compiler_library(&library)
        })
        .expect_err("source directory names alone do not prove compiler ownership");
        assert!(
            std::iter::successors(Some(&error as &(dyn std::error::Error + 'static)), |source| source.source())
                .any(<dyn std::error::Error + 'static>::is::<UnownedCoverageSourceError>)
        );
        let compiler_file = library.join("std/src/sys/thread_local/native/mod.rs");
        filter_reports_with_compiler_loader(vec![parse_lcov(&lcov_section(&compiler_file, &[(4, 0)]))], &metadata, |_| Ok(None))
            .expect_err("without an established compiler root the source remains unknown");
        filter_reports_with_compiler_loader(vec![parse_lcov(&lcov_section(&compiler_file, &[(4, 0)]))], &metadata, |_| {
            Err(CompilerSourceDiscoveryError::new("proxy".to_owned(), "unavailable".to_owned()).into())
        })
        .expect_err("failed discovery must not turn into exclusion");
        let missing = library.join("std/missing.rs");
        let error = filter_reports_with_compiler_loader(vec![parse_lcov(&lcov_section(&missing, &[(4, 0)]))], &metadata, |_| {
            panic!("an unresolvable input path must fail before source discovery")
        })
        .expect_err("missing source still fails");
        assert!(
            std::iter::successors(Some(&error as &(dyn std::error::Error + 'static)), |source| source.source())
                .any(<dyn std::error::Error + 'static>::is::<ResolveCoverageSourceError>)
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "canonicalizes immutable fixture paths, which Miri isolation forbids")]
    fn explicit_cargo_ownership_takes_precedence_over_compiler_source_location() {
        let root = source_index_fixture_root();
        let mut metadata = source_index_fixture_metadata(&root);
        let compiler_file = root.join("toolchain/lib/rustlib/src/rust/library/std/src/sys/thread_local/native/mod.rs");
        metadata.packages[0].targets[0].src_path = compiler_file.to_string_lossy().into_owned().into();
        let filtered = filter_reports_with_compiler_loader(vec![parse_lcov(&lcov_section(&compiler_file, &[(4, 0)]))], &metadata, |_| {
            panic!("a Cargo-owned source must not be discarded as compiler code")
        })
        .expect("explicit Cargo source is retained");
        let report = parse_lcov(&filtered[0]);
        assert_eq!(report.sections.len(), 1);
        let section = report.sections.values().next().expect("retained project source");
        assert_eq!(section.lines.len(), 1);
        assert_eq!(section.lines.values().next().expect("production miss").count, 0);
    }

    #[test]
    #[cfg_attr(miri, ignore = "canonicalizes checked-in fixture paths, which Miri isolation forbids")]
    fn included_source_is_parsed_when_a_released_module_also_reaches_it() {
        let root = source_index_fixture_root();
        let mut metadata = source_index_fixture_metadata(&root);
        metadata.packages[0].targets[0].src_path = root
            .join("member/src/promoted-include-root.rs")
            .to_string_lossy()
            .into_owned()
            .into();

        let index = SourceIndex::from_metadata(&metadata).expect("included source is promoted to released module analysis");
        let included = fs::canonicalize(root.join("member/src/included-test.rs")).expect("canonical included source");
        let indexed = index.files.get(&included).expect("included source indexed");
        assert!(indexed.analysis.syntax.is_some());
        assert_eq!(
            indexed.released_path.as_deref(),
            Some(root.join("member/src/included-test.rs").as_path())
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "canonicalizes checked-in fixture paths, which Miri isolation forbids")]
    fn promoted_included_source_still_rejects_unresolvable_include_paths() {
        let root = source_index_fixture_root();
        let mut metadata = source_index_fixture_metadata(&root);
        metadata.packages[0].targets[0].src_path = root
            .join("member/src/promoted-unsupported-root.rs")
            .to_string_lossy()
            .into_owned()
            .into();

        let error = SourceIndex::from_metadata(&metadata).expect_err("literal include sources must still resolve");
        assert!(
            std::iter::successors(Some(&error as &(dyn std::error::Error + 'static)), |source| source.source())
                .any(<dyn std::error::Error + 'static>::is::<ResolveCoverageSourceError>)
        );
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn source_index_rejects_conflicting_editions_and_package_test_policy() {
        let root = source_index_fixture_root();
        let mut metadata = source_index_fixture_metadata(&root);
        let duplicate = metadata.packages[0].targets[0].clone();
        metadata.packages[0].targets.push(duplicate);
        let index = SourceIndex::from_metadata(&metadata).expect("duplicate target visit is ignored");
        let lib = fs::canonicalize(root.join("member/src/lib.rs")).expect("canonical library source");
        assert_eq!(
            index.files[&lib]
                .visit_contexts
                .iter()
                .filter(|(surface, role, _, _, _)| *surface == Surface::Released && *role == SourceRole::CrateRoot)
                .count(),
            1
        );

        let mut metadata = source_index_fixture_metadata(&root);
        metadata.packages[0].targets[1].src_path = metadata.packages[0].targets[0].src_path.clone();
        metadata.packages[0].targets[1].edition = CargoEdition::E2021;
        let error = SourceIndex::from_metadata(&metadata)
            .expect_err("one source cannot use conflicting editions")
            .to_string();
        assert!(error.contains("conflicting editions"), "{error}");

        let mut metadata = source_index_fixture_metadata(&root);
        metadata.packages[0].metadata = json!({"coverage-gate": {"test-attributes": ["company::test"]}});
        let error = SourceIndex::from_metadata(&metadata)
            .expect_err("package-scoped test attributes must fail")
            .to_string();
        assert!(error.contains("workspace-scoped"), "{error}");

        let member = root.join("member");
        let mut fail_member_resolution = |path: &Path| {
            if path == member {
                Err(ResolveCoverageSourceError::caused_by(
                    diagnostic_path(path),
                    std::io::Error::new(std::io::ErrorKind::PermissionDenied, "member resolution denied"),
                )
                .into())
            } else {
                resolve_source_path(path)
            }
        };
        let error = SourceIndex::from_metadata_with_source_resolver(&source_index_fixture_metadata(&root), &mut fail_member_resolution)
            .expect_err("member directory resolution errors propagate")
            .to_string();
        assert!(error.contains("member resolution denied"), "{error}");
    }

    #[test]
    #[cfg_attr(miri, ignore = "canonicalizes checked-in fixture paths, which Miri isolation forbids")]
    fn declaration_enqueue_resolution_errors_propagate_for_new_and_revisited_sources() {
        let root = source_index_fixture_root();
        let shared_suffix = Path::new("member/src/shared.rs");
        let mut fail_initial_enqueue = |path: &Path| {
            if path.ends_with(shared_suffix) {
                Err(ResolveCoverageSourceError::caused_by(
                    diagnostic_path(path),
                    std::io::Error::new(std::io::ErrorKind::PermissionDenied, "initial enqueue denied"),
                )
                .into())
            } else {
                resolve_source_path(path)
            }
        };
        let error = SourceIndex::from_metadata_with_source_resolver(&source_index_fixture_metadata(&root), &mut fail_initial_enqueue)
            .expect_err("initial declaration enqueue errors propagate")
            .to_string();
        assert!(error.contains("initial enqueue denied"), "{error}");

        let mut metadata = source_index_fixture_metadata(&root);
        metadata.packages[0].targets[1].src_path = metadata.packages[0].targets[0].src_path.clone();
        let attempts = std::cell::Cell::new(0);
        let mut fail_revisit_enqueue = |path: &Path| {
            if path.ends_with(shared_suffix) {
                let attempt = attempts.get() + 1;
                attempts.set(attempt);
                if attempt == 2 {
                    return Err(ResolveCoverageSourceError::caused_by(
                        diagnostic_path(path),
                        std::io::Error::new(std::io::ErrorKind::PermissionDenied, "revisit enqueue denied"),
                    )
                    .into());
                }
            }
            resolve_source_path(path)
        };
        let error = SourceIndex::from_metadata_with_source_resolver(&metadata, &mut fail_revisit_enqueue)
            .expect_err("revisited declaration enqueue errors propagate")
            .to_string();
        assert!(error.contains("revisit enqueue denied"), "{error}");
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn module_edge_cases_preserve_plain_resolution_and_reject_invalid_paths() {
        let root = PathBuf::from("/workspace/lib.rs");
        let child = PathBuf::from("/workspace/child.rs");
        let analysis = analyze(&root, "#[cfg_attr(test, cfg(unix))] mod child;\n");
        let found = declarations(&root, &analysis, SourceRole::CrateRoot, &BTreeSet::from([child.clone()]))
            .expect("non-path cfg_attr is supported");
        assert_eq!(lexical_normalize(&found[0].path), child);
        assert_eq!(
            owned_directory(Path::new("/workspace/outer/mod.rs"), SourceRole::PlainModule),
            Some(PathBuf::from("/workspace/outer"))
        );
        assert_eq!(owned_directory(Path::new(""), SourceRole::PlainModule), None);
        let empty_path_analysis = analyze(Path::new(""), "mod child;\n");
        assert!(
            declarations(Path::new(""), &empty_path_analysis, SourceRole::PlainModule, &BTreeSet::new())
                .expect("a path without an owned directory has no declarations")
                .is_empty()
        );
        let missing = analyze(&root, "mod missing;\n");
        assert!(
            declarations(&root, &missing, SourceRole::CrateRoot, &BTreeSet::new())
                .expect("a missing module source is ignored")
                .is_empty()
        );

        for source in [
            "#[path = 42] mod child;\n",
            "#[path = concat!(\"child.rs\")] mod child;\n",
            "#[path = \"child.rs\"] #[path = \"other.rs\"] mod child;\n",
        ] {
            let analysis = analyze(&root, source);
            let error = declarations(&root, &analysis, SourceRole::CrateRoot, &BTreeSet::new())
                .expect_err("invalid path attribute must fail")
                .to_string();
            assert!(
                error.contains("unsupported `#[path]`") || error.contains("multiple #[path]"),
                "{error}"
            );
        }

        let analysis = analyze(&root, "mod child;\n");
        let mut fail_inspection = |path: &Path| {
            Err(InspectCoverageSourceError::caused_by(
                diagnostic_path(path),
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, "inspection denied"),
            )
            .into())
        };
        let error = module_declarations(
            &root,
            analysis.syntax(),
            SourceRole::CrateRoot,
            &CfgFormula::Value(true),
            &BTreeSet::new(),
            &mut fail_inspection,
        )
        .expect_err("filesystem inspection failures must propagate");
        let inspection = std::iter::successors(Some(&error as &(dyn std::error::Error + 'static)), |source| source.source())
            .find_map(|source| source.downcast_ref::<InspectCoverageSourceError>())
            .expect("inspection error cause");
        assert!(inspection.path.contains("child.rs"), "{}", inspection.path);
        assert_eq!(
            std::error::Error::source(inspection).expect("inspection I/O cause").to_string(),
            "inspection denied"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "inspects checked-in fixture paths, which Miri isolation forbids")]
    fn source_path_inspection_treats_only_not_found_as_absent() {
        let root = source_index_fixture_root();
        assert!(source_path_is_file(&root.join("member/src/lib.rs")).expect("fixture file is inspectable"));
        assert!(!source_path_is_file(&root.join("member/src/missing.rs")).expect("missing file is absent"));

        let denied = classify_source_path_metadata(
            Path::new("denied.rs"),
            Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "inspection denied")),
        )
        .expect_err("permission errors must propagate")
        .to_string();
        assert!(denied.contains("inspection denied"), "{denied}");
        assert!(denied.contains("denied.rs"), "{denied}");

        assert!(
            !classify_source_path_metadata(
                Path::new("parent/mod.rs"),
                Err(std::io::Error::new(std::io::ErrorKind::NotADirectory, "parent is a file")),
            )
            .expect("not-a-directory means this module candidate is absent")
        );
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn inner_cfg_test_propagates_to_external_module_files() {
        let root = PathBuf::from("/workspace/src/lib.rs");
        let child = PathBuf::from("/workspace/src/child.rs");
        let analysis = analyze(&root, "#![cfg(test)]\nmod child;\n");
        let found = declarations(&root, &analysis, SourceRole::CrateRoot, &BTreeSet::from([child.clone()]))
            .expect("crate-root declarations resolve");
        assert_eq!(found.len(), 1);
        assert_eq!(lexical_normalize(&found[0].path), child);
        assert!(declaration_is_test_only(&found[0]));
        assert_eq!(found[0].role, SourceRole::PlainModule);

        let analysis = analyze(&root, "#[cfg(r#test)] mod child;\n");
        let found =
            declarations(&root, &analysis, SourceRole::CrateRoot, &BTreeSet::from([child])).expect("raw test cfg declaration resolves");
        assert!(declaration_is_test_only(&found[0]));

        let nested = PathBuf::from("/workspace/src/outer/nested.rs");
        let analysis = analyze(&root, "mod outer { #![cfg(test)] mod nested; }\n");
        let found =
            declarations(&root, &analysis, SourceRole::CrateRoot, &BTreeSet::from([nested.clone()])).expect("inline declarations resolve");
        assert_eq!(found.len(), 1);
        assert_eq!(lexical_normalize(&found[0].path), nested);
        assert!(declaration_is_test_only(&found[0]));

        let child = PathBuf::from("/workspace/src/outer/child.rs");
        let analysis = analyze(
            &root,
            "#[cfg(any(test, feature = \"x\"))]\n\
             #[cfg(any(test, not(feature = \"x\")))]\n\
             mod outer { mod child; }\n",
        );
        let found = declarations(&root, &analysis, SourceRole::CrateRoot, &BTreeSet::from([child])).expect("stacked module guards resolve");
        assert!(declaration_is_test_only(&found[0]));

        let child = PathBuf::from("/workspace/src/stacked_inner.rs");
        let analysis = analyze(
            &root,
            "#![cfg(any(test, feature = \"x\"))]\n\
             #![cfg(any(test, not(feature = \"x\")))]\n\
             mod stacked_inner;\n",
        );
        let found =
            declarations(&root, &analysis, SourceRole::CrateRoot, &BTreeSet::from([child])).expect("stacked inner guards propagate");
        assert!(declaration_is_test_only(&found[0]));

        let child = PathBuf::from("/workspace/src/cfg_attr.rs");
        let analysis = analyze(
            &root,
            "#[cfg(feature = \"x\")]\n\
             #[cfg_attr(feature = \"x\", cfg(test))]\n\
             mod cfg_attr;\n",
        );
        let found =
            declarations(&root, &analysis, SourceRole::CrateRoot, &BTreeSet::from([child])).expect("correlated cfg_attr guards propagate");
        assert!(declaration_is_test_only(&found[0]));

        let child = PathBuf::from("/workspace/src/guarded.rs");
        let analysis = analyze(&root, "#[cfg(any(test, feature = \"x\"))] mod guarded;\n");
        let found = declarations(&root, &analysis, SourceRole::CrateRoot, &BTreeSet::from([child.clone()]))
            .expect("guarded module declaration resolves");
        let child_analysis = analyze(
            &child,
            "#![cfg(any(test, not(feature = \"x\")))]\n\
             pub fn helper() {}\n",
        );
        assert_eq!(
            test_only_lines_for_guard(&child_analysis, &found[0].guard, &BTreeSet::new()),
            BTreeSet::from([1, 2])
        );
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn block_local_modules_are_discovered_with_enclosing_test_semantics() {
        let root = PathBuf::from("/workspace/src/lib.rs");
        let released = PathBuf::from("/workspace/src/released_helper.rs");
        let test_only = PathBuf::from("/workspace/src/test_helper.rs");
        let analysis = analyze(
            &root,
            "fn released() { #[path = \"released_helper.rs\"] mod helper; }\n\
             #[cfg(test)] fn tests() { #[path = \"test_helper.rs\"] mod helper; }\n",
        );
        let found = declarations(
            &root,
            &analysis,
            SourceRole::CrateRoot,
            &BTreeSet::from([released.clone(), test_only.clone()]),
        )
        .expect("block-local modules resolve");
        assert_eq!(
            found
                .into_iter()
                .map(|declaration| {
                    let test_only = declaration_is_test_only(&declaration);
                    (lexical_normalize(&declaration.path), test_only, declaration.role)
                })
                .collect::<Vec<_>>(),
            [(released, false, SourceRole::PathModule), (test_only, true, SourceRole::PathModule),]
        );
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn plain_and_path_modules_use_their_distinct_child_directories() {
        let source = PathBuf::from("/workspace/src/lib.rs");
        assert_eq!(owned_directory(&source, SourceRole::IncludedSource), None);
        let crate_root_child = PathBuf::from("/workspace/src/child.rs");
        let plain_child = PathBuf::from("/workspace/src/lib/child.rs");
        let plain_alternative = PathBuf::from("/workspace/src/lib/child/mod.rs");
        let path_child = PathBuf::from("/workspace/src/child.rs");
        let analysis = analyze(&source, "mod child;\n");
        let existing = BTreeSet::from([crate_root_child.clone(), plain_child.clone(), plain_alternative, path_child.clone()]);
        let root = declarations(&source, &analysis, SourceRole::CrateRoot, &existing)
            .expect("an ambiguity in an unused plain-module role must not fail the crate root");
        assert_eq!(root.len(), 1);
        assert_eq!(lexical_normalize(&root[0].path), crate_root_child);

        let error = declarations(&source, &analysis, SourceRole::PlainModule, &existing)
            .expect_err("both plain module candidates are ambiguous")
            .to_string();
        assert!(error.contains("ambiguous"), "{error}");

        let existing = BTreeSet::from([plain_child.clone(), path_child.clone()]);
        let plain = declarations(&source, &analysis, SourceRole::PlainModule, &existing).expect("plain module resolves");
        let path = declarations(&source, &analysis, SourceRole::PathModule, &existing).expect("path module resolves");

        assert_eq!(plain.len(), 1);
        assert_eq!(lexical_normalize(&plain[0].path), plain_child);
        assert_eq!(path.len(), 1);
        assert_eq!(lexical_normalize(&path[0].path), path_child);

        let root = PathBuf::from("/workspace/lib.rs");
        let path_target = PathBuf::from("/workspace/weird/file.rs");
        let analysis = analyze(&root, "#[path /* formatting trivia */ = \"weird/file.rs\"] mod alias;\n");
        let found = declarations(&root, &analysis, SourceRole::CrateRoot, &BTreeSet::from([path_target.clone()]))
            .expect("path declaration resolves");
        assert_eq!(found.len(), 1);
        assert_eq!(lexical_normalize(&found[0].path), path_target);
        assert_eq!(found[0].role, SourceRole::PathModule);
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn conditional_module_paths_fail_explicitly() {
        let root = PathBuf::from("/workspace/lib.rs");
        let analysis = analyze(&root, "#[cfg_attr(test, path = \"tests.rs\")] mod selected;\n");
        let error = declarations(&root, &analysis, SourceRole::CrateRoot, &BTreeSet::new())
            .expect_err("conditional path ownership must not be guessed")
            .to_string();

        assert!(error.contains("conditional path attribute"), "{error}");
        assert!(error.contains("not supported"), "{error}");
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn qualified_tool_path_attributes_do_not_override_module_sources() {
        let root = PathBuf::from("/workspace/lib.rs");
        let child = PathBuf::from("/workspace/child.rs");
        let ignored = PathBuf::from("/workspace/ignored.rs");
        let existing = BTreeSet::from([child.clone(), ignored]);

        for source in [
            "#[clippy::path = \"ignored.rs\"] mod child;\n",
            "#[cfg_attr(test, clippy::path = \"ignored.rs\")] mod child;\n",
        ] {
            let analysis = analyze(&root, source);
            let found =
                declarations(&root, &analysis, SourceRole::CrateRoot, &existing).expect("qualified tool attributes are not module paths");
            assert_eq!(found.len(), 1);
            assert_eq!(lexical_normalize(&found[0].path), child);
            assert_eq!(found[0].role, SourceRole::PlainModule);
        }
    }

    #[test]
    fn released_target_kinds_include_every_library_form_and_bins() {
        for kind in [
            TargetKind::Lib,
            TargetKind::RLib,
            TargetKind::DyLib,
            TargetKind::CDyLib,
            TargetKind::StaticLib,
            TargetKind::ProcMacro,
            TargetKind::Bin,
        ] {
            assert!(target_is_released(&[kind]));
        }
        for kind in [TargetKind::Test, TargetKind::Example, TargetKind::Bench, TargetKind::CustomBuild] {
            assert!(!target_is_released(&[kind]));
        }
        assert!(!target_is_released(&[TargetKind::Bin, TargetKind::Example]));
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    #[expect(
        clippy::too_many_lines,
        reason = "one table-shaped scenario keeps target ownership, cfg regions, and LCOV record filtering in one agreement test"
    )]
    fn filtering_keeps_only_released_targets_and_non_test_cfg_regions() {
        let package = PathBuf::from("/workspace/surface");
        let lib = package.join("src/lib.rs");
        let unit_support = package.join("src/unit_support.rs");
        let shared_support = package.join("tests/support/mod.rs");
        let bin = package.join("src/main.rs");
        let integration = package.join("tests/integration.rs");
        let example = package.join("examples/demo.rs");
        let bench = package.join("benches/speed.rs");
        let build = package.join("build.rs");
        let existing = BTreeSet::from([unit_support.clone(), shared_support.clone()]);
        let lib_analysis = analyze(
            &lib,
            "pub fn released() {}\n\
             #[cfg(test)] pub fn cfg_test() {}\n\
             #[cfg(all(test, unix))] pub fn cfg_all_test() {}\n\
             #[cfg(any(test, feature = \"test-util\"))] pub fn cfg_any_test() {}\n\
             #[cfg(feature = \"test-util\")] pub fn feature_test_util() {}\n\
             #[cfg(not(test))] pub fn not_test() {}\n\
             #[test] fn direct_test() {}\n\
             #[cfg(test)] mod unit_support;\n\
             #[cfg(any(test, feature = \"test-util\"))] #[path = \"../tests/support/mod.rs\"] mod shared_support;\n",
        );
        let lib_declarations = declarations(&lib, &lib_analysis, SourceRole::CrateRoot, &existing).expect("library declarations resolve");
        assert_eq!(
            lib_declarations
                .iter()
                .map(|declaration| {
                    (
                        lexical_normalize(&declaration.path),
                        declaration_is_test_only(declaration),
                        declaration.role,
                    )
                })
                .collect::<Vec<_>>(),
            [
                (unit_support.clone(), true, SourceRole::PlainModule),
                (shared_support.clone(), false, SourceRole::PathModule),
            ]
        );
        let integration_analysis = analyze(&integration, "mod support;\n#[test] fn integration() {}\n");
        let integration_declarations = declarations(&integration, &integration_analysis, SourceRole::CrateRoot, &existing)
            .expect("integration declarations resolve")
            .iter()
            .map(|declaration| lexical_normalize(&declaration.path))
            .collect::<Vec<_>>();
        assert_eq!(integration_declarations.len(), 1);
        assert_eq!(integration_declarations[0], shared_support);

        let sources = BTreeMap::from([
            (
                lib.clone(),
                FixtureSource {
                    released: true,
                    analysis: lib_analysis,
                },
            ),
            (
                unit_support.clone(),
                FixtureSource {
                    released: false,
                    analysis: analyze(&unit_support, "pub fn helper() {}\npub fn missed_helper() {}\n"),
                },
            ),
            (
                shared_support.clone(),
                FixtureSource {
                    // A released cfg path wins even when a non-release test target also reaches the file.
                    released: true,
                    analysis: analyze(&shared_support, "pub fn released_test_util() {}\n"),
                },
            ),
            (
                bin.clone(),
                FixtureSource {
                    released: true,
                    analysis: analyze(&bin, "fn main() {}\n"),
                },
            ),
            (
                integration.clone(),
                FixtureSource {
                    released: false,
                    analysis: integration_analysis,
                },
            ),
            (
                example.clone(),
                FixtureSource {
                    released: false,
                    analysis: analyze(&example, "fn main() {}\n"),
                },
            ),
            (
                bench.clone(),
                FixtureSource {
                    released: false,
                    analysis: analyze(&bench, "fn main() {}\n"),
                },
            ),
            (
                build.clone(),
                FixtureSource {
                    released: false,
                    analysis: analyze(&build, "fn main() {}\n"),
                },
            ),
        ]);

        let mut input = String::new();
        write!(
            &mut input,
            "TN:\nSF:{}\n\
             FN:1,released\nFNDA:1,released\n\
             FN:2,cfg_test\nFNDA:0,cfg_test\n\
             FN:4,cfg_any_test\nFNDA:0,cfg_any_test\n\
             BRDA:2,0,0,0\nBRDA:4,0,0,-\n\
             DA:1,1\nDA:2,0\nDA:3,0\nDA:4,0\nDA:5,0\nDA:6,1\nDA:7,1\nDA:8,1\nDA:9,1\nend_of_record\n",
            lib.display()
        )
        .expect("write to String");
        input.push_str(&lcov_section(&unit_support, &[(1, 1), (2, 0)]));
        input.push_str(&lcov_section(&shared_support, &[(1, 0)]));
        input.push_str(&lcov_section(&bin, &[(1, 0)]));
        input.push_str(&lcov_section(&integration, &[(1, 1), (2, 1)]));
        input.push_str(&lcov_section(&example, &[(1, 1)]));
        input.push_str(&lcov_section(&bench, &[(1, 1)]));
        input.push_str(&lcov_section(&build, &[(1, 1)]));

        let filtered = filter_report_with(parse_lcov(&input), None, |path| {
            let source = sources
                .get(path)
                .ok_or_else(|| UnownedCoverageSourceError::new(path.display().to_string()))?;
            if source.released {
                Ok(SourceDisposition::Include {
                    source_file: path.to_path_buf(),
                    line_count: source.analysis.line_count,
                    test_only_lines: &source.analysis.test_only_lines,
                })
            } else {
                Ok(SourceDisposition::Exclude {
                    line_count: source.analysis.line_count,
                })
            }
        })
        .expect("released-surface filtering succeeds");
        let report = parse_lcov(&filtered);
        let by_path = report
            .sections
            .iter()
            .map(|(key, section)| (key.source_file.clone(), section))
            .collect::<BTreeMap<_, _>>();

        assert_eq!(
            by_path[&lib].lines.keys().map(|line| line.line).collect::<Vec<_>>(),
            [1, 4, 5, 6, 9]
        );
        assert!(by_path[&lib].functions.is_empty());
        assert!(by_path[&lib].branches.is_empty());
        assert_eq!(by_path[&shared_support].lines.values().next().expect("shared line").count, 0);
        assert!(by_path.contains_key(&bin));
        for excluded in [unit_support, integration, example, bench, build] {
            assert!(!by_path.contains_key(&excluded), "{excluded:?} must be excluded");
        }
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn unsupported_source_states_fail_instead_of_hiding_counters() {
        let lib = PathBuf::from("/workspace/surface/src/lib.rs");
        let orphan = PathBuf::from("/workspace/surface/src/orphan.rs");
        let unowned = lcov_section(&orphan, &[(1, 0)]);
        let error = filter_report_with(parse_lcov(&unowned), None, |path| {
            Err::<SourceDisposition<'static>, CoverageGateError>(UnownedCoverageSourceError::new(path.display().to_string()).into())
        })
        .expect_err("unowned source must fail")
        .to_string();
        assert!(error.contains("not reachable from any Cargo target"), "{error}");

        let stale = lcov_section(&lib, &[(3, 0)]);
        let test_only = BTreeSet::new();
        let error = filter_report_with(parse_lcov(&stale), None, |_| {
            Ok(SourceDisposition::Include {
                source_file: lib.clone(),
                line_count: 2,
                test_only_lines: &test_only,
            })
        })
        .expect_err("stale line number must fail")
        .to_string();
        assert!(error.contains("references line 3"), "{error}");
        assert!(error.contains("regenerate coverage"), "{error}");

        let error = filter_report_with(parse_lcov(&stale), None, |_| Ok(SourceDisposition::Exclude { line_count: 2 }))
            .expect_err("stale line data must fail even when the target is excluded")
            .to_string();
        assert!(error.contains("references line 3"), "{error}");

        let missing_function_line = format!("TN:\nSF:{}\nFNDA:0,unknown\nDA:1,0\nend_of_record\n", lib.display());
        let filtered = filter_report_with(parse_lcov(&missing_function_line), None, |_| {
            Ok(SourceDisposition::Include {
                source_file: lib.clone(),
                line_count: 2,
                test_only_lines: &test_only,
            })
        })
        .expect("function records are removed from line-only filtered LCOV");
        let report = parse_lcov(&filtered);
        let section = report.sections.values().next().expect("filtered section remains");
        assert!(section.functions.is_empty());

        let error = analyze_source_text(
            &lib,
            "pub fn valid() {}\npub fn broken( {\n",
            Edition::Edition2024,
            &BTreeSet::new(),
        )
        .expect_err("invalid Rust source must fail")
        .to_string();
        assert!(error.contains("could not parse Rust source"), "{error}");
        assert!(error.contains("at 2:"), "{error}");
        assert_eq!(error.matches("at 2:15").count(), 2, "{error}");
        assert_eq!(error.matches("at 2:17").count(), 1, "{error}");

        let error = parse_reports(&["not lcov"])
            .expect_err("malformed LCOV must fail before metadata discovery")
            .to_string();
        assert!(error.contains("lcov tracefile"), "{error}");
    }

    #[test]
    fn retained_source_paths_are_normalized_for_package_attribution() {
        let normalized = PathBuf::from("/workspace/surface/src/lib.rs");
        let test_only = BTreeSet::new();
        let filtered = filter_report_with(parse_lcov("TN:\nSF:surface/src/lib.rs\nDA:1,1\nend_of_record\n"), None, |_| {
            Ok(SourceDisposition::Include {
                source_file: normalized.clone(),
                line_count: 1,
                test_only_lines: &test_only,
            })
        })
        .expect("source path normalization succeeds");
        let report = parse_lcov(&filtered);

        assert_eq!(
            report.sections.keys().map(|key| key.source_file.as_path()).collect::<Vec<_>>(),
            [normalized.as_path()]
        );
    }

    #[test]
    fn attributed_paths_use_the_longest_member_and_reported_namespace() {
        let member_dirs = MemberDirectories {
            reported_by_canonical: BTreeMap::from([
                (PathBuf::from("/canonical/outer/nested"), PathBuf::from("/reported/nested")),
                (PathBuf::from("/canonical/outer"), PathBuf::from("/reported/outer")),
            ]),
        };

        assert_eq!(
            attributed_path(&member_dirs, Path::new("/canonical/outer/nested/src/lib.rs")),
            Some(PathBuf::from("/reported/nested/src/lib.rs"))
        );
    }

    #[test]
    fn normalized_source_sections_merge_without_replacing_counters() {
        let normalized = PathBuf::from("/workspace/surface/src/lib.rs");
        let test_only = BTreeSet::new();
        let input = "TN:same\nSF:/a/spelling/lib.rs\nDA:1,1,checksum\nDA:2,0\nend_of_record\n\
                     TN:same\nSF:/z/spelling/lib.rs\nDA:1,2,checksum\nDA:2,1\nDA:3,0\nend_of_record\n";
        let filtered = filter_report_with(parse_lcov(input), None, |_| {
            Ok(SourceDisposition::Include {
                source_file: normalized.clone(),
                line_count: 3,
                test_only_lines: &test_only,
            })
        })
        .expect("normalized sections merge");
        let report = parse_lcov(&filtered);
        let section = report.sections.values().next().expect("one merged section");
        assert_eq!(report.sections.len(), 1);
        assert_eq!(section.lines.len(), 3);
        assert_eq!(section.lines.values().map(|line| line.count).collect::<Vec<_>>(), [3, 1, 0]);
        assert_eq!(
            section.lines.values().next().expect("line one").checksum.as_deref(),
            Some("checksum")
        );
    }

    #[test]
    fn normalized_sections_preserve_and_fill_optional_checksums_and_saturate_counts() {
        for (first, second, expected) in [
            ("", "", None),
            (",digest", "", Some("digest")),
            ("", ",digest", Some("digest")),
            (",digest", ",digest", Some("digest")),
        ] {
            let input = format!(
                "TN:same\nSF:first.rs\nDA:1,18446744073709551615{first}\nend_of_record\n\
                 TN:same\nSF:second.rs\nDA:1,2{second}\nend_of_record\n"
            );
            let excluded = BTreeSet::new();
            let filtered = filter_report_with(parse_lcov(&input), None, |_| {
                Ok(SourceDisposition::Include {
                    source_file: PathBuf::from("normalized.rs"),
                    line_count: 1,
                    test_only_lines: &excluded,
                })
            })
            .unwrap();
            let report = parse_lcov(&filtered);
            assert_eq!(report.sections.len(), 1);
            let (key, section) = report.sections.iter().next().unwrap();
            assert_eq!(key.source_file, Path::new("normalized.rs"));
            assert_eq!(key.test_name, "same");
            assert_eq!(section.lines.len(), 1);
            let line = section.lines.values().next().unwrap();
            assert_eq!(line.count, u64::MAX);
            assert_eq!(line.checksum.as_deref(), expected, "{first} / {second}");
        }
    }

    #[test]
    fn cfg_assignment_carries_across_every_bit_and_wraps_only_after_all_assignments() {
        let mut assignment = [false; 3];
        for expected in [
            [true, false, false],
            [false, true, false],
            [true, true, false],
            [false, false, true],
            [true, false, true],
            [false, true, true],
            [true, true, true],
            [false, false, false],
        ] {
            advance_cfg_assignment(&mut assignment);
            assert_eq!(assignment, expected);
        }
        advance_cfg_assignment(&mut []);
    }

    #[test]
    fn compiled_cfg_formulas_preserve_three_variable_truth_tables() {
        let indexes = BTreeMap::from([("a".to_owned(), 2), ("b".to_owned(), 0), ("c".to_owned(), 1)]);
        let a = CfgFormula::Atom("a".into());
        let b = CfgFormula::Atom("b".into());
        let c = CfgFormula::Atom("c".into());
        let formula = CfgFormula::Any(vec![
            CfgFormula::All(vec![a.clone(), CfgFormula::Not(Box::new(b.clone()))]),
            CfgFormula::All(vec![CfgFormula::Not(Box::new(a)), c]),
        ]);
        let expression = compile_cfg_expression(&formula, &indexes);
        for (assignment, expected) in [
            ([false, false, false], false),
            ([true, false, false], false),
            ([false, true, false], true),
            ([true, true, false], true),
            ([false, false, true], true),
            ([true, false, true], false),
            ([false, true, true], true),
            ([true, true, true], false),
        ] {
            assert_eq!(expression.evaluate(&assignment), expected, "{assignment:?}");
        }
        assert_eq!(cfg_formula_possibilities_without_test(&formula), CfgPossibilities::EITHER);
        let contradiction = CfgFormula::All(vec![b.clone(), CfgFormula::Not(Box::new(b.clone()))]);
        let tautology = CfgFormula::Any(vec![b.clone(), CfgFormula::Not(Box::new(b))]);
        assert_eq!(cfg_formula_possibilities_without_test(&contradiction), CfgPossibilities::FALSE);
        assert_eq!(cfg_formula_possibilities_without_test(&tautology), CfgPossibilities::TRUE);
    }

    #[test]
    fn approximate_cfg_combinations_keep_mixed_true_false_and_unknown_possibilities() {
        let unknown = CfgFormula::Atom("feature=optional".into());
        for (formula, expected) in [
            (
                CfgFormula::All(vec![CfgFormula::Value(false), unknown.clone()]),
                CfgPossibilities::FALSE,
            ),
            (
                CfgFormula::All(vec![CfgFormula::Value(true), unknown.clone()]),
                CfgPossibilities::EITHER,
            ),
            (
                CfgFormula::Any(vec![CfgFormula::Value(true), unknown.clone()]),
                CfgPossibilities::TRUE,
            ),
            (
                CfgFormula::Any(vec![CfgFormula::Value(false), unknown.clone()]),
                CfgPossibilities::EITHER,
            ),
            (CfgFormula::Not(Box::new(unknown)), CfgPossibilities::EITHER),
        ] {
            assert_eq!(approximate_cfg_possibilities_without_test(&formula), expected, "{formula:?}");
        }
    }

    #[test]
    fn cfg_canonicalization_retains_distinct_nested_terms_in_sorted_order() {
        let a = CfgFormula::Atom("a".into());
        let b = CfgFormula::Atom("b".into());
        let c = CfgFormula::Atom("c".into());
        assert_eq!(
            CfgFormula::all([
                b.clone(),
                CfgFormula::All(vec![c.clone(), a.clone()]),
                a.clone(),
                CfgFormula::Value(true)
            ]),
            CfgFormula::All(vec![a.clone(), b.clone(), c.clone()])
        );
        assert_eq!(
            CfgFormula::any([
                b.clone(),
                CfgFormula::Any(vec![c.clone(), a.clone()]),
                a.clone(),
                CfgFormula::Value(false)
            ]),
            CfgFormula::Any(vec![a, b, c])
        );
    }

    #[test]
    fn sections_with_only_test_lines_are_removed() {
        let normalized = PathBuf::from("/workspace/surface/src/lib.rs");
        let test_only = BTreeSet::from([1]);
        let filtered = filter_report_with(parse_lcov("TN:\nSF:surface/src/lib.rs\nDA:1,1\nend_of_record\n"), None, |_| {
            Ok(SourceDisposition::Include {
                source_file: normalized.clone(),
                line_count: 1,
                test_only_lines: &test_only,
            })
        })
        .expect("test-only section filters");

        assert!(parse_lcov(&filtered).sections.is_empty());
    }

    #[test]
    fn normalized_source_sections_reject_conflicting_checksums() {
        let normalized = PathBuf::from("/workspace/surface/src/lib.rs");
        let test_only = BTreeSet::new();
        let input = "TN:same\nSF:surface/src/lib.rs\nDA:1,1,first\nend_of_record\n\
                     TN:same\nSF:/other/spelling/lib.rs\nDA:1,2,second\nend_of_record\n";
        let error = filter_report_with(parse_lcov(input), None, |_| {
            Ok(SourceDisposition::Include {
                source_file: normalized.clone(),
                line_count: 1,
                test_only_lines: &test_only,
            })
        })
        .expect_err("conflicting checksums must fail")
        .to_string();

        assert!(error.contains("conflicting checksums"), "{error}");
        assert!(error.contains("line 1"), "{error}");
    }

    #[test]
    #[cfg_attr(miri, ignore = "canonicalizes a missing path, which Miri isolation forbids")]
    fn lcov_control_characters_are_encoded_in_filter_diagnostics() {
        let controlled = "bad\u{1b}[2J";
        let normalized = PathBuf::from(format!("/workspace/{controlled}.rs"));
        let test_only = BTreeSet::new();
        let input = format!(
            "TN:{controlled}\nSF:first.rs\nDA:1,1,first\nend_of_record\n\
             TN:{controlled}\nSF:second.rs\nDA:1,2,second\nend_of_record\n"
        );
        let error = filter_report_with(parse_lcov(&input), None, |_| {
            Ok(SourceDisposition::Include {
                source_file: normalized.clone(),
                line_count: 1,
                test_only_lines: &test_only,
            })
        })
        .expect_err("controlled checksum conflict must fail")
        .to_string();
        assert!(error.contains(r"bad\u{1b}[2J"), "{error:?}");
        assert!(!error.contains('\u{1b}'), "{error:?}");

        let error = resolve_source_path(Path::new(controlled))
            .expect_err("missing controlled path must fail")
            .to_string();
        assert!(error.contains(r"bad\u{1b}[2J"), "{error:?}");
        assert!(!error.contains('\u{1b}'), "{error:?}");
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn survivor_policy_errors_identify_the_workspace_and_rejected_value() {
        for value in [json!("not-an-array"), json!([17])] {
            let error = parse_configured_test_attributes(Some(&value)).unwrap_err().to_string();
            let rejected = if value.is_array() { json!(17) } else { value };
            assert!(
                error.contains(&format!(
                    "workspace: `coverage-gate.test-attributes` must be an array of Rust attribute paths, got {rejected}"
                )),
                "{error}"
            );
        }
        assert_eq!(
            parse_configured_test_attribute_for_edition("fn without_attributes() {}", "#[test]", Edition::Edition2024),
            None
        );
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn survivor_source_diagnostics_preserve_all_paths_and_line_bounds() {
        let path = Path::new("survivor-source.rs");
        for line in [0, 4] {
            let error = validate_line(path, line, 3).unwrap_err().to_string();
            assert!(error.contains("`survivor-source.rs`"), "{error}");
            assert!(error.contains(&format!("references line {line}")), "{error}");
            assert!(error.contains("source has 3 lines"), "{error}");
        }
        validate_line(path, 1, 3).unwrap();
        validate_line(path, 3, 3).unwrap();
        let error = analyze_source_text(
            path,
            "pub fn valid() {}\npub fn broken( {\n",
            Edition::Edition2024,
            &BTreeSet::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("`survivor-source.rs`"), "{error}");
        assert!(error.contains("; "), "{error}");
        assert!(!error.contains("xyzzy"), "{error}");

        let pending = pending_source(Surface::Released, SourceRole::CrateRoot, path, CfgFormula::Value(true));
        let mut indexed = IndexedSource {
            analysis: analyze(path, "fn released() {}\n"),
            edition: Edition::Edition2024,
            released_path: Some(PathBuf::from("first-owner.rs")),
            test_only_lines: BTreeSet::new(),
            visit_contexts: BTreeSet::new(),
        };
        let error = indexed
            .add_visit(&pending, Some(PathBuf::from("second-owner.rs")))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("`survivor-source.rs` is shared by workspace packages as both `first-owner.rs` and `second-owner.rs`"),
            "{error}"
        );
        let mut conflicting = pending.clone();
        conflicting.edition = Edition::Edition2021;
        let directories = MemberDirectories {
            reported_by_canonical: BTreeMap::new(),
        };
        let error = SourceIndex::revisit_indexed_source(&mut indexed, &conflicting, &directories, &BTreeSet::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains("`survivor-source.rs`"), "{error}");
        assert!(error.contains("`Edition2024` and `Edition2021`"), "{error}");

        indexed.add_visit(&pending, None).unwrap();
        assert!(
            SourceIndex::revisit_indexed_source(&mut indexed, &pending, &directories, &BTreeSet::new())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn survivor_module_diagnostics_preserve_ordered_candidates_and_attribute_details() {
        let root = Path::new("survivor/lib.rs");
        let analysis = analyze(root, "mod child;\n");
        let error = declarations(
            root,
            &analysis,
            SourceRole::CrateRoot,
            &BTreeSet::from([PathBuf::from("survivor/child.rs"), PathBuf::from("survivor/child/mod.rs")]),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains(&format!(
                "declared in `survivor/lib.rs` is ambiguous: both `{}` and `{}` exist",
                root.parent().unwrap().join("child.rs").display(),
                root.parent().unwrap().join("child").join("mod.rs").display()
            )),
            "{error}"
        );
        for (source, detail) in [
            (
                "#[cfg_attr(test, path = \"x.rs\", allow(dead_code))] mod child;",
                "conditional path attribute",
            ),
            ("#[path = 42] mod child;", "unsupported `#[path]` attribute"),
            ("#[path = \"a.rs\"] #[path = \"b.rs\"] mod child;", "multiple #[path] attributes"),
        ] {
            let analysis = analyze(root, source);
            let error = declarations(root, &analysis, SourceRole::CrateRoot, &BTreeSet::new())
                .unwrap_err()
                .to_string();
            assert!(error.contains("declared in `survivor/lib.rs`"), "{error}");
            assert!(error.contains(detail), "{error}");
        }
        let analysis = analyze(root, "#[allow(dead_code)] #[path = \"renamed.rs\"] mod child;\nmod r#type;\n");
        let found = declarations(
            root,
            &analysis,
            SourceRole::CrateRoot,
            &BTreeSet::from([PathBuf::from("survivor/renamed.rs"), PathBuf::from("survivor/type.rs")]),
        )
        .unwrap();
        assert_eq!(
            found
                .iter()
                .map(|declaration| lexical_normalize(&declaration.path))
                .collect::<Vec<_>>(),
            [PathBuf::from("survivor/renamed.rs"), PathBuf::from("survivor/type.rs")]
        );
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn survivor_cfg_atoms_preserve_raw_names_values_and_invalid_literals() {
        for (text, expected) in [
            ("r#feature = \"yes\"", Some(CfgFormula::Atom("feature=yes".into()))),
            ("test = \"yes\"", Some(CfgFormula::Atom("test=yes".into()))),
            ("r#test", Some(CfgFormula::Value(false))),
            ("feature = \"\\q\"", None),
        ] {
            let parse = SourceFile::parse(&format!("#[cfg({text})] fn item() {{}}"), Edition::Edition2024);
            let predicate = parse.tree().syntax().descendants().find_map(ast::CfgPredicate::cast).unwrap();
            assert_eq!(cfg_formula_from_predicate(&predicate), expected, "{text}");
        }
        let parse = SourceFile::parse("#[cfg(feature =)] fn item() {}", Edition::Edition2024);
        let predicate = parse.tree().syntax().descendants().find_map(ast::CfgPredicate::cast).unwrap();
        assert_eq!(cfg_formula_from_predicate(&predicate), None);
        let ast::CfgPredicate::CfgAtom(atom) = predicate else {
            panic!("a key without a value remains an atom")
        };
        assert_eq!(cfg_atom_key(&atom), None);
        let parse = SourceFile::parse("#[cfg(true)] fn item() {}", Edition::Edition2024);
        let atom = parse.tree().syntax().descendants().find_map(ast::CfgAtom::cast).unwrap();
        assert_eq!(cfg_atom_key(&atom), None);
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn survivor_include_tokens_distinguish_builtin_paths_and_static_arguments() {
        for (path, expected) in [
            ("include", true),
            ("::include", true),
            ("std::include", true),
            ("core::nested::include", true),
            ("my::include", false),
            ("xyzzy::include", false),
            ("stdx::include", false),
            ("std::other", false),
            ("core::nested::other", false),
        ] {
            let parse = SourceFile::parse(&format!("{path}!(\"fragment.rs\");"), Edition::Edition2024);
            let call = parse.tree().syntax().descendants().find_map(ast::MacroCall::cast).unwrap();
            assert_eq!(path_is_builtin_include(&call.path().unwrap()), expected, "{path}");
        }
        for (arguments, expected) in [
            ("", None),
            ("42", None),
            ("\"fragment.rs\"", Some("fragment.rs")),
            ("\"fragment.rs\",", Some("fragment.rs")),
            ("\"fragment.rs\" other", None),
            ("\"fragment.rs\", other", None),
        ] {
            let parse = SourceFile::parse(&format!("include!({arguments});"), Edition::Edition2024);
            let call = parse.tree().syntax().descendants().find_map(ast::MacroCall::cast).unwrap();
            assert_eq!(static_include_path(&call), expected.map(PathBuf::from), "{arguments}");
        }
        let parse = SourceFile::parse("include!;", Edition::Edition2024);
        let call = parse.tree().syntax().descendants().find_map(ast::MacroCall::cast).unwrap();
        assert_eq!(static_include_path(&call), None);
        let analysis = analyze(Path::new("root.rs"), "fn outer() { if true { include!(\"fragment.rs\"); } }\n");
        assert!(syntax_contains_opaque_source_macro(
            analysis.syntax().syntax(),
            &BTreeSet::new(),
            &CfgFormula::Value(true),
            CfgPossibilities::TRUE
        ));
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn survivor_cached_guard_lines_and_range_boundaries_are_exact() {
        let mut analysis = analyze(Path::new("root.rs"), "#[test]\nfn example() {}\nfn released() {}\n");
        analysis.test_only_lines = BTreeSet::from([91]);
        assert_eq!(
            test_only_lines_for_guard(&analysis, &CfgFormula::Value(true), &BTreeSet::new()),
            BTreeSet::from([91])
        );
        let mut flags = [0; 3];
        mark_range_lines(&mut flags, &[0, 4, 8], TextRange::new(4.into(), 8.into()), LINE_TEST_ONLY);
        assert_eq!(flags, [0, LINE_TEST_ONLY, 0]);
        let analysis = analyze(Path::new("root.rs"), "#[test]\nfn example() {}fn released() {}\n");
        assert_eq!(analysis.test_only_lines, BTreeSet::from([1]));
        let parse = SourceFile::parse("#[test]\nfn example() {}\n\n// outside\nfn released() {}", Edition::Edition2024);
        let item = parse.tree().items().next().unwrap();
        let range = syntax_owner_range(item.syntax());
        assert_eq!(
            &parse.tree().syntax().text().to_string()[std::ops::Range::<usize>::from(range)],
            "#[test]\nfn example() {}"
        );
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn classify_test_only_lines_advances_across_disjoint_test_ranges_and_released_tokens() {
        let analysis = analyze(
            Path::new("root.rs"),
            "#[test]\nfn first() {\n    assert!(true);\n}\nfn released() {}\n#[test]\nfn second() {}\nfn tail() {}\n",
        );
        assert_eq!(analysis.test_only_lines, BTreeSet::from([1, 2, 3, 4, 6, 7]));
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn module_path_attribute_keeps_searching_after_non_key_value_metadata() {
        for attribute in ["#[allow(dead_code)]", "#[cold]", "#[doc = \"another attribute\"]"] {
            let source = format!("{attribute}\n#[path = \"actual.rs\"] mod child;");
            let parse = SourceFile::parse(&source, Edition::Edition2024);
            let module = parse.tree().syntax().descendants().find_map(ast::Module::cast).unwrap();
            assert_eq!(
                module_path_attribute(&module, Path::new("root.rs"), "child").unwrap(),
                Some(PathBuf::from("actual.rs"))
            );
        }
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn module_walker_preserves_inherited_guards_through_unattributed_syntax() {
        let root = Path::new("root.rs");
        let analysis = analyze(root, "#[cfg(feature = \"outer\")]\nfn outer() { { mod child; } }\n");
        let child = PathBuf::from("child.rs");
        let found = declarations(root, &analysis, SourceRole::CrateRoot, &BTreeSet::from([child.clone()])).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(lexical_normalize(&found[0].path), child);
        assert_eq!(found[0].guard, CfgFormula::Atom("feature=outer".to_owned()));
    }

    #[test]
    fn survivor_missing_sources_return_errors_without_panicking() {
        let missing = missing_source_path();
        let mut read = |path: &Path| {
            assert_eq!(path, missing);
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        };
        for error in [
            analyze_source_with_reader(&missing, Edition::Edition2024, &BTreeSet::new(), &mut read).unwrap_err(),
            analyze_included_source_with_reader(&missing, &mut read).unwrap_err(),
        ] {
            let message = error.to_string();
            assert!(message.contains(&diagnostic_path(&missing)), "{message}");
            assert!(message.contains("failed to read Rust source"), "{message}");
        }
        let index = SourceIndex {
            workspace_root: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            files: BTreeMap::new(),
            member_dirs: MemberDirectories {
                reported_by_canonical: BTreeMap::new(),
            },
            uncertain_sources: BTreeSet::new(),
        };
        let error = index
            .source_for_lcov_with_resolver(&missing, |path| {
                Err(ResolveCoverageSourceError::caused_by(diagnostic_path(path), std::io::Error::from(std::io::ErrorKind::NotFound)).into())
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("failed to resolve Rust source path"), "{error}");
        let present = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs");
        let canonical = PathBuf::from("canonical/lib.rs");
        let (resolved, source) = index.source_for_lcov_with_resolver(&present, |_| Ok(canonical.clone())).unwrap();
        assert_eq!(resolved, canonical);
        assert!(source.is_none());
        let text = "fn released() {}\nfn another() {}\n";
        let analysis = analyze_included_source_with_reader(&present, &mut |_| Ok(text.to_owned())).unwrap();
        assert_eq!(analysis.line_starts, line_starts(text));
        assert!(analysis.line_starts.len() > 1);
    }

    #[test]
    fn survivor_metadata_resolution_failures_are_returned_and_empty_reports_do_not_resolve() {
        let root = source_index_fixture_root();
        let metadata = source_index_fixture_metadata(&root);
        let error = SourceIndex::from_metadata_with_source_resolver(&metadata, &mut |path| {
            Err(ResolveCoverageSourceError::caused_by(
                diagnostic_path(path),
                std::io::Error::other("injected workspace resolution failure"),
            )
            .into())
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("injected workspace resolution failure"), "{error}");
        let target = metadata.packages[0].targets[0].src_path.as_std_path();
        let error = source_roots(&metadata, &mut |path| {
            if path == target {
                Err(ResolveCoverageSourceError::caused_by(
                    diagnostic_path(path),
                    std::io::Error::other("injected target resolution failure"),
                )
                .into())
            } else {
                Ok(path.to_path_buf())
            }
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("injected target resolution failure"), "{error}");
        let mut absent = source_index_fixture_metadata(&root.join("absent"));
        assert!(filter_reports_with_metadata(Vec::new(), &absent).unwrap().is_empty());
        absent.packages[0].targets[0].src_path = missing_source_path().try_into().unwrap();
        let parent = absent.packages[0].targets[0].src_path.parent().unwrap().to_path_buf();
        let error = SourceIndex::from_metadata_with_source_reader(
            &absent,
            &mut |path| {
                if path == parent.as_std_path() {
                    Err(
                        ResolveCoverageSourceError::caused_by(diagnostic_path(path), std::io::Error::from(std::io::ErrorKind::NotFound))
                            .into(),
                    )
                } else {
                    Ok(path.to_path_buf())
                }
            },
            &mut |_| panic!("resolution failure must precede reading"),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("failed to resolve Rust source path"), "{error}");
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn survivor_index_read_and_promotion_errors_preserve_the_missing_source() {
        let root = source_index_fixture_root();
        let missing = missing_source_path();
        for (entry, failed) in [("lib.rs", "lib.rs"), ("test-include-root.rs", "included-test.rs")] {
            let mut metadata = source_index_fixture_metadata(&root);
            metadata.packages[0].targets.truncate(1);
            metadata.packages[0].targets[0].src_path = root.join("member/src").join(entry).try_into().unwrap();
            let error = SourceIndex::from_metadata_with_source_reader(
                &metadata,
                &mut |path| {
                    if path.file_name().is_some_and(|name| name == failed) {
                        Ok(missing.clone())
                    } else {
                        Ok(path.to_path_buf())
                    }
                },
                &mut |path| {
                    if path == missing {
                        Err(std::io::Error::from(std::io::ErrorKind::NotFound))
                    } else {
                        assert_eq!(path.file_name().unwrap(), "test-include-root.rs");
                        Ok("#[cfg(test)]\ninclude!(\"included-test.rs\");".to_owned())
                    }
                },
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("failed to read Rust source"), "{entry}: {error}");
            assert!(error.contains(&diagnostic_path(&missing)), "{entry}: {error}");
        }
        let mut indexed = IndexedSource {
            analysis: SourceAnalysis {
                syntax: None,
                line_count: 1,
                line_starts: vec![0],
                test_only_lines: BTreeSet::new(),
            },
            edition: Edition::Edition2024,
            released_path: None,
            test_only_lines: BTreeSet::new(),
            visit_contexts: BTreeSet::new(),
        };
        let pending = pending_source(Surface::Released, SourceRole::PlainModule, &missing, CfgFormula::Value(true));
        let directories = MemberDirectories {
            reported_by_canonical: BTreeMap::new(),
        };
        let error = SourceIndex::revisit_indexed_source_with_reader(&mut indexed, &pending, &directories, &BTreeSet::new(), &mut |path| {
            assert_eq!(path, missing);
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains(&diagnostic_path(&missing)), "{error}");
        assert!(error.contains("failed to read Rust source"), "{error}");

        indexed.analysis = analyze(Path::new("isolated.rs"), "fn released() {}");
        let pending = pending_source(
            Surface::Released,
            SourceRole::CrateRoot,
            Path::new("isolated.rs"),
            CfgFormula::Value(true),
        );
        let error = released_source_attributed_path(&directories, &pending).unwrap_err().to_string();
        assert!(error.contains("`isolated.rs` cannot be attributed"), "{error}");
        let error = SourceIndex::revisit_indexed_source(&mut indexed, &pending, &directories, &BTreeSet::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains("`isolated.rs` cannot be attributed"), "{error}");
        let directories = MemberDirectories {
            reported_by_canonical: BTreeMap::from([(PathBuf::from("isolated.rs"), PathBuf::from("new-owner.rs"))]),
        };
        indexed.released_path = Some(PathBuf::from("old-owner.rs"));
        let error = SourceIndex::revisit_indexed_source(&mut indexed, &pending, &directories, &BTreeSet::new())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&format!(
                "both `old-owner.rs` and `{}`",
                Path::new("new-owner.rs").join("").display()
            )),
            "{error}"
        );
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn survivor_nested_module_walks_propagate_inspection_errors() {
        assert_eq!(owned_directory(Path::new("."), SourceRole::PlainModule), None);
        for source in ["fn outer() { mod child; }", "mod inline { fn outer() { mod child; } }"] {
            let parse = SourceFile::parse(source, Edition::Edition2024);
            let error = module_declarations(
                Path::new("root.rs"),
                &parse.tree(),
                SourceRole::CrateRoot,
                &CfgFormula::Value(true),
                &BTreeSet::new(),
                &mut |_| Err(InspectCoverageSourceError::caused_by("child.rs", std::io::Error::other("nested inspection failed")).into()),
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("nested inspection failed"), "{source}: {error}");
        }
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn survivor_revisits_reuse_parsed_analysis_and_canonical_member_attribution() {
        let path = missing_source_path();
        let mut indexed = IndexedSource {
            analysis: analyze(&path, "fn released() {}"),
            edition: Edition::Edition2024,
            released_path: None,
            test_only_lines: BTreeSet::new(),
            visit_contexts: BTreeSet::new(),
        };
        let pending = pending_source(Surface::NonReleased, SourceRole::CrateRoot, &path, CfgFormula::Value(true));
        let directories = MemberDirectories {
            reported_by_canonical: BTreeMap::new(),
        };
        assert!(
            SourceIndex::revisit_indexed_source(&mut indexed, &pending, &directories, &BTreeSet::new())
                .unwrap()
                .unwrap()
                .is_empty()
        );
        let mut pending = pending_source(
            Surface::Released,
            SourceRole::CrateRoot,
            Path::new("canonical/source.rs"),
            CfgFormula::Value(true),
        );
        pending.owning_member = MemberDirectory {
            canonical: PathBuf::from("canonical"),
            reported: PathBuf::from("owning-member"),
        };
        let directories = MemberDirectories {
            reported_by_canonical: BTreeMap::from([(PathBuf::from("canonical"), PathBuf::from("canonical-member"))]),
        };
        assert_eq!(
            released_source_attributed_path(&directories, &pending).unwrap(),
            Some(Path::new("canonical-member").join("source.rs"))
        );
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn survivor_owner_ranges_include_commas_after_trivia_and_nested_guards_remain_released() {
        let text = "enum E { #[cfg(test)] A /* trivia */ , B }";
        let parse = SourceFile::parse(text, Edition::Edition2024);
        let variant = parse.tree().syntax().descendants().find_map(ast::Variant::cast).unwrap();
        let range = syntax_owner_range(variant.syntax());
        assert_eq!(usize::from(range.end()), text.find(',').unwrap() + 1);
        let path = Path::new("root.rs");
        let analysis = analyze(path, "fn outer() { mod inner { mod child; } }");
        let child = Path::new("inner").join("child.rs");
        let found = declarations(path, &analysis, SourceRole::CrateRoot, &BTreeSet::from([child.clone()])).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(lexical_normalize(&found[0].path), child);
        assert_eq!(found[0].guard, CfgFormula::Value(true));
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn survivor_parser_diagnostics_preserve_start_of_line_offsets() {
        for (source, coordinate) in [("}", "1:1"), ("\n}", "2:1")] {
            let error = analyze_source_text(Path::new("broken.rs"), source, Edition::Edition2024, &BTreeSet::new())
                .unwrap_err()
                .to_string();
            assert!(error.contains(coordinate), "{error}");
            assert!(error.contains("broken.rs"), "{error}");
        }
    }

    #[test]
    fn survivor_cfg_constructors_canonicalize_boolean_and_double_negation_operands() {
        let atom = CfgFormula::Atom("feature=example".to_owned());
        assert_eq!(CfgFormula::any([CfgFormula::Value(true), atom.clone()]), CfgFormula::Value(true));
        assert_eq!(CfgFormula::any([CfgFormula::Value(false), atom.clone()]), atom);
        assert_eq!(CfgFormula::not(CfgFormula::Value(true)), CfgFormula::Value(false));
        assert_eq!(CfgFormula::not(CfgFormula::Not(Box::new(atom.clone()))), atom);
    }

    #[test]
    fn survivor_normalized_checksum_errors_name_the_exact_source_not_only_the_test() {
        let path = PathBuf::from("canonical-source.rs");
        let empty = BTreeSet::new();
        let input = "TN:named-test\nSF:first.rs\nDA:1,1,first\nend_of_record\n\
                     TN:named-test\nSF:second.rs\nDA:1,2,second\nend_of_record\n";
        let error = filter_report_with(parse_lcov(input), None, |_| {
            Ok(SourceDisposition::Include {
                source_file: path.clone(),
                line_count: 1,
                test_only_lines: &empty,
            })
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("canonical-source.rs"), "{error}");
        assert!(error.contains("named-test"), "{error}");
        assert!(error.contains("conflicting checksums"), "{error}");
    }

    #[test]
    fn survivor_enqueuing_declarations_preserves_parent_resolution_errors() {
        let missing = missing_source_path().join("child.rs");
        let mut queue = VecDeque::new();
        let declaration = ModuleDeclaration {
            path: missing.clone(),
            role: SourceRole::PlainModule,
            guard: CfgFormula::Value(true),
        };
        let member = MemberDirectory {
            canonical: PathBuf::from("member"),
            reported: PathBuf::from("member"),
        };
        let error = enqueue_declarations_with_source_resolver(
            &mut queue,
            vec![declaration],
            Edition::Edition2024,
            Surface::Released,
            &member,
            &mut |path| {
                if path == missing.parent().unwrap() {
                    Err(
                        ResolveCoverageSourceError::caused_by(diagnostic_path(path), std::io::Error::from(std::io::ErrorKind::NotFound))
                            .into(),
                    )
                } else {
                    Ok(path.to_path_buf())
                }
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("failed to resolve Rust source path"), "{error}");
        assert!(error.contains(&diagnostic_path(missing.parent().unwrap())), "{error}");
        assert!(queue.is_empty());
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "rowan 0.15.18 triggers rust-analyzer/rowan#192 under Miri while parsing Rust syntax"
    )]
    fn survivor_metadata_reports_attribution_failures_before_indexing() {
        let root = source_index_fixture_root();
        let mut metadata = source_index_fixture_metadata(&root);
        metadata.packages[0].targets.truncate(1);
        let member = root.join("member");
        let error = SourceIndex::from_metadata_with_source_reader(
            &metadata,
            &mut |path| {
                if path == member {
                    Ok(PathBuf::from("unrelated-relative-member"))
                } else {
                    Ok(path.to_path_buf())
                }
            },
            &mut |_| Ok("fn released() {}".to_owned()),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("cannot be attributed to its owning workspace package"), "{error}");
        assert!(error.contains("lib.rs"), "{error}");
    }

    #[test]
    fn survivor_cfg_negation_swaps_both_possibility_fields_without_collapsing_unknown() {
        for (input, expected) in [
            (CfgPossibilities::TRUE, CfgPossibilities::FALSE),
            (CfgPossibilities::FALSE, CfgPossibilities::TRUE),
            (CfgPossibilities::EITHER, CfgPossibilities::EITHER),
        ] {
            assert_eq!(input.not(), expected);
            assert_eq!(input.not().not(), input);
        }
    }

    #[test]
    fn survivor_target_kind_subsets_reject_empty_and_mixed_nonreleased_sets() {
        let kinds = [TargetKind::Lib, TargetKind::Bin, TargetKind::ProcMacro, TargetKind::Test];
        for subset in 0..16 {
            let selected: Vec<_> = kinds
                .iter()
                .enumerate()
                .filter(|(index, _)| subset & (1 << index) != 0)
                .map(|(_, kind)| kind.clone())
                .collect();
            assert_eq!(target_is_released(&selected), subset != 0 && subset & 8 == 0, "{selected:?}");
        }
    }

    fn missing_source_path() -> PathBuf {
        // Impossible beneath an immutable regular file, not just absent by convention.
        Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml/survivor-absent-parent/lib.rs")
    }
}
