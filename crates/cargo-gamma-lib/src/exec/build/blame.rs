// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Which mutant a compiler error belongs to.
//!
//! Rustc spans first provide direct replacement blame or the deletion-only containment fallback.
//! A non-empty replacement that remains unresolved is handed back to the parent build coordinator
//! for proof-build isolation. Every resulting withdrawal is summarized as a bounded
//! [`CompilerReason`].

use core::ops::{Range, RangeInclusive};

use camino::Utf8Path;

use super::Guards;
use super::messages::{CompilerMessage, Span, cargo_message};
use crate::schema::{Guard, Position};
use crate::{HashMap, HashSet};

/// A bounded, host-independent account of why rustc rejected a mutant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(super) enum AttributionEvidence {
    #[default]
    OtherDiagnostic,
    ReplacementPrimary,
    ProofBuild,
}

/// A bounded, host-independent account of why rustc rejected a mutant.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub(super) struct CompilerReason {
    /// The primary rustc error code, empty when the diagnostic carried none.
    pub(super) code: String,

    /// A normalized form of the primary diagnostic message.
    pub(super) category: String,

    /// How the compiler failure was attributed to this mutant.
    pub(super) evidence: AttributionEvidence,
}

impl CompilerReason {
    pub(super) fn isolated() -> Self {
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
/// line. Primary spans drive containment; secondary spans are accepted only when they intersect
/// generated replacement text, because notes routinely point at innocent declarations.
///
/// A diagnostic landing in some guard's mutated branch names its cause exactly, since that branch
/// is the only text in the tree that is not a copy of the original and no two of them overlap.
/// A deletion has no replacement text to land in, so the innermost deleted site containing the
/// diagnostic remains sufficient evidence. Containment involving a non-empty replacement is only
/// a candidate: proof builds must isolate it before the mutant is reported as unviable.
#[expect(clippy::too_many_lines, reason = "the attribution tiers share one parsed diagnostic walk")]
pub(super) fn blame(stdout: &str, root: &Utf8Path, guards: &Guards) -> HashMap<u32, CompilerReason> {
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
                if span.is_primary && guard.mutated.as_ref().is_some_and(|mutated| overlaps(mutated, &reported)) {
                    let _ = replacement_sites.insert(ordinal);
                }

                if guard.mutated.as_ref().is_some_and(|mutated| covers(mutated, &reported)) {
                    let _ = exact.insert(ordinal);
                } else if guard.mutated.is_none() && covers(&guard.site, &reported) {
                    let width = guard.site.end.line().saturating_sub(guard.site.start.line());

                    match &mut enclosing {
                        Some((best, ordinals)) if *best == width => {
                            let _ = ordinals.insert(ordinal);
                        }
                        Some((best, _ordinals)) if *best < width => {}
                        _ => enclosing = Some((width, HashSet::from_iter([ordinal]))),
                    }
                } else if guard.mutated.is_none() && covers(&reported, &guard.site) {
                    // The diagnostic encloses the guard rather than the other way round, which is
                    // what a borrow checker error looks like: the guard makes some subexpression
                    // non-constant and the complaint lands on the whole construct that depended on
                    // it. Every deletion inside the smallest such region is a candidate, because
                    // nothing narrower distinguishes them.
                    let width = reported.end.line().saturating_sub(reported.start.line());

                    match &mut contained {
                        Some((best, ordinals)) if *best == width => {
                            let _ = ordinals.insert(ordinal);
                        }
                        Some((best, _ordinals)) if *best < width => {}
                        _ => contained = Some((width, HashSet::from_iter([ordinal]))),
                    }
                }
            }
        }

        // rustc sometimes puts the consequence of a mutation in the primary span and the cause in
        // a secondary "expected because of this" span. A secondary span is too broad a basis for
        // enclosing-site attribution—it routinely names innocent declarations—but intersection
        // with a mutated branch is exact evidence: that text exists only because gamma emitted it.
        if exact.is_empty() {
            secondary_attribution(&diagnostic, root, &by_path, &mut exact, SECONDARY_ATTRIBUTION_BUDGET);
        }

        let (ordinals, blamed) = if exact.is_empty() {
            (
                enclosing
                    .or(contained)
                    .map(|(_width, ordinals)| ordinals)
                    .or_else(|| diverted(&diagnostic, root, &by_path)),
                &mut deletion_blame,
            )
        } else {
            (Some(exact), &mut exact_blame)
        };

