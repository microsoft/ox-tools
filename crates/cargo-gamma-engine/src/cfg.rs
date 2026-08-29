// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The configuration predicates that hold for one build, and what they say about an attribute.

use syn::punctuated::Punctuated;
use syn::{Attribute, Expr, Lit, Meta, Token};

use crate::HashSet;

/// The configuration predicates that hold for a build.
///
/// Construct one with [`CfgSet::parse`] from captured `rustc` output or
/// [`CfgSet::unconditional`] for a context where nothing should be stripped.
#[derive(Clone, Debug, Default)]
pub struct CfgSet {
    /// Bare names, such as `unix` or a `--cfg loom` the build passes.
    names: HashSet<String>,

    /// Key/value pairs, such as `target_arch="x86_64"` or `feature="std"`.
    pairs: HashSet<(String, String)>,

    /// Names that are in force under a condition this cannot evaluate.
    ///
    /// Answering one of these either way would settle a predicate on a guess, so they are
    /// unanswerable and the code they gate stays mutable.
    undecided: HashSet<String>,

    /// Whether predicates are checked at all.
    ///
    /// An unresolved set holds everything, so a caller with no cfg information behaves exactly as
    /// this tool did before cfg evaluation existed.
    enforced: bool,
}

impl CfgSet {
    /// Returns a set under which every predicate holds, so nothing is ever stripped.
    #[must_use]
    pub fn unconditional() -> Self {
        Self::default()
    }

    /// Reads a set out of the lines `rustc --print cfg` prints.
    ///
    /// Each line is either a bare name or `key="value"`. Anything else is skipped rather than
    /// guessed at.
    ///
    /// ```rust
    /// # use cargo_gamma_engine::cfg::CfgSet;
    /// let set = CfgSet::parse("unix\ntarget_os=\"linux\"\n");
    ///
    /// assert!(set.holds_str("unix"));
    /// assert!(set.holds_str("target_os = \"linux\""));
    /// ```
    #[must_use]
    pub fn parse(printed: &str) -> Self {
        let mut set = Self {
            enforced: true,
            ..Self::default()
        };

        for line in printed.lines() {
            let line = line.trim();

            if line.is_empty() {
                continue;
            }

            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim();
                let quoted = value.trim();

                // The pair is dropped when the line names no value at all — `nonsense=` is not
                // something `rustc` prints and guessing what it meant would invent a predicate.
                // A value that is *written* and empty is another matter: `target_abi=""` on most
                // targets and `target_env=""` on Apple and wasm are real answers, and
                // `#[cfg(target_env = "")]` is how source asks for exactly them. Refusing that
                // pair would not leave the predicate unanswerable, since the key is not marked
                // undecided either, so the lookup would answer `No` to something the compiler
                // says is true and take the code it gates out of the population.
                if !key.is_empty() && !quoted.is_empty() {
                    let _added = set.pairs.insert((key.to_owned(), quoted.trim_matches('"').to_owned()));
                }
            } else {
                let _added = set.names.insert(line.to_owned());
            }
        }

