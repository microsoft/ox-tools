// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::ops::Range;

use proc_macro2::Span;
use syn::spanned::Spanned as _;
use syn::visit::{self, Visit};
use syn::{Attribute, Expr, ForeignItem, ImplItem, Item, Stmt, TraitItem};

use crate::HashMap;
use crate::parse::SourceFile;

/// The spans a directive can attach to.
#[derive(Debug, Default)]
pub(super) struct Scopes {
    /// Every span a directive can attach to: items of every kind, their members, statements and
    /// expressions.
    ///
    /// Completeness is the point rather than economy. A construct missing from here does not make
    /// a directive above it inert — [`following`](Self::following) hands that directive to the
    /// next construct that *is* here, which is usually the function below, and the suppression
    /// then silences mutants nobody looked at while `idle` stays quiet because the directive did
    /// govern something.
    ///
    /// # Invariant
    ///
    /// Always sorted by `start` ascending, ties broken by `end` descending, so the widest span at
    /// any shared start comes first. Every constructor establishes this regardless of the order
    /// its input arrived in, and [`following`](Self::following) relies on it to binary-search the
    /// candidate region instead of scanning the whole vector.
    spans: Vec<Range<usize>>,

    /// Every attribute, paired with the span of what it is attached to.
    pub(super) attributes: Vec<(Attribute, Range<usize>)>,

    /// Line of the start of each span, for resolving trailing comments.
    lines: HashMap<usize, Range<usize>>,
}

impl Scopes {
    /// Collects every span a directive could govern.
    pub(super) fn of(file: &SourceFile) -> Self {
        let mut collector = ScopeCollector {
            scopes: Self::default(),
            text_len: file.text.len(),
        };

        collector.visit_file(&file.ast);

        let mut scopes = collector.scopes;

        scopes.sort_spans();

        for span in &scopes.spans {
            let line = file.line_of(span.start);
            let entry = scopes.lines.entry(line).or_insert_with(|| span.clone());

            // Prefer the widest span starting on a line, so a trailing comment on a one-line
            // function governs the function rather than its first sub-expression.
            if span.end > entry.end {
                *entry = span.clone();
            }
        }

        scopes
    }

    /// Establishes the `spans` invariant: sorted by `start` ascending, ties by `end` descending.
    fn sort_spans(&mut self) {
        self.spans
            .sort_by(|left, right| left.start.cmp(&right.start).then_with(|| right.end.cmp(&left.end)));
    }

    /// Returns the span a directive placed on its own line governs.
    ///
    /// It is the *outermost* construct beginning after the directive. A directive above a function
    /// governs the whole function, not merely its first statement, which is what a reader writing
    /// one above a function obviously intends.
    ///
    /// Because `spans` is sorted by `start`, a binary search locates the first span starting at or
    /// after the offset in `O(log n)`; only the short run of spans sharing that start is scanned to
    /// pick the widest.
    pub(super) fn following(&self, offset: usize) -> Option<Range<usize>> {
        let index = self.spans.partition_point(|span| span.start < offset);

        let mut candidates = self.spans.iter().skip(index);
        let first = candidates.next()?;
        let start = first.start;
        let mut best = first.clone();

        for span in candidates {
            if span.start != start {
                break;
            }

            if span.end > best.end {
                best = span.clone();
            }
        }

        Some(best)
    }

    /// Builds a scope set directly from spans, for testing the selection rules.
    ///
    /// The walk that normally fills this happens to yield spans outermost-first, so a rule stated
    /// to be independent of order would otherwise only ever be exercised in one order. The rules
    /// are what the suppression contract rests on, so they are tested as rules. Sorting here is
    /// what lets the rule be tested against unsorted input while `following` still binary-searches.
    #[cfg(test)]
    pub(super) fn from_spans(spans: Vec<Range<usize>>) -> Self {
        let mut scopes = Self { spans, ..Self::default() };

        scopes.sort_spans();

        scopes
    }

    /// Returns the span a directive trailing on a line of code governs.
    pub(super) fn enclosing_on_line(&self, line: usize) -> Option<Range<usize>> {
        self.lines.get(&line).cloned()
    }
}

/// Whether a span is one a directive could meaningfully govern.
///
/// An empty span governs no code, and one reaching past the end of the file came from a macro
/// expansion rather than from anything the author wrote. Attaching a directive to either would
/// suppress something the reader cannot see.
fn admissible(range: &Range<usize>, text_len: usize) -> bool {
    !range.is_empty() && range.end <= text_len
}

/// Walks a file gathering the spans a directive can attach to.
struct ScopeCollector {
    scopes: Scopes,
    text_len: usize,
}

