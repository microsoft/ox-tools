// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Released-surface filtering for cargo-llvm-cov LCOV reports.
//!
//! Cargo target roots establish whether a source file belongs to a released
//! library/binary surface or only to tests, examples, benches, and build
//! scripts. Out-of-line modules are followed with Rust's module-resolution
//! rules. Within released files, syntax ranges guarded exclusively by test
//! configuration are removed from every line-bearing LCOV record.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use cargo_metadata::{Edition as CargoEdition, Metadata, TargetKind};
use ra_ap_syntax::ast::{self, HasAttrs, HasModuleItem, HasName, LiteralKind};
use ra_ap_syntax::{AstNode, Edition, SourceFile, SyntaxKind, TextRange};

use crate::CoverageGateError;
use crate::error::{
    AmbiguousModuleSourceError, ConflictingSourceEditionError, CoverageLineOutOfRangeError, InvalidModulePathAttributeError,
    MissingFunctionStartLineError, ParseCoverageSourceError, ParseLcovError, ReadCoverageSourceError, ResolveCoverageSourceError,
    UnattributableReleasedSourceError, UnownedCoverageSourceError, UnsupportedSourceEditionError,
};
use crate::workspace::load_metadata;

const LINE_RELEASED: u8 = 1;
const LINE_TEST_ONLY: u8 = 1 << 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Surface {
    Released,
    NonReleased,
}

#[derive(Debug, Clone)]
struct PendingSource {
    path: PathBuf,
    edition: Edition,
    surface: Surface,
    crate_root: bool,
}

#[derive(Debug, Clone)]
struct ModuleDeclaration {
    path: PathBuf,
    test_only: bool,
}

#[derive(Debug)]
struct SourceAnalysis {
    crate_root_declarations: Vec<ModuleDeclaration>,
    module_declarations: Vec<ModuleDeclaration>,
    line_count: usize,
    test_only_lines: BTreeSet<u32>,
}

impl SourceAnalysis {
    fn declarations(&self, crate_root: bool) -> &[ModuleDeclaration] {
        if crate_root {
            &self.crate_root_declarations
        } else {
            &self.module_declarations
        }
    }
}

#[derive(Debug)]
struct IndexedSource {
    analysis: SourceAnalysis,
    edition: Edition,
    released: bool,
    visits: u8,
}

impl IndexedSource {
    fn has_visit(&self, surface: Surface, crate_root: bool) -> bool {
        self.visits & visit_bit(surface, crate_root) != 0
    }

    fn add_visit(&mut self, surface: Surface, crate_root: bool) {
        self.visits |= visit_bit(surface, crate_root);
        if surface == Surface::Released {
            self.released = true;
        }
    }
}

fn visit_bit(surface: Surface, crate_root: bool) -> u8 {
    match (surface, crate_root) {
        (Surface::Released, true) => 1,
        (Surface::Released, false) => 1 << 1,
        (Surface::NonReleased, true) => 1 << 2,
        (Surface::NonReleased, false) => 1 << 3,
    }
}

#[derive(Debug)]
struct SourceIndex {
    workspace_root: PathBuf,
    member_dirs: Vec<PathBuf>,
    files: BTreeMap<PathBuf, IndexedSource>,
}

