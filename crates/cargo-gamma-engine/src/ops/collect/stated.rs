// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The value a site states for its own return-value mutant.
//!
//! Return types are read syntactically rather than resolved, which costs twice. An alias is not
//! seen through, so the guess falls back to `Default::default()` and a build round is spent
//! discovering that the mutant does not compile. Where no guess could hold at all — a bare type
//! parameter, an associated type, a `Box<dyn Trait>`, a non-iterator `impl Trait` — no mutant is
//! offered and the site goes unmeasured by the family whose whole question is "does anything check
//! what this function returns?".
//!
//! `#[gamma::value(<expr>)]` answers both. It states what to substitute, which replaces the guess
//! in the first case and creates the mutant in the second.
//!
//! It can only ever add a mutant or change what one substitutes. There is no spelling of it that
//! removes a site or reaches another family, which is what keeps it from becoming a suppression
//! channel that evades review. Nothing about it is taken on trust either: the stated expression
//! becomes an ordinary mutant, and one that does not type-check is withdrawn by the same rollback
//! that withdraws a bad guess.

use core::ops::Range;

use proc_macro2::TokenStream;
use syn::spanned::Spanned as _;
use syn::visit::{self, Visit};
use syn::{Attribute, Expr, ImplItemFn, ItemFn, Meta, TraitItemFn};

use crate::error::Error;
use crate::parse::SourceFile;
use crate::{HashSet, Result};

/// The two path segments a stated value is written under.
const PATH: [&str; 2] = ["gamma", "value"];

/// What to say about an argument list that is not one expression.
const MALFORMED: &str = "a site states one value, so `#[gamma::value(...)]` takes one Rust expression, as in `#[gamma::value(0)]`";

/// What to say about a stated value written anywhere but on a function.
const MISPLACED: &str = "`#[gamma::value(...)]` states the value one function returns, so it goes on a function or a method; \
                         one expression cannot be the body of every function beneath an `impl` block or a module";

/// What to say about two stated values on one item.
const DUPLICATED: &str = "an item may state one value; two would leave which of them applies to the order they were written in";

/// What to say about a stated value on a function that has no body.
const BODILESS: &str = "`#[gamma::value(...)]` replaces the body of the function it is written on, and a declaration has none; \
                        a trait method's implementations do not inherit it";

/// What to say about a stated value on a `const fn`.
const CONSTANT: &str = "`#[gamma::value(...)]` states what a mutant substitutes, and a mutant is spliced in behind a run-time guard \
                        call that no `const fn` body may make; this value would replace nothing";

/// What to say about a stated value on a function whose body is empty.
const EMPTY: &str = "`#[gamma::value(...)]` requires a non-empty function body; empty bodies are not eligible for stated-value mutation";

/// Returns the byte range of the expression an item's attributes state, if they state one.
///
/// A range rather than the tokens, because what the mutant substitutes is the text the user wrote:
/// re-rendering a token stream spaces it as `Box :: new (File)`, which compiles but reads as
/// something nobody typed, and every report, diff and patch would carry that spelling instead of
/// theirs.
///
/// A malformed argument yields `None`. It is diagnosed once, by [`check`], and that diagnostic
/// stops the run — so nothing downstream has to decide what half an attribute means.
pub(super) fn stated_range(attrs: &[Attribute]) -> Option<Range<usize>> {
    let arguments = attrs.iter().find_map(arguments)?;

    if syn::parse2::<Expr>(arguments.clone()).is_err() {
        return None;
    }

    let mut tokens = arguments.into_iter();
    let first = tokens.next()?.span().byte_range();
    let last = tokens.last().map_or_else(|| first.clone(), |tree| tree.span().byte_range());

    (first.start < last.end).then_some(first.start..last.end)
}

/// Checks every stated value a file carries.
///
/// # Errors
///
/// Returns an error if a stated value is malformed, duplicated, written on something that is not a
/// function, written on a function with no body to replace, or written on a function from which
/// discovery never reads a value — a `const fn`, or one whose body is empty. The proc macro rejects
/// all of them at compile time, so a crate that builds cannot reach them — but this tool reads
/// source rather than build output, and `gamma list mutants` runs against trees that have never
/// been compiled. Ignoring one there would leave a hint that reads as if it works and does nothing,
/// which is the failure mode the whole channel exists to avoid.
///
/// Fatal rather than a warning, for the same reason a suppression naming no mutator is: the run
/// that swallows it reports a score computed from a population the author did not ask for.
pub fn check(file: &SourceFile) -> Result<()> {
    let mut audit = Audit::default();

    audit.visit_file(&file.ast);

    fault(file, audit)
}