impl ScopeCollector {
    /// Records a span if it lies inside the file text.
    fn record(&mut self, span: Span) -> Option<Range<usize>> {
        let range = span.byte_range();

        if !admissible(&range, self.text_len) {
            return None;
        }

        self.scopes.spans.push(range.clone());

        Some(range)
    }

    /// Records the attributes attached to a construct.
    fn record_attributes(&mut self, attributes: &[Attribute], span: Span) {
        let Some(range) = self.record(span) else {
            return;
        };

        for attribute in attributes {
            self.scopes.attributes.push((attribute.clone(), range.clone()));
        }
    }

    /// Adds the function-token start as an attachment point for an attributed function.
    ///
    /// `syn` starts an item's span at its first attribute. A generated comment is inserted after
    /// existing attributes and before the function declaration, so the ordinary item span begins
    /// before the comment and [`Scopes::following`] skips it. This second span lets that comment
    /// govern the whole function while a comment inside the body still starts after this point and
    /// continues to govern the following statement or expression.
    fn record_attributed_function_head(&mut self, attributes: &[Attribute], function: Span, item: Span) {
        if attributes.is_empty() {
            return;
        }

        let start = function.byte_range().start;
        let end = item.byte_range().end;
        let range = start..end;

        if admissible(&range, self.text_len) {
            self.scopes.spans.push(range);
        }
    }
}

#[expect(
    clippy::renamed_function_params,
    reason = "syn names every visitor parameter `i`, which says nothing about what it is"
)]
impl<'ast> Visit<'ast> for ScopeCollector {
    fn visit_item(&mut self, node: &'ast Item) {
        self.record_attributes(item_attributes(node), node.span());
        if let Item::Fn(function) = node {
            self.record_attributed_function_head(&function.attrs, function.sig.fn_token.span, node.span());
        }
        visit::visit_item(self, node);
    }

    fn visit_impl_item(&mut self, node: &'ast ImplItem) {
        self.record_attributes(impl_item_attributes(node), node.span());
        if let ImplItem::Fn(function) = node {
            self.record_attributed_function_head(&function.attrs, function.sig.fn_token.span, node.span());
        }
        visit::visit_impl_item(self, node);
    }

    fn visit_trait_item(&mut self, node: &'ast TraitItem) {
        self.record_attributes(trait_item_attributes(node), node.span());
        if let TraitItem::Fn(function) = node {
            self.record_attributed_function_head(&function.attrs, function.sig.fn_token.span, node.span());
        }
        visit::visit_trait_item(self, node);
    }

    fn visit_foreign_item(&mut self, node: &'ast ForeignItem) {
        self.record_attributes(foreign_item_attributes(node), node.span());
        if let ForeignItem::Fn(function) = node {
            self.record_attributed_function_head(&function.attrs, function.sig.fn_token.span, node.span());
        }
        visit::visit_foreign_item(self, node);
    }

    fn visit_stmt(&mut self, node: &'ast Stmt) {
        let _ = self.record(node.span());
        visit::visit_stmt(self, node);
    }

    fn visit_expr(&mut self, node: &'ast Expr) {
        let _ = self.record(node.span());
        visit::visit_expr(self, node);
    }
}

/// The attributes an item carries.
///
/// Written out variant by variant because `syn` gives items no common accessor for them, and the
/// alternative — walking only the kinds that happen to hold code today — is the whole defect this
/// exists to close: an item kind nobody listed does not merely go unsuppressed, it hands its
/// directive to whatever construct comes next.
///
/// `Item` is non-exhaustive and `Item::Verbatim` is tokens `syn` could not parse, so the fallback
/// yields nothing. That is the safe direction: a directive on such an item still governs the
/// item's own span, which suppresses nothing, rather than reaching past it.
fn item_attributes(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(node) => &node.attrs,
        Item::Enum(node) => &node.attrs,
        Item::ExternCrate(node) => &node.attrs,
        Item::Fn(node) => &node.attrs,
        Item::ForeignMod(node) => &node.attrs,
        Item::Impl(node) => &node.attrs,
        Item::Macro(node) => &node.attrs,
        Item::Mod(node) => &node.attrs,
        Item::Static(node) => &node.attrs,
        Item::Struct(node) => &node.attrs,
        Item::Trait(node) => &node.attrs,
        Item::TraitAlias(node) => &node.attrs,
        Item::Type(node) => &node.attrs,
        Item::Union(node) => &node.attrs,
        Item::Use(node) => &node.attrs,
        _ => &[],
    }
}