impl SourceIndex {
    #[ohno::enrich_err("failed to load Cargo target ownership for coverage filtering")]
    fn from_metadata(metadata: &Metadata) -> Result<Self, CoverageGateError> {
        let workspace_root = resolve_source_path(metadata.workspace_root.as_std_path())?;
        let mut member_dirs = metadata
            .workspace_packages()
            .iter()
            .map(|package| {
                let directory = package
                    .manifest_path
                    .parent()
                    .expect("cargo metadata always reports a manifest path with a parent directory");
                resolve_source_path(directory.as_std_path())
            })
            .collect::<Result<Vec<_>, CoverageGateError>>()?;
        member_dirs.sort();
        member_dirs.dedup();

        let mut queue = VecDeque::new();
        for package in metadata.workspace_packages() {
            for target in &package.targets {
                let edition = syntax_edition(target.edition, target.src_path.as_std_path())?;
                queue.push_back(PendingSource {
                    path: resolve_source_path(target.src_path.as_std_path())?,
                    edition,
                    surface: if target_is_released(&target.kind) {
                        Surface::Released
                    } else {
                        Surface::NonReleased
                    },
                    crate_root: true,
                });
            }
        }

        let mut files: BTreeMap<PathBuf, IndexedSource> = BTreeMap::new();
        while let Some(pending) = queue.pop_front() {
            if let Some(indexed) = files.get_mut(&pending.path) {
                if indexed.edition != pending.edition {
                    return Err(ConflictingSourceEditionError::new(
                        pending.path.display().to_string(),
                        format!("{:?}", indexed.edition),
                        format!("{:?}", pending.edition),
                    )
                    .into());
                }
                if indexed.has_visit(pending.surface, pending.crate_root) {
                    continue;
                }
                indexed.add_visit(pending.surface, pending.crate_root);
                let declarations = indexed.analysis.declarations(pending.crate_root).to_vec();
                enqueue_declarations(&mut queue, declarations, pending.edition, pending.surface)?;
                continue;
            }

            let analysis = analyze_source(&pending.path, pending.edition)?;
            let declarations = analysis.declarations(pending.crate_root).to_vec();
            let mut indexed = IndexedSource {
                analysis,
                edition: pending.edition,
                released: false,
                visits: 0,
            };
            indexed.add_visit(pending.surface, pending.crate_root);
            let _ = files.insert(pending.path.clone(), indexed);
            enqueue_declarations(&mut queue, declarations, pending.edition, pending.surface)?;
        }

        Ok(Self {
            workspace_root,
            member_dirs,
            files,
        })
    }

    fn source_for_lcov(&self, path: &Path) -> Result<(&IndexedSource, PathBuf), CoverageGateError> {
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace_root.join(path)
        };
        let resolved = resolve_source_path(&candidate)?;
        let source = self
            .files
            .get(&resolved)
            .ok_or_else(|| UnownedCoverageSourceError::new(path.display().to_string()))?;
        Ok((source, resolved))
    }

    fn can_attribute(&self, path: &Path) -> bool {
        self.member_dirs.iter().any(|directory| path.starts_with(directory))
    }
}