/// Turns a completed audit into the same result [`check`] has always returned.
///
/// Split out so the fused phase-one pass (see `collector::phase_one`) can run this exact check
/// against an `Audit` it filled during its own single walk, rather than paying for a second,
/// standalone one just to reach this diagnostic.
pub(super) fn fault(file: &SourceFile, mut audit: Audit) -> Result<()> {
    for span in audit.stated {
        if !audit.on_functions.contains(&span.start) {
            audit.faults.push((span, MISPLACED.to_owned()));
        }
    }

    // By position rather than by the order the walk happened to reach them, so a file with two
    // mistakes always reports the same one first.
    audit.faults.sort_by_key(|(at, _message)| at.start);

    let Some((at, message)) = audit.faults.first() else {
        return Ok(());
    };

    Err(Error::new(format!("{}:{}: {message}", file.path, file.line_of(at.start))).usage())
}

/// Every stated value in a file, and everything wrong with the ones that are wrong.
///
/// The two are collected in one walk because the misplacement rule is the difference between them:
/// an attribute is misplaced exactly when it was seen and no function claimed it. Deciding that by
/// enumerating the item kinds it must *not* appear on would have to be revised every time the
/// language grows another one.
///
/// `pub(super)`, and each visited node kind has a matching `on_*` method with no recursive
/// continuation of its own, so the fused phase-one pass can drive this exact per-node logic from
/// its own single traversal instead of running this visitor's `visit_file` a second time.
#[derive(Debug, Default)]
pub(super) struct Audit {
    /// Where every `#[gamma::value(...)]` in the file sits.
    stated: Vec<Range<usize>>,

    /// Where the ones written on a function or a method sit, keyed by start offset.
    on_functions: HashSet<usize>,

    /// What is wrong, and where.
    faults: Vec<(Range<usize>, String)>,
}

impl Audit {
    /// Records what an item's own attributes get wrong, and which of them a function claimed.
    ///
    /// `inert` explains why discovery never reads a stated value from the function — because it is
    /// a `const fn`, or because its body is empty. Both return before `stated_range` is consulted,
    /// so an attribute there produces no mutant and reads as if it does; naming the reason is what
    /// turns that silence into a diagnostic.
    ///
    /// A malformed argument list is reported ahead of an inert position, because it is the more
    /// specific mistake: an author who wrote `#[gamma::value(1 +)]` on a `const fn` has two things
    /// to fix, and the expression is the one they can see is wrong.
    fn item(&mut self, attrs: &[Attribute], inert: Option<&'static str>) {
        let stated: Vec<&Attribute> = attrs.iter().filter(|attribute| is_stated_value(attribute)).collect();

        if let Some(second) = stated.get(1) {
            self.faults.push((second.span().byte_range(), DUPLICATED.to_owned()));
        }

        for attribute in stated {
            let _claimed = self.on_functions.insert(attribute.span().byte_range().start);

            let malformed = arguments(attribute).is_none_or(|tokens| syn::parse2::<Expr>(tokens).is_err());

            if malformed {
                self.faults.push((attribute.span().byte_range(), MALFORMED.to_owned()));
            } else if let Some(message) = inert {
                self.faults.push((attribute.span().byte_range(), message.to_owned()));
            }
        }
    }

    /// The local update `visit_attribute` makes, without its recursive continuation.
    pub(super) fn on_attribute(&mut self, node: &Attribute) {
        if is_stated_value(node) {
            self.stated.push(node.span().byte_range());
        }
    }

    /// The local update `visit_item_fn` makes, without its recursive continuation.
    pub(super) fn on_item_fn(&mut self, node: &ItemFn) {
        self.item(&node.attrs, inert_reason(node.sig.constness.is_some(), node.block.stmts.is_empty()));
    }

    /// The local update `visit_impl_item_fn` makes, without its recursive continuation.
    pub(super) fn on_impl_item_fn(&mut self, node: &ImplItemFn) {
        self.item(&node.attrs, inert_reason(node.sig.constness.is_some(), node.block.stmts.is_empty()));
    }

