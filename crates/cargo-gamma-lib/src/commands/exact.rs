// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! All-or-nothing current identity selection and corroboration of embedded historical sites.

use core::ops::Range;
use std::collections::{BTreeMap, BTreeSet};

use camino::Utf8Path;
use cargo_gamma_engine::model::token_position;
use proc_macro2::TokenStream;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use crate::commands::SelectArgs;
use crate::discover::{Scanned, Survey};
use crate::elements::{ExactSelection, FRAMEWORK_NAME, Location, MutantResult, Position, Report, SelectionKind};
use crate::error::error;
use crate::model::{MUTANT_ID_HEX_LEN, MUTANT_ID_VERSION, Mutant, MutantId, Outcome, normalize_site_text};
use crate::ops::Selection;
use crate::parse::{SourceFile, strip_bom};
use crate::report::encode_controls;
use crate::{HashMap, Result};

/// A retained exact request; historical verdicts are never adopted by this operation.
pub(super) struct Request {
    ids: BTreeSet<MutantId>,
    report: Option<Report>,
    parent: Option<String>,
}

impl Request {
    pub(super) fn load(ids: &[String], path: Option<&Utf8Path>) -> Result<Option<Self>> {
        if ids.is_empty() && path.is_none() {
            return Ok(None);
        }
        let mut requested = ids.iter().map(MutantId::new).collect::<BTreeSet<_>>();
        let mut report = None;
        let mut parent = None;
        if let Some(path) = path {
            let input = crate::merge::read_limited(path, u64::MAX).map_err(|cause| {
                error!(
                    "cannot replay `{}`: {}",
                    encode_controls(path.as_str()),
                    encode_controls(&cause.to_string())
                )
                .usage()
            })?;
            validate_report(&input.report)?;
            if input.report.framework.name != FRAMEWORK_NAME {
                return Err(error!(
                    "exact replay requires a cargo-gamma report, not `{}`",
                    encode_controls(&input.report.framework.name)
                )
                .usage());
            }
            if input.report.config.as_ref().and_then(|config| config.mutant_id_version) != Some(MUTANT_ID_VERSION) {
                return Err(error!(
                    "exact replay requires explicit mutant-ID scheme {MUTANT_ID_VERSION}; rediscover with `list mutants --json-report PATH`"
                )
                .usage());
            }
            if requested.is_empty() {
                requested.extend(
                    input
                        .report
                        .files
                        .values()
                        .flat_map(|file| file.mutants.iter())
                        .map(|mutant| MutantId::new(&mutant.id)),
                );
            }
            parent = Some(input.identity);
            report = Some(input.report);
        }
        if requested.is_empty() {
            return Err(error!("the report names no mutants; an empty report is not an exact replay request").usage());
        }
        let invalid = requested
            .iter()
            .filter(|id| !is_id(id))
            .map(|id| encode_controls(id))
            .collect::<Vec<_>>();
        if !invalid.is_empty() {
            return Err(error!(
                "invalid mutant IDs: {}; use complete {MUTANT_ID_HEX_LEN}-character lowercase hexadecimal IDs, not prefixes, ordinals or mutator names",
                invalid.join(", ")
            ).usage());
        }
        Ok(Some(Self {
            ids: requested,
            report,
            parent,
        }))
    }

    pub(super) fn len(&self) -> usize {
        self.ids.len()
    }

    pub(super) fn resolve(&self, survey: &mut Survey, selection: &Selection, allow_suppressed: bool) -> Result<()> {
        let scanned = survey.scan(None, selection, &mut 0)?;
        survey.pin_exact(self.resolve_scan(scanned, allow_suppressed)?);
        Ok(())
    }