fn enqueue_declarations(
    queue: &mut VecDeque<PendingSource>,
    declarations: Vec<ModuleDeclaration>,
    edition: Edition,
    parent_surface: Surface,
) -> Result<(), CoverageGateError> {
    for declaration in declarations {
        queue.push_back(PendingSource {
            path: resolve_source_path(&declaration.path)?,
            edition,
            surface: if parent_surface == Surface::Released && !declaration.test_only {
                Surface::Released
            } else {
                Surface::NonReleased
            },
            crate_root: false,
        });
    }
    Ok(())
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

fn syntax_edition(edition: CargoEdition, path: &Path) -> Result<Edition, CoverageGateError> {
    match edition {
        CargoEdition::E2015 => Ok(Edition::Edition2015),
        CargoEdition::E2018 => Ok(Edition::Edition2018),
        CargoEdition::E2021 => Ok(Edition::Edition2021),
        CargoEdition::E2024 => Ok(Edition::Edition2024),
        _ => Err(UnsupportedSourceEditionError::new(path.display().to_string(), format!("{edition:?}")).into()),
    }
}

fn resolve_source_path(path: &Path) -> Result<PathBuf, CoverageGateError> {
    fs::canonicalize(path).map_err(|error| ResolveCoverageSourceError::caused_by(path.display().to_string(), error).into())
}

fn analyze_source(path: &Path, edition: Edition) -> Result<SourceAnalysis, CoverageGateError> {
    let source = fs::read_to_string(path).map_err(|error| ReadCoverageSourceError::caused_by(path.display().to_string(), error))?;
    let mut is_file = Path::is_file;
    analyze_source_text(path, &source, edition, &mut is_file)
}

fn analyze_source_text(
    path: &Path,
    source: &str,
    edition: Edition,
    is_file: &mut impl FnMut(&Path) -> bool,
) -> Result<SourceAnalysis, CoverageGateError> {
    let parse = SourceFile::parse(source, edition);
    if !parse.errors().is_empty() {
        let diagnostics = parse.errors().iter().map(ToString::to_string).collect::<Vec<_>>().join("; ");
        return Err(ParseCoverageSourceError::new(path.display().to_string(), diagnostics).into());
    }
    let tree = parse.tree();
    let root = tree.syntax();
    let line_starts = line_starts(source);
    let test_ranges = root
        .descendants()
        .filter_map(ast::Attr::cast)
        .filter(attribute_marks_test)
        .map(|attribute| {
            attribute
                .syntax()
                .parent()
                .expect("every parsed attribute is owned by a syntax node")
                .text_range()
        })
        .collect::<Vec<_>>();
    let mut line_flags = vec![0_u8; line_starts.len()];
    for range in &test_ranges {
        mark_range_lines(&mut line_flags, &line_starts, *range, LINE_TEST_ONLY);
    }
    for token in root.descendants_with_tokens().filter_map(ra_ap_syntax::NodeOrToken::into_token) {
        if matches!(token.kind(), SyntaxKind::WHITESPACE | SyntaxKind::COMMENT) {
            continue;
        }
        if !test_ranges.iter().any(|range| range.contains_range(token.text_range())) {
            mark_range_lines(&mut line_flags, &line_starts, token.text_range(), LINE_RELEASED);
        }
    }
    let test_only_lines = line_flags
        .iter()
        .enumerate()
        .filter(|(_, flags)| **flags & LINE_TEST_ONLY != 0 && **flags & LINE_RELEASED == 0)
        .map(|(index, _)| {
            u32::try_from(index + 1)
                .expect("a source with more than u32::MAX lines cannot produce an LCOV line number for the excess lines")
        })
        .collect();
    let crate_root_declarations = module_declarations(path, &tree, true, is_file)?;
    let module_declarations = module_declarations(path, &tree, false, is_file)?;

    Ok(SourceAnalysis {
        crate_root_declarations,
        module_declarations,
        line_count: line_starts.len(),
        test_only_lines,
    })
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
    if start == end {
        return;
    }
    let first = offset_to_line(starts, start);
    let last = offset_to_line(starts, end - 1);
    for line in first..=last {
        if let Some(value) = flags.get_mut(line) {
            *value |= flag;
        }
    }
}

fn offset_to_line(starts: &[usize], offset: usize) -> usize {
    starts.binary_search(&offset).unwrap_or_else(|line| line.saturating_sub(1))
}

fn attribute_marks_test(attribute: &ast::Attr) -> bool {
    let Some(meta) = attribute.meta() else {
        return false;
    };
    match meta {
        ast::Meta::CfgMeta(cfg) => cfg.cfg_predicate().is_some_and(|predicate| cfg_predicate_is_test_only(&predicate)),
        ast::Meta::PathMeta(path_meta) => path_meta
            .path()
            .and_then(|path| path.segment())
            .and_then(|segment| segment.name_ref())
            .is_some_and(|name| matches!(&*name.text(), "test" | "bench")),
        ast::Meta::CfgAttrMeta(_) | ast::Meta::KeyValueMeta(_) | ast::Meta::TokenTreeMeta(_) | ast::Meta::UnsafeMeta(_) => false,
    }
}

fn cfg_predicate_is_test_only(predicate: &ast::CfgPredicate) -> bool {
    match predicate {
        ast::CfgPredicate::CfgAtom(atom) => atom.eq_token().is_none() && atom.ident_token().is_some_and(|token| token.text() == "test"),
        ast::CfgPredicate::CfgComposite(composite) => {
            let keyword = composite.keyword().map(|token| token.text().to_owned()).unwrap_or_default();
            let parts = composite.cfg_predicates().collect::<Vec<_>>();
            match keyword.as_str() {
                "all" => parts.iter().any(cfg_predicate_is_test_only),
                "any" => !parts.is_empty() && parts.iter().all(cfg_predicate_is_test_only),
                _ => false,
            }
        }
    }
}

fn module_declarations(
    path: &Path,
    source: &ast::SourceFile,
    crate_root: bool,
    is_file: &mut impl FnMut(&Path) -> bool,
) -> Result<Vec<ModuleDeclaration>, CoverageGateError> {
    let Some(directory) = owned_directory(path, crate_root) else {
        return Ok(Vec::new());
    };
    let Some(base) = path.parent() else {
        return Ok(Vec::new());
    };
    let mut declarations = Vec::new();
    let source_test_only = ast::attrs_including_inner(source).any(|attribute| attribute_marks_test(&attribute));
    walk_modules(
        source.items().collect(),
        &directory,
        base,
        path,
        source_test_only,
        &mut declarations,
        is_file,
    )?;
    Ok(declarations)
}

fn owned_directory(path: &Path, crate_root: bool) -> Option<PathBuf> {
    let parent = path.parent()?;
    if crate_root {
        return Some(parent.to_path_buf());
    }
    let stem = path.file_stem()?.to_string_lossy();
    if matches!(&*stem, "lib" | "main" | "mod") {
        Some(parent.to_path_buf())
    } else {
        Some(parent.join(&*stem))
    }
}

fn walk_modules(
    items: Vec<ast::Item>,
    directory: &Path,
    base: &Path,
    declaring_file: &Path,
    inherited_test_only: bool,
    declarations: &mut Vec<ModuleDeclaration>,
    is_file: &mut impl FnMut(&Path) -> bool,
) -> Result<(), CoverageGateError> {
    for item in items {
        let ast::Item::Module(module) = item else {
            continue;
        };
        let name = module
            .name()
            .expect("a parsed module declaration always has a name")
            .text()
            .trim_start_matches("r#")
            .to_owned();
        let test_only = inherited_test_only || ast::attrs_including_inner(&module).any(|attribute| attribute_marks_test(&attribute));
        let path_override = module_path_attribute(&module, declaring_file, &name)?;

        if let Some(item_list) = module.item_list() {
            let nested = path_override.map_or_else(|| directory.join(&name), |relative| base.join(relative));
            walk_modules(
                item_list.items().collect(),
                &nested,
                &nested,
                declaring_file,
                test_only,
                declarations,
                is_file,
            )?;
            continue;
        }

        let candidates = path_override.map_or_else(
            || vec![directory.join(format!("{name}.rs")), directory.join(&name).join("mod.rs")],
            |relative| vec![base.join(relative)],
        );
        let existing = candidates.into_iter().filter(|candidate| is_file(candidate)).collect::<Vec<_>>();
        if existing.len() > 1 {
            return Err(AmbiguousModuleSourceError::new(
                declaring_file.display().to_string(),
                name,
                existing[0].display().to_string(),
                existing[1].display().to_string(),
            )
            .into());
        }
        if let Some(path) = existing.into_iter().next() {
            declarations.push(ModuleDeclaration { path, test_only });
        }
    }
    Ok(())
}

fn module_path_attribute(module: &ast::Module, declaring_file: &Path, module_name: &str) -> Result<Option<PathBuf>, CoverageGateError> {
    let mut found = None;
    for attribute in module.attrs() {
        let Some(ast::Meta::KeyValueMeta(meta)) = attribute.meta() else {
            continue;
        };
        let is_path = meta
            .path()
            .and_then(|path| path.segment())
            .and_then(|segment| segment.name_ref())
            .is_some_and(|name| name.text() == "path");
        if !is_path {
            continue;
        }
        let value = match meta.expr() {
            Some(ast::Expr::Literal(literal)) => match literal.kind() {
                LiteralKind::String(text) => text.value().ok().map(|value| PathBuf::from(value.as_ref())),
                _ => None,
            },
            _ => None,
        };
        let Some(value) = value else {
            return Err(InvalidModulePathAttributeError::new(
                declaring_file.display().to_string(),
                module_name.to_owned(),
                attribute.syntax().text().to_string(),
            )
            .into());
        };
        if found.replace(value).is_some() {
            return Err(InvalidModulePathAttributeError::new(
                declaring_file.display().to_string(),
                module_name.to_owned(),
                "multiple #[path] attributes".to_owned(),
            )
            .into());
        }
    }
    Ok(found)
}

enum SourceDisposition<'a> {
    Exclude {
        line_count: usize,
    },
    Include {
        line_count: usize,
        test_only_lines: &'a BTreeSet<u32>,
    },
}