/// The attributes a member of an `impl` block carries. See [`item_attributes`].
fn impl_item_attributes(item: &ImplItem) -> &[Attribute] {
    match item {
        ImplItem::Const(node) => &node.attrs,
        ImplItem::Fn(node) => &node.attrs,
        ImplItem::Macro(node) => &node.attrs,
        ImplItem::Type(node) => &node.attrs,
        _ => &[],
    }
}

/// The attributes a member of a trait carries. See [`item_attributes`].
fn trait_item_attributes(item: &TraitItem) -> &[Attribute] {
    match item {
        TraitItem::Const(node) => &node.attrs,
        TraitItem::Fn(node) => &node.attrs,
        TraitItem::Macro(node) => &node.attrs,
        TraitItem::Type(node) => &node.attrs,
        _ => &[],
    }
}

/// The attributes a member of an `extern` block carries. See [`item_attributes`].
fn foreign_item_attributes(item: &ForeignItem) -> &[Attribute] {
    match item {
        ForeignItem::Fn(node) => &node.attrs,
        ForeignItem::Macro(node) => &node.attrs,
        ForeignItem::Static(node) => &node.attrs,
        ForeignItem::Type(node) => &node.attrs,
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use syn::ItemFn;

    use super::super::tests::{file, mutants_of};
    use super::super::{directives, suppress};
    use super::*;

    fn suppressed(source: &str, mutators: &str) -> (usize, usize) {
        let (parsed, mut mutants) = mutants_of(source, mutators);
        let found = directives(&parsed).unwrap();
        let count = suppress(&mut mutants, &found);

        (count, mutants.len())
    }

    #[test]
    fn a_comment_directive_suppresses_the_following_statement() {
        let source = "fn f(a: i32, b: i32) {\n    // #[gamma::skip(arith)]\n    let x = a + b;\n    let y = a - b;\n}";
        let (count, total) = suppressed(source, "arith");

        assert_eq!(total, 4);
        assert_eq!(count, 2);
    }

    #[test]
    fn a_comment_directive_above_a_function_covers_the_whole_function() {
        let source = "// #[gamma::skip(arith)]\nfn f(a: i32, b: i32) -> i32 {\n    let x = a + b;\n    x - b\n}";
        let (count, total) = suppressed(source, "arith");

        assert_eq!(count, total);
        assert!(total > 0);
    }

    /// `cargo gamma suppress` inserts its comment immediately above the function declaration,
    /// after attributes already attached to that function. `syn` includes those attributes in the
    /// item span, so scope lookup must also expose the declaration as an attachment point or the
    /// comment falls through to the first construct inside the body.
    #[test]
    fn a_comment_between_attributes_and_an_impl_method_suppresses_its_whole_body_mutant() {
        let source = "struct S;
impl S {
    #[cfg_attr(test, mutants::skip)]
    // #[gamma::skip(fn_value.none)]
    pub fn next(&self) -> Option<i32> {
        Some(1)
    }
}";
        let (count, total) = suppressed(source, "fn_value.none");

        assert_eq!(total, 1);
        assert_eq!(count, total);
    }

    #[test]
    fn an_attribute_directive_covers_the_whole_function() {
        let source = "#[gamma::skip(arith)]\nfn f(a: i32, b: i32) -> i32 { a + b }";
        let (count, total) = suppressed(source, "arith");

        assert_eq!(count, total);
        assert!(total > 0);
    }

    #[test]
    fn a_trailing_directive_governs_its_own_line() {
        let source = "fn f(a: i32, b: i32) {\n    let x = a + b; // #[gamma::skip(arith)]\n    let y = a - b;\n}";
        let (count, total) = suppressed(source, "arith");

        assert_eq!(total, 4);
        assert_eq!(count, 2);
    }

    #[test]
    fn one_line_function_trailing_directive_prefers_the_widest_scope() {
        let source = "fn f(a: i32, b: i32) -> i32 { a + b } // #[gamma::skip(arith)]";
        let (count, total) = suppressed(source, "arith");

        assert_eq!(count, total);
        assert!(total > 0);
    }

    #[test]
    fn following_prefers_the_widest_scope_at_the_earliest_start() {
        let parsed = file("// directive\nfn f(a: i32, b: i32) -> i32 { a + b }\n");
        let scopes = Scopes::of(&parsed);
        let scope = scopes.following(0).unwrap();

        assert_eq!(parsed.slice(&scope), "fn f(a: i32, b: i32) -> i32 { a + b }");
    }

    #[test]
    fn directives_attach_to_impl_trait_and_module_scopes() {
        let source = "trait T {
            // #[gamma::skip(arith)]
            fn f(&self) -> i32 { 1 + 1 }
        }
        struct S;
        impl S {
            // #[gamma::skip(arith)]
            fn g(&self) -> i32 { 2 + 2 }
        }
        // #[gamma::skip(arith)]
        impl T for S { fn f(&self) -> i32 { 3 + 3 } }
        // #[gamma::skip(arith)]
        mod m { pub fn h() -> i32 { 4 + 4 } }";
        let (count, total) = suppressed(source, "arith");

        assert_eq!(count, total);
        assert!(total >= 8, "{total}");
    }

    /// A directive above a construct that carries no mutants governs *that* construct.
    ///
    /// The hazard is not that the directive does nothing — it is that a scope set which does not
    /// know about `struct` hands the directive the next thing it does know about, which is the
    /// function below. The suppression then silences mutants the author never looked at, the
    /// denominator shrinks, and the score rises with nothing to show it: `idle` only reports a
    /// directive that governed nothing, and this one governed plenty.
    #[test]
    fn a_directive_above_a_struct_does_not_reach_the_function_below_it() {
        let source = "// #[gamma::skip(arith)]\nstruct S(u32);\nfn f(a: i32, b: i32) -> i32 { a + b }";
        let (count, total) = suppressed(source, "arith");

        assert!(total > 0, "the fixture must offer something to suppress");
        assert_eq!(count, 0, "the struct's directive must not reach the function");
    }

    /// An attribute directive on a trait is collected, and governs the trait.
    ///
    /// An attribute is inert, so a spelling nothing collects compiles perfectly and reads as a
    /// working suppression forever. Every item kind that can carry one has to be walked for
    /// exactly that reason.
    #[test]
    fn an_attribute_directive_on_a_trait_covers_its_default_methods() {
        let source = "#[gamma::skip(arith)]\ntrait T {\n    fn f(&self) -> i32 { 1 + 1 }\n    fn g(&self) -> i32 { 2 + 2 }\n}";
        let (count, total) = suppressed(source, "arith");

        assert!(total > 0);
        assert_eq!(count, total);
    }

    /// A comment directive above a trait governs the whole trait, not its first method.
    ///
    /// "The outermost construct that begins after it" is what the README promises and what a
    /// reader writing one above a trait means; resolving it to the first method it happens to
    /// contain honours neither.
    #[test]
    fn a_comment_directive_above_a_trait_governs_the_whole_trait() {
        let source = "// #[gamma::skip(arith)]\ntrait T {\n    fn f(&self) -> i32 { 1 + 1 }\n    fn g(&self) -> i32 { 2 + 2 }\n}";
        let (count, total) = suppressed(source, "arith");

        assert!(total > 0);
        assert_eq!(count, total);
    }

    /// A line holding two constructs is governed by the wider of them, not the first one seen.
    #[test]
    fn the_widest_span_starting_on_a_line_wins() {
        let source = "fn a() { let _ = 1 + 1; } fn bbbbb() { let _ = 2 + 2; let _ = 3 + 3; }\n";
        let parsed = file(source);
        let scopes = Scopes::of(&parsed);
        let governing = scopes.enclosing_on_line(1).expect("a span on the only line");

        // The second function ends later than the first, so it is what a trailing directive on
        // this line has to govern.
        assert_eq!(governing.end, source.trim_end().len());
    }

    /// The widest span at a shared start wins whichever order the spans arrive in.
    #[test]
    fn the_outermost_span_at_a_shared_start_wins_in_either_order() {
        let inner_first = Scopes::from_spans(vec![0..10, 0..40, 0..25]);
        let outer_first = Scopes::from_spans(vec![0..40, 0..25, 0..10]);

        // A directive above a nested construct means the outer one; the traversal that normally
        // fills this yields spans outermost-first, so the rule must not depend on that.
        assert_eq!(inner_first.following(0), Some(0..40));
        assert_eq!(outer_first.following(0), Some(0..40));
    }

    /// The earliest span at or after the offset wins, whatever order they arrive in.
    #[test]
    fn the_nearest_span_after_the_offset_wins() {
        let scopes = Scopes::from_spans(vec![50..60, 20..30, 5..8]);

        // A directive governs what follows it, which is the construct that starts soonest after
        // it rather than the widest one anywhere below.
        assert_eq!(scopes.following(10), Some(20..30));
        assert_eq!(scopes.following(61), None);
    }

    /// Among several spans that begin together, the widest is chosen, whatever order they arrive.
    #[test]
    fn among_spans_sharing_a_start_the_widest_is_chosen() {
        let scopes = Scopes::from_spans(vec![5..8, 5..20, 5..12]);

        assert_eq!(scopes.following(0), Some(5..20));
    }

    /// An offset landing exactly on a span start selects that span, not the one after it.
    #[test]
    fn an_offset_on_a_span_start_selects_that_span() {
        let scopes = Scopes::from_spans(vec![10..20, 30..40]);

        assert_eq!(scopes.following(10), Some(10..20));
    }

    /// An offset past every span start governs nothing.
    #[test]
    fn an_offset_past_the_last_span_start_governs_nothing() {
        let scopes = Scopes::from_spans(vec![10..20, 30..40]);

        assert_eq!(scopes.following(41), None);
    }

    /// A scope set with no spans governs nothing at any offset.
    #[test]
    fn an_empty_scope_set_governs_nothing() {
        let scopes = Scopes::from_spans(vec![]);

        assert_eq!(scopes.following(0), None);
    }

    /// Where an outermost and an innermost construct begin together, the outermost is chosen even
    /// when other spans start later; the binary search must not stop at a narrower shared start.
    #[test]
    fn nested_spans_sharing_a_start_resolve_to_the_outermost() {
        let scopes = Scopes::from_spans(vec![0..5, 0..100, 0..50, 10..20]);

        assert_eq!(scopes.following(0), Some(0..100));
    }

    /// With many spans and many directive offsets, the binary search must still select exactly the
    /// widest span at the earliest start at or after each offset — this guards the complexity
    /// change against a selection regression at scale.
    #[test]
    #[cfg_attr(
        miri,
        ignore = "builds a thousand span groups to exercise the binary search at scale; the scale is the point and Miri only re-times the search"
    )]
    fn many_spans_resolve_the_widest_at_the_earliest_start_at_every_offset() {
        let mut spans = Vec::new();

        for group in 0..1_000_usize {
            let start = group * 10;
            spans.push(start..start + 5);
            spans.push(start..start + 8);
            spans.push(start..start + 2);
        }

        let scopes = Scopes::from_spans(spans);

        assert_eq!(scopes.following(0), Some(0..8));
        assert_eq!(scopes.following(1), Some(10..18));
        assert_eq!(scopes.following(5_001), Some(5_010..5_018));
        assert_eq!(scopes.following(9_990), Some(9_990..9_998));
        assert_eq!(scopes.following(9_991), None);
    }

    /// A span governing nothing visible is never recorded.
    #[test]
    fn an_empty_or_overrunning_span_is_not_a_scope() {
        // An empty span governs no code, and one past the end of the file came from a macro
        // expansion; a directive attached to either would suppress something invisible.
        assert!(!admissible(&(5..5), 100));
        assert!(!admissible(&(90..120), 100));
        assert!(admissible(&(90..100), 100));
    }

    /// A directive above two constructs that begin together governs the outer one.
    #[test]
    fn the_outermost_span_from_a_real_parse_is_the_governing_one() {
        let source = "fn f() {\n    let _ = 1;\n}\n";
        let parsed = file(source);
        let scopes = Scopes::of(&parsed);
        let governing = scopes.following(0).expect("a span after the start of the file");

        assert_eq!(governing, 0..source.trim_end().len());
    }

    /// A span reaching past the file text the collector was told about looks exactly like one a
    /// macro expansion invented out of thin air, and recording it would let a directive claim to
    /// suppress code the reader cannot see anywhere in the source. The collector has to drop it.
    #[test]
    fn a_span_reaching_past_the_recorded_file_end_is_dropped_by_the_collector() {
        let source = "fn f() { let _ = 1; }";
        let item: ItemFn = syn::parse_str(source).expect("the fixture parses");

        // A `text_len` shorter than the function's own span makes every span in it "reach past
        // the end of the file" from the collector's point of view.
        let mut collector = ScopeCollector {
            scopes: Scopes::default(),
            text_len: 3,
        };

        assert!(collector.record(item.span()).is_none());
        assert!(collector.scopes.spans.is_empty(), "an inadmissible span must not be recorded");
    }

    /// The attributes on a construct whose own span is inadmissible must be dropped along with it;
    /// keeping them around paired with no real span would let a directive attach to an attribute
    /// the reader has no way to locate in the file.
    #[test]
    fn attributes_on_a_span_reaching_past_the_recorded_file_end_are_dropped_too() {
        let source = "#[gamma::skip(arith)]\nfn f() { let _ = 1; }";
        let item: ItemFn = syn::parse_str(source).expect("the fixture parses");

        let mut collector = ScopeCollector {
            scopes: Scopes::default(),
            text_len: 3,
        };

        collector.record_attributes(&item.attrs, item.span());

        assert!(
            collector.scopes.attributes.is_empty(),
            "an inadmissible span's attributes must not survive"
        );
    }
}