        set
    }

    /// Adds the Cargo features enabled for the package this set will be used on.
    ///
    /// Features are per package, so one set per package is built from one shared `rustc` answer.
    #[must_use]
    pub fn with_features(mut self, features: impl IntoIterator<Item = String>) -> Self {
        for feature in features {
            let _added = self.pairs.insert(("feature".to_owned(), feature));
        }

        self
    }

    /// Marks names whose truth this set cannot decide.
    ///
    /// A name here answers neither yes nor no, so a predicate that turns on it is unanswerable and
    /// the code it gates stays mutable. That is the direction every uncertainty in this module
    /// resolves in: a mutant that should not exist is visible, while one silently missing from the
    /// population is a hole in the measurement nobody can see.
    #[must_use]
    pub fn with_undecided(mut self, names: impl IntoIterator<Item = String>) -> Self {
        for name in names {
            let _added = self.undecided.insert(name);
        }

        self
    }

    /// Adds the bare `test` predicate, for a set describing a `cargo test` build.
    ///
    /// The instrumented build is `cargo test --no-run`, so `rustc` compiles each library's
    /// unit-test target with `--cfg test` — but `rustc --print cfg`, which is asked about the
    /// target rather than about any particular crate, never mentions it. Without this, an item
    /// gated `#[cfg(any(feature = "x", test))]` with the feature off looks like code the compiler
    /// never sees, and is silently dropped from the population although the unit tests exercise it.
    ///
    /// ```rust
    /// # use cargo_gamma_engine::cfg::CfgSet;
    /// let set = CfgSet::parse("unix\n").with_test();
    ///
    /// // Compiled into the unit-test target, so it is real code that real tests run.
    /// assert!(set.holds_str("any(feature = \"absent\", test)"));
    ///
    /// // And `#[cfg(not(test))]` is ordinary production code, which stays mutable.
    /// assert!(set.holds_str("not(test)"));
    /// ```
    #[must_use]
    pub fn with_test(mut self) -> Self {
        let _added = self.names.insert("test".to_owned());

        self
    }

    /// Returns whether every effective `#[cfg(...)]` among `attrs` holds.
    ///
    /// Active `cfg_attr` attributes are expanded before their `cfg` children are consulted. An
    /// undecidable `cfg_attr` condition is left unapplied: stripping code because an attribute
    /// might have appeared would be the same unsupported guess this module avoids elsewhere.
    #[must_use]
    pub fn holds_for(&self, attrs: &[Attribute]) -> bool {
        self.holds_effective(&self.effective(attrs))
    }

    /// Returns whether effective attributes confine an item to test code.
    ///
    /// `cfg_attr` can add either `cfg(test)` or a test attribute itself. Both have to be read
    /// after its condition is evaluated, or the collector can mutate a test helper that rustc
    /// treats as a test, while a false condition can hide ordinary production code on a guess.
    #[must_use]
    pub fn test_gated(&self, attrs: &[Attribute]) -> bool {
        self.test_gated_effective(attrs, &self.effective(attrs))
    }

    /// Returns whether `attrs` take an item out of the population: it is gated to test code, or it
    /// is behind a configuration predicate that does not hold for this build.
    ///
    /// Every call site that asks this asks both [`Self::test_gated`] and [`Self::holds_for`]
    /// together, and each independently expands `cfg_attr` metadata — cloning every attribute and
    /// shifting a vector — to answer its one question. This shares that expansion between both.
    #[must_use]
    pub fn skip_gate(&self, attrs: &[Attribute]) -> bool {
        let effective = self.effective(attrs);

        self.test_gated_effective(attrs, &effective) || !self.holds_effective(&effective)
    }

    /// The [`Self::holds_for`] answer, given an already-expanded attribute list.
    fn holds_effective(&self, effective: &[Meta]) -> bool {
        // #[gamma::skip(cond.always_false, reason = "`decide` also returns `Unknown` whenever enforcement is off, so this is only an early return and removing it cannot change an answer")]
        if !self.enforced {
            return true;
        }

        effective.iter().all(|attribute| {
            // An attribute this module cannot parse says nothing about whether the code is built,
            // so the code stays mutable.
            cfg_predicate(attribute).is_none_or(|predicate| !is_test_only(&predicate) && self.holds(&predicate))
        })
    }

    /// The [`Self::test_gated`] answer, given an already-expanded attribute list.
    fn test_gated_effective(&self, attrs: &[Attribute], effective: &[Meta]) -> bool {
        effective
            .iter()
            .any(|attribute| cfg_predicate(attribute).is_some_and(|predicate| is_test_only(&predicate)) || is_test_attribute(attribute))
            || attrs.iter().any(|attribute| {
                cfg_attr(&attribute.meta).is_some_and(|(condition, nested)| {
                    self.decide(&condition, true) == Verdict::Yes
                        && nested.iter().any(|attribute| {
                            cfg_predicate(attribute).is_some_and(|predicate| is_test_only(&predicate)) || is_test_attribute(attribute)
                        })
                })
            })
    }

    /// Returns whether a predicate written as source text holds.
    ///
    /// An unparsable predicate holds, for the same reason an unparsable attribute does.
    ///
    /// The tool itself always has real attributes to hand and so calls [`Self::holds_for`]; this is
    /// the spelling the doctests and the unit tests use, because writing a predicate as text is the
    /// only way to state one readably.
    #[must_use]
    pub fn holds_str(&self, predicate: &str) -> bool {
        if crate::parse::exceeds_nesting_limit(predicate) {
            return true;
        }

        syn::parse_str::<Meta>(predicate).map_or(true, |meta| self.holds(&meta))
    }

    /// Decides a predicate written as source text, without collapsing an unknown answer.
    ///
    /// [`Self::holds_str`] answers the question source code asks — "does this item survive" — and
    /// so reads an unknown as "keep it". A caller deciding whether a *configuration table* is in
    /// force needs the three answers apart: applying a table on an unknown would put names in
    /// force that the build may not set, which strips the code their negation gates.
    #[must_use]
    pub fn decide_str(&self, predicate: &str) -> Verdict {
        if crate::parse::exceeds_nesting_limit(predicate) {
            return Verdict::Unknown;
        }

        syn::parse_str::<Meta>(predicate).map_or(Verdict::Unknown, |meta| self.verdict(&meta))
    }

    /// Decides a parsed predicate without collapsing an unknown answer.
    #[must_use]
    pub fn decide_meta(&self, predicate: &Meta) -> Verdict {
        self.verdict(predicate)
    }

    /// Returns whether one parsed predicate holds, treating an unknown answer as holding.
    fn holds(&self, meta: &Meta) -> bool {
        !matches!(self.verdict(meta), Verdict::No)
    }

    /// Expands every `cfg_attr` whose condition is definitely true.
    ///
    /// The worklist rather than recursion matters for nested `cfg_attr`s: source is input, and
    /// the expansion must not add another unbounded walk beside the parser's nesting guard.
    fn effective(&self, attrs: &[Attribute]) -> Vec<Meta> {
        let mut effective: Vec<Meta> = attrs.iter().map(|attribute| attribute.meta.clone()).collect();
        let mut at = 0;

        while at < effective.len() {
            let Some((condition, nested)) = cfg_attr(&effective[at]) else {
                at += 1;
                continue;
            };

            let _removed = effective.remove(at);

            if self.verdict(&condition) == Verdict::Yes {
                for attribute in nested.into_iter().rev() {
                    effective.insert(at, attribute);
                }
            }
        }

        effective
    }

    /// Returns whether this set describes a build that compiles some target with `--cfg test`.
    fn is_test_build(&self) -> bool {
        self.names.contains("test")
    }

    /// Decides a predicate under both halves of a `cargo test` build.
    ///
    /// `cargo test --no-run` compiles a library twice: once as its own unit-test target, where
    /// `--cfg test` is set, and once as the plain library that the integration tests and binaries
    /// link, where it is not. A predicate mentioning `test` therefore has two answers at once, and
    /// code that *either* of them keeps is code the build really contains.
    ///
    /// So the two halves are evaluated separately and disagreement comes out as `Unknown`, which
    /// keeps the mutant. That is what makes `any(feature = "x", test)` mutable when the feature is
    /// off, and what stops `not(test)` — ordinary production code, compiled into every target
    /// except the unit-test one — from being deleted from the population.
    fn verdict(&self, meta: &Meta) -> Verdict {
        // #[gamma::skip(cond.always_false, reason = "when this is not a test build, `decide` ignores its `test` argument, so evaluating both halves produces the same verdict as this fast path")]
        if !self.is_test_build() {
            // #[gamma::skip(literal.bool_flip, reason = "outside a test build `decide` never consults its `test` argument, so true and false produce the identical verdict")]
            return self.decide(meta, false);
        }

        let unit_test = self.decide(meta, true);
        let library = self.decide(meta, false);

        if unit_test == library { unit_test } else { Verdict::Unknown }
    }

    /// Decides one parsed predicate, which may be unanswerable.
    ///
    /// The three-valued answer is not pedantry. `not(version("1.80"))` has to come out *unknown*
    /// rather than false, because negating a predicate this module cannot evaluate would remove
    /// code from the population on the strength of a guess.
    ///
    /// `test` says which half of a `cargo test` build is being decided; see [`Self::verdict`].
    fn decide(&self, meta: &Meta, test: bool) -> Verdict {
        if !self.enforced {
            return Verdict::Unknown;
        }

        match meta {
            // A bare name: `unix`, `loom`, `test`. `rustc` lists every name that is on, so a name
            // that is absent is genuinely off rather than merely unrecognised — unless the build
            // description marked it unanswerable. `test` is the one name whose answer depends on
            // which target of the build is being compiled.
            Meta::Path(path) => path.get_ident().map_or(Verdict::Unknown, |name| {
                let name = name.to_string();

                if name == "test" && self.is_test_build() {
                    return Verdict::from(test);
                }

                if self.undecided.contains(&name) {
                    return Verdict::Unknown;
                }

                Verdict::from(self.names.contains(&name))
            }),

            // `key = "value"`: `target_arch = "x86_64"`, `feature = "std"`.
            Meta::NameValue(pair) => {
                let (Some(key), Expr::Lit(literal)) = (pair.path.get_ident(), &pair.value) else {
                    return Verdict::Unknown;
                };

                let Lit::Str(value) = &literal.lit else {
                    return Verdict::Unknown;
                };

                let key = key.to_string();

                if self.undecided.contains(&key) {
                    return Verdict::Unknown;
                }

                Verdict::from(self.pairs.contains(&(key, value.value())))
            }

            Meta::List(list) => self.decide_list(list, test),
        }
    }

    /// Decides an `all(..)`, `any(..)` or `not(..)` predicate.
    fn decide_list(&self, list: &syn::MetaList, test: bool) -> Verdict {
        let Some(name) = list.path.get_ident().map(ToString::to_string) else {
            return Verdict::Unknown;
        };

        // `version(..)`, and whatever the language adds next, is not modelled here, and an
        // unmodelled predicate must not remove code from the population.
        if !matches!(name.as_str(), "all" | "any" | "not") {
            return Verdict::Unknown;
        }

        let parser = Punctuated::<Meta, Token![,]>::parse_terminated;

        let Ok(inner) = list.parse_args_with(parser) else {
            return Verdict::Unknown;
        };

        let mut answers = inner.iter().map(|meta| self.decide(meta, test));

        match name.as_str() {
            // One `No` settles an `all`, and one `Yes` settles an `any`, however unknown the rest
            // is. Otherwise an unknown among them leaves the whole thing unknown.
            "all" => combine(&mut answers, Verdict::No, Verdict::Yes),
            "any" => combine(&mut answers, Verdict::Yes, Verdict::No),

            // `not` takes exactly one predicate. Anything else is malformed, and a malformed
            // predicate is as unanswerable as an unmodelled one.
            // #[gamma::skip(iter.first_to_last, reason = "this branch accepts exactly one operand, for which `first` and `last` are the same; with any other length the guard rejects it")]
            _ => match inner.first() {
                Some(only) if inner.len() == 1 => self.decide(only, test).negated(),
                _ => Verdict::Unknown,
            },
        }
    }
}

