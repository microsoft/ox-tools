// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Which mutant a compiler error belongs to.
//!
//! Rustc spans first provide direct replacement blame or the deletion-only containment fallback.
//! A non-empty replacement that remains unresolved is handed back to the parent build coordinator
//! for proof-build isolation. Every resulting withdrawal is summarized as a bounded
//! [`CompilerReason`].
//! Flow-sensitive failures always require proof builds: even a primary replacement span may
//! describe the consequence of a neighboring guard rather than an invalid individual edit.

use core::ops::Range;

use camino::Utf8Path;

use super::Guards;
use super::messages::{CompilerMessage, Span, cargo_message};
use crate::schema::{Guard, Position};
use crate::{HashMap, HashSet};

/// A bounded, host-independent account of why rustc rejected a mutant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum AttributionEvidence {
    #[default]
    OtherDiagnostic,
    ReplacementPrimary,
    ProofBuild,
}

/// A bounded, host-independent account of why rustc rejected a mutant.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct CompilerReason {
    /// The primary rustc error code, empty when the diagnostic carried none.
    pub code: String,

    /// A normalized form of the primary diagnostic message.
    pub category: String,

    /// How the compiler failure was attributed to this mutant.
    pub evidence: AttributionEvidence,
}

impl CompilerReason {
    #[must_use]
    pub fn isolated() -> Self {
        Self {
            category: "isolated compiler failure".to_owned(),
            evidence: AttributionEvidence::ProofBuild,
            ..Self::default()
        }
    }

    pub(super) const fn replacement_site(&self) -> bool {
        matches!(self.evidence, AttributionEvidence::ReplacementPrimary)
    }

    pub(super) fn note(&self) -> String {
        let code = if self.code.is_empty() {
            String::new()
        } else {
            format!(" {}", self.code)
        };
        let site = match self.evidence {
            AttributionEvidence::ReplacementPrimary => "; primary span identified the replacement site",
            AttributionEvidence::OtherDiagnostic => "; primary span did not identify the replacement site",
            AttributionEvidence::ProofBuild => "; isolated by proof builds without diagnostic-span attribution",
        };

        format!("rustc{code}: {}{site}", self.category)
    }
}