    fn resolve_scan(&self, mut scanned: Scanned, allow_suppressed: bool) -> Result<Scanned> {
        let mut current = BTreeMap::new();
        let mut diagnostics = BTreeSet::new();
        for mutant in &scanned.mutants {
            if current.insert(&mutant.id, mutant).is_some() {
                let _ = diagnostics.insert(format!(
                    "{}: ambiguous duplicate current ID; narrow package/file selection and rediscover",
                    mutant.id
                ));
            }
        }
        let historical: BTreeMap<_, _> = self
            .report
            .iter()
            .flat_map(|report| &report.files)
            .flat_map(|(path, file)| file.mutants.iter().map(move |mutant| (mutant.id.as_str(), (path, file, mutant))))
            .collect();
        let mut sources = HashMap::default();
        let mut embedded = BTreeMap::new();
        for id in &self.ids {
            let prior = historical.get(id.as_str());
            if self.report.is_some() && prior.is_none() {
                let _ = diagnostics.insert(format!("{id}: not present in the input report"));
            }
            let Some(mutant) = current.get(id) else {
                let _ = diagnostics.insert(format!(
                    "{id}: unknown current mutant or excluded by current package/file/mutator policy; rediscover with `list mutants --json` in the selected workspace"
                ));
                continue;
            };
            if !allow_suppressed && mutant.outcome == Outcome::Ignored {
                let reason = mutant
                    .suppression
                    .as_ref()
                    .and_then(|suppression| suppression.reason.as_deref())
                    .unwrap_or("current suppression policy");
                let _ = diagnostics.insert(format!(
                    "{id}: suppressed by {reason}; choose another set or explicitly change that policy"
                ));
            }
            if let Some((path, file, previous)) = prior {
                let result = (|| {
                    let report = self.report.as_ref().expect("historical entries come only from the retained report");
                    if report.verdict_policy(id).0 != Some(FRAMEWORK_NAME) {
                        return Err(error!("original producer is foreign or unknown"));
                    }
                    if !sources.contains_key(mutant.file.as_ref()) {
                        let source = scanned
                            .sources
                            .get(mutant.file.as_ref())
                            .ok_or_else(|| error!("current source generation is unavailable"))?;
                        let _ = sources.insert(
                            mutant.file.to_path_buf(),
                            SourceFile::parse(mutant.file.to_path_buf(), source.clone())?,
                        );
                    }
                    if !embedded.contains_key(*path) {
                        let _ = embedded.insert((*path).clone(), SourceFile::parse(path.as_str(), file.source.clone())?);
                    }
                    let old_span = source_span(&file.source, previous.location)?;
                    let bom = file.source.len() - strip_bom(&file.source).len();
                    let old_span = old_span
                        .start
                        .checked_sub(bom)
                        .zip(old_span.end.checked_sub(bom))
                        .map(|(start, end)| start..end)
                        .ok_or_else(|| error!("location points into a byte-order mark"))?;
                    corroborate(&sources[mutant.file.as_ref()], &embedded[*path], &old_span, mutant, previous)
                })();
                if let Err(cause) = result {
                    let _ = diagnostics.insert(format!(
                        "{id}: stale or incompatible historical site: {cause}; rediscover or select an explicitly valid subset"
                    ));
                }
            }
        }
        if !diagnostics.is_empty() {
            return Err(error!(
                "exact selection failed; no mutants will execute:\n  {}",
                diagnostics
                    .iter()
                    .map(|entry| encode_controls(entry))
                    .collect::<Vec<_>>()
                    .join("\n  ")
            )
            .usage());
        }
        // Missing-ID accounting and corroboration must precede this destructive filter.
        scanned.mutants.retain(|mutant| self.ids.contains(&mutant.id));
        scanned.suppressed = scanned.mutants.iter().filter(|mutant| mutant.outcome == Outcome::Ignored).count();
        scanned.idle.clear();
        if let Some(scope) = &mut scanned.population {
            scope.selection = SelectionKind::ExactIds;
            scope.complete_files.clear();
            let _ = scope.reductions.insert("exactIds".to_owned());
            scope.exact = Some(ExactSelection {
                ids: self.ids.iter().map(ToString::to_string).collect(),
                parent_report: self.parent.clone(),
            });
        }
        Ok(scanned)
    }
}