/// Returns the predicate inside one `cfg` attribute.
///
/// A malformed attribute is deliberately not interpreted: source rustc cannot understand is not
/// evidence that code should be omitted from the population.
fn cfg_predicate(attribute: &Meta) -> Option<Meta> {
    let Meta::List(list) = attribute else {
        return None;
    };

    if !attribute.path().is_ident("cfg") {
        return None;
    }

    list.parse_args::<Meta>().ok()
}

/// Returns a `cfg_attr` condition and the attributes it conditionally applies.
///
/// `cfg_attr` requires at least a predicate and one attribute. A malformed spelling is ignored
/// just like an unparsable `cfg`: it must not shrink the population on a guess.
fn cfg_attr(attribute: &Meta) -> Option<(Meta, Vec<Meta>)> {
    let Meta::List(list) = attribute else {
        return None;
    };

    if !attribute.path().is_ident("cfg_attr") {
        return None;
    }

    let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
    let arguments = list.parse_args_with(parser).ok()?;
    let condition = arguments.first()?.clone();

    (arguments.len() > 1).then(|| (condition, arguments.into_iter().skip(1).collect()))
}

/// Returns whether an effective attribute is a test function attribute.
///
/// The collector has always excluded `#[test]` and framework-qualified variants such as
/// `#[tokio::test]`; `cfg_attr` makes the same rule conditional rather than different.
fn is_test_attribute(attribute: &Meta) -> bool {
    attribute.path().segments.last().is_some_and(|segment| segment.ident == "test")
}