fn filter_report(report: lcov::Report, sources: &SourceIndex) -> Result<String, CoverageGateError> {
    filter_report_with(report, |path| {
        let (source, resolved_path) = sources.source_for_lcov(path)?;
        if !source.released {
            return Ok(SourceDisposition::Exclude {
                line_count: source.analysis.line_count,
            });
        }
        if !sources.can_attribute(&resolved_path) {
            return Err(UnattributableReleasedSourceError::new(path.display().to_string()).into());
        }
        Ok(SourceDisposition::Include {
            line_count: source.analysis.line_count,
            test_only_lines: &source.analysis.test_only_lines,
        })
    })
}

fn filter_report_with<'a>(
    mut report: lcov::Report,
    mut classify: impl FnMut(&Path) -> Result<SourceDisposition<'a>, CoverageGateError>,
) -> Result<String, CoverageGateError> {
    let mut filtered = lcov::report::section::Sections::new();
    for (key, mut section) in report.sections {
        let (line_count, test_only) = match classify(&key.source_file)? {
            SourceDisposition::Exclude { line_count } => {
                validate_section_lines(&key.source_file, &section, line_count)?;
                continue;
            }
            SourceDisposition::Include {
                line_count,
                test_only_lines,
            } => (line_count, test_only_lines),
        };
        validate_section_lines(&key.source_file, &section, line_count)?;
        section.lines.retain(|line, _| !test_only.contains(&line.line));
        section.branches.retain(|branch, _| !test_only.contains(&branch.line));
        section.functions.retain(|_, function| {
            let line = function
                .start_line
                .expect("validate_section_lines rejects every function without a start line before filtering");
            !test_only.contains(&line)
        });
        if !section.is_empty() {
            let _ = filtered.insert(key, section);
        }
    }
    report.sections = filtered;

    let mut output = String::new();
    for record in report.into_records() {
        writeln!(&mut output, "{record}").expect("writing LCOV records to a String cannot fail");
    }
    Ok(output)
}

