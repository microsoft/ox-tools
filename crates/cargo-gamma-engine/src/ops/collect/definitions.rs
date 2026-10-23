// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Turning the candidates a file admits into mutant definitions with stable identities.

use std::sync::Arc;

use camino::Utf8Path;
use compact_str::CompactString;
use quote::ToTokens as _;

use super::Candidate;
use crate::HashMap;
use crate::model::{Interner, MutantDefinition, MutationSite, SiteIndex, mutant_id_with_discriminator, normalize_site_text, site_key};
use crate::parse::SourceFile;

/// Turns candidates into source-level mutant definitions, assigning stable ids.
///
/// # Panics
///
/// Panics if a caller supplies a `fn_value.stated` candidate whose replacement is not a Rust
/// expression. Collected stated candidates have already passed expression validation.
#[must_use]
pub fn into_definitions(file: &SourceFile, candidates: Vec<Candidate>) -> Vec<MutantDefinition> {
    // One copy of the path for the whole file, rather than one per mutation.
    let path: Arc<Utf8Path> = Arc::from(Utf8Path::new(file.path.as_str()));

    // The mutator names and item paths repeat within a file too — a few dozen distinct values
    // across every mutant it produces — so they are shared as they are met. Sharing them across
    // files as well is the survey's job, once it has the whole population.
    let mut interner = Interner::default();

    // Per-span site table: candidates that target the same byte range share one MutationSite, and
    // its normalized text, computed once per span rather than once per candidate — every mutator
    // offered at a site re-derives the same identity component from the same bytes otherwise, and a
    // site with several replacements pays for the normalization as many times as it has mutants.
    let mut sites: HashMap<core::ops::Range<usize>, (Arc<MutationSite>, CompactString)> = HashMap::default();
    for candidate in &candidates {
        let _site = sites.entry(candidate.span.clone()).or_insert_with(|| {
            let original = CompactString::new(file.slice(&candidate.span));
            let (line, column) = file.location(candidate.span.start);
            let end_line = file.location(candidate.span.end).0;
            let normalized = normalize_site_text(&original);
            (
                Arc::new(MutationSite {
                    span: candidate.span.clone(),
                    line,
                    end_line,
                    column,
                    original,
                }),
                normalized,
            )
        });
    }

    let mut occurrences: HashMap<u128, u32> = HashMap::default();
    let mut site_occurrences: HashMap<(u128, core::ops::Range<usize>), u32> = HashMap::default();
    for candidate in &candidates {
        let Some(index) = candidate.identity_occurrence else {
            continue;
        };
        let (_, normalized) = sites
            .get(&candidate.span)
            .expect("every candidate span was inserted into the site table above");
        let key = site_key(&candidate.item_path, candidate.mutator, normalized);
        let next = index.saturating_add(1);
        occurrences
            .entry(key)
            .and_modify(|occurrence| *occurrence = (*occurrence).max(next))
            .or_insert(next);
        let _existing = site_occurrences.entry((key, candidate.span.clone())).or_insert(index);
    }

    let mut definitions = Vec::with_capacity(candidates.len());

    for candidate in candidates {
        let (site, normalized) = sites
            .get(&candidate.span)
            .expect("every candidate span was inserted into the site table above");
        let site = Arc::clone(site);

        let key = site_key(&candidate.item_path, candidate.mutator, normalized);
        let site_key = (key, candidate.span.clone());
        let index = if let Some(index) = candidate.identity_occurrence {
            index
        } else if let Some(index) = site_occurrences.get(&site_key) {
            *index
        } else {
            let occurrence = occurrences.entry(key).or_insert(0);
            let index = *occurrence;
            *occurrence += 1;
            let _previous = site_occurrences.insert(site_key, index);
            index
        };

        // Stated annotations can change independently of the site text; distinguish their
        // replacements while keeping formatting-only annotation changes identity-neutral.
        let stated = (candidate.mutator == "fn_value.stated").then(|| {
            syn::parse_str::<syn::Expr>(&candidate.replacement)
                .expect("fn_value.stated candidates must contain a valid Rust expression")
                .to_token_stream()
                .to_string()
        });
        let discriminator = stated
            .as_deref()
            .or_else(|| (candidate.mutator == "fn_value.err_with").then_some(candidate.replacement.as_str()));

        definitions.push(MutantDefinition {
            id: mutant_id_with_discriminator(
                &file.path,
                &candidate.item_path,
                candidate.mutator,
                normalized,
                SiteIndex::new(index, candidate.replacement_index),
                discriminator,
            ),
            file: Arc::clone(&path),
            site,
            mutator: interner.text(candidate.mutator),
            item_path: interner.text(&candidate.item_path),
            trait_impl: candidate.trait_impl,
            occurrence: index,
            replacement_index: candidate.replacement_index,
            replacement: candidate.replacement,
            shape: candidate.shape,
            confidence: candidate.confidence,
        });
    }

    definitions
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::cfg::CfgSet;
    use crate::ops::collect::collect_in;
    use crate::ops::registry::Selection;

    #[test]
    #[should_panic(expected = "fn_value.stated candidates must contain a valid Rust expression")]
    fn manually_supplied_stated_candidates_require_valid_expressions() {
        let file = SourceFile::parse("fixture.rs", "#[gamma::value(2)] fn f() -> u32 { 1 }".into()).unwrap();
        let mut candidates = collect_in(&file, &Selection::parse("fn_value.stated").unwrap(), &CfgSet::unconditional());
        candidates[0].replacement = "1 +".into();
        let _definitions = into_definitions(&file, candidates);
    }

    #[test]
    fn stated_identity_tracks_normalized_replacement_without_changing_other_mutants() {
        let identify = |value: &str| {
            let source = format!("#[gamma::value({value})] fn f() -> u32 {{ 1 }} fn other() -> u32 {{ 3 }}");
            let file = SourceFile::parse("fixture.rs", source).unwrap();
            into_definitions(&file, collect_in(&file, &Selection::everything(), &CfgSet::unconditional()))
        };
        let invalid = identify("false");
        let corrected = identify("2");
        let formatted = identify(" /* trivia */ 2 ");
        let stated = |definitions: &[MutantDefinition]| {
            definitions
                .iter()
                .find(|definition| definition.mutator.as_ref() == "fn_value.stated")
                .unwrap()
                .id
                .clone()
        };
        assert_ne!(stated(&invalid), stated(&corrected));
        assert_eq!(stated(&corrected), stated(&formatted));
        for (compact, spaced) in [
            ("1+2", "1 /* trivia */ + 2"),
            ("(1,2).0", "( 1 , 2 ) . 0"),
            ("Some::<u32>(2)", "Some :: < u32 > ( 2 )"),
            ("\"a  b\"", " /* trivia */ \"a  b\" "),
        ] {
            assert_eq!(stated(&identify(compact)), stated(&identify(spaced)), "{compact}");
        }
        for (left, right) in [
            ("1+2", "1-2"),
            ("\"a b\"", "\"a  b\""),
            ("r#\"a b\"#", "r#\"a  b\"#"),
            ("left&&right", "left & &right"),
        ] {
            assert_ne!(stated(&identify(left)), stated(&identify(right)), "{left} versus {right}");
        }
        let unrelated = |definitions: Vec<MutantDefinition>| {
            definitions
                .into_iter()
                .filter(|definition| definition.mutator.as_ref() != "fn_value.stated")
                .map(|definition| (definition.item_path, definition.mutator, definition.id))
                .collect::<Vec<_>>()
        };
        assert!(!unrelated(invalid.clone()).is_empty());
        assert_eq!(unrelated(invalid), unrelated(corrected));
    }

    #[test]
    fn externally_supplied_unreserved_candidates_start_each_site_family_at_zero() {
        let file = SourceFile::parse("fixture.rs", "fn f() { alpha(); alpha(); beta(); }".into()).unwrap();
        let mut candidates = collect_in(&file, &Selection::parse("stmt.delete_call").unwrap(), &CfgSet::unconditional());
        for candidate in &mut candidates {
            candidate.identity_occurrence = None;
        }
        let definitions = into_definitions(&file, candidates);
        assert_eq!(definitions.iter().map(|value| value.occurrence).collect::<Vec<_>>(), [0, 1, 0]);
        assert_eq!(
            definitions.iter().map(MutantDefinition::original).collect::<Vec<_>>(),
            ["alpha();", "alpha();", "beta();"]
        );
        assert_ne!(definitions[0].id, definitions[1].id);
    }

    #[test]
    fn reserved_occurrences_advance_unreserved_sites_and_share_same_span() {
        let file = SourceFile::parse("fixture.rs", "fn f() { a(); a(); a(); a(); }".to_owned()).unwrap();
        let selection = Selection::parse("stmt.delete_call").unwrap();
        let mut candidates = collect_in(&file, &selection, &CfgSet::unconditional());
        assert_eq!(candidates.len(), 4);
        candidates[0].identity_occurrence = Some(4);
        candidates[1].identity_occurrence = Some(2);
        candidates[2].identity_occurrence = None;
        candidates[3].identity_occurrence = None;
        let mut duplicate = candidates[2].clone();
        duplicate.replacement_index = 7;
        candidates.push(duplicate);
        let definitions = into_definitions(&file, candidates);
        assert_eq!(
            definitions.iter().map(|value| value.occurrence).collect::<Vec<_>>(),
            [4, 2, 5, 6, 5]
        );
        assert_eq!(definitions[4].replacement_index, 7);
        assert_ne!(definitions[2].id, definitions[4].id);
    }
}