fn corroborate(source: &SourceFile, old: &SourceFile, old_span: &Range<usize>, mutant: &Mutant, previous: &MutantResult) -> Result<()> {
    if previous.mutator_name.as_str() != mutant.mutator.as_ref()
        || previous.replacement.as_deref() != Some(mutant.replacement.as_str())
        || source.path() != old.path()
    {
        return Err(error!("path, mutator or replacement differs"));
    }
    if tokens(old.slice(old_span))? != tokens(&mutant.original)? || context(old, old_span)? != context(source, &mutant.span)? {
        return Err(error!("enclosing source item changed; the ID may now name another occurrence"));
    }
    Ok(())
}

pub(super) fn is_id(subject: &str) -> bool {
    subject.len() == MUTANT_ID_HEX_LEN && subject.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(super) fn selection(select: &SelectArgs, exact: bool) -> Result<Selection> {
    if exact && select.mutators.is_none() {
        let mut all = select.clone();
        all.mutators = Some("all".to_owned());
        all.selection()
    } else {
        select.selection()
    }
}

pub(super) fn validate_options(select: &SelectArgs, exact: bool, survivors: bool) -> Result<()> {
    if exact && (survivors || select.in_diff.is_some() || select.shard_count.is_some() || select.shard_index.is_some()) {
        return Err(error!("exact mutant selection cannot be combined with --only-survivors-from, --in-diff or sharding (including configured sharding); remove the conflicting selection").usage());
    }
    Ok(())
}

/// Validates report data without resolving any path against its claimed project root.
pub(super) fn validate_report(report: &Report) -> Result<()> {
    for (path, file) in &report.files {
        if path.is_empty() || path.contains(['\\', ':']) || path.split('/').any(|part| matches!(part, "" | "." | "..")) {
            return Err(error!("invalid workspace-relative report path `{}`", encode_controls(path)).usage());
        }
        for mutant in &file.mutants {
            let _ = source_span(&file.source, mutant.location).map_err(|cause| {
                error!(
                    "invalid location for mutant `{}` in `{}`: {cause}",
                    encode_controls(&mutant.id),
                    encode_controls(path)
                )
                .usage()
            })?;
        }
    }
    Ok(())
}

/// Converts character-based report coordinates to checked UTF-8 byte offsets.
pub(super) fn source_span(source: &str, location: Location) -> Result<Range<usize>> {
    let start = offset(source, location.start)?;
    let end = offset(source, location.end)?;
    if start >= end {
        return Err(error!("source range must be nonempty and ordered"));
    }
    Ok(start..end)
}

fn offset(source: &str, position: Position) -> Result<usize> {
    let line = position.line.checked_sub(1).ok_or_else(|| error!("line must be one-based"))?;
    let column = position.column.checked_sub(1).ok_or_else(|| error!("column must be one-based"))?;
    let mut lines = source.split('\n');
    let mut start = 0;
    for _ in 0..line {
        let text = lines.next().ok_or_else(|| error!("line exceeds embedded source"))?;
        start += text.len() + 1;
    }
    let text = lines.next().ok_or_else(|| error!("line exceeds embedded source"))?;
    let byte = text
        .char_indices()
        .map(|(at, _)| at)
        .chain(std::iter::once(text.len()))
        .nth(column)
        .ok_or_else(|| error!("column exceeds embedded source"))?;
    Ok(start + byte)
}

fn tokens(text: &str) -> Result<String> {
    normalize_site_text(text)
        .parse::<TokenStream>()
        .map(|tokens| tokens.to_string())
        .map_err(|cause| error!("cannot normalize source tokens: {cause}"))
}

/// The enclosing item's tokens plus its module/implementation headers.
///
/// Sibling items are excluded so test additions and item motion preserve correspondence.
/// The entire containing body is retained because site ordinals alone cannot disambiguate deletion.
fn context(source: &SourceFile, span: &Range<usize>) -> Result<Vec<String>> {
    let mut enclosing = Enclosing {
        source,
        span,
        parts: Vec::new(),
        item: None,
    };
    enclosing.visit_file(source.ast());
    let item = enclosing
        .item
        .ok_or_else(|| error!("no enclosing source item for the reported location"))?;
    let text = source.slice(&item);
    let start = token_position(text, span.start - item.start);
    let end = token_position(text, span.end - item.start);
    let mut parts = enclosing.parts.into_iter().map(tokens).collect::<Result<Vec<_>>>()?;
    parts.push(format!("site position: {start:?}..{end:?}"));
    Ok(parts)
}

/// Visits only containers enclosing one checked source span.
struct Enclosing<'a> {
    source: &'a SourceFile,
    span: &'a Range<usize>,
    parts: Vec<&'a str>,
    item: Option<Range<usize>>,
}