fn validate_section_lines(path: &Path, section: &lcov::report::section::Value, line_count: usize) -> Result<(), CoverageGateError> {
    for line in section
        .lines
        .keys()
        .map(|line| line.line)
        .chain(section.branches.keys().map(|branch| branch.line))
    {
        validate_line(path, line, line_count)?;
    }
    for (function, data) in &section.functions {
        let line = data
            .start_line
            .ok_or_else(|| MissingFunctionStartLineError::new(path.display().to_string(), function.name.clone()))?;
        validate_line(path, line, line_count)?;
    }
    Ok(())
}

fn validate_line(path: &Path, line: u32, line_count: usize) -> Result<(), CoverageGateError> {
    let valid = line != 0 && usize::try_from(line).is_ok_and(|line| line <= line_count);
    if valid {
        Ok(())
    } else {
        Err(CoverageLineOutOfRangeError::new(path.display().to_string(), line, line_count).into())
    }
}

pub(crate) fn filter_many(inputs: &[&str], manifest_path: Option<&Path>) -> Result<Vec<String>, CoverageGateError> {
    let reports = parse_reports(inputs)?;
    if reports.is_empty() {
        return Ok(Vec::new());
    }
    let metadata = load_metadata(manifest_path)?;
    filter_reports_with_metadata(reports, &metadata)
}

pub(crate) fn filter_reports_with_metadata(reports: Vec<lcov::Report>, metadata: &Metadata) -> Result<Vec<String>, CoverageGateError> {
    if reports.is_empty() {
        return Ok(Vec::new());
    }
    let sources = SourceIndex::from_metadata(metadata)?;
    reports.into_iter().map(|report| filter_report(report, &sources)).collect()
}