/// Works out which mutants to blame for a failed build.
///
/// Guard positions come from the instrumented text rather than from the mutants' source lines,
/// because a guard emits the original text alongside the mutated one and so shifts every later
/// line. Primary spans drive containment; secondary spans must lie wholly within generated
/// replacement text, because notes routinely enclose innocent neighboring guards.
///
/// For positional errors, a diagnostic wholly inside generated replacement text identifies that
/// branch. A deletion has no replacement text, so the innermost deleted site containing the
/// diagnostic remains sufficient evidence. Flow-sensitive errors and containment involving a
/// non-empty replacement require proof-build isolation before reporting individual unviability.
#[expect(clippy::too_many_lines, reason = "the attribution tiers share one parsed diagnostic walk")]
pub fn blame(stdout: &str, root: &Utf8Path, guards: &Guards) -> HashMap<u32, CompilerReason> {
    const SECONDARY_ATTRIBUTION_BUDGET: usize = 65_536;

    let mut exact_blame: HashMap<u32, CompilerReason> = HashMap::default();
    let mut deletion_blame: HashMap<u32, CompilerReason> = HashMap::default();

    // A failing build reports many diagnostics and a large workspace has many guards, so pairing
    // them off one at a time is quadratic. Grouping by file first makes the common case a lookup.
    let mut by_path: HashMap<&Utf8Path, Vec<(u32, &Guard)>> = HashMap::default();

    for (ordinal, (file, guard)) in guards {
        by_path.entry(file.as_path()).or_default().push((*ordinal, guard));
    }

    for line in stdout.lines() {
        let Some(message) = cargo_message(line) else {
            continue;
        };

        if message.reason != "compiler-message" {
            continue;
        }

        let Some(diagnostic) = message.message else {
            continue;
        };

        if diagnostic.level != "error" {
            continue;
        }

        if diagnostic
            .code
            .as_ref()
            .is_some_and(|code| FLOW_SENSITIVE.contains(&code.code.as_ref()))
        {
            continue;
        }

        let primary: Vec<&Span<'_>> = diagnostic.spans.iter().filter(|span| span.is_primary).collect();
        let considered = if primary.is_empty() {
            diagnostic.spans.iter().collect()
        } else {
            primary
        };
        let mut exact = HashSet::default();
        let mut replacement_sites = HashSet::default();
        let mut enclosing: Option<(u32, HashSet<u32>)> = None;
        let mut contained: Option<(u32, HashSet<u32>)> = None;

        for span in considered {
            let Some(file_name) = span.file_name.as_deref() else {
                continue;
            };

            let relative = Utf8Path::new(file_name)
                .strip_prefix(root.as_str())
                .unwrap_or_else(|_ignored| Utf8Path::new(file_name));

            let Some(reported) = position_range(span) else {
                continue;
            };

            // The exact relative path is the normal case; the scan is the fallback for a diagnostic
            // whose path is spelled differently, and only runs when the lookup found nothing.
            let matched = by_path.get(relative).map(Vec::as_slice).unwrap_or_default();
            let scanned;

            let here = if matched.is_empty() {
                scanned = by_path
                    .iter()
                    .filter(|(file, _found)| file_name.ends_with(file.as_str()))
                    .flat_map(|(_file, found)| found.iter().copied())
                    .collect::<Vec<_>>();

                scanned.as_slice()
            } else {
                matched
            };

            for (ordinal, guard) in here.iter().copied() {
                if guard.mutated.as_ref().is_some_and(|mutated| covers(mutated, &reported)) {
                    let _ = exact.insert(ordinal);
                    if span.is_primary {
                        let _ = replacement_sites.insert(ordinal);
                    }
                } else if guard.mutated.is_none() && covers(&guard.site, &reported) {
                    let width = guard.site.end.line().saturating_sub(guard.site.start.line());

                    match &mut enclosing {
                        Some((best, ordinals)) if *best == width => {
                            let _ = ordinals.insert(ordinal);
                        }
                        Some((best, _ordinals)) if
                        // #[gamma::skip(relational.lt_to_le, reason = "Both ordered match expressions first handle best==width by inserting ordinal. The mutated next guard is therefore evaluated only with best!=width; best<width and best<=width are identical on that reachable domain.", tag = "equivalent")]
                        *best < width => {}
                        _ => enclosing = Some((width, HashSet::from_iter([ordinal]))),
                    }
                } else if guard.mutated.is_none() && covers(&reported, &guard.site) {
                    // A positional complaint can cover a whole construct whose type depended
                    // on a deleted exit. Keep the smallest reported region.
                    let width = reported.end.line().saturating_sub(reported.start.line());

                    match &mut contained {
                        Some((best, ordinals)) if *best == width => {
                            let _ = ordinals.insert(ordinal);
                        }
                        Some((best, _ordinals)) if
                        // #[gamma::skip(relational.lt_to_le, reason = "Both ordered match expressions first handle best==width by inserting ordinal. The mutated next guard is therefore evaluated only with best!=width; best<width and best<=width are identical on that reachable domain.", tag = "equivalent")]
                        *best < width => {}
                        _ => contained = Some((width, HashSet::from_iter([ordinal]))),
                    }
                }
            }
        }

        // rustc sometimes puts the consequence of a mutation in the primary span and the cause in
        // a secondary span. Only a span wholly inside replacement text identifies that branch;
        // an enclosing match or a partially overlapping consequence also covers innocent guards.
        if exact.is_empty() {
            secondary_attribution(&diagnostic, root, &by_path, &mut exact, SECONDARY_ATTRIBUTION_BUDGET);
        }

        let (ordinals, blamed) = if exact.is_empty() {
            (enclosing.or(contained).map(|(_width, ordinals)| ordinals), &mut deletion_blame)
        } else {
            (Some(exact), &mut exact_blame)
        };

        if let Some(ordinals) = ordinals {
            if
            // #[gamma::skip(cond.always_false, reason = "When all ordinals already occur in blamed, the original skip avoids only pure code/category normalization. Without skip, the following per-ordinal loop checks blamed.contains_key and continues for every ordinal, so the same map/first causes are preserved and no side-effecting callback runs.", tag = "equivalent")]
            ordinals.iter().all(|ordinal| blamed.contains_key(ordinal)) {
                // #[gamma::skip(loop.delete_continue, reason = "When all ordinals already occur in blamed, the original skip avoids only pure code/category normalization. Without skip, the following per-ordinal loop checks blamed.contains_key and continues for every ordinal, so the same map/first causes are preserved and no side-effecting callback runs.", tag = "equivalent")]
                continue;
            }

            // The first diagnostic to name a mutant is the one kept. A single unviable mutant can
            // draw a thousand follow-on complaints, and the later ones describe the wreckage rather
            // than the cause; the census is only worth reading if each mutant contributes the one
            // error that explains it.
            let code = diagnostic.code.as_ref().map_or("", |code| code.code.as_ref());
            let category = normalize_category(&diagnostic.message);

            for ordinal in ordinals {
                if blamed.contains_key(&ordinal) {
                    continue;
                }
                let reason = CompilerReason {
                    code: code.to_owned(),
                    category: category.clone(),
                    evidence: if replacement_sites.contains(&ordinal) {
                        AttributionEvidence::ReplacementPrimary
                    } else {
                        AttributionEvidence::OtherDiagnostic
                    },
                };
                let _previous = blamed.insert(ordinal, reason);
            }
        }
    }

    // Exact generated-text evidence wins for the whole compiler batch. rustc commonly emits a
    // root diagnostic followed by consequences whose broad spans cover neighboring guards. Taking
    // deletion containment from those follow-ons in the same round would falsely withdraw siblings.
    // If an independent deletion also fails, the next rollback round will expose and attribute it.
    if exact_blame.is_empty() { deletion_blame } else { exact_blame }
}