    /// The local update `visit_trait_item_fn` makes, without its recursive continuation.
    pub(super) fn on_trait_item_fn(&mut self, node: &TraitItemFn) {
        // A declaration has no body to replace, and a stated value is not inherited by the
        // implementations any more than it is inherited from an `impl` block. Left unreported, it
        // would read as a hint that works and generate nothing anywhere.
        let Some(default) = node.default.as_ref() else {
            for attribute in node.attrs.iter().filter(|attribute| is_stated_value(attribute)) {
                self.faults.push((attribute.span().byte_range(), BODILESS.to_owned()));
                let _claimed = self.on_functions.insert(attribute.span().byte_range().start);
            }

            return;
        };

        self.item(&node.attrs, inert_reason(node.sig.constness.is_some(), default.stmts.is_empty()));
    }
}

#[expect(
    clippy::renamed_function_params,
    reason = "syn names every visitor parameter `i`, which says nothing about what it is"
)]
impl<'ast> Visit<'ast> for Audit {
    fn visit_attribute(&mut self, node: &'ast Attribute) {
        self.on_attribute(node);
        visit::visit_attribute(self, node);
    }

    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        self.on_item_fn(node);
        visit::visit_item_fn(self, node);
    }

    fn visit_impl_item_fn(&mut self, node: &'ast ImplItemFn) {
        self.on_impl_item_fn(node);
        visit::visit_impl_item_fn(self, node);
    }

    fn visit_trait_item_fn(&mut self, node: &'ast TraitItemFn) {
        self.on_trait_item_fn(node);

        visit::visit_trait_item_fn(self, node);
    }
}

/// Returns why discovery would never read a stated value from a function it reaches.
///
/// The two conditions are exactly the early returns discovery makes before it consults an item's
/// stated value. Keeping them here prevents the three function grammars from drifting apart.
/// `const` is checked first because resolving it is required before any mutant can exist.
const fn inert_reason(constant: bool, empty: bool) -> Option<&'static str> {
    if constant {
        Some(CONSTANT)
    } else if empty {
        Some(EMPTY)
    } else {
        None
    }
}

/// Returns whether an attribute is a stated value, whatever it happens to state.
///
/// Asked where the answer decides a diagnostic as well as where it decides a mutant, so an
/// attribute nobody can read still counts: that is exactly the one worth reporting.
fn is_stated_value(attribute: &Attribute) -> bool {
    let mut segments = attribute.path().segments.iter();

    PATH.iter()
        .all(|expected| segments.next().is_some_and(|segment| segment.ident == expected))
        && segments.next().is_none()
}