pub(crate) fn filter_one(input: &str, manifest_path: Option<&Path>) -> Result<String, CoverageGateError> {
    let report = parse_report(input)?;
    let metadata = load_metadata(manifest_path)?;
    let sources = SourceIndex::from_metadata(&metadata)?;
    filter_report(report, &sources)
}

pub(crate) fn parse_reports(inputs: &[&str]) -> Result<Vec<lcov::Report>, CoverageGateError> {
    inputs.iter().map(|input| parse_report(input)).collect()
}

fn parse_report(input: &str) -> Result<lcov::Report, CoverageGateError> {
    lcov::Report::from_reader(lcov::Reader::new(input.as_bytes())).map_err(|error| CoverageGateError::from(ParseLcovError::from(error)))
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    struct FixtureSource {
        released: bool,
        analysis: SourceAnalysis,
    }

    fn no_file(_: &Path) -> bool {
        false
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

    fn analyze(path: &Path, source: &str, existing: &BTreeSet<PathBuf>) -> SourceAnalysis {
        let mut is_file = |candidate: &Path| existing.contains(&lexical_normalize(candidate));
        analyze_source_text(path, source, Edition::Edition2024, &mut is_file).expect("fixture source parses")
    }

    fn parse_lcov(input: &str) -> lcov::Report {
        lcov::Report::from_reader(lcov::Reader::new(input.as_bytes())).expect("fixture LCOV parses")
    }

    #[test]
    fn cfg_semantics_only_exclude_predicates_that_require_test() {
        let source = SourceFile::parse(
            "#[cfg(test)] fn a() {}\n\
             #[cfg(all(test, unix))] fn b() {}\n\
             #[cfg(any(test, feature = \"test-util\"))] fn c() {}\n\
             #[cfg(feature = \"test-util\")] fn d() {}\n\
             #[cfg(not(test))] fn e() {}\n\
             #[tokio::test] fn f() {}\n\
             #[test_case] fn g() {}\n",
            Edition::Edition2024,
        )
        .tree();
        let marks = source
            .items()
            .map(|item| item.attrs().any(|attribute| attribute_marks_test(&attribute)))
            .collect::<Vec<_>>();
        assert_eq!(marks, [true, true, false, false, false, true, false]);
    }

    #[test]
    fn inner_cfg_test_propagates_to_external_module_files() {
        let root = PathBuf::from("/workspace/src/lib.rs");
        let child = PathBuf::from("/workspace/src/child.rs");
        let analysis = analyze(&root, "#![cfg(test)]\nmod child;\n", &BTreeSet::from([child.clone()]));
        assert_eq!(analysis.crate_root_declarations.len(), 1);
        assert_eq!(lexical_normalize(&analysis.crate_root_declarations[0].path), child);
        assert!(analysis.crate_root_declarations[0].test_only);

        let nested = PathBuf::from("/workspace/src/outer/nested.rs");
        let analysis = analyze(
            &root,
            "mod outer { #![cfg(test)] mod nested; }\n",
            &BTreeSet::from([nested.clone()]),
        );
        assert_eq!(analysis.crate_root_declarations.len(), 1);
        assert_eq!(lexical_normalize(&analysis.crate_root_declarations[0].path), nested);
        assert!(analysis.crate_root_declarations[0].test_only);
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
            &existing,
        );
        assert_eq!(
            lib_analysis
                .crate_root_declarations
                .iter()
                .map(|declaration| (lexical_normalize(&declaration.path), declaration.test_only))
                .collect::<Vec<_>>(),
            [(unit_support.clone(), true), (shared_support.clone(), false)]
        );
        let integration_analysis = analyze(&integration, "mod support;\n#[test] fn integration() {}\n", &existing);
        let integration_declarations = integration_analysis
            .crate_root_declarations
            .iter()
            .map(|declaration| lexical_normalize(&declaration.path))
            .collect::<Vec<_>>();
        assert_eq!(integration_declarations.len(), 1);
        assert_eq!(integration_declarations[0], shared_support);

        let no_files = BTreeSet::new();
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
                    analysis: analyze(&unit_support, "pub fn helper() {}\npub fn missed_helper() {}\n", &no_files),
                },
            ),
            (
                shared_support.clone(),
                FixtureSource {
                    // A released cfg path wins even when a non-release test target also reaches the file.
                    released: true,
                    analysis: analyze(&shared_support, "pub fn released_test_util() {}\n", &no_files),
                },
            ),
            (
                bin.clone(),
                FixtureSource {
                    released: true,
                    analysis: analyze(&bin, "fn main() {}\n", &no_files),
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
                    analysis: analyze(&example, "fn main() {}\n", &no_files),
                },
            ),
            (
                bench.clone(),
                FixtureSource {
                    released: false,
                    analysis: analyze(&bench, "fn main() {}\n", &no_files),
                },
            ),
            (
                build.clone(),
                FixtureSource {
                    released: false,
                    analysis: analyze(&build, "fn main() {}\n", &no_files),
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

        let filtered = filter_report_with(parse_lcov(&input), |path| {
            let source = sources
                .get(path)
                .ok_or_else(|| UnownedCoverageSourceError::new(path.display().to_string()))?;
            if source.released {
                Ok(SourceDisposition::Include {
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
        assert_eq!(
            by_path[&lib]
                .functions
                .keys()
                .map(|function| function.name.as_str())
                .collect::<Vec<_>>(),
            ["cfg_any_test", "released"]
        );
        assert_eq!(by_path[&lib].branches.keys().map(|branch| branch.line).collect::<Vec<_>>(), [4]);
        assert_eq!(by_path[&shared_support].lines.values().next().expect("shared line").count, 0);
        assert!(by_path.contains_key(&bin));
        for excluded in [unit_support, integration, example, bench, build] {
            assert!(!by_path.contains_key(&excluded), "{excluded:?} must be excluded");
        }
    }

    #[test]
    fn unsupported_source_states_fail_instead_of_hiding_counters() {
        let lib = PathBuf::from("/workspace/surface/src/lib.rs");
        let orphan = PathBuf::from("/workspace/surface/src/orphan.rs");
        let unowned = lcov_section(&orphan, &[(1, 0)]);
        let error = filter_report_with(parse_lcov(&unowned), |path| {
            Err::<SourceDisposition<'static>, CoverageGateError>(UnownedCoverageSourceError::new(path.display().to_string()).into())
        })
        .expect_err("unowned source must fail")
        .to_string();
        assert!(error.contains("not reachable from any Cargo target"), "{error}");

        let stale = lcov_section(&lib, &[(3, 0)]);
        let test_only = BTreeSet::new();
        let error = filter_report_with(parse_lcov(&stale), |_| {
            Ok(SourceDisposition::Include {
                line_count: 2,
                test_only_lines: &test_only,
            })
        })
        .expect_err("stale line number must fail")
        .to_string();
        assert!(error.contains("references line 3"), "{error}");
        assert!(error.contains("regenerate coverage"), "{error}");

        let error = filter_report_with(parse_lcov(&stale), |_| Ok(SourceDisposition::Exclude { line_count: 2 }))
            .expect_err("stale line data must fail even when the target is excluded")
            .to_string();
        assert!(error.contains("references line 3"), "{error}");

        let missing_function_line = format!("TN:\nSF:{}\nFNDA:0,unknown\nDA:1,0\nend_of_record\n", lib.display());
        let error = filter_report_with(parse_lcov(&missing_function_line), |_| {
            Ok(SourceDisposition::Include {
                line_count: 2,
                test_only_lines: &test_only,
            })
        })
        .expect_err("function data without a source line must fail")
        .to_string();
        assert!(error.contains("has no start line"), "{error}");

        let mut no_files = no_file;
        let error = analyze_source_text(&lib, "pub fn broken( {\n", Edition::Edition2024, &mut no_files)
            .expect_err("invalid Rust source must fail")
            .to_string();
        assert!(error.contains("could not parse Rust source"), "{error}");

        let error = filter_many(&["not lcov"], None)
            .expect_err("malformed LCOV must fail before metadata discovery")
            .to_string();
        assert!(error.contains("lcov tracefile"), "{error}");
    }
}