#[expect(
    clippy::renamed_function_params,
    reason = "item describes the visited syntax better than syn's single-letter name"
)]
impl<'ast> Visit<'ast> for Enclosing<'_> {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        let range = item.span().byte_range();
        if range.start > self.span.start || range.end < self.span.end {
            return;
        }
        let header_end = match item {
            syn::Item::Mod(item) => item.content.as_ref().map(|(brace, _)| brace.span.open().byte_range().start),
            syn::Item::Impl(item) => Some(item.brace_token.span.open().byte_range().start),
            syn::Item::Trait(item) => Some(item.brace_token.span.open().byte_range().start),
            _ => None,
        };
        if let Some(end) = header_end {
            self.parts.push(self.source.slice(&(range.start..end)));
        } else {
            self.parts.push(self.source.slice(&range));
            self.item = Some(range);
        }
        visit::visit_item(self, item);
    }

    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        let range = item.span().byte_range();
        if range.start <= self.span.start && range.end >= self.span.end {
            self.parts.push(self.source.slice(&range));
            self.item = Some(range);
            visit::visit_impl_item(self, item);
        }
    }

    fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
        let range = item.span().byte_range();
        if range.start <= self.span.start && range.end >= self.span.end {
            self.parts.push(self.source.slice(&range));
            self.item = Some(range);
            visit::visit_trait_item(self, item);
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::iter::once;

    use camino::Utf8PathBuf;

    use super::*;
    use crate::elements::{FileResult, Framework, RunInfo, Thresholds};

    fn scan(text: &str) -> Scanned {
        scan_with(text, &Selection::parse("arith.add_to_sub").unwrap())
    }

    fn scan_with(text: &str, selected: &Selection) -> Scanned {
        let source = SourceFile::parse("src/lib.rs", text.to_owned()).unwrap();
        let mutants = crate::ops::into_mutants(&source, "subject", crate::ops::collect_candidates(&source, selected));
        Scanned {
            mutants,
            sources: once((Utf8PathBuf::from("src/lib.rs"), text.to_owned())).collect(),
            ..Scanned::default()
        }
    }

    fn request(scanned: &Scanned, historical: bool) -> Request {
        let report = historical.then(|| {
            let text = &scanned.sources[Utf8Path::new("src/lib.rs")];
            let source = SourceFile::parse("src/lib.rs", text.clone()).unwrap();
            let mutants = scanned
                .mutants
                .iter()
                .map(|mutant| {
                    let (line, column) = source.location(mutant.span.end);
                    MutantResult {
                        id: mutant.id.to_string().into(),
                        mutator_name: mutant.mutator.to_string().into(),
                        location: Location {
                            start: Position {
                                line: mutant.line,
                                column: mutant.column,
                            },
                            end: Position { line, column },
                        },
                        replacement: Some(mutant.replacement.clone()),
                        ..crate::fixtures::mutant_result()
                    }
                })
                .collect();
            Report {
                schema_version: "2".to_owned(),
                framework: Framework {
                    name: FRAMEWORK_NAME.to_owned(),
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                },
                project_root: None,
                thresholds: Thresholds::default(),
                files: [(
                    "src/lib.rs".to_owned(),
                    FileResult {
                        source: text.clone(),
                        language: "rust".to_owned(),
                        mutants,
                    },
                )]
                .into(),
                config: Some(RunInfo {
                    mutant_id_version: Some(MUTANT_ID_VERSION),
                    ..crate::fixtures::report_with(None, 100, Vec::new()).config.unwrap()
                }),
            }
        });
        Request {
            ids: scanned.mutants.iter().map(|mutant| mutant.id.clone()).collect(),
            report,
            parent: None,
        }
    }

    #[test]
    fn exact_resolution_collects_missing_and_duplicate_diagnostics_before_filtering() {
        let mut scanned = scan("fn f(a: i32) -> i32 { a + 1 }");
        let mut requested = request(&scanned, false);
        let _ = requested.ids.insert(MutantId::new("0123456789ab"));
        scanned.mutants.push(scanned.mutants[0].clone());
        let failure = requested.resolve_scan(scanned, false).unwrap_err();
        assert!(failure.is_usage());
        assert!(failure.to_string().contains("ambiguous duplicate current ID"), "{failure}");
        assert!(failure.to_string().contains("0123456789ab: unknown current mutant"), "{failure}");
    }

    #[test]
    fn unchanged_site_id_does_not_authorize_a_changed_body_or_reused_occurrence() {
        let original = scan("fn f(a: i32) -> i32 { let x = a + 1; let y = a + 1; x + y }");
        let old_id = original.mutants[0].id.clone();
        let mut requested = request(&original, true);
        requested.ids = [old_id.clone()].into();
        for changed in [
            "fn f(a: i32) -> i32 { let y = a + 1; y }",
            "fn f(a: i32) -> i32 { let x = a + 1; let y = a + 1; x - y }",
        ] {
            let current = scan(changed);
            assert_eq!(current.mutants[0].id, old_id);
            let failure = requested.resolve_scan(current, false).unwrap_err();
            assert!(failure.to_string().contains("enclosing source item changed"), "{failure}");
        }
    }

    #[test]
    fn historical_matching_ignores_comments_layout_sibling_tests_and_item_motion() {
        let original = scan("mod m { struct X; impl X { fn f(a: i32) -> i32 { a + 1 } } }");
        let current = scan(
            "#[cfg(test)] fn added_test() { assert!(true); }\n\
                     mod m { /* explanation */ struct X;\n impl X { /// documentation\n\
                     fn f(a:i32)->i32 {\n a /* comment */ + 1\n } } }",
        );
        assert_eq!(original.mutants[0].id, current.mutants[0].id);
        let resolved = request(&original, true).resolve_scan(current, false).unwrap();
        assert_eq!(resolved.mutants.len(), 1);
        assert_eq!(resolved.mutants[0].outcome, Outcome::Pending);
    }

    #[test]
    fn historical_matching_checks_each_source_identity_component() {
        let original = scan("fn f(a: i32) -> i32 { a + 1 }");
        for change in 0..5 {
            let mut requested = request(&original, true);
            let report = requested.report.as_mut().unwrap();
            let file = report.files.get_mut("src/lib.rs").unwrap();
            match change {
                0 => file.mutants[0].mutator_name = "arith.sub_to_add".into(),
                1 => file.mutants[0].replacement = Some("a * 1".into()),
                2 => file.mutants[0].replacement = None,
                3 => file.source = "fn g(a: i32) -> i32 { a + 1 }".to_owned(),
                _ => report.framework.name = "foreign".to_owned(),
            }
            let _ = requested.resolve_scan(scan("fn f(a: i32) -> i32 { a + 1 }"), false).unwrap_err();
        }
    }

    #[test]
    fn suppressed_exact_requests_conflict_but_explanation_can_show_them() {
        let mut scanned = scan("fn f(a: i32) -> i32 { a + 1 }");
        let requested = request(&scanned, false);
        scanned.mutants[0].outcome = Outcome::Ignored;
        let failure = requested.resolve_scan(scanned.clone(), false).unwrap_err();
        assert!(failure.to_string().contains("suppressed"), "{failure}");
        assert_eq!(requested.resolve_scan(scanned, true).unwrap().suppressed, 1);
    }

    #[test]
    fn report_coordinates_are_checked_in_characters_including_bom_and_crlf() {
        let source = "\u{feff}éx\r\nz\n";
        let span = source_span(
            source,
            Location {
                start: Position { line: 1, column: 2 },
                end: Position { line: 2, column: 2 },
            },
        )
        .unwrap();
        assert_eq!(&source[span], "éx\r\nz");
        for (line, column) in [(0, 1), (1, 0), (4, 1), (2, 3), (usize::MAX, usize::MAX)] {
            let _ = offset(source, Position { line, column }).unwrap_err();
        }
        let _ = source_span(
            source,
            Location {
                start: Position { line: 2, column: 1 },
                end: Position { line: 1, column: 1 },
            },
        )
        .unwrap_err();
    }

    #[test]
    fn exact_lookup_includes_opt_in_mutators_without_widening_explicit_selection() {
        let source = SourceFile::parse("src/lib.rs", "fn f() -> bool { false } fn g() -> Option<u8> { None }".to_owned()).unwrap();
        let all = selection(&SelectArgs::default(), true).unwrap();
        let found = crate::ops::into_mutants(&source, "subject", crate::ops::collect_candidates(&source, &all));
        for name in ["fn_value.bool_true", "fn_value.some", "literal.bool_flip"] {
            let selected = Selection::parse(name).unwrap();
            let narrow = crate::ops::into_mutants(&source, "subject", crate::ops::collect_candidates(&source, &selected));
            assert!(!narrow.is_empty(), "{name}");
            assert!(
                narrow.iter().all(|mutant| found.iter().any(|candidate| candidate.id == mutant.id)),
                "{name}"
            );
        }
        let constrained = selection(
            &SelectArgs {
                mutators: Some("arith".to_owned()),
                ..SelectArgs::default()
            },
            true,
        )
        .unwrap();
        assert!(!constrained.contains("fn_value.some"));
    }

    #[test]
    fn exact_lookup_preserves_ordinary_occurrences_and_report_checks_token_position() {
        let text = "pub fn f(g: fn()) -> bool { if true { g(); } let x = true; x }";
        let default = scan_with(text, &Selection::default_preset());
        let all = selection(&SelectArgs::default(), true).unwrap();
        let exact = scan_with(text, &all);
        for mutant in &default.mutants {
            let found = exact.mutants.iter().find(|candidate| candidate.id == mutant.id).unwrap();
            assert_eq!(found.span, mutant.span, "{}", mutant.mutator);
        }
        let narrow = Selection::parse("literal.bool_flip").unwrap();
        let original = scan_with(text, &narrow);
        let mut requested = request(&original, true);
        requested.ids = [original.mutants[0].id.clone()].into();
        let failure = requested.resolve_scan(exact, false).unwrap_err();
        assert!(failure.to_string().contains("enclosing source item changed"), "{failure}");
        let matched = requested.resolve_scan(scan_with(text, &narrow), false).unwrap();
        assert_eq!(matched.mutants[0].span, original.mutants[0].span);
    }

    #[test]
    fn bounded_request_permutations_preserve_sets_and_repeated_site_deletion_is_stale() {
        bolero::check!().with_type::<[u8; 8]>().for_each(|input| {
            let original = scan("fn f(a: i32) -> i32 { let x = a + 1; let y = a + 1; x + y }");
            let chosen = input
                .iter()
                .map(|byte| original.mutants[usize::from(*byte) % original.mutants.len()].id.to_string())
                .collect::<Vec<_>>();
            let first = Request::load(&chosen, None).unwrap().unwrap();
            let mut reversed = chosen.clone();
            reversed.reverse();
            reversed.extend(chosen);
            let second = Request::load(&reversed, None).unwrap().unwrap();
            let ids = |resolved: Scanned| resolved.mutants.into_iter().map(|mutant: Mutant| mutant.id).collect::<Vec<_>>();
            assert_eq!(
                ids(first.resolve_scan(original.clone(), false).unwrap()),
                ids(second.resolve_scan(original.clone(), false).unwrap())
            );
            let mut historical = request(&original, true);
            historical.ids = [original.mutants[0].id.clone()].into();
            let moved = format!(
                "{}fn f(a: i32) -> i32 {{ let y = a + 1; y }}",
                "\n".repeat(usize::from(input[0] % 8))
            );
            let _ = historical.resolve_scan(scan(&moved), false).unwrap_err();
        });
    }
}