fn secondary_attribution(
    diagnostic: &CompilerMessage<'_>,
    root: &Utf8Path,
    by_path: &HashMap<&Utf8Path, Vec<(u32, &Guard)>>,
    exact: &mut HashSet<u32>,
    mut remaining: usize,
) {
    for span in diagnostic.spans.iter().filter(|span| !span.is_primary) {
        let Some(file_name) = span.file_name.as_deref() else {
            continue;
        };
        let relative = Utf8Path::new(file_name)
            .strip_prefix(root.as_str())
            .unwrap_or_else(|_ignored| Utf8Path::new(file_name));
        let Some(reported) = position_range(span) else {
            continue;
        };
        let matched = by_path.get(relative).map(Vec::as_slice).unwrap_or_default();
        if matched.len() > remaining {
            break;
        }
        remaining -= matched.len();
        for (ordinal, guard) in matched.iter().copied() {
            if guard.mutated.as_ref().is_some_and(|mutated| covers(mutated, &reported)) {
                let _ = exact.insert(ordinal);
            }
        }
    }
}

/// Reduces rustc's primary message to a bounded category without retaining source or host text.
///
/// Dynamic values are conventionally quoted by rustc and become one placeholder. Path-shaped
/// tokens become another, digit runs collapse, whitespace is normalized, and the result is capped.
/// The category remains readable enough to distinguish type, ownership, and initialization
/// failures while never becoming a copy of an arbitrarily long compiler message.
fn normalize_category(message: &str) -> String {
    // Long enough to retain rustc's distinguishing category while bounding source-authored text
    // in diagnostics; the redaction policy is applied after this normalization.
    const LIMIT: usize = 96;

    fn consume_path_tail(characters: &mut core::iter::Peekable<core::str::Chars<'_>>) {
        loop {
            // #[gamma::skip(unary.remove_not, tag = "timeout", reason = "the whitespace predicate must become false at the end of the path tail")]
            // #[gamma::skip(cond.negate, tag = "timeout", reason = "each path component must be consumed before lookahead; retaining it repeatedly rediscovers the same component")]
            while characters.peek().is_some_and(|next| !next.is_whitespace()) {
                let _ = characters
                    .next()
                    .expect("the loop's peek guard established an unconsumed path character");
            }

            let mut lookahead = characters.clone();
            while lookahead.peek().is_some_and(|next| next.is_whitespace()) {
                let _ = lookahead
                    .next()
                    .expect("the loop's peek guard established an unconsumed whitespace character");
            }
            let next_component: String = lookahead.take_while(|next| !next.is_whitespace()).collect();
            // #[gamma::skip(cond.always_false, cond.negate, unary.remove_not, tag = "timeout", reason = "without the component stop condition an exhausted path iterator is scanned forever")]
            if !next_component.contains('/') && !next_component.contains('\\') {
                // #[gamma::skip(loop.break_to_continue, loop.delete_break, tag = "timeout", reason = "the path-tail scan must stop when no further path component exists")]
                break;
            }

            // #[gamma::skip(cond.negate, tag = "timeout", reason = "separating whitespace must be consumed before scanning the next path component; retaining it prevents forward progress")]
            while characters.peek().is_some_and(|next| next.is_whitespace()) {
                let _ = characters
                    .next()
                    .expect("the loop's peek guard established an unconsumed whitespace character");
            }
        }
    }

    let mut normalized = String::new();
    let mut characters = message.chars().peekable();

    while let Some(character) = characters.next() {
        if matches!(character, '`' | '"') {
            let delimiter = character;
            for next in characters.by_ref() {
                if next == delimiter {
                    break;
                }
            }
            push_piece(&mut normalized, "<value>", LIMIT);
            continue;
        }

        if character.is_whitespace() {
            if !normalized.ends_with(' ') && !normalized.is_empty() {
                push_piece(&mut normalized, " ", LIMIT);
            }
            continue;
        }

        if character.is_ascii_digit() {
            // #[gamma::skip(cond.negate, tag = "timeout", reason = "negating the digit predicate keeps scanning an exhausted iterator forever")]
            while characters.peek().is_some_and(char::is_ascii_digit) {
                let _ = characters.next();
            }
            push_piece(&mut normalized, "#", LIMIT);
            continue;
        }

        if character == '/' || character == '\\' {
            consume_path_tail(&mut characters);
            for _ in
                // #[gamma::skip(range.exclusive_to_inclusive, reason = "Path-token pop loop stops changing the string once the final character is whitespace or the string is empty. One extra bounded iteration,or continuing rather than breaking at that state, cannot pop past the same boundary.", tag = "equivalent")]
                0..
            // #[gamma::skip(expr.increment, reason = "Path-token pop loop stops changing the string once the final character is whitespace or the string is empty. One extra bounded iteration,or continuing rather than breaking at that state, cannot pop past the same boundary.", tag = "equivalent")]
            normalized.len()
            {
                if normalized.chars().next_back().is_none_or(char::is_whitespace) {
                    // #[gamma::skip(loop.break_to_continue, reason = "Path-token pop loop stops changing the string once the final character is whitespace or the string is empty. One extra bounded iteration,or continuing rather than breaking at that state, cannot pop past the same boundary.", tag = "equivalent")]
                    break;
                }
                let _last = normalized.pop();
            }
            push_piece(&mut normalized, "<path>", LIMIT);
            continue;
        }

        if normalized.len().saturating_add(character.len_utf8()) <= LIMIT {
            normalized.push(character);
        }
    }

    let normalized = normalized
        .trim()
        .trim_end_matches(
            // #[gamma::skip(collection.reverse_array, reason = "The character array is a pattern membership set; reversing its order leaves every match unchanged.", tag = "equivalent")]
            [':', ';', ',', '.'],
        )
        .trim();

    if normalized.is_empty() {
        "uncategorized compiler error".to_owned()
    } else {
        normalized.to_owned()
    }
}