/// Returns whether effective configuration gates confine an item to a unit-test build under `cfg`.
///
/// Module discovery needs this narrower answer: `#[test]` belongs to a function, while only
/// `cfg(test)` controls whether a separately declared module file is test scaffolding.
#[must_use]
pub fn test_gated_for(cfg: &CfgSet, attrs: &[Attribute]) -> bool {
    cfg.effective(attrs)
        .iter()
        .any(|attribute| cfg_predicate(attribute).is_some_and(|predicate| is_test_only(&predicate)))
}

/// Returns whether a predicate can only hold in a unit-test target.
///
/// `#[cfg(test)]` marks code that exists only to test other code, and mutating it measures the
/// tests' tests, which nobody has. The same is true of `all(test, feature = "x")`: a conjunction
/// requires every operand, so the item is unreachable outside the unit-test target however the
/// rest of the predicate turns out.
///
/// Only `all` is descended into. Under `any`, `test` is an alternative rather than a requirement —
/// `any(feature = "x", test)` also describes the code the feature builds — and under `not` it says
/// the opposite of test-only. Either would take live production code out of the population.
fn is_test_only(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) => path.is_ident("test"),

        Meta::List(list) if list.path.is_ident("all") => {
            let parser = Punctuated::<Meta, Token![,]>::parse_terminated;

            list.parse_args_with(parser).is_ok_and(|inner| inner.iter().any(is_test_only))
        }

        _ => false,
    }
}

/// What this module can say about a predicate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verdict {
    /// The predicate holds, so the code is in the build.
    Yes,

    /// The predicate does not hold, so the compiler strips the code.
    No,

    /// This module cannot tell, so the code is left mutable.
    Unknown,
}

impl From<bool> for Verdict {
    fn from(held: bool) -> Self {
        if held { Self::Yes } else { Self::No }
    }
}

impl Verdict {
    /// Returns the answer to the negation of whatever produced this one.
    const fn negated(self) -> Self {
        match self {
            Self::Yes => Self::No,
            Self::No => Self::Yes,
            Self::Unknown => Self::Unknown,
        }
    }
}