        if let Some(ordinals) = ordinals {
            if ordinals.iter().all(|ordinal| blamed.contains_key(ordinal)) {
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
            if guard.mutated.as_ref().is_some_and(|mutated| overlaps(mutated, &reported)) {
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
            while characters.peek().is_some_and(|next| !next.is_whitespace()) {
                let _ = characters.next();
            }

            let mut lookahead = characters.clone();
            while lookahead.peek().is_some_and(|next| next.is_whitespace()) {
                let _ = lookahead.next();
            }
            let next_component: String = lookahead.take_while(|next| !next.is_whitespace()).collect();
            // #[gamma::skip(cond.always_false, cond.negate, unary.remove_not, tag = "timeout", reason = "without the component stop condition an exhausted path iterator is scanned forever")]
            if !next_component.contains('/') && !next_component.contains('\\') {
                // #[gamma::skip(loop.break_to_continue, loop.delete_break, tag = "timeout", reason = "the path-tail scan must stop when no further path component exists")]
                break;
            }

            while characters.peek().is_some_and(|next| next.is_whitespace()) {
                let _ = characters.next();
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
            for _ in 0..normalized.len() {
                if normalized.chars().next_back().is_none_or(char::is_whitespace) {
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

    let normalized = normalized.trim().trim_end_matches([':', ';', ',', '.']).trim();

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

/// The rustc error codes whose diagnostics need not point anywhere near their cause.
///
/// Every one of these comes from a flow-sensitive analysis — borrow checking, move checking,
/// initialization tracking — which reasons about paths through a function rather than about the
/// text of an expression. Such an analysis reports at the point where the consequence becomes
/// visible, which can be an arbitrary distance from the change that made it reachable, and in a
/// span that need not contain the change at all. Every other class of error rustc reports is
/// positional: a type error lands on the expression whose type is wrong.
///
/// The list is a gate rather than a hint. What it guards is a fallback that blames mutants a
/// diagnostic does not name, and applying that to an error whose position *is* meaningful would
/// withdraw innocent mutants for a fault they did not cause.
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

/// Blames a flow-sensitive diagnostic on the mutants that could have changed which paths exist.
///
/// The three positional tiers have already found nothing, which for these error codes says little:
/// the cause is not required to be inside any span the diagnostic reports. What is left is to look
/// at the region the diagnostic talks about as a whole — every span it names, including the ones on
/// its notes, which is where "value moved here" and "this reinitialization might get skipped" live —
/// and to ask which mutants inside that region could have changed reachability at all.
///
/// Deletion mutants are preferred, and they are the reason this works. A deletion is recorded with
/// no `mutated` range, and deleting a statement is the only edit that can make a path statically
/// reachable that was not before: guard a `continue` or a `return` and the code after it is suddenly
/// live, so a value moved earlier is now seen to be used again. A substitution changes what a value
/// is, never where control goes.
///
/// Substitutions are never returned by this fallback. A non-empty replacement needs an exact span
/// or an isolation build before it can be called compiler-unviable.
pub(super) fn diverted(
    diagnostic: &CompilerMessage<'_>,
    root: &Utf8Path,
    by_path: &HashMap<&Utf8Path, Vec<(u32, &Guard)>>,
) -> Option<HashSet<u32>> {
    let code = diagnostic.code.as_ref()?;

    if !FLOW_SENSITIVE.contains(&code.code.as_ref()) {
        return None;
    }

    let mut deletions = HashSet::default();
    for (file, region) in regions(diagnostic) {
        let relative = Utf8Path::new(file.as_str())
            .strip_prefix(root.as_str())
            .unwrap_or_else(|_ignored| Utf8Path::new(file.as_str()));

        let here = by_path.get(relative).map_or_else(
            || {
                by_path
                    .iter()
                    .filter(|(known, _found)| file.ends_with(known.as_str()))
                    .flat_map(|(_known, found)| found.iter().copied())
                    .collect::<Vec<_>>()
            },
            Clone::clone,
        );

        for (ordinal, guard) in here {
            if !region.contains(&guard.site.start.line()) {
                continue;
            }

            if guard.mutated.is_none() {
                let _ = deletions.insert(ordinal);
            }
        }
    }

    if deletions.is_empty() { None } else { Some(deletions) }
}

/// The line span each file contributes to a diagnostic, notes and all.
///
/// Children are walked because a flow-sensitive diagnostic keeps the interesting half of what it
/// knows in them: the primary span is where the error surfaced, and the note spans are where the
/// value was moved, borrowed or conditionally skipped. The cause sits between those points far more
/// often than it sits in any one of them.
pub(super) fn regions(diagnostic: &CompilerMessage<'_>) -> HashMap<String, RangeInclusive<u32>> {
    let mut spread: HashMap<String, RangeInclusive<u32>> = HashMap::default();
    let mut pending = vec![diagnostic];

    while let Some(node) = pending.pop() {
        for span in &node.spans {
            let (Some(file), Some(from), Some(to)) = (span.file_name.as_deref(), span.line_start, span.line_end) else {
                continue;
            };

            let from = clamped(from);
            let to = clamped(to);

            let _widened = spread
                .entry(file.to_owned())
                .and_modify(|known| *known = (*known.start()).min(from)..=(*known.end()).max(to))
                .or_insert(from..=to);
        }

        pending.extend(&node.children);
    }

    spread
}

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

/// Whether two non-empty source regions share any text.
fn overlaps(left: &Range<Position>, right: &Range<Position>) -> bool {
    left.start < right.end && right.start < left.end
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod final_survivor_tests {
    use core::num::NonZeroU32;

    use super::*;

    #[test]
    fn diagnostic_categories_collapse_each_interior_whitespace_run() {
        for message in ["alpha   beta", "alpha \t\n beta", "\talpha  beta\n"] {
            assert_eq!(normalize_category(message), "alpha beta");
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
                "E0382",
                &[span(Some("a.rs"), 6, 7, true), span(Some("known.rs"), 6, 7, true)],
            );
            let blamed = blame(&format!("{first}\n{second}"), Utf8Path::new("workspace"), &scoped);
            assert_eq!(blamed.len(), 5);
            for ordinal in 1..=5 {
                assert_eq!(blamed[&ordinal].code, if ordinal == known { "E0308" } else { "E0382" });
            }
        }
    }

    #[test]
    fn flow_fallback_skips_an_early_outside_guard_requires_a_flow_code_and_returns_none_for_no_deletions() {
        let outside = Guard {
            site: range(NonZeroU32::new(1).unwrap(), NonZeroU32::new(2).unwrap()),
            mutated: None,
        };
        let deletion = Guard {
            site: range(NonZeroU32::new(15).unwrap(), NonZeroU32::new(16).unwrap()),
            mutated: None,
        };
        let replacement = Guard {
            site: range(NonZeroU32::new(16).unwrap(), NonZeroU32::new(17).unwrap()),
            mutated: Some(range(NonZeroU32::new(16).unwrap(), NonZeroU32::new(17).unwrap())),
        };
        let by_path = HashMap::from_iter([(Utf8Path::new("a.rs"), vec![(1, &outside), (2, &deletion), (3, &replacement)])]);
        for (code, from, to, expected) in [
            ("E0382", 10, 20, Some(HashSet::from_iter([2]))),
            ("E9999", 10, 20, None),
            ("E0382", 35, 36, None),
        ] {
            let message = diagnostic("error", code, &[span(Some("a.rs"), from, to, true)]);
            let message = cargo_message(&message).unwrap().message.unwrap();
            assert_eq!(diverted(&message, Utf8Path::new("workspace"), &by_path), expected);
        }
    }

    #[test]
    fn region_collection_continues_after_an_unusable_span_in_the_same_diagnostic_node() {
        let message = diagnostic("error", "E0382", &[span(None, 1, 2, true), span(Some("later.rs"), 10, 20, true)]);
        let message = cargo_message(&message).unwrap().message.unwrap();
        assert_eq!(regions(&message), HashMap::from_iter([("later.rs".to_owned(), 10..=20)]));
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
        let deletion = diagnostic("error", "E0382", &[span(Some("src/lib.rs"), 23, 24, true)]);
        let blamed = blame(&deletion, Utf8Path::new("workspace"), &guards);
        assert_eq!(blamed.len(), 2);
        for ordinal in [3, 4] {
            assert_eq!(blamed[&ordinal].code, "E0382");
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
    fn adjacent_ranges_do_not_attribute_each_others_text() {
        let at = |column| Position::new(1, column).unwrap();
        for (a, b, expected) in [
            ((1, 4), (4, 7), false),
            ((4, 7), (1, 4), false),
            ((1, 3), (4, 7), false),
            ((4, 7), (1, 3), false),
            ((1, 5), (4, 7), true),
            ((4, 7), (1, 5), true),
        ] {
            assert_eq!(overlaps(&(at(a.0)..at(a.1)), &(at(b.0)..at(b.1))), expected);
        }
    }
}