fn push_piece(target: &mut String, piece: &str, limit: usize) {
    for character in piece.chars() {
        if target.len().saturating_add(character.len_utf8()) > limit {
            break;
        }
        target.push(character);
    }
}

/// Flow-sensitive diagnostics require singleton proof builds, even when their primary span
/// names generated text. Guarding another statement can make an otherwise valid move, borrow,
/// or initialization fail; span proximity cannot distinguish that interaction from invalidity.
pub(super) const FLOW_SENSITIVE: &[&str] = &[
    "E0381", // used binding is possibly-uninitialized
    "E0382", // use of moved value
    "E0383", // partial reinitialization of an uninitialized structure
    "E0384", // cannot assign twice to immutable variable
    "E0499", // cannot borrow as mutable more than once
    "E0502", // cannot borrow as mutable because also borrowed as immutable
    "E0503", // cannot use value because it was mutably borrowed
    "E0505", // cannot move out of value because it is borrowed
    "E0506", // cannot assign to value because it is borrowed
    "E0507", // cannot move out of borrowed content
    "E0508", // cannot move out of type, a non-copy array
    "E0509", // cannot move out of type which implements Drop
    "E0510", // cannot mutate place in this match guard
    "E0594", // cannot assign to borrowed content
    "E0596", // cannot borrow as mutable
    "E0597", // borrowed value does not live long enough
    "E0716", // temporary value dropped while borrowed
];

/// Narrows one of cargo's line or column numbers, saturating rather than refusing it.
pub(super) fn clamped(number: u64) -> u32 {
    u32::try_from(number).unwrap_or(u32::MAX)
}

/// Reads the region a compiler diagnostic points at out of its JSON span.
///
/// A span whose line or column is absent, or is zero, yields no region: cargo counts both from
/// one, so zero means the producer supplied no position rather than the first character.
pub(super) fn position_range(span: &Span<'_>) -> Option<Range<Position>> {
    let at = |line: Option<u64>, column: Option<u64>| Position::new(clamped(line?), clamped(column?));

    Some(at(span.line_start, span.column_start)?..at(span.line_end, span.column_end)?)
}