/// The argument list of an attribute that states a value.
///
/// A bare `#[gamma::value]` has none, and neither does `#[gamma::value = 1]`. Both state nothing an
/// expression could be read out of, so both are reported rather than guessed at.
fn arguments(attribute: &Attribute) -> Option<TokenStream> {
    if !is_stated_value(attribute) {
        return None;
    }

    match &attribute.meta {
        Meta::List(list) if !list.tokens.is_empty() => Some(list.tokens.clone()),
        _other => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(source: &str) -> SourceFile {
        SourceFile::parse("test.rs", source.to_owned()).expect("the fixture must parse")
    }

    fn function_item(parsed: &SourceFile) -> &ItemFn {
        parsed
            .ast
            .items
            .iter()
            .find_map(|item| if let syn::Item::Fn(item) = item { Some(item) } else { None })
            .expect("the fixture contains a function")
    }

    fn rejection(source: &str) -> String {
        check(&file(source))
            .expect_err("the fixture states a value that cannot be honoured")
            .to_string()
    }

    /// The expression is taken from the source verbatim, because that spelling is what every
    /// report, diff and applied patch shows the user afterwards. Re-rendering the tokens instead
    /// would hand them back `Some (Config { size : 8 })`, which compiles and which nobody wrote.
    #[test]
    fn the_stated_expression_is_the_text_the_user_wrote() {
        let parsed = file("#[gamma::value(Some(Config { size: 8 }))]\nfn f() -> Option<Config> { g() }");
        let item = function_item(&parsed);

        let range = stated_range(&item.attrs).expect("the fixture states a value");

        assert_eq!(parsed.slice(&range), "Some(Config { size: 8 })");
    }

    /// A single-token expression has one span rather than a first and a last, and is the shortest
    /// thing anybody states.
    #[test]
    fn a_one_token_expression_is_read_whole() {
        let parsed = file("#[gamma::value(7)]\nfn f() -> u32 { 1 }");
        let item = function_item(&parsed);

        let range = stated_range(&item.attrs).expect("the fixture states a value");

        assert_eq!(parsed.slice(&range), "7");
    }

    /// An item with no stated value has none, which is what leaves the guessing rules in charge of
    /// every site that says nothing.
    #[test]
    fn an_item_that_states_nothing_has_no_range() {
        let parsed = file("#[inline]\n#[gamma::skip(arith)]\nfn f() -> u32 { 1 }");
        let item = function_item(&parsed);

        assert_eq!(stated_range(&item.attrs), None);
    }

    #[test]
    fn a_malformed_stated_value_has_no_range() {
        let parsed = file("#[gamma::value(0, 1)]\nfn f() -> u32 { 1 }");
        let item = function_item(&parsed);

        assert_eq!(stated_range(&item.attrs), None);
    }

    /// A file that states nothing is not a file with a problem, however many other attributes it
    /// carries.
    #[test]
    fn a_file_with_no_stated_values_is_accepted() {
        check(&file("#[gamma::skip(arith)]\nfn f() -> u32 { 1 }\n#[test]\nfn t() {}"))
            .expect("a file with no stated values has nothing to report");
    }

    /// The three positions a function is written in are all functions, and a stated value is at
    /// home in each.
    #[test]
    fn a_value_stated_on_a_function_a_method_or_a_trait_method_is_accepted() {
        let sources = [
            "#[gamma::value(0)]\nfn f() -> u32 { 1 }",
            "struct S;\nimpl S {\n#[gamma::value(0)]\nfn f(&self) -> u32 { 1 }\n}",
            "trait T {\n#[gamma::value(0)]\nfn f(&self) -> u32 { 1 }\n}",
        ];

        for source in sources {
            check(&file(source)).unwrap_or_else(|error| panic!("`{source}` states a value on a function: {error}"));
        }
    }

    /// Two values on one item would be settled by which was written first, and a rule that subtle
    /// is one nobody reads. The line reported is the second one, which is the one to delete.
    #[test]
    fn two_stated_values_on_one_item_are_reported() {
        let rejected = rejection("#[gamma::value(0)]\n#[gamma::value(1)]\nfn f() -> u32 { 2 }");

        assert!(rejected.contains("test.rs:2: "), "{rejected}");
        assert!(rejected.contains("an item may state one value"), "{rejected}");
    }

    /// Everything that is not one expression is reported the same way, because the fix is the same
    /// in every case: write one.
    #[test]
    fn an_argument_that_is_not_one_expression_is_reported() {
        for arguments in ["", "0, 1", "1 +", "let x = 1;"] {
            let rejected = rejection(&format!("#[gamma::value({arguments})]\nfn f() -> u32 {{ 2 }}"));

            assert!(rejected.contains("test.rs:1: "), "`{arguments}`: {rejected}");
            assert!(
                rejected.contains("states one value") || rejected.contains("is not a single Rust expression"),
                "`{arguments}`: {rejected}"
            );
        }
    }

    /// A bare path and a name-value pair carry no argument list at all, and neither states a value.
    #[test]
    fn a_stated_value_with_no_argument_list_is_reported() {
        for attribute in ["#[gamma::value]", "#[gamma::value = 1]"] {
            let rejected = rejection(&format!("{attribute}\nfn f() -> u32 {{ 2 }}"));

            assert!(rejected.contains("states one value"), "`{attribute}`: {rejected}");
        }
    }

    /// Inheritance is not invented: an `impl` block or a module would have to state one expression
    /// that type-checks as the body of every function beneath it, which essentially never holds.
    #[test]
    fn a_value_stated_on_anything_but_a_function_is_reported() {
        let sources = [
            "struct S;\n#[gamma::value(0)]\nimpl S { fn f(&self) -> u32 { 1 } }",
            "#[gamma::value(0)]\nmod m { pub fn f() -> u32 { 1 } }",
            "#[gamma::value(0)]\nstruct S { n: u8 }",
            "#[gamma::value(0)]\nconst N: u8 = 1;",
        ];

        for source in sources {
            let rejected = check(&file(source)).expect_err("the fixture misplaces a stated value").to_string();

            assert!(rejected.contains("goes on a function or a method"), "`{source}`: {rejected}");
        }
    }

    /// A trait method that is only declared has no body, so there is nothing for a stated value to
    /// replace — and the implementations do not inherit it, for the same reason an `impl` block
    /// does not hand one down. Silence here would be a hint that reads as working and does nothing
    /// anywhere in the tree.
    #[test]
    fn a_value_stated_on_a_declaration_is_reported() {
        let rejected = rejection("trait T {\n#[gamma::value(0)]\nfn f(&self) -> u32;\n}");

        assert!(rejected.contains("test.rs:2: "), "{rejected}");
        assert!(rejected.contains("a declaration has none"), "{rejected}");
    }

    /// A `const fn` body is a const context throughout, and the guard a mutant is spliced in behind
    /// is a run-time call. Collection returns before it reads the stated value there, so silence
    /// would leave a hint that reads as working and produces no mutant anywhere.
    #[test]
    fn a_value_stated_on_a_const_function_is_reported() {
        let sources = [
            "#[gamma::value(0)]\nconst fn f() -> u32 { 1 }",
            "struct S;\nimpl S {\n#[gamma::value(0)]\nconst fn f(&self) -> u32 { 1 }\n}",
            "trait T {\n#[gamma::value(0)]\nconst fn f(&self) -> u32 { 1 }\n}",
        ];

        for source in sources {
            let rejected = check(&file(source)).expect_err("a const function can carry no mutant").to_string();

            assert!(rejected.contains("no `const fn` body may make"), "`{source}`: {rejected}");
        }
    }

    /// Empty bodies are not eligible for stated-value mutation, so accepting an attribute there
    /// would leave a hint that produces no mutant.
    #[test]
    fn a_value_stated_on_an_empty_bodied_function_is_reported() {
        let sources = [
            "#[gamma::value(())]\nfn f() {}",
            "struct S;\nimpl S {\n#[gamma::value(())]\nfn f(&self) {}\n}",
            "trait T {\n#[gamma::value(())]\nfn f(&self) {}\n}",
        ];

        for source in sources {
            let rejected = check(&file(source)).expect_err("an empty body has nothing to replace").to_string();

            assert!(
                rejected.contains("not eligible for stated-value mutation"),
                "`{source}`: {rejected}"
            );
        }
    }

    /// A `const fn` with an empty body is inert twice over, and the const reason is the one
    /// reported: making the function non-`const` is what an author has to do before a mutant is
    /// possible at all, and only then does the empty body become the remaining problem.
    #[test]
    fn a_doubly_inert_function_reports_the_const_reason() {
        let rejected = rejection("#[gamma::value(())]\nconst fn f() {}");

        assert!(rejected.contains("no `const fn` body may make"), "{rejected}");
    }

    /// A malformed expression is reported ahead of the inert position it sits on, because it is
    /// the mistake the author can see — and reporting both would be two diagnostics for one
    /// attribute.
    #[test]
    fn a_malformed_value_on_an_inert_function_reports_the_expression() {
        let rejected = rejection("#[gamma::value(1 +)]\nconst fn f() -> u32 { 1 }");

        assert!(rejected.contains("states one value"), "{rejected}");
        assert!(!rejected.contains("no `const fn` body may make"), "{rejected}");
    }

    /// Neither rule reaches past the function it is about: a `const` *item* inside an ordinary
    /// body, and an ordinary function whose body is a single expression, both still state values.
    #[test]
    fn a_function_that_can_carry_a_mutant_is_still_accepted() {
        let sources = [
            "#[gamma::value(0)]\nfn f() -> u32 { const N: u32 = 1; N }",
            "#[gamma::value(0)]\nasync fn f() -> u32 { 1 }",
            "struct S;\nimpl S {\n#[gamma::value(0)]\nfn f(&self) -> u32 { 1 }\n}",
        ];

        for source in sources {
            check(&file(source)).unwrap_or_else(|error| panic!("`{source}` can carry a mutant: {error}"));
        }
    }

    /// A nested function states its own value, at its own site, and is not the enclosing function
    /// stating a second one.
    #[test]
    fn a_function_inside_a_function_may_state_its_own_value() {
        let source = "#[gamma::value(0)]\nfn f() -> u32 { #[gamma::value(1)] fn g() -> u32 { 2 } g() }";

        check(&file(source)).expect("both values are stated on a function of their own");
    }

    /// A file with two mistakes reports the earlier one, so the message does not depend on the
    /// order the walk reached them in.
    #[test]
    fn the_first_fault_in_the_file_is_the_one_reported() {
        let rejected = rejection("#[gamma::value(0, 1)]\nfn f() -> u32 { 2 }\n#[gamma::value()]\nfn g() -> u32 { 3 }");

        assert!(rejected.contains("test.rs:1: "), "{rejected}");
    }
}