/// Folds the answers of a combinator's operands.
///
/// `settles` is the answer that decides the whole combinator on its own — `No` for `all`, `Yes`
/// for `any` — and `otherwise` is what an operand-free or entirely undecisive list comes to.
fn combine(answers: &mut dyn Iterator<Item = Verdict>, settles: Verdict, otherwise: Verdict) -> Verdict {
    let mut unknown = false;

    for answer in answers {
        if answer == settles {
            return settles;
        }

        unknown |= answer == Verdict::Unknown;
    }

    if unknown { Verdict::Unknown } else { otherwise }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set() -> CfgSet {
        CfgSet::parse("unix\ntarget_arch=\"x86_64\"\ntarget_os=\"linux\"\npanic=\"unwind\"\n")
            .with_features(["std".to_owned(), "serde".to_owned()])
    }

    fn attribute(text: &str) -> Vec<Attribute> {
        let item: syn::ItemFn = syn::parse_str(&format!("{text} fn f() {{}}")).expect("the fixture parses");

        item.attrs
    }

    fn test_gated(attrs: &[Attribute]) -> bool {
        test_gated_for(&CfgSet::default(), attrs)
    }

    #[test]
    fn a_bare_name_is_looked_up() {
        assert!(set().holds_str("unix"));
        assert!(!set().holds_str("windows"));
        assert!(!set().holds_str("loom"), "a custom cfg nobody passed is off");
    }

    #[test]
    fn a_key_value_pair_is_looked_up() {
        assert!(set().holds_str("target_arch = \"x86_64\""));
        assert!(!set().holds_str("target_arch = \"aarch64\""));
        assert!(set().holds_str("panic = \"unwind\""));
    }

    /// `""` is a value rustc really prints — `target_abi` on most targets, `target_env` on Apple
    /// and on wasm — and a predicate testing for it is the idiomatic way to say "no particular
    /// one". Dropping the pair would not leave the predicate unanswerable, because the key is not
    /// undecided either: the lookup would answer `No` for something rustc says is true, and the
    /// code it gates would leave the population.
    #[test]
    fn an_empty_value_is_a_value_like_any_other() {
        let set = CfgSet::parse("target_env=\"\"\ntarget_abi=\"\"\ntarget_os=\"macos\"\n");

        assert!(set.holds_str("target_env = \"\""));
        assert!(set.holds_str("target_abi = \"\""));
        assert!(!set.holds_str("target_env = \"gnu\""));
        assert!(set.holds_str("not(target_env = \"gnu\")"));
    }

    /// A name marked undecided answers neither yes nor no: `holds_str` keeps the code (an unknown
    /// holds), while `decide_str` reports the unknown rather than collapsing it.
    #[test]
    fn an_undecided_name_is_unknown_rather_than_looked_up() {
        let undecided = set().with_undecided(["loom".to_owned()]);

        assert!(undecided.holds_str("loom"), "an unknown predicate must not strip the code it gates");
        assert_eq!(undecided.decide_str("loom"), Verdict::Unknown);

        // A name not marked undecided is looked up exactly as before.
        assert!(!undecided.holds_str("windows"));
    }

    /// The same three-valued treatment applies to a key/value pair whose key is undecided.
    #[test]
    fn an_undecided_key_value_pair_is_unknown_rather_than_looked_up() {
        let undecided = set().with_undecided(["target_arch".to_owned()]);

        assert!(undecided.holds_str("target_arch = \"x86_64\""));
        assert_eq!(undecided.decide_str("target_arch = \"x86_64\""), Verdict::Unknown);

        // An unrelated pair is decided normally.
        assert!(undecided.holds_str("panic = \"unwind\""));
        assert_eq!(undecided.decide_str("panic = \"unwind\""), Verdict::Yes);
    }

    #[test]
    fn a_feature_is_a_pair_like_any_other() {
        assert!(set().holds_str("feature = \"std\""));
        assert!(!set().holds_str("feature = \"stats\""));
    }

    #[test]
    fn the_combinators_compose() {
        assert!(set().holds_str("all(unix, target_arch = \"x86_64\")"));
        assert!(!set().holds_str("all(unix, windows)"));
        assert!(set().holds_str("any(windows, unix)"));
        assert!(!set().holds_str("any(windows, feature = \"stats\")"));
        assert!(set().holds_str("not(windows)"));
        assert!(!set().holds_str("not(unix)"));
        assert!(set().holds_str("not(all(unix, windows))"));
        assert!(!set().holds_str("any()"), "an empty `any` holds for nothing");
        assert!(set().holds_str("all()"), "an empty `all` holds vacuously");
    }

    #[test]
    fn an_unmodelled_predicate_holds() {
        // Removing code because this module has not heard of a predicate would silently shrink the
        // population, which is the one failure mode nobody can see in a report.
        assert!(set().holds_str("version(\"1.80\")"));
        assert!(set().holds_str("version(unix)"));
        assert!(set().holds_str("not(version(\"1.80\"))"));
        assert!(set().holds_str("this is not valid syntax at all"));
    }

    #[test]
    fn an_unknown_operand_leaves_a_combinator_unknown() {
        // `not` of something unanswerable is unanswerable, not false. Getting this wrong would
        // strip code on the strength of a predicate this module has never heard of.
        assert!(set().holds_str("not(version(\"1.80\"))"));
        assert!(set().holds_str("all(unix, version(\"1.80\"))"));
        assert!(set().holds_str("any(windows, version(\"1.80\"))"));

        // A decisive operand still settles it, however unknown its neighbours are.
        assert!(!set().holds_str("all(windows, version(\"1.80\"))"));
        assert!(set().holds_str("any(unix, version(\"1.80\"))"));
        assert!(!set().holds_str("not(any(unix, version(\"1.80\")))"));
    }

    #[test]
    fn a_malformed_not_holds() {
        assert!(set().holds_str("not(unix, windows)"), "a `not` of two things is not a `not`");
        assert!(set().holds_str("not()"));
    }

    /// `all(..)`/`any(..)`/`not(..)` take a comma-separated list of predicates, and a token stream
    /// that does not parse that way — two predicates with no comma between them — is malformed in
    /// a way the compiler itself would also reject; treating it as false would strip code on the
    /// strength of a predicate this module could not actually read.
    #[test]
    fn a_combinator_whose_arguments_do_not_parse_as_a_meta_list_holds() {
        assert!(set().holds_str("all(unix windows)"));
    }

    /// `key = value` with anything but a string literal on the right — a number, a boolean, a
    /// path — is not a shape `rustc --print cfg` ever produces, so guessing at it would risk
    /// stripping code on a predicate this module cannot actually evaluate.
    #[test]
    fn a_name_value_predicate_whose_literal_is_not_a_string_holds() {
        assert!(set().holds_str("target_pointer_width = 64"));
    }

    /// A combinator whose head is a path rather than a bare identifier — `a::b(unix)` — is not one
    /// of `all`, `any` or `not` however it is spelled, and treating it as false would remove code
    /// this module simply does not recognise.
    #[test]
    fn a_combinator_whose_head_is_not_a_bare_identifier_holds() {
        assert!(set().holds_str("a::b(unix)"));
    }

    #[test]
    fn an_unresolved_set_holds_everything() {
        let set = CfgSet::unconditional();

        assert!(set.holds_str("windows"));
        assert!(set.holds_str("feature = \"nothing-like-this\""));
        assert!(set.holds_str("not(unix)"));
        assert!(set.holds_for(&attribute("#[cfg(windows)]")));
    }

    /// `decide_str` is the three-valued counterpart of `holds_str`: a decisive predicate reports
    /// its actual verdict rather than collapsing straight to a boolean.
    #[test]
    fn decide_str_reports_the_undecided_verdict() {
        assert_eq!(set().decide_str("unix"), Verdict::Yes);
        assert_eq!(set().decide_str("windows"), Verdict::No);
        assert_eq!(set().decide_str("version(\"1.80\")"), Verdict::Unknown);
    }

    /// A predicate written deeply enough to overflow a recursive-descent parser is refused by the
    /// same nesting guard the source parser uses, before `syn` ever sees it — for both the
    /// boolean-collapsing and three-valued entry points.
    #[test]
    fn a_predicate_nested_deeper_than_the_limit_is_refused_rather_than_parsed() {
        let depth = 4_096;
        let predicate = format!("{}unix{}", "not(".repeat(depth), ")".repeat(depth));

        assert!(
            set().holds_str(&predicate),
            "an unparsed predicate must not strip the code it gates"
        );
        assert_eq!(set().decide_str(&predicate), Verdict::Unknown);
    }

    #[test]
    fn printed_lines_that_are_not_settings_are_skipped() {
        let set = CfgSet::parse("unix\n\n   \ntarget_os=\"linux\"\nnonsense=\n=orphan\n");

        assert!(set.holds_str("unix"));
        assert!(set.holds_str("target_os = \"linux\""));
        assert!(!set.holds_str("nonsense = \"\""));
        assert!(!set.holds_str("orphan"));
        assert!(!set.names.contains(""));
    }

    #[test]
    fn only_cfg_attributes_are_consulted() {
        assert!(set().holds_for(&attribute("#[inline]")));
        assert!(set().holds_for(&attribute("#[doc = \"windows\"]")));
        assert!(set().holds_for(&attribute("#[allow(windows)]")));

        // An inactive `cfg_attr` adds nothing.
        assert!(set().holds_for(&attribute("#[cfg_attr(windows, inline)]")));
    }

    #[test]
    fn an_active_cfg_attr_applies_its_cfg_attribute() {
        let active = attribute("#[cfg_attr(unix, cfg(windows))]");
        let inactive = attribute("#[cfg_attr(windows, cfg(windows))]");
        let nested = attribute("#[cfg_attr(unix, cfg_attr(unix, cfg(windows)))]");

        assert!(!set().holds_for(&active), "the active inner cfg removes the item");
        assert!(set().holds_for(&inactive), "an inactive cfg_attr contributes no cfg");
        assert!(
            !set().holds_for(&nested),
            "active cfg_attrs expand until the effective attribute is reached"
        );
    }

    #[test]
    fn every_cfg_attribute_has_to_hold() {
        assert!(set().holds_for(&attribute("#[cfg(unix)]")));
        assert!(!set().holds_for(&attribute("#[cfg(windows)]")));
        assert!(set().holds_for(&attribute("#[cfg(unix)]\n#[cfg(target_os = \"linux\")]")));
        assert!(!set().holds_for(&attribute("#[cfg(unix)]\n#[cfg(windows)]")));
    }

    #[test]
    fn an_unparsable_cfg_attribute_holds() {
        // A bare literal is not a `Meta`, so this is the shape that fails to parse.
        assert!(set().holds_for(&attribute("#[cfg(\"windows\")]")));
    }

    #[test]
    fn a_qualified_predicate_holds() {
        // A path with more than one segment is not something `cfg` accepts, so nothing is known
        // about it and the code stays mutable.
        assert!(set().holds_str("some::thing"));
        assert!(set().holds_str("some::thing(unix)"));
        assert!(set().holds_str("some::thing = \"x\""));
    }

    /// The set the collector uses describes `cargo test --no-run`, which compiles a library both
    /// with and without `--cfg test`.
    fn test_build() -> CfgSet {
        set().with_test()
    }

    /// `any(feature = "x", test)` with the feature off is compiled into the unit-test target, so
    /// the unit tests run it and a mutant in it is killable. Leaving it out of the population was
    /// issue #22.
    #[test]
    fn an_item_a_disjunction_admits_into_the_test_target_is_mutable() {
        assert!(test_build().holds_for(&attribute("#[cfg(any(feature = \"absent\", test))]")));
        assert!(test_build().holds_for(&attribute("#[cfg(any(windows, test))]")));

        // Nothing about `test` rescues a disjunction that is false in both halves of the build.
        assert!(!test_build().holds_for(&attribute("#[cfg(any(windows, feature = \"absent\"))]")));
    }

    /// A conjunction with `test` in it cannot hold outside the unit-test target, so the item is
    /// test code — the tests' own scaffolding — whatever the rest of the predicate says.
    #[test]
    fn an_item_a_conjunction_confines_to_the_test_target_is_not_mutable() {
        assert!(!test_build().holds_for(&attribute("#[cfg(all(test, feature = \"std\"))]")));
        assert!(!test_build().holds_for(&attribute("#[cfg(all(unix, all(test, unix)))]")));

        // A conjunction that never mentions `test` is decided exactly as before.
        assert!(test_build().holds_for(&attribute("#[cfg(all(unix, feature = \"std\"))]")));
    }

    /// `#[cfg(not(test))]` is ordinary production code: every target of the build except the
    /// unit-test one compiles it, and the integration tests link and can kill mutants in it.
    #[test]
    fn an_item_kept_out_of_the_test_target_stays_mutable() {
        assert!(test_build().holds_for(&attribute("#[cfg(not(test))]")));
        assert!(test_build().holds_for(&attribute("#[cfg(all(not(test), unix))]")));

        // `not` says the opposite of test-only, so it must not be read as a test gate.
        assert!(!is_test_only(&syn::parse_str::<Meta>("not(test)").expect("the fixture parses")));
    }

    /// Plain `#[cfg(test)]` is the unit-test module itself. It is in the build, so the predicate
    /// is not false, but it is test code and nothing in it is mutated.
    #[test]
    fn a_bare_test_gate_is_in_the_build_but_not_mutated() {
        assert!(test_build().holds_str("test"), "the unit-test target compiles it");
        assert!(!test_build().holds_for(&attribute("#[cfg(test)]")));

        // A set that knows nothing of `--cfg test` still answers as it always did.
        assert!(!set().holds_str("test"));
    }

    #[test]
    fn an_active_cfg_attr_can_make_an_item_test_only() {
        let cfg = test_build();
        let gated = attribute("#[cfg_attr(unix, cfg(test))]");
        let test_attribute = attribute("#[cfg_attr(unix, test)]");
        let inactive = attribute("#[cfg_attr(windows, cfg(test))]");

        assert!(!cfg.holds_for(&gated), "the effective cfg(test) excludes test scaffolding");
        assert!(cfg.test_gated(&gated));
        assert!(cfg.test_gated(&test_attribute), "an effective #[test] is test code too");
        assert!(!cfg.test_gated(&inactive), "an inactive cfg_attr adds no test gate");
    }

    /// `skip_gate` shares one `cfg_attr` expansion between the two questions callers otherwise ask
    /// separately, so it has to agree with them exactly, on every attribute shape that exercises
    /// either question: an inactive predicate, an active `cfg(test)`, an active `cfg_attr` adding
    /// `#[test]`, and a plain unconditional attribute list.
    #[test]
    fn skip_gate_agrees_with_test_gated_or_not_holds_for_on_every_shape() {
        let unresolved = set();
        let resolved = test_build();

        let fixtures: &[&[Attribute]] = &[
            &attribute("#[cfg(unix)]"),
            &attribute("#[cfg(windows)]"),
            &attribute("#[cfg(test)]"),
            &attribute("#[cfg(all(test, unix))]"),
            &attribute("#[cfg_attr(unix, cfg(test))]"),
            &attribute("#[cfg_attr(unix, test)]"),
            &attribute("#[cfg_attr(windows, cfg(test))]"),
            &attribute("#[test]"),
            &[],
        ];

        for cfg in [&unresolved, &resolved] {
            for attrs in fixtures {
                assert_eq!(
                    cfg.skip_gate(attrs),
                    cfg.test_gated(attrs) || !cfg.holds_for(attrs),
                    "diverged on {attrs:?}"
                );
            }
        }
    }

    /// Module discovery and item collection use the same recursive test-gate rule. Each once read
    /// only the top-level path of the attribute, saw `all` rather than the `test` inside it, and
    /// mutated the tests' own helpers.
    #[test]
    fn the_shared_classifier_reads_a_compound_gate_recursively() {
        assert!(test_gated(&attribute("#[cfg(test)]")));
        assert!(test_gated(&attribute("#[cfg(all(test, unix))]")));
        assert!(test_gated(&attribute("#[cfg(all(unix, all(test, feature = \"x\")))]")));

        // `test` as one alternative among several describes code the other alternative compiles,
        // so it is production code and stays in the population.
        assert!(!test_gated(&attribute("#[cfg(any(test, feature = \"runtime\"))]")));
        assert!(!test_gated(&attribute("#[cfg(not(test))]")));
        assert!(!test_gated(&attribute("#[cfg(unix)]")));
        assert!(!test_gated(&attribute("#[test]")), "only a cfg gate says what is compiled");

        // An attribute that does not parse says nothing, so the item stays mutable.
        assert!(!test_gated(&attribute("#[cfg(\"test\")]")));
    }

    #[test]
    fn any_test_gate_marks_an_item_among_other_attributes() {
        assert!(test_gated(&attribute("#[cfg(unix)]\n#[cfg(test)]")));
        assert!(!test_gated(&attribute("#[cfg(unix)]\n#[cfg(windows)]")));
    }

    /// `test_gated_for` is the narrower, `cfg`-only sibling of `CfgSet::test_gated` that module
    /// discovery uses: it reads through active `cfg_attr`s using the given set, exactly as
    /// `CfgSet::holds_for` does, but does not treat a bare `#[test]` attribute as a gate — only
    /// `cfg(test)`, direct or expanded, marks a module file as test scaffolding.
    #[test]
    fn test_gated_for_reads_through_an_active_cfg_attr() {
        let cfg = test_build();

        assert!(test_gated_for(&cfg, &attribute("#[cfg(test)]")));
        assert!(!test_gated_for(&cfg, &attribute("#[cfg(unix)]")));

        // The condition of the `cfg_attr` is decided against `cfg` before the nested `cfg(test)`
        // is inspected, so only a `cfg_attr` whose own condition holds contributes its gate.
        assert!(test_gated_for(&cfg, &attribute("#[cfg_attr(unix, cfg(test))]")));
        assert!(!test_gated_for(&cfg, &attribute("#[cfg_attr(windows, cfg(test))]")));

        // A bare `#[test]` is not a `cfg` at all, so this narrower classifier leaves it alone —
        // unlike `CfgSet::test_gated`, which module discovery does not call.
        assert!(!test_gated_for(&cfg, &attribute("#[test]")));
    }

    /// The alphabet the law tests below build their predicate trees out of.
    ///
    /// One leaf of each kind the evaluator distinguishes, and — the point of the exercise — one
    /// that it cannot answer. A law checked only over answerable leaves would be satisfied by a
    /// two-valued evaluator, which is exactly the evaluator this module must not become.
    const LEAVES: [&str; 6] = [
        "unix",                      // a name that is on
        "windows",                   // a name that is off
        "target_arch = \"x86_64\"",  // a pair that holds
        "target_arch = \"aarch64\"", // a pair that does not
        "version(\"1.80\")",         // unmodelled, and so unanswerable
        "all()",                     // vacuously true, and the identity of `all`
    ];

    /// Every predicate of the shape the laws are stated over, smallest first.
    ///
    /// Enumerated rather than sampled. The interesting domain here is tiny — six leaves, three
    /// combinators, two levels — so it can be covered completely, and a complete answer is worth
    /// more than a random one: a law that holds for every tree of this shape holds, full stop,
    /// with no seed to reproduce and no shrinking to interpret.
    fn trees() -> Vec<String> {
        let mut out: Vec<String> = LEAVES.iter().map(|leaf| (*leaf).to_owned()).collect();

        for a in LEAVES {
            out.push(format!("not({a})"));

            for b in LEAVES {
                out.push(format!("all({a}, {b})"));
                out.push(format!("any({a}, {b})"));
            }
        }

        out
    }

    /// Evaluates a predicate to its three-valued verdict, which `holds_str` flattens away.
    ///
    /// The laws are about all three answers. Asserting them through `holds_str` would collapse
    /// `Yes` and `Unknown` together and let an evaluator that answered `Yes` to everything pass.
    fn verdict_of(set: &CfgSet, predicate: &str) -> Verdict {
        let meta = syn::parse_str::<Meta>(predicate).expect("every generated predicate parses");

        set.verdict(&meta)
    }

    /// Negating twice returns the original answer, including when that answer is `Unknown`.
    ///
    /// The `Unknown` half is the one that matters: an evaluator that resolved unknowns to `No`
    /// under negation would satisfy this law on the four answerable leaves and break it here, and
    /// breaking it means code that might be in the build being deleted from the population.
    #[test]
    fn double_negation_is_the_identity_on_all_three_answers() {
        let set = set();

        for tree in trees() {
            assert_eq!(
                verdict_of(&set, &format!("not(not({tree}))")),
                verdict_of(&set, &tree),
                "not(not({tree}))"
            );
        }
    }

    /// Negating something unanswerable leaves it unanswerable.
    #[test]
    fn negation_never_turns_an_unknown_into_an_answer() {
        let set = set();

        for tree in trees() {
            if verdict_of(&set, &tree) == Verdict::Unknown {
                assert_eq!(
                    verdict_of(&set, &format!("not({tree})")),
                    Verdict::Unknown,
                    "not({tree}) must stay unknown"
                );
            }
        }
    }

    /// `all` and `any` do not care what order their operands are written in.
    #[test]
    fn the_combinators_are_commutative() {
        let set = set();

        for a in LEAVES {
            for b in LEAVES {
                for name in ["all", "any"] {
                    assert_eq!(
                        verdict_of(&set, &format!("{name}({a}, {b})")),
                        verdict_of(&set, &format!("{name}({b}, {a})")),
                        "{name}({a}, {b})"
                    );
                }
            }
        }
    }

    /// Nor how they are bracketed.
    #[test]
    fn the_combinators_are_associative() {
        let set = set();

        for a in LEAVES {
            for b in LEAVES {
                for c in LEAVES {
                    for name in ["all", "any"] {
                        assert_eq!(
                            verdict_of(&set, &format!("{name}({name}({a}, {b}), {c})")),
                            verdict_of(&set, &format!("{name}({a}, {name}({b}, {c}))")),
                            "{name} over ({a}, {b}, {c})"
                        );
                    }
                }
            }
        }
    }

    /// De Morgan's laws hold in all three values, which is what pins `all` and `any` to each other.
    ///
    /// Without this, an arm added later could make `any` short-circuit on an unknown while `all`
    /// did not, and each combinator's own tests would still pass.
    #[test]
    fn de_morgan_holds_including_where_the_answer_is_unknown() {
        let set = set();

        for a in LEAVES {
            for b in LEAVES {
                assert_eq!(
                    verdict_of(&set, &format!("not(all({a}, {b}))")),
                    verdict_of(&set, &format!("any(not({a}), not({b}))")),
                    "not(all({a}, {b}))"
                );
                assert_eq!(
                    verdict_of(&set, &format!("not(any({a}, {b}))")),
                    verdict_of(&set, &format!("all(not({a}), not({b}))")),
                    "not(any({a}, {b}))"
                );
            }
        }
    }

    /// Each combinator's identity leaves its operand alone, and its annihilator settles it.
    ///
    /// `all(x, all())` is `x` and `all(x, any())` is `No`, whatever `x` is — including unknown.
    /// The annihilator half is where a three-valued evaluator earns its keep: one `No` settles an
    /// `all` however unanswerable the rest of it is, and refusing to settle there would leave
    /// dead code in the population instead of taking it out.
    #[test]
    fn the_identities_and_annihilators_of_the_combinators_hold() {
        let set = set();

        for tree in trees() {
            assert_eq!(verdict_of(&set, &format!("all({tree}, all())")), verdict_of(&set, &tree));
            assert_eq!(verdict_of(&set, &format!("any({tree}, any())")), verdict_of(&set, &tree));
            assert_eq!(verdict_of(&set, &format!("all({tree}, any())")), Verdict::No, "all({tree}, any())");
            assert_eq!(verdict_of(&set, &format!("any({tree}, all())")), Verdict::Yes, "any({tree}, all())");
        }
    }

    /// No predicate this module builds is ever answered by guessing.
    ///
    /// The safety argument of the whole module is that an unanswerable predicate keeps the code,
    /// so the one thing no law may permit is an `Unknown` operand producing an answer that is not
    /// forced by the other operands. `all` may answer `No` and `any` may answer `Yes` with an
    /// unknown present, because one operand settles those outright; nothing else is allowed.
    #[test]
    fn an_unknown_operand_only_settles_a_combinator_the_other_operand_had_already_settled() {
        let set = set();

        for a in LEAVES {
            for b in LEAVES {
                if verdict_of(&set, a) != Verdict::Unknown && verdict_of(&set, b) != Verdict::Unknown {
                    continue;
                }

                let (all, any) = (
                    verdict_of(&set, &format!("all({a}, {b})")),
                    verdict_of(&set, &format!("any({a}, {b})")),
                );

                assert!(
                    all == Verdict::Unknown || (all == Verdict::No && [a, b].iter().any(|one| verdict_of(&set, one) == Verdict::No)),
                    "all({a}, {b}) answered {all:?} with an unknown operand and nothing to force it"
                );
                assert!(
                    any == Verdict::Unknown || (any == Verdict::Yes && [a, b].iter().any(|one| verdict_of(&set, one) == Verdict::Yes)),
                    "any({a}, {b}) answered {any:?} with an unknown operand and nothing to force it"
                );
            }
        }
    }
}