/// Reports whether a guard's region wholly contains the one a diagnostic points at.
pub(super) fn covers(range: &Range<Position>, reported: &Range<Position>) -> bool {
    range.start <= reported.start && range.end >= reported.end
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod final_survivor_tests {
    use core::num::NonZeroU32;

    use super::*;

    #[test]
    fn recovered_category_leading_whitespace_does_not_spend_the_content_budget() {
        let content = "a".repeat(96);
        assert_eq!(normalize_category(&format!(" \t{content}")), content);
    }

    #[test]
    fn recovered_category_redacts_entire_path_tokens_and_space_separated_path_components() {
        for (message, expected) in [
            ("secret/path.rs", "<path>"),
            ("cannot read /secret/path.rs after", "cannot read <path> after"),
            ("cannot read C:\\Secret Folder\\module.rs after", "cannot read <path> after"),
            ("λ/secret.rs", "<path>"),
        ] {
            assert_eq!(normalize_category(message), expected, "{message}");
        }
    }

    #[test]
    fn diagnostic_categories_collapse_each_interior_whitespace_run() {
        for message in ["alpha   beta", "alpha \t\n beta", "\talpha  beta\n"] {
            assert_eq!(normalize_category(message), "alpha beta");
        }
    }

    #[test]
    fn recovered_full_category_budget_does_not_preserve_a_space_before_path_redaction() {
        assert_eq!(normalize_category(&format!("{} /private/source.rs", "x".repeat(96))), "<path>");
    }

    #[test]
    fn recovered_exact_diagnostic_paths_precede_suffix_aliases_and_later_narrower_deletions_win() {
        let shared = Guard {
            site: range(NonZeroU32::new(1).unwrap(), NonZeroU32::new(20).unwrap()),
            mutated: Some(range(NonZeroU32::new(6).unwrap(), NonZeroU32::new(8).unwrap())),
        };
        let guards = Guards::from_iter([(1, ("nested/a.rs".into(), shared.clone())), (2, ("a.rs".into(), shared))]);
        let message = diagnostic("error", "E0308", &[span(Some("nested/a.rs"), 6, 7, true)]);
        let blamed = blame(&message, Utf8Path::new("workspace"), &guards);
        assert_eq!(blamed.keys().copied().collect::<HashSet<_>>(), HashSet::from_iter([1]));

        let mut guards = guards;
        let last = *guards.keys().last().unwrap();
        for (ordinal, (path, guard)) in &mut guards {
            *path = "a.rs".into();
            guard.mutated = None;
            if *ordinal == last {
                guard.site = range(NonZeroU32::new(5).unwrap(), NonZeroU32::new(10).unwrap());
            }
        }
        let message = diagnostic("error", "E0308", &[span(Some("a.rs"), 6, 7, true)]);
        let blamed = blame(&message, Utf8Path::new("workspace"), &guards);
        assert_eq!(blamed.keys().copied().collect::<HashSet<_>>(), HashSet::from_iter([last]));
    }

    #[test]
    fn recovered_flow_sensitive_and_repeated_diagnostics_do_not_hide_later_independent_errors() {
        let guard = Guard {
            site: range(NonZeroU32::new(5).unwrap(), NonZeroU32::new(10).unwrap()),
            mutated: Some(range(NonZeroU32::new(6).unwrap(), NonZeroU32::new(8).unwrap())),
        };
        let guards = Guards::from_iter([(1, ("a.rs".into(), guard.clone())), (2, ("b.rs".into(), guard))]);
        let stream = [
            diagnostic("error", "E0382", &[span(Some("a.rs"), 6, 7, true)]),
            diagnostic("error", "E0308", &[span(Some("a.rs"), 6, 7, true)]),
            diagnostic("error", "E0999", &[span(Some("a.rs"), 6, 7, true)]),
            diagnostic("error", "E0277", &[span(Some("b.rs"), 6, 7, true)]),
        ]
        .join("\n");
        let blamed = blame(&stream, Utf8Path::new("workspace"), &guards);
        assert_eq!(blamed.len(), 2);
        assert_eq!(blamed[&1].code, "E0308");
        assert_eq!(blamed[&2].code, "E0277");
    }

    #[test]
    fn recovered_secondary_attribution_checks_the_exact_global_guard_budget() {
        let guard = Guard {
            site: range(NonZeroU32::new(5).unwrap(), NonZeroU32::new(10).unwrap()),
            mutated: Some(range(NonZeroU32::new(6).unwrap(), NonZeroU32::new(8).unwrap())),
        };
        let diagnostic = diagnostic(
            "error",
            "E0308",
            &[span(Some("missing.rs"), 100, 101, true), span(Some("a.rs"), 6, 7, false)],
        );
        for count in [65_535, 65_536, 65_537] {
            let guards = (1..=count).map(|ordinal| (ordinal, ("a.rs".into(), guard.clone()))).collect();
            let blamed = blame(&diagnostic, Utf8Path::new("workspace"), &guards);
            assert_eq!(blamed.len(), if count <= 65_536 { count as usize } else { 0 }, "{count}");
            assert!(
                blamed
                    .values()
                    .all(|reason| { reason.code == "E0308" && reason.evidence == AttributionEvidence::OtherDiagnostic })
            );
        }
    }

    fn span(file: Option<&str>, start: u32, end: u32, primary: bool) -> serde_json::Value {
        serde_json::json!({
            "file_name": file, "line_start": start, "line_end": end,
            "column_start": 1, "column_end": 4, "is_primary": primary
        })
    }

    fn diagnostic(level: &str, code: &str, spans: &[serde_json::Value]) -> String {
        serde_json::json!({
            "reason": "compiler-message",
            "message": {"level": level, "code": {"code": code}, "message": "mismatched types", "spans": spans}
        })
        .to_string()
    }

    fn range(start: NonZeroU32, end: NonZeroU32) -> Range<Position> {
        Position::new(start.get(), 1).unwrap()..Position::new(end.get(), 8).unwrap()
    }

    #[test]
    fn secondary_attribution_spends_the_exact_remaining_budget_and_stops_before_later_cheaper_spans() {
        let hit = Guard {
            site: range(NonZeroU32::new(5).unwrap(), NonZeroU32::new(10).unwrap()),
            mutated: Some(range(NonZeroU32::new(6).unwrap(), NonZeroU32::new(8).unwrap())),
        };
        let miss = Guard {
            site: range(NonZeroU32::new(20).unwrap(), NonZeroU32::new(30).unwrap()),
            mutated: Some(range(NonZeroU32::new(21).unwrap(), NonZeroU32::new(22).unwrap())),
        };
        let by_path = HashMap::from_iter([
            (Utf8Path::new("a.rs"), vec![(101, &hit), (102, &miss)]),
            (Utf8Path::new("b.rs"), vec![(201, &hit), (202, &miss)]),
            (Utf8Path::new("c.rs"), vec![(301, &hit)]),
        ]);
        let message = diagnostic(
            "error",
            "E0308",
            &[
                span(None, 6, 7, false),
                span(Some("a.rs"), 0, 7, false),
                span(Some("a.rs"), 6, 7, false),
                span(Some("b.rs"), 6, 7, false),
                span(Some("c.rs"), 6, 7, false),
            ],
        );
        let message = cargo_message(&message).unwrap().message.unwrap();
        for (budget, wanted) in [(4, vec![101, 201]), (3, vec![101])] {
            let mut exact = HashSet::default();
            secondary_attribution(&message, Utf8Path::new("workspace"), &by_path, &mut exact, budget);
            assert_eq!(exact, wanted.into_iter().collect(), "budget={budget}");
        }
    }

    #[test]
    fn primary_generated_evidence_excludes_an_innocent_secondary_replacement() {
        let guards = Guards::from_iter([
            (
                1,
                (
                    "a.rs".into(),
                    Guard {
                        site: range(NonZeroU32::new(5).unwrap(), NonZeroU32::new(10).unwrap()),
                        mutated: Some(range(NonZeroU32::new(6).unwrap(), NonZeroU32::new(8).unwrap())),
                    },
                ),
            ),
            (
                2,
                (
                    "a.rs".into(),
                    Guard {
                        site: range(NonZeroU32::new(15).unwrap(), NonZeroU32::new(20).unwrap()),
                        mutated: Some(range(NonZeroU32::new(16).unwrap(), NonZeroU32::new(18).unwrap())),
                    },
                ),
            ),
        ]);
        let text = diagnostic(
            "error",
            "E0308",
            &[span(Some("a.rs"), 6, 7, true), span(Some("a.rs"), 16, 17, false)],
        );
        let blamed = blame(&text, Utf8Path::new("workspace"), &guards);
        assert_eq!(blamed.keys().copied().collect::<HashSet<_>>(), HashSet::from_iter([1]));
        assert_eq!(blamed[&1].evidence, AttributionEvidence::ReplacementPrimary);
    }

    #[test]
    fn enclosing_deletions_choose_the_later_narrower_guard_and_contained_ties_retain_every_ordinal() {
        let direct = diagnostic("error", "E0308", &[span(Some("a.rs"), 6, 7, true)]);
        for (wide, narrow) in [(1, 2), (2, 1)] {
            let guards = Guards::from_iter([
                (
                    wide,
                    (
                        "a.rs".into(),
                        Guard {
                            site: range(NonZeroU32::new(1).unwrap(), NonZeroU32::new(20).unwrap()),
                            mutated: None,
                        },
                    ),
                ),
                (
                    narrow,
                    (
                        "a.rs".into(),
                        Guard {
                            site: range(NonZeroU32::new(5).unwrap(), NonZeroU32::new(10).unwrap()),
                            mutated: None,
                        },
                    ),
                ),
            ]);
            assert_eq!(
                blame(&direct, Utf8Path::new("workspace"), &guards)
                    .keys()
                    .copied()
                    .collect::<HashSet<_>>(),
                HashSet::from_iter([narrow])
            );
        }
        let guards = Guards::from_iter([
            (
                1,
                (
                    "a.rs".into(),
                    Guard {
                        site: range(NonZeroU32::new(5).unwrap(), NonZeroU32::new(6).unwrap()),
                        mutated: None,
                    },
                ),
            ),
            (
                2,
                (
                    "a.rs".into(),
                    Guard {
                        site: range(NonZeroU32::new(8).unwrap(), NonZeroU32::new(9).unwrap()),
                        mutated: None,
                    },
                ),
            ),
        ]);
        let contained = diagnostic("error", "E0308", &[span(Some("a.rs"), 1, 20, true)]);
        assert_eq!(
            blame(&contained, Utf8Path::new("workspace"), &guards)
                .keys()
                .copied()
                .collect::<HashSet<_>>(),
            HashSet::from_iter([1, 2])
        );
    }

    #[test]
    fn partially_repeated_batches_keep_old_causes_while_adding_every_previously_unseen_ordinal() {
        let guards: Guards = (1..=5)
            .map(|ordinal| {
                (
                    ordinal,
                    (
                        "a.rs".into(),
                        Guard {
                            site: range(NonZeroU32::new(5).unwrap(), NonZeroU32::new(10).unwrap()),
                            mutated: Some(range(NonZeroU32::new(6).unwrap(), NonZeroU32::new(8).unwrap())),
                        },
                    ),
                )
            })
            .collect();
        for known in 1..=5 {
            let first = diagnostic("error", "E0308", &[span(Some("a.rs"), 6, 7, true)]);
            let mut first: serde_json::Value = serde_json::from_str(&first).unwrap();
            first["message"]["spans"][0]["file_name"] = "known.rs".into();
            let mut scoped = guards.clone();
            scoped.get_mut(&known).unwrap().0 = "known.rs".into();
            let second = diagnostic(
                "error",
                "E0277",
                &[span(Some("a.rs"), 6, 7, true), span(Some("known.rs"), 6, 7, true)],
            );
            let blamed = blame(&format!("{first}\n{second}"), Utf8Path::new("workspace"), &scoped);
            assert_eq!(blamed.len(), 5);
            for ordinal in 1..=5 {
                assert_eq!(blamed[&ordinal].code, if ordinal == known { "E0308" } else { "E0277" });
            }
        }
    }

    #[test]
    fn every_flow_sensitive_code_requires_isolation_even_with_exact_primary_or_deletion_spans() {
        let guards = Guards::from_iter([
            (
                1,
                (
                    "a.rs".into(),
                    Guard {
                        site: range(NonZeroU32::new(5).unwrap(), NonZeroU32::new(10).unwrap()),
                        mutated: Some(range(NonZeroU32::new(6).unwrap(), NonZeroU32::new(8).unwrap())),
                    },
                ),
            ),
            (
                2,
                (
                    "a.rs".into(),
                    Guard {
                        site: range(NonZeroU32::new(15).unwrap(), NonZeroU32::new(20).unwrap()),
                        mutated: None,
                    },
                ),
            ),
        ]);
        for code in FLOW_SENSITIVE {
            for spans in [
                vec![span(Some("a.rs"), 6, 7, true)],
                vec![span(Some("a.rs"), 16, 17, true)],
                vec![span(Some("a.rs"), 1, 30, true), span(Some("a.rs"), 6, 7, false)],
            ] {
                let text = diagnostic("error", code, &spans);
                assert!(blame(&text, Utf8Path::new("workspace"), &guards).is_empty(), "{code}");
            }
        }
    }

    #[test]
    fn diagnostic_walk_skips_unusable_records_and_distinguishes_primary_from_secondary_replacements() {
        let guards = Guards::from_iter([
            (
                1,
                (
                    "src/lib.rs".into(),
                    Guard {
                        site: range(NonZeroU32::new(5).unwrap(), NonZeroU32::new(10).unwrap()),
                        mutated: Some(range(NonZeroU32::new(6).unwrap(), NonZeroU32::new(8).unwrap())),
                    },
                ),
            ),
            (
                2,
                (
                    "src/lib.rs".into(),
                    Guard {
                        site: range(NonZeroU32::new(20).unwrap(), NonZeroU32::new(30).unwrap()),
                        mutated: None,
                    },
                ),
            ),
        ]);
        let prefix = [
            "not json".to_owned(),
            r#"{"reason":"compiler-artifact"}"#.to_owned(),
            r#"{"reason":"compiler-message"}"#.to_owned(),
            diagnostic("warning", "W000", &[span(Some("src/lib.rs"), 6, 7, true)]),
        ]
        .join("\n");
        for (primary, expected_evidence) in [
            (true, AttributionEvidence::ReplacementPrimary),
            (false, AttributionEvidence::OtherDiagnostic),
        ] {
            let mut spans = vec![
                span(None, 6, 7, true),
                span(Some("src/lib.rs"), 0, 7, true),
                span(Some("absent.rs"), 6, 7, true),
                span(Some("src/lib.rs"), 6, 7, primary),
            ];
            if !primary {
                spans.push(span(Some("src/lib.rs"), 100, 101, true));
            }
            let first = diagnostic("error", "E0308", &spans);
            let consequence = diagnostic("error", "E9999", &[span(Some("src/lib.rs"), 6, 7, true)]);
            let blamed = blame(&format!("{prefix}\n{first}\n{consequence}"), Utf8Path::new("workspace"), &guards);
            assert_eq!(blamed.len(), 1);
            assert_eq!(
                blamed[&1],
                CompilerReason {
                    code: "E0308".into(),
                    category: "mismatched types".into(),
                    evidence: expected_evidence,
                }
            );
        }
        let without_primary = diagnostic("error", "E0308", &[span(Some("src/lib.rs"), 6, 7, false)]);
        let blamed = blame(&without_primary, Utf8Path::new("workspace"), &guards);
        assert_eq!(blamed.len(), 1);
        assert_eq!(blamed[&1].evidence, AttributionEvidence::OtherDiagnostic);
    }

    #[test]
    fn deletion_blame_keeps_all_equal_narrowest_sites_but_exact_generated_text_wins_the_batch() {
        let guards = Guards::from_iter([
            (
                1,
                (
                    "src/lib.rs".into(),
                    Guard {
                        site: range(NonZeroU32::new(5).unwrap(), NonZeroU32::new(10).unwrap()),
                        mutated: Some(range(NonZeroU32::new(6).unwrap(), NonZeroU32::new(8).unwrap())),
                    },
                ),
            ),
            (
                2,
                (
                    "src/lib.rs".into(),
                    Guard {
                        site: range(NonZeroU32::new(20).unwrap(), NonZeroU32::new(40).unwrap()),
                        mutated: None,
                    },
                ),
            ),
            (
                3,
                (
                    "src/lib.rs".into(),
                    Guard {
                        site: range(NonZeroU32::new(22).unwrap(), NonZeroU32::new(28).unwrap()),
                        mutated: None,
                    },
                ),
            ),
            (
                4,
                (
                    "src/lib.rs".into(),
                    Guard {
                        site: range(NonZeroU32::new(22).unwrap(), NonZeroU32::new(28).unwrap()),
                        mutated: None,
                    },
                ),
            ),
        ]);
        let deletion = diagnostic("error", "E0308", &[span(Some("src/lib.rs"), 23, 24, true)]);
        let blamed = blame(&deletion, Utf8Path::new("workspace"), &guards);
        assert_eq!(blamed.len(), 2);
        for ordinal in [3, 4] {
            assert_eq!(blamed[&ordinal].code, "E0308");
            assert_eq!(blamed[&ordinal].evidence, AttributionEvidence::OtherDiagnostic);
        }
        let exact = diagnostic("error", "E0308", &[span(Some("workspace/src/lib.rs"), 6, 7, true)]);
        for batch in [format!("{deletion}\n{exact}"), format!("{exact}\n{deletion}")] {
            let blamed = blame(&batch, Utf8Path::new("workspace"), &guards);
            assert_eq!(blamed.len(), 1);
            assert_eq!(blamed[&1].evidence, AttributionEvidence::ReplacementPrimary);
        }
    }

    #[test]
    fn normalization_has_exact_byte_boundaries_and_trailing_punctuation() {
        for punctuation in [':', ';', ',', '.'] {
            assert_eq!(normalize_category(&format!("  error{punctuation} \t")), "error");
        }
        assert_eq!(normalize_category(" \t\r\n"), "uncategorized compiler error");
        assert_eq!(normalize_category(" \tfirst \n \tsecond   third  "), "first second third");
        assert_eq!(normalize_category("a1234 b567"), "a# b#");
        assert_eq!(
            normalize_category("before `secret` after \"other\""),
            "before <value> after <value>"
        );
        assert_eq!(normalize_category("before C:\\private\\file.rs after"), "before <path> after");
        assert_eq!(normalize_category("before /private/file.rs after"), "before <path> after");
        for length in [95, 96, 97] {
            assert_eq!(normalize_category(&"x".repeat(length)), "x".repeat(length.min(96)));
        }
        assert_eq!(normalize_category(&format!("{}éz", "x".repeat(94))), format!("{}é", "x".repeat(94)));
        assert_eq!(normalize_category(&format!("{}éz", "x".repeat(95))), format!("{}z", "x".repeat(95)));
        for (prefix, expected) in [(88, "x".repeat(88) + "<value>"), (89, "x".repeat(89) + "<value>")] {
            assert_eq!(normalize_category(&format!("{}\"hidden\"", "x".repeat(prefix))), expected);
        }
        assert_eq!(
            normalize_category(&format!("{}\"hidden\"", "x".repeat(90))),
            "x".repeat(90) + "<value"
        );
        assert_eq!(normalize_category(&format!("{} y", "x".repeat(95))), "x".repeat(95));
        for prefix in [95, 96] {
            let expected = if prefix == 95 { "x".repeat(95) + "#" } else { "x".repeat(96) };
            assert_eq!(normalize_category(&format!("{}123", "x".repeat(prefix))), expected);
        }
        for prefix in [89, 90] {
            let expected = ("x".repeat(prefix) + " <path>").chars().take(96).collect::<String>();
            assert_eq!(normalize_category(&format!("{} /private/source.rs", "x".repeat(prefix))), expected);
        }
    }

    #[test]
    fn pieces_stop_at_the_first_character_that_does_not_fit() {
        for (initial, piece, limit, expected) in [
            ("", "abc", 0, ""),
            ("", "abc", 1, "a"),
            ("x", "éz", 2, "x"),
            ("x", "éz", 3, "xé"),
            ("x", "éz", 4, "xéz"),
        ] {
            let mut output = initial.to_owned();
            push_piece(&mut output, piece, limit);
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn a_broad_primary_does_not_upgrade_narrow_secondary_evidence_to_primary_replacement_blame() {
        let guards = Guards::from_iter([(
            1,
            (
                "a.rs".into(),
                Guard {
                    site: range(NonZeroU32::new(5).unwrap(), NonZeroU32::new(10).unwrap()),
                    mutated: Some(range(NonZeroU32::new(6).unwrap(), NonZeroU32::new(8).unwrap())),
                },
            ),
        )]);
        let text = diagnostic(
            "error",
            "E0308",
            &[span(Some("a.rs"), 5, 10, true), span(Some("a.rs"), 6, 7, false)],
        );
        assert_eq!(
            blame(&text, Utf8Path::new("workspace"), &guards)[&1].evidence,
            AttributionEvidence::OtherDiagnostic
        );
    }
}
