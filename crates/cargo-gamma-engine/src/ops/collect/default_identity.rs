// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Declaration identities and alias links composable under labeled module roots.

use syn::visit::{self, Visit};
use syn::{File, Item, Type, UseTree};

use super::defaults::{DefaultPaths, derives_default, item_attrs, value_macro_name};
use crate::cfg::{CfgSet, Verdict};
use crate::{HashMap, HashSet};

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod survivor_tests {
    use syn::parse_file;

    use super::*;

    #[test]
    fn parsed_block_macro_routes_never_forward_anonymous_item_macros() {
        #[derive(Default)]
        struct Routes {
            named_items: usize,
            anonymous_items: usize,
            invocations: usize,
        }

        impl<'ast> Visit<'ast> for Routes {
            fn visit_item_macro(&mut self, i: &'ast syn::ItemMacro) {
                if i.ident.is_some() {
                    self.named_items += 1;
                } else {
                    self.anonymous_items += 1;
                }
            }

            fn visit_macro(&mut self, _: &'ast syn::Macro) {
                self.invocations += 1;
            }

            fn visit_item_mod(&mut self, _: &'ast syn::ItemMod) {}
        }

        let block = syn::parse_str(
            "{
                macro_rules! named { () => {} }
                statement!();
                braced! {}
                let _ = expression!();
                impl Shape { associated!(); }
                trait Marker { associated!(); }
                unsafe extern \"C\" { foreign!(); }
                const VALUE: () = { nested!(); };
                fn nested() { nested!(); }
                mod ignored { item!(); }
            }",
        )
        .unwrap();
        let mut routes = Routes::default();
        routes.visit_block(&block);
        assert_eq!((routes.named_items, routes.anonymous_items, routes.invocations), (1, 0, 8));
    }

    #[test]
    fn macro_path_backtracking_stops_at_non_identifier_prefixes() {
        for source in [
            "vec![::::stringify!(impl Default for Shape {})]",
            "vec![prefix::::stringify!(impl Default for Shape {})]",
        ] {
            let node = syn::parse_str(source).unwrap();
            assert!(nominal_macro(&node, &DefaultPaths::default()), "{source}");
        }
    }

    #[test]
    fn module_targets_prefer_the_longest_alias_and_keep_unknown_links_fallible() {
        let identities = Identities {
            complete_modules: HashSet::from_iter([vec!["long".into()], vec!["short".into(), "nested".into()]]),
            links: HashMap::from_iter([
                (vec!["alias".into()], Some(vec!["short".into()])),
                (vec!["alias".into(), "nested".into()], Some(vec!["long".into()])),
                (vec!["unknown".into()], None),
            ]),
            ..Identities::default()
        };
        let mut work = 3;
        assert_eq!(
            identities.module_target(&["alias".into(), "nested".into()], &mut work),
            Some(vec!["long".into()])
        );
        assert_eq!(work, 1);
        assert_eq!(identities.module_target(&["unknown".into()], &mut work), None);
        assert_eq!(work, 0);
    }

    #[test]
    fn glob_opacity_reaches_a_fixed_point_without_skipping_later_certain_imports() {
        let source = "
            mod c { #[cfg(maybe)] struct Unseen; }
            mod b { pub use crate::c::*; }
            mod a { pub use crate::b::*; pub use crate::good::*; }
            mod good { #[derive(Default)] pub struct Shape; }
            mod z { use crate::good::*; }";
        let identities = Identities::of(&parse_file(source).unwrap(), &CfgSet::parse("").with_undecided(["maybe".into()]));
        assert_eq!(identities.default_for(&["a".into(), "Shape".into()]), None);
        assert_eq!(identities.default_for(&["z".into(), "Shape".into()]), Some(true));
        assert_eq!(identities.default_for(&["good".into(), "Shape".into()]), Some(true));
    }

    #[test]
    fn sorted_glob_work_is_independent_of_discovery_order_at_the_opacity_boundary() {
        for (export_count, expected) in [(4084, Some(false)), (4085, None)] {
            for reverse in [false, true] {
                let mut globs = vec![
                    (vec!["a".into()], Some(vec!["b".into()]), false),
                    (vec!["b".into()], Some(vec!["c".into()]), false),
                    (vec!["z".into()], Some(vec!["source".into()]), false),
                ];
                if reverse {
                    globs.reverse();
                }
                let mut exports = HashSet::from_iter([vec!["source".into(), "Shape".into()]]);
                exports.extend((1..export_count).map(|index| vec![format!("unrelated{index}")]));
                let mut identities = Identities {
                    declarations: HashMap::from_iter([(vec!["source".into(), "Shape".into()], Some(false))]),
                    complete_modules: HashSet::from_iter([vec!["b".into()], vec!["c".into()], vec!["source".into()]]),
                    opaque_modules: HashSet::from_iter([vec!["c".into()]]),
                    exports,
                    globs,
                    ..Identities::default()
                };
                identities.finish_globs();
                assert_eq!(
                    identities.default_for(&["z".into(), "Shape".into()]),
                    expected,
                    "{export_count}, reversed={reverse}"
                );
            }
        }
    }

    #[test]
    fn opaque_reexports_do_not_spend_the_work_reserved_for_certain_imports() {
        let mut exports = HashSet::from_iter([vec!["source".into(), "Shape".into()]]);
        exports.extend((1..4092).map(|index| vec![format!("unrelated{index}")]));
        let mut identities = Identities {
            declarations: HashMap::from_iter([(vec!["source".into(), "Shape".into()], Some(false))]),
            complete_modules: HashSet::from_iter([vec!["source".into()]]),
            opaque_modules: HashSet::from_iter([vec!["a".into()]]),
            exports,
            globs: vec![
                (vec!["a".into()], Some(vec!["source".into()]), true),
                (vec!["z".into()], Some(vec!["source".into()]), false),
            ],
            ..Identities::default()
        };
        identities.finish_globs();
        assert_eq!(identities.default_for(&["z".into(), "Shape".into()]), Some(false));
        assert_eq!(identities.default_for(&["a".into(), "Shape".into()]), None);
    }

    #[test]
    fn glob_export_growth_shares_the_exact_work_budget_with_candidate_inspection() {
        for (export_count, expected) in [(1364, Some(false)), (1365, None)] {
            let mut exports = HashSet::from_iter([vec!["source".into(), "Shape".into()]]);
            exports.extend((1..export_count).map(|index| vec![format!("unrelated{index}")]));
            let mut identities = Identities {
                declarations: HashMap::from_iter([(vec!["source".into(), "Shape".into()], Some(false))]),
                complete_modules: HashSet::from_iter([vec!["source".into()]]),
                exports,
                globs: vec![(vec!["facade".into()], Some(vec!["source".into()]), true)],
                ..Identities::default()
            };
            identities.finish_globs();
            assert_eq!(
                identities.default_for(&["facade".into(), "Shape".into()]),
                expected,
                "{export_count}"
            );
        }
    }

    #[test]
    fn glob_exports_require_direct_children_of_the_correct_source_module() {
        let source = "
            mod source { mod Hidden { pub struct Deep; } pub struct Direct; }
            mod other { #[derive(Default)] pub struct Hidden; }
            mod facade { pub use crate::source::*; }
            mod client { use crate::facade::*; use crate::other::*; }";
        let identities = Identities::of(&parse_file(source).unwrap(), &CfgSet::parse(""));
        assert_eq!(
            identities.resolve_path(&["client".into(), "Hidden".into()]),
            Some(vec!["other".into(), "Hidden".into()])
        );
        assert_eq!(identities.default_for(&["client".into(), "Hidden".into()]), Some(true));
        assert_eq!(identities.resolve_path(&["facade".into(), "Hidden".into()]), None);
        assert_eq!(identities.default_for(&["facade".into(), "Direct".into()]), Some(false));
    }

    #[test]
    fn private_direct_declarations_and_modules_block_glob_reexports() {
        let identities = Identities::of(
            &parse_file(
                "mod source { #[derive(Default)] pub struct Hidden; }
                 mod facade { struct Hidden; pub use crate::source::*; }
                 mod client { use crate::facade::*; }",
            )
            .unwrap(),
            &CfgSet::parse(""),
        );
        assert_eq!(identities.default_for(&["facade".into(), "Hidden".into()]), Some(false));
        assert_eq!(identities.default_for(&["client".into(), "Hidden".into()]), None);
        let identities = Identities::of(
            &parse_file(
                "mod source { pub mod Hidden { #[derive(Default)] pub struct Provided; } }
                 mod facade { mod Hidden {} pub use crate::source::*; }
                 mod client { use crate::facade::*; }",
            )
            .unwrap(),
            &CfgSet::parse(""),
        );
        assert_eq!(identities.default_for(&["client".into(), "Hidden".into(), "Provided".into()]), None);
    }

    #[test]
    fn mixed_duplicate_globs_require_all_canonical_targets_to_agree() {
        let identities = Identities::of(
            &parse_file(
                "mod first { #[derive(Default)] pub struct Shape; }
                 mod second { #[derive(Default)] pub struct Shape; }
                 mod a { pub use crate::first::Shape; }
                 mod b { pub use crate::first::Shape; }
                 mod c { pub use crate::second::Shape; }
                 mod d { pub use crate::second::Shape; }
                 mod client { use crate::a::*; use crate::b::*; use crate::c::*; use crate::d::*; }",
            )
            .unwrap(),
            &CfgSet::parse(""),
        );
        assert_eq!(identities.resolve_path(&["client".into(), "Shape".into()]), None);
        assert_eq!(identities.default_for(&["client".into(), "Shape".into()]), None);
        assert_eq!(identities.default_for(&["first".into(), "Shape".into()]), Some(true));
        assert_eq!(identities.default_for(&["second".into(), "Shape".into()]), Some(true));
    }

    #[test]
    fn an_unresolved_implementation_does_not_skip_later_providers_or_default_generic_specializations() {
        let identities = Identities::of(
            &parse_file(
                "struct Ready;
             struct Generic<T>(T);
             impl Default for Unresolved {}
             impl Default for Ready {}
             impl Default for Generic<u8> {}",
            )
            .unwrap(),
            &CfgSet::parse(""),
        );
        assert!(identities.unexpanded_implementations);
        assert_eq!(identities.default_for(&["Ready".into()]), Some(true));
        assert_eq!(identities.default_for(&["Generic".into()]), None);
    }

    #[test]
    fn composing_sources_rebases_enum_identities_without_exporting_unqualified_children() {
        let cfg = CfgSet::parse("");
        let child = Pending::of(&parse_file("pub enum Choice { A } pub union Bits { byte: u8 }").unwrap(), &cfg);
        let mut root = Pending::default();
        root.absorb_at(&child, &["outer".into(), "child".into()]);
        let identities = root.finish();
        assert!(identities.is_enum(&["outer".into(), "child".into(), "Choice".into()]));
        assert!(!identities.is_enum(&["Choice".into()]));
        assert!(!identities.is_enum(&["outer".into(), "child".into(), "Bits".into()]));
    }

    #[test]
    fn named_import_exports_distinguish_public_names_and_renames_from_private_bindings() {
        let pending = Pending::of(
            &parse_file(
                "mod source { pub struct Named; pub struct Renamed; }
             mod public { pub use crate::source::{Named, Renamed as Alias}; }
             mod private { use crate::source::{Named, Renamed as Alias}; }
             mod client { use crate::public::*; }
             mod hidden { use crate::private::*; }",
            )
            .unwrap(),
            &CfgSet::parse(""),
        );
        assert_eq!(
            pending.identities.exports,
            HashSet::from_iter([
                vec!["source".into(), "Named".into()],
                vec!["source".into(), "Renamed".into()],
                vec!["public".into(), "Named".into()],
                vec!["public".into(), "Alias".into()],
            ])
        );
        let identities = pending.finish();
        for name in ["Named", "Alias"] {
            assert_eq!(identities.default_for(&["client".into(), name.into()]), Some(false));
            assert_eq!(identities.default_for(&["hidden".into(), name.into()]), None);
        }
    }

    #[test]
    fn grouped_globs_preserve_public_reexports_but_not_private_imports() {
        let identities = Identities::of(
            &parse_file(
                "mod source { pub struct Shape; }
             mod public { pub use crate::source::{*}; }
             mod private { use crate::source::{*}; }
             mod client { use crate::public::{*}; }
             mod hidden { use crate::private::{*}; }
             mod external { use ::source::*; }",
            )
            .unwrap(),
            &CfgSet::parse(""),
        );
        assert_eq!(identities.default_for(&["client".into(), "Shape".into()]), Some(false));
        assert_eq!(identities.default_for(&["hidden".into(), "Shape".into()]), None);
        assert_eq!(identities.default_for(&["external".into(), "Shape".into()]), None);
    }

    #[test]
    fn uncertain_glob_targets_do_not_spend_the_work_reserved_for_certain_imports() {
        let cfg = CfgSet::parse("").with_undecided(["maybe".into()]);
        let mut pending = Pending::of(
            &parse_file(
                "mod source { pub struct Shape; }
                 mod uncertain { #[cfg(maybe)] use crate::source::*; }
                 mod client { use crate::source::*; }",
            )
            .unwrap(),
            &cfg,
        );
        pending
            .identities
            .exports
            .extend((1..4094).map(|index| vec![format!("unrelated{index}")]));
        assert_eq!(pending.finish().default_for(&["client".into(), "Shape".into()]), Some(false));
    }

    #[test]
    fn uncertain_or_expanding_items_poison_glob_sources_even_when_other_exports_are_defaulted() {
        let cfg = CfgSet::parse("").with_undecided(["maybe".into()]);
        for item in ["#[cfg(maybe)] struct Hidden;", "#[transform] struct Hidden;", "expand!();"] {
            let source = format!(
                "mod source {{ #[derive(Default)] pub struct Ready; {item} }}
             mod client {{ use crate::source::*; }}"
            );
            let identities = Identities::of(&parse_file(&source).unwrap(), &cfg);
            assert_eq!(identities.default_for(&["source".into(), "Ready".into()]), Some(true), "{item}");
            assert_eq!(identities.default_for(&["client".into(), "Ready".into()]), None, "{item}");
        }
    }

    #[test]
    fn exported_enums_unions_and_modules_remain_reachable_through_globs() {
        let identities = Identities::of(
            &parse_file(
                "mod source {
                pub enum Choice { A }
                pub union Bits { byte: u8 }
                pub mod nested { pub struct Shape; }
             }
             mod client { use crate::source::*; }",
            )
            .unwrap(),
            &CfgSet::parse(""),
        );
        assert!(identities.is_enum(&["client".into(), "Choice".into()]));
        assert_eq!(
            identities.resolve_path(&["client".into(), "Bits".into()]),
            Some(vec!["source".into(), "Bits".into()])
        );
        assert_eq!(
            identities.default_for(&["client".into(), "nested".into(), "Shape".into()]),
            Some(false)
        );
    }

    #[test]
    fn uncertain_imports_withhold_targets_and_restore_certainty_for_following_imports() {
        let cfg = CfgSet::parse("").with_undecided(["maybe".into()]);
        for attribute in ["#[cfg(maybe)]", "#[transform]"] {
            let source = format!(
                "#[derive(Default)] struct Ready;
             {attribute} use crate::Ready as Uncertain;
             use crate::Ready as Certain;"
            );
            let identities = Identities::of(&parse_file(&source).unwrap(), &cfg);
            assert_eq!(identities.resolve_path(&["Uncertain".into()]), None, "{attribute}");
            assert_eq!(identities.default_for(&["Certain".into()]), Some(true), "{attribute}");
        }
    }

    #[test]
    fn uncertain_modules_poison_glob_sets_and_propagate_into_unconditional_children() {
        let cfg = CfgSet::parse("").with_undecided(["maybe".into()]);
        let identities = Identities::of(
            &parse_file(
                "mod known { #[derive(Default)] pub struct Ready; }
             #[cfg(maybe)] mod uncertain { mod inner { struct Shape; } }
             mod client { use crate::known::*; use crate::uncertain::*; }
             mod sibling { struct Certain; }",
            )
            .unwrap(),
            &cfg,
        );
        assert_eq!(identities.default_for(&["client".into(), "Ready".into()]), None);
        assert_eq!(identities.default_for(&["uncertain".into(), "inner".into(), "Shape".into()]), None);
        assert_eq!(identities.default_for(&["sibling".into(), "Certain".into()]), Some(false));
    }

    #[test]
    fn conditional_enums_and_uncertain_or_generic_implementations_do_not_claim_positive_evidence() {
        let cfg = CfgSet::parse("").with_undecided(["maybe".into()]);
        let identities = Identities::of(&parse_file("#[cfg(maybe)] enum Choice { A }").unwrap(), &cfg);
        assert!(!identities.is_enum(&["Choice".into()]));
        for implementation in [
            "#[cfg(maybe)] mod uncertain { impl Default for crate::Shape {} }",
            "impl<'a> Default for Shape {}",
        ] {
            let source = format!("struct Shape; {implementation}");
            let identities = Identities::of(&parse_file(&source).unwrap(), &cfg);
            assert_eq!(identities.default_for(&["Shape".into()]), None, "{implementation}");
        }
    }

    #[test]
    fn declaration_and_implementation_descent_finds_nested_macro_providers() {
        for provider in [
            "struct Holder { field: provide_type!() }",
            "enum Holder { A(provide_type!()) }",
            "union Holder { field: provide_type!() }",
            "type Holder = provide_type!();",
            "impl Shape { fn method() { implement_default!(); } }",
        ] {
            let source = format!("struct Shape; {provider}");
            let identities = Identities::of(&parse_file(&source).unwrap(), &CfgSet::parse(""));
            assert!(identities.unexpanded_implementations, "{provider}");
            assert_eq!(identities.default_for(&["Shape".into()]), None, "{provider}");
        }
    }

    #[test]
    fn macro_inspection_keeps_uncertainty_monotone_and_does_not_poison_known_value_macros() {
        for (providers, expected) in [
            ("type Unknown = provide_type!();", None),
            ("type Known = [u8; line!() as usize];", Some(false)),
            ("type Unknown = provide_type!(); type Known = [u8; line!() as usize];", None),
        ] {
            let source = format!("struct Shape; {providers}");
            let identities = Identities::of(&parse_file(&source).unwrap(), &CfgSet::parse(""));
            assert_eq!(identities.default_for(&["Shape".into()]), expected, "{providers}");
        }
        let identities = Identities::of(
            &parse_file("struct Shape; fn body() { #[transform] struct Local; }").unwrap(),
            &CfgSet::parse(""),
        );
        assert_eq!(identities.default_for(&["Shape".into()]), None);
    }
}

/// Bounds syntax inspection and alias/export expansion work. Exhaustion withholds nominal
/// evidence (unknown), rather than treating incomplete inspection as proof of presence or absence.
const SYNTAX_WORK_LIMIT: usize = 4096;

/// Caps expanded module paths independently of the number of resolution steps.
const MODULE_PATH_SEGMENT_LIMIT: usize = 4096;

pub(super) fn nominal_attributes(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().all(|attribute| {
        attribute.path().get_ident().is_some_and(|name| {
            matches!(
                name.to_string().as_str(),
                "cfg"
                    | "doc"
                    | "derive"
                    | "repr"
                    | "allow"
                    | "expect"
                    | "warn"
                    | "deny"
                    | "forbid"
                    | "must_use"
                    | "deprecated"
                    | "non_exhaustive"
                    | "inline"
                    | "cold"
                    | "track_caller"
                    | "automatically_derived"
                    | "macro_export"
                    | "path"
                    | "test"
                    | "ignore"
            )
        })
    })
}

/// Resolves only decided conditional attributes; an unresolved expansion is not nominal evidence.
pub(super) fn nominal_expansion(attributes: &[syn::Attribute], cfg: &CfgSet) -> Option<Vec<syn::Attribute>> {
    let mut pending = attributes.to_vec();
    let mut expanded = Vec::new();
    let mut remaining = SYNTAX_WORK_LIMIT;
    while let Some(attribute) = pending.pop() {
        remaining = remaining.checked_sub(1)?;
        if !attribute.path().is_ident("cfg_attr") {
            expanded.push(attribute);
            continue;
        }
        let nested = attribute
            .parse_args_with(syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)
            .ok()?;
        if nested.len() < 2 {
            return
            // #[gamma::skip(return_value.default, reason = "The return type is Option<Vec<Attribute>>; its Default is the same None returned for malformed conditional attributes.", tag = "equivalent")]
            None;
        }
        let mut nested = nested.into_iter();
        let condition =
        // #[gamma::skip(try.propagate_to_unwrap, reason = "The nested.len() >= 2 guard above guarantees that the first element exists before this iterator has been advanced.", tag = "equivalent")]
        nested.next()?;
        match cfg.decide_meta(&condition) {
            Verdict::Yes => pending.extend(nested.map(|meta| syn::parse_quote!(#[#meta]))),
            Verdict::No => {}
            Verdict::Unknown => {
                return
            // #[gamma::skip(return_value.default, reason = "The return type is Option<Vec<Attribute>>; its Default is the same None returned for an undecided expansion.", tag = "equivalent")]
            None;
            }
        }
    }
    Some(expanded)
}

pub(super) fn known_derives(attributes: &[syn::Attribute], paths: &DefaultPaths) -> bool {
    attributes
        .iter()
        .filter(|attribute| attribute.path().is_ident("derive"))
        .all(|attribute| {
            attribute
                .parse_args_with(syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated)
                .is_ok_and(|derives| {
                    derives.iter().all(|path|
                        // #[gamma::skip(logical.and_remove_right, literal.str_to_empty, literal.str_to_xyzzy, reason = "A known built-in Default path is necessarily the unshadowed bare identifier Default in this successfully parsed derive list. That occurrence already makes derives_default(attributes, paths) true; every other known built-in satisfies the left disjunct.", tag = "equivalent")]
                        paths.known_builtin_derive(path) && (!path.is_ident("Default") || derives_default(attributes, paths)))
                })
        })
}

fn nominal_macro(node: &syn::Macro, paths: &DefaultPaths) -> bool {
    if !paths.known_value_macro(&node.path) {
        return
        // #[gamma::skip(return_value.default, reason = "The return type is bool and Default::default() is exactly this false.", tag = "equivalent")]
        false;
    }
    if node.path.segments.last().is_some_and(|segment| segment.ident == "stringify") {
        return true;
    }
    let mut pending = vec![node.tokens.clone()];
    let mut remaining = SYNTAX_WORK_LIMIT;
    while let Some(stream) = pending.pop() {
        let tokens = stream
            .into_iter()
            .take(
                // #[gamma::skip(literal.int_increment, expr.increment, reason = "remaining never exceeds 4096, so both additions are safe. Taking remaining+2 instead of remaining+1 only materializes one extra sentinel token: either the whole stream fits identically, or the unchanged length check returns false before inspecting any token. Only bounded temporary token storage differs.", tag = "resource")]
                remaining + 1,
            )
            .collect::<Vec<_>>();
        if
        // #[gamma::skip(cond.always_false, reason = "If this early overflow check is removed, the extra sentinel token still exhausts checked_sub(1) and returns false before any queued group is visited. No state escapes this pure predicate; only inspection work differs.", tag = "resource")]
        tokens.len() > remaining {
            return
            // #[gamma::skip(return_value.default, reason = "The return type is bool and Default::default() is exactly this false.", tag = "equivalent")]
            false;
        }
        let mut inert_group = None;
        for (index, token) in tokens.iter().enumerate() {
            let Some(next) = remaining.checked_sub(1) else {
                return
                // #[gamma::skip(literal.bool_flip, return_value.default, reason = "The per-stream length check guarantees at least one remaining unit for every token in this loop, so this failure arm is unreachable. The unchanged value is also bool::default().", tag = "equivalent")]
                false;
            };
            remaining = next;
            match token {
                proc_macro2::TokenTree::Group(group) if inert_group != Some(index) => pending.push(group.stream()),
                proc_macro2::TokenTree::Ident(name) if name == "impl" => {
                    return
                // #[gamma::skip(return_value.default, reason = "The return type is bool and Default::default() is exactly this false.", tag = "equivalent")]
                false;
                }
                proc_macro2::TokenTree::Punct(punct) if matches!(punct.as_char(), '#' | '$') => {
                    return
                // #[gamma::skip(return_value.default, reason = "The return type is bool and Default::default() is exactly this false.", tag = "equivalent")]
                false;
                }
                proc_macro2::TokenTree::Punct(punct)
                    if punct.as_char() == '!' && matches!(tokens.get(index + 1), Some(proc_macro2::TokenTree::Group(_))) && index > 0 =>
                {
                    let mut start = index - 1;
                    while (
                        // #[gamma::skip(relational.ge_to_gt, literal.int_increment, reason = "Skipping the start==2 iteration retains only a leading :: in the parsed path instead of stripping it. known_value_macro ignores leading_colon, so both spellings receive the same verdict.", tag = "equivalent")]
                        start >= 2
                    ) && matches!(&tokens[start - 1], proc_macro2::TokenTree::Punct(punct) if punct.as_char() == ':')
                        && matches!(&tokens[start - 2], proc_macro2::TokenTree::Punct(punct) if punct.as_char() == ':')
                    {
                        start -= 2;
                        if start > 0 && matches!(&tokens[start - 1], proc_macro2::TokenTree::Ident(_)) {
                            start -= 1;
                        } else {
                            break;
                        }
                    }
                    let path = tokens[start..index].iter().cloned().collect();
                    if let Ok(path) = syn::parse2::<syn::Path>(path) {
                        if !paths.known_value_macro(&path) {
                            return
                            // #[gamma::skip(return_value.default, reason = "The return type is bool and Default::default() is exactly this false.", tag = "equivalent")]
                            false;
                        }
                        if path.segments.last().is_some_and(|segment| segment.ident == "stringify") {
                            inert_group = Some(index + 1);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    true
}

// Block-local impls also apply crate-wide, but local declarations must not enter the module index.
struct BlockImplementations<'a> {
    paths: &'a DefaultPaths,
    cfg: &'a CfgSet,
    unknown: bool,
}

#[expect(clippy::renamed_function_params, reason = "visitor parameters describe their syntax")]
impl<'ast> Visit<'ast> for BlockImplementations<'_> {
    fn visit_item(&mut self, node: &'ast Item) {
        if !self.cfg.skip_gate(item_attrs(node)) {
            visit::visit_item(self, node);
        }
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        self.unknown |= node.trait_.as_ref().is_some_and(|(path, _)| !self.paths.is_local_trait(path));
        visit::visit_item_impl(self, node);
    }

    fn visit_item_mod(&mut self, _: &'ast syn::ItemMod) {
        self.unknown = true;
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        self.unknown |= !nominal_macro(node, self.paths);
    }

    fn visit_item_macro(&mut self, node: &'ast syn::ItemMacro) {
        if let Some(name) = &node.ident {
            self.unknown |= value_macro_name(&name.to_string());
        } else {
            // #[gamma::skip(stmt.delete_call, reason = "For parsed production blocks, Syn routes anonymous invocations through Stmt::Macro or Expr::Macro directly to visit_macro. Stmt::Item can contain ItemMacro only for a named definition; associated and foreign invocations have distinct visitors, and inline modules stop at visit_item_mod. This anonymous ItemMacro fallback is therefore unreachable here.", tag = "equivalent")]
            self.visit_macro(&node.mac);
        }
    }

    fn visit_attribute(&mut self, node: &'ast syn::Attribute) {
        self.unknown |= nominal_expansion(core::slice::from_ref(node), self.cfg)
            .is_none_or(|attrs| !nominal_attributes(&attrs) || !known_derives(&attrs, self.paths));
    }

    fn visit_block(&mut self, node: &'ast syn::Block) {
        let paths = self.paths.with_items(
            node.stmts.iter().filter_map(|statement| match statement {
                syn::Stmt::Item(item) => Some(item),
                _ => None,
            }),
            self.cfg,
        );
        let mut nested = BlockImplementations {
            paths: &paths,
            cfg: self.cfg,
            unknown: false,
        };
        visit::visit_block(&mut nested, node);
        self.unknown |= nested.unknown;
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct Identities {
    declarations: HashMap<Vec<String>, Option<bool>>,
    links: HashMap<Vec<String>, Option<Vec<String>>>,
    modules: HashSet<Vec<String>>,
    implementations: Vec<(Vec<String>, bool)>,
    generic: HashSet<Vec<String>>,
    enums: HashSet<Vec<String>>,
    exports: HashSet<Vec<String>>,
    complete_modules: HashSet<Vec<String>>,
    opaque_modules: HashSet<Vec<String>>,
    globs: Vec<(Vec<String>, Option<Vec<String>>, bool)>,
    glob_links: HashSet<Vec<String>>,
    pub(super) unexpanded_implementations: bool,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Pending {
    identities: Identities,
}

impl Pending {
    pub(super) fn of(file: &File, cfg: &CfgSet) -> Self {
        let mut identities = Identities::default();
        let _inserted = identities.complete_modules.insert(Vec::new());
        let mut walk = Walk {
            identities,
            module: Vec::new(),
            paths: DefaultPaths::of_in(file, cfg),
            cfg,
            certain: true,
        };
        walk.visit_file(file);
        Self {
            identities: walk.identities,
        }
    }

    pub(super) fn finish(self) -> Identities {
        let mut identities = self.identities;
        identities.finish_globs();
        for (implementation, certain) in identities.implementations.clone() {
            let Some(target) = identities.resolve_path(&implementation) else {
                identities.unexpanded_implementations = true;
                continue;
            };
            if let Some(default) = identities.declarations.get_mut(&target)
                && default.is_some()
            {
                *default = (certain && !identities.generic.contains(&target)).then_some(true);
            }
        }
        identities
    }

    pub(super) fn absorb_at(&mut self, other: &Self, module: &[String]) {
        let identities = &mut self.identities;
        let other = &other.identities;
        let local = |path: &[String]| module.iter().chain(path).cloned().collect::<Vec<_>>();
        let target = |path: &[String]| {
            if path.first().is_some_and(|name| name == "crate") {
                return Some(
                    path[
                // #[gamma::skip(literal.int_decrement, reason = "Sources::bind supplies absorb_at only original per-file Pending entries; composed roots are receivers, never re-absorbed as children. Keeping the leading crate marker thus does not cause a second module rebase. Identities::resolve_path strips that marker before every declaration/link lookup and visited-path check, so finish and subsequent queries resolve the same declaration identities.", tag = "equivalent")]
                1..]
                        .to_vec(),
                );
            }
            let mut prefix = module.to_vec();
            let mut path = path;
            while path.first().is_some_and(|name| name == "super") {
                let _parent = prefix.pop()?;
                path = &path[1..];
            }
            prefix.extend_from_slice(path);
            Some(prefix)
        };
        for (path, default) in &other.declarations {
            let _entry = identities
                .declarations
                .entry(local(path))
                .and_modify(|value| *value = None)
                .or_insert(*default);
        }
        for (path, link) in &other.links {
            let _entry = identities
                .links
                .entry(local(path))
                .and_modify(|value| *value = None)
                .or_insert_with(|| link.as_ref().and_then(|path| target(path)));
        }
        identities.modules.extend(other.modules.iter().map(|path| local(path)));
        identities.enums.extend(other.enums.iter().map(|path| local(path)));
        identities.generic.extend(other.generic.iter().map(|path| local(path)));
        identities.unexpanded_implementations |= other.unexpanded_implementations;
        identities.exports.extend(other.exports.iter().map(|path| local(path)));
        identities
            .complete_modules
            .extend(other.complete_modules.iter().map(|path| local(path)));
        identities
            .opaque_modules
            .extend(other.opaque_modules.iter().map(|path| local(path)));
        identities.globs.extend(
            other
                .globs
                .iter()
                .map(|(module, path, exported)| (local(module), path.as_ref().and_then(|path| target(path)), *exported)),
        );
        identities.implementations.extend(
            other
                .implementations
                .iter()
                .filter_map(|(path, certain)| target(path).map(|path| (path, *certain))),
        );
    }
}

impl From<Identities> for Pending {
    fn from(mut identities: Identities) -> Self {
        for path in identities.glob_links.drain() {
            let _old = identities.links.remove(&path);
        }
        Self { identities }
    }
}

impl Identities {
    fn module_target(&self, path: &[String], work: &mut usize) -> Option<Vec<String>> {
        let mut path = path.to_vec();
        let mut remaining = self.links.len().saturating_add(path.len()).saturating_add(1);
        while remaining > 0 {
            *work = work.checked_sub(1)?;
            if path.len() > MODULE_PATH_SEGMENT_LIMIT {
                return
                // #[gamma::skip(return_value.default, reason = "The return type is Option<Vec<String>> and its Default is exactly this None.", tag = "equivalent")]
                None;
            }
            remaining -= 1;
            if matches!(path.first().map(String::as_str), Some("crate" | "self")) {
                let _root = path.remove(0);
            }
            if self.complete_modules.contains(&path) {
                return Some(path);
            }
            let end = (
                // #[gamma::skip(literal.int_decrement, reason = "Every production link key comes from Walk::key, which appends a binding name, or from glob materialization, which also appends a name. Absorption only prefixes those nonempty keys, so an added empty-prefix lookup cannot match.", tag = "equivalent")]
                1..=path.len()
            )
                .rev()
                .find(|end| self.links.contains_key(&path[..*end]))?;
            let target =
            // #[gamma::skip(try.propagate_to_unwrap, reason = "find above selected end only after contains_key on this same immutable links map. The following get cannot fail; an unknown Some(None) target is still handled by the separate as_ref()? below.", tag = "equivalent")]
            self.links.get(&path[..end])?;
            let target = target.as_ref()?.clone();
            let _old = path.splice(..end, target);
        }
        None
    }

    /// Materializes only exports from fully indexed local modules. Unknown glob sources poison
    /// their whole import set: not finding a name there is not evidence that it is absent.
    fn finish_globs(&mut self) {
        if
        // #[gamma::skip(cond.always_false, reason = "For an empty glob set every following loop is empty, both fixed-point loops exit immediately, and no link or glob_link is written. Removing this shortcut only performs temporary clones.", tag = "resource")]
        self.globs.is_empty() {
            return;
        }
        let mut remaining = SYNTAX_WORK_LIMIT;
        let mut opaque = self.opaque_modules.clone();
        let mut globs = Vec::new();
        for (module, target, exported) in &self.globs {
            if let Some(target) = target.as_ref().and_then(|path| self.module_target(path, &mut remaining)) {
                globs.push((module.clone(), target, *exported));
            } else {
                let _inserted = opaque.insert(module.clone());
            }
            if
            // #[gamma::skip(cond.always_false, literal.int_increment, reason = "All publication is deferred until after opacity and candidate inspection. With zero work, the next nonempty opacity scan returns; with one unit, any publishable binding still requires an opacity step plus a candidate step. Empty or wholly opaque scans publish nothing. Thus returning at zero, at one, or at the next exhausted scan preserves the same unchanged index.", tag = "equivalent")]
            remaining == 0 {
                return;
            }
        }
        globs.sort();
        loop {
            let mut changed = false;
            for (module, target, _) in &globs {
                let Some(next) = remaining.checked_sub(1) else { return };
                remaining = next;
                if opaque.contains(target) {
                    changed |= opaque.insert(module.clone());
                }
            }
            if !changed {
                break;
            }
        }
        let direct = |path: &[String]| self.declarations.contains_key(path) || self.links.contains_key(path) || self.modules.contains(path);
        let mut exports = self.exports.clone();
        loop {
            let mut additions = Vec::new();
            for (module, target, exported) in &globs {
                if !exported || opaque.contains(module) {
                    continue;
                }
                for path in &exports {
                    if path.len() == target.len() + 1 && path.starts_with(target) {
                        let mut introduced = module.clone();
                        introduced.push(path[target.len()].clone());
                        if !direct(&introduced) && !exports.contains(&introduced) {
                            additions.push(introduced);
                        }
                    }
                    let Some(next) = remaining.checked_sub(1) else { return };
                    remaining = next;
                }
            }
            if additions.is_empty() {
                break;
            }
            exports.extend(additions);
        }
        let mut candidates: HashMap<Vec<String>, Vec<Vec<String>>> = HashMap::default();
        for (module, target, _) in &globs {
            if opaque.contains(module) {
                continue;
            }
            for path in &exports {
                if path.len() == target.len() + 1 && path.starts_with(target) {
                    let mut introduced = module.clone();
                    introduced.push(path[target.len()].clone());
                    if !direct(&introduced) {
                        candidates.entry(introduced).or_default().push(path.clone());
                    }
                }
                let Some(next) = remaining.checked_sub(1) else { return };
                remaining = next;
            }
        }
        // Unique bindings first allow duplicate reexports of the same declaration to converge.
        for (path, targets) in &candidates {
            if let [target] = targets.as_slice() {
                let _old = self.links.insert(path.clone(), Some(target.clone()));
                let _inserted = self.glob_links.insert(path.clone());
            }
        }
        let ambiguous = candidates
            .into_iter()
            .filter(|(_, targets)| targets.len() > 1)
            .map(|(path, targets)| {
                let mut resolved = targets.iter().map(|target| self.resolve_path(target));
                let first = resolved.next().flatten();
                let target = first.filter(|first| resolved.all(|target| target.as_ref() == Some(first)));
                (path, target)
            })
            .collect::<Vec<_>>();
        for (path, target) in ambiguous {
            let _old = self.links.insert(path.clone(), target);
            let _inserted = self.glob_links.insert(path);
        }
    }

    pub(super) fn of(file: &File, cfg: &CfgSet) -> Self {
        Pending::of(file, cfg).finish()
    }

    pub(super) fn is_enum(&self, path: &[String]) -> bool {
        self.resolve_path(path).is_some_and(|path| self.enums.contains(&path))
    }

    pub(super) fn default_for(&self, path: &[String]) -> Option<bool> {
        let path = self.resolve_path(path)?;
        self.declarations
            .get(&path)
            .copied()
            .flatten()
            .filter(|default| *default || !self.unexpanded_implementations)
    }

    pub(super) fn local_module(&self, module: &[String], name: &str) -> bool {
        let mut path = module.to_vec();
        path.push(name.to_owned());
        self.modules.contains(&path)
    }

    pub(super) fn resolve_path(&self, path: &[String]) -> Option<Vec<String>> {
        let mut path = path.to_vec();
        if matches!(path.first().map(String::as_str), Some("crate" | "self")) {
            let _root = path.remove(0);
        }
        let mut visited = HashSet::default();
        let mut remaining = self.links.len().saturating_add(path.len()).saturating_add(1);
        loop {
            if path.first().is_some_and(|name| name == "crate") {
                let _root = path.remove(0);
            }
            if
            // #[gamma::skip(logical.or_remove_right, reason = "Identities::resolve_path retains its finite decreasing budget and visited-path guard. Removing the visited check still returns None at the budget for cycles; permitting one extra unchanged iteration after no link matches immediately hits visited (or the budget) and returns the same None. Positive declaration resolutions and chosen link replacements are unchanged.", tag = "equivalent")]
            remaining == 0 || !visited.insert(path.clone()) {
                return
                // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
                None;
            }
            // #[gamma::skip(all, reason = "this decreasing budget bounds alias cycles whose expanded paths never repeat")]
            remaining -= 1;
            if self.declarations.contains_key(&path) {
                return Some(path);
            }
            let mut replaced =
            // #[gamma::skip(literal.bool_flip, reason = "Identities::resolve_path retains its finite decreasing budget and visited-path guard. Removing the visited check still returns None at the budget for cycles; permitting one extra unchanged iteration after no link matches immediately hits visited (or the budget) and returns the same None. Positive declaration resolutions and chosen link replacements are unchanged.", tag = "equivalent")]
            false;
            for end in (
                // #[gamma::skip(literal.int_decrement, reason = "Every link key is Walk::key(name), which always appends a name (even an empty introduced name), and absorb_at prefixes that nonempty key. The added empty-prefix lookup therefore cannot find a link and changes no resolution.", tag = "equivalent")]
                1..=path.len()
            )
                .rev()
            {
                if let Some(target) = self.links.get(&path[..end]) {
                    let _old = path.splice(..end, target.as_ref()?.iter().cloned());
                    replaced = true;
                    break;
                }
            }
            if
            // #[gamma::skip(cond.always_false, reason = "Identities::resolve_path retains its finite decreasing budget and visited-path guard. Removing the visited check still returns None at the budget for cycles; permitting one extra unchanged iteration after no link matches immediately hits visited (or the budget) and returns the same None. Positive declaration resolutions and chosen link replacements are unchanged.", tag = "equivalent")]
            !replaced {
                return
                // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
                None;
            }
        }
    }
}

struct Walk<'a> {
    identities: Identities,
    module: Vec<String>,
    paths: DefaultPaths,
    cfg: &'a CfgSet,
    certain: bool,
}

impl Walk<'_> {
    fn key(&self, name: String) -> Vec<String> {
        let mut key = self.module.clone();
        key.push(name);
        key
    }

    fn target(&self, mut names: Vec<String>) -> Option<Vec<String>> {
        let mut prefix = self.module.clone();
        match names.first()?.as_str() {
            "crate" => {
                return Some(names);
            }
            "self" => {
                let _root = names.remove(0);
            }
            "super" => {
                while names.first().is_some_and(|name| name == "super") {
                    if prefix.last().is_none_or(|name| name == "super") {
                        prefix.push("super".to_owned());
                    } else {
                        let _parent = prefix.pop();
                    }
                    let _root = names.remove(0);
                }
            }
            _ => {}
        }
        prefix.extend(names);
        Some(prefix)
    }

    fn link(&mut self, tree: &UseTree, prefix: &[String], exported: bool) {
        match tree {
            UseTree::Path(path) => {
                let mut prefix = prefix.to_vec();
                prefix.push(path.ident.to_string());
                self.link(&path.tree, &prefix, exported);
            }
            UseTree::Name(name) => {
                let mut target = prefix.to_vec();
                if name.ident != "self" {
                    target.push(name.ident.to_string());
                }
                let introduced = target.last().cloned().unwrap_or_default();
                if exported {
                    let _inserted = self.identities.exports.insert(self.key(introduced.clone()));
                }
                self.insert_link(introduced, target);
            }
            UseTree::Rename(rename) => {
                let mut target = prefix.to_vec();
                if rename.ident != "self" {
                    target.push(rename.ident.to_string());
                }
                if exported {
                    let _inserted = self.identities.exports.insert(self.key(rename.rename.to_string()));
                }
                self.insert_link(rename.rename.to_string(), target);
            }
            UseTree::Group(group) => {
                for tree in &group.items {
                    self.link(tree, prefix, exported);
                }
            }
            UseTree::Glob(_) => {}
        }
    }

    fn glob(&mut self, tree: &UseTree, prefix: &[String], local: bool, exported: bool) {
        match tree {
            UseTree::Path(path) => {
                let mut prefix = prefix.to_vec();
                prefix.push(path.ident.to_string());
                self.glob(&path.tree, &prefix, local, exported);
            }
            UseTree::Group(group) => {
                for tree in &group.items {
                    self.glob(tree, prefix, local, exported);
                }
            }
            UseTree::Glob(_) => {
                let target = local.then(|| self.target(prefix.to_vec())).flatten();
                self.identities.globs.push((self.module.clone(), target, exported));
            }
            UseTree::Name(_) | UseTree::Rename(_) => {}
        }
    }

    fn insert_link(&mut self, name: String, target: Vec<String>) {
        let key = self.key(name);
        let target = self.certain.then(|| self.target(target)).flatten();
        let _entry = self.identities.links.entry(key).and_modify(|value| *value = None).or_insert(target);
    }

    fn declaration(&mut self, name: String, attrs: &[syn::Attribute], generic: bool) {
        let key = self.key(name);
        if generic {
            let _inserted = self.identities.generic.insert(key.clone());
        }
        let attrs = nominal_expansion(attrs, self.cfg);
        let default = attrs
            .as_ref()
            .filter(|attrs| {
                (
                    // #[gamma::skip(logical.and_remove_right, reason = "Walk::visit_item sets self.certain only when this item's cfg is guaranteed and its nominal expansion exists with nominal attributes. These two repeated right-hand predicates therefore cannot reject a declaration while self.certain is true.", tag = "equivalent")]
                    self.certain && self.cfg.guaranteed_for(attrs) && nominal_attributes(attrs)
                ) && known_derives(attrs, &self.paths)
            })
            .map(|attrs| derives_default(attrs, &self.paths))
            .filter(|derived| !generic || !derived);
        let _entry = self
            .identities
            .declarations
            .entry(key)
            .and_modify(|value| *value = None)
            .or_insert(default);
    }
}

#[expect(clippy::renamed_function_params, reason = "visitor parameters describe their syntax")]
impl<'ast> Visit<'ast> for Walk<'_> {
    fn visit_item(&mut self, node: &'ast Item) {
        if !self.cfg.skip_gate(item_attrs(node)) {
            let expands = nominal_expansion(item_attrs(node), self.cfg)
                .is_none_or(|attrs| !nominal_attributes(&attrs) || !known_derives(&attrs, &self.paths))
                || matches!(node, Item::Macro(mac) if mac.ident.is_none());
            self.identities.unexpanded_implementations |= expands;
            if !self.cfg.guaranteed_for(item_attrs(node)) || expands {
                let _inserted = self.identities.opaque_modules.insert(self.module.clone());
            }
            let exported = match node {
                Item::Struct(item) => Some((&item.vis, item.ident.to_string())),
                Item::Enum(item) => Some((&item.vis, item.ident.to_string())),
                Item::Union(item) => Some((&item.vis, item.ident.to_string())),
                Item::Type(item) => Some((&item.vis, item.ident.to_string())),
                Item::Mod(item) => Some((&item.vis, item.ident.to_string())),
                _ => None,
            };
            if let Some((visibility, name)) = exported
                && !matches!(visibility, syn::Visibility::Inherited)
            {
                let _inserted = self.identities.exports.insert(self.key(name));
            }
            let certain = self.certain;
            self.certain &= self.cfg.guaranteed_for(item_attrs(node))
                && nominal_expansion(item_attrs(node), self.cfg).is_some_and(|attrs| nominal_attributes(&attrs));
            visit::visit_item(self, node);
            self.certain = certain;
        }
    }

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        let module = self.key(node.ident.to_string());
        let _inserted = self.identities.modules.insert(module);
        if let Some((_, items)) = &node.content {
            let _inserted = self.identities.complete_modules.insert(self.key(node.ident.to_string()));
            let certain = self.certain;
            // #[gamma::skip(stmt.delete_assign, reason = "Walk::visit_item has already conjoined these same cfg and nominal-attribute predicates before dispatching this module. Repeating the conjunction cannot change self.certain.", tag = "equivalent")]
            self.certain &=
                // #[gamma::skip(logical.and_remove_left, logical.and_remove_right, logical.and_to_or, reason = "On entry from Walk::visit_item, self.certain implies both predicates. If it is false, this &= remains false for every RHS; if true, both RHS predicates are true.", tag = "equivalent")]
                self.cfg.guaranteed_for(&node.attrs)
                    && nominal_expansion(&node.attrs, self.cfg).is_some_and(|attrs| nominal_attributes(&attrs));
            if !self.certain {
                let _inserted = self.identities.opaque_modules.insert(self.key(node.ident.to_string()));
            }
            let mut file = syn::parse_file("").expect("empty Rust source always parses");
            file.items.clone_from(items);
            let paths = core::mem::replace(&mut self.paths, DefaultPaths::of_in(&file, self.cfg));
            self.module.push(node.ident.to_string());
            for item in items {
                self.visit_item(item);
            }
            let _module = self.module.pop();
            self.paths = paths;
            // #[gamma::skip(stmt.delete_assign, reason = "The enclosing Walk::visit_item restores its saved certainty immediately after this module callback returns; no intervening operation observes this duplicate restoration.", tag = "equivalent")]
            self.certain = certain;
        }
    }

    fn visit_item_struct(&mut self, node: &'ast syn::ItemStruct) {
        self.declaration(node.ident.to_string(), &node.attrs, !node.generics.params.is_empty());
        visit::visit_item_struct(self, node);
    }

    fn visit_item_enum(&mut self, node: &'ast syn::ItemEnum) {
        if nominal_expansion(&node.attrs, self.cfg).is_some_and(|attrs| self.cfg.guaranteed_for(&attrs) && nominal_attributes(&attrs)) {
            let _added = self.identities.enums.insert(self.key(node.ident.to_string()));
        }
        self.declaration(node.ident.to_string(), &node.attrs, !node.generics.params.is_empty());
        visit::visit_item_enum(self, node);
    }

    fn visit_item_union(&mut self, node: &'ast syn::ItemUnion) {
        self.declaration(node.ident.to_string(), &node.attrs, !node.generics.params.is_empty());
        visit::visit_item_union(self, node);
    }

    fn visit_item_use(&mut self, node: &'ast syn::ItemUse) {
        let exported = !matches!(node.vis, syn::Visibility::Inherited);
        if node.leading_colon.is_none() {
            self.link(&node.tree, &[], exported);
        }
        self.glob(&node.tree, &[], node.leading_colon.is_none() && self.certain, exported);
    }

    fn visit_item_type(&mut self, node: &'ast syn::ItemType) {
        if node.generics.params.is_empty()
            && let Type::Path(path) = &*node.ty
            && path.qself.is_none()
            && path.path.leading_colon.is_none()
        {
            self.insert_link(
                node.ident.to_string(),
                path.path.segments.iter().map(|segment| segment.ident.to_string()).collect(),
            );
        }
        visit::visit_item_type(self, node);
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        if let Some((path, _)) = &node.trait_
            && self.paths.is_standard_trait(path)
            && let Type::Path(ty) = &*node.self_ty
            && let Some(target) = self.target(ty.path.segments.iter().map(|segment| segment.ident.to_string()).collect())
        {
            let certain = (
                // #[gamma::skip(logical.and_remove_right, reason = "Walk::visit_item already conjoins this impl's guaranteed cfg and nominal expansion into self.certain before dispatch. The repeated right-hand attribute predicates cannot change the result.", tag = "equivalent")]
                self.certain
                    && self.cfg.guaranteed_for(&node.attrs)
                    && nominal_expansion(&node.attrs, self.cfg).is_some_and(|attrs| nominal_attributes(&attrs))
            ) && node.generics.params.is_empty()
                && node.generics.where_clause.is_none();
            self.identities.implementations.push((target, certain));
        }
        visit::visit_item_impl(self, node);
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        self.identities.unexpanded_implementations |= !nominal_macro(node, &self.paths);
    }

    // #[gamma::skip(fn_value.unit, reason = "Walk reaches ItemMacro through visit_item, which has already marked every unnamed macro invocation unexpanded. Named macro definitions intentionally do nothing; this callback therefore cannot change the accumulated flag.", tag = "equivalent")]
    fn visit_item_macro(&mut self, node: &'ast syn::ItemMacro) {
        if
        // #[gamma::skip(cond.always_false, reason = "For every unnamed ItemMacro invocation, the enclosing Walk::visit_item already set unexpanded_implementations=true before dispatching here.", tag = "equivalent")]
        node.ident.is_none() {
            // #[gamma::skip(stmt.delete_call, reason = "For every unnamed ItemMacro invocation, the enclosing Walk::visit_item already set unexpanded_implementations=true. visit_macro only ORs into that flag, so this call cannot change it.", tag = "equivalent")]
            self.visit_macro(&node.mac);
        }
    }

    fn visit_block(&mut self, node: &'ast syn::Block) {
        let mut implementations = BlockImplementations {
            paths: &self.paths,
            cfg: self.cfg,
            unknown: false,
        };
        implementations.visit_block(node);
        self.identities.unexpanded_implementations |= implementations.unknown;
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use syn::{Attribute, parse_file, parse_quote, parse_str};

    use super::{Identities, Pending, nominal_expansion, nominal_macro};
    use crate::cfg::CfgSet;
    use crate::ops::collect::Defaults;
    use crate::ops::collect::defaults::DefaultPaths;
    use crate::{HashMap, HashSet};

    #[test]
    fn incomplete_conditional_attributes_and_exhausted_expansions_withhold_evidence() {
        let cfg = CfgSet::parse("");
        for attribute in [
            parse_quote!(#[cfg_attr()]),
            parse_quote!(#[cfg_attr(all())]),
            parse_quote!(#[cfg_attr(all(), derive(Default) unexpected)]),
        ] {
            assert_eq!(nominal_expansion(&[attribute], &cfg), None);
        }
        let attribute: Attribute = parse_quote!(#[doc = "inert"]);
        assert_eq!(nominal_expansion(&vec![attribute.clone(); 4096], &cfg).unwrap().len(), 4096);
        assert_eq!(nominal_expansion(&vec![attribute; 4097], &cfg), None);
    }

    #[test]
    fn absorbed_opaque_modules_prevent_globs_from_publishing_partial_nominal_evidence() {
        let cfg = CfgSet::parse("").with_undecided(["maybe".into()]);
        let mut parent = Pending::of(&parse_file("mod child; use crate::child::c::*;").unwrap(), &cfg);
        let child = Pending::of(
            &parse_file("pub mod c { pub struct Shape; #[cfg(maybe)] pub struct Unknown; }").unwrap(),
            &cfg,
        );
        parent.absorb_at(&child, &["child".into()]);
        let identities = parent.finish();
        assert_eq!(identities.default_for(&["Shape".into()]), None);
        assert_eq!(identities.default_for(&["child".into(), "c".into(), "Shape".into()]), Some(false));
    }

    #[test]
    fn absolute_globs_cannot_borrow_a_same_named_local_module() {
        let cfg = CfgSet::parse("");
        for (prefix, expected) in [("", Some(true)), ("::", None)] {
            let source = format!("mod core {{ #[derive(Default)] pub struct Shape; }} use {prefix}core::*;");
            let identities = Identities::of(&parse_file(&source).unwrap(), &cfg);
            assert_eq!(identities.default_for(&["Shape".into()]), expected, "{source}");
        }
    }

    #[test]
    fn nominal_macros_distinguish_inert_stringification_from_nested_expansions() {
        let paths = DefaultPaths::default();
        for (source, expected) in [
            ("stringify!(impl Default for Shape { #[unknown] $tokens })", true),
            ("format!(\"{}\", ::core::stringify!(impl Default for Shape {}))", true),
            ("vec![::alloc::vec![1, 2]]", true),
            ("vec![::println!(\"inert\")]", true),
            ("vec![::external::expand!()]", false),
            ("vec![!{}]", true),
            ("vec![unknown {}]", true),
            ("vec![!unknown]", true),
            ("vec![value & { 0 }]", true),
            ("vec![value != 0]", true),
            ("vec![Record { field: stringify!(impl Default for Shape {}) }]", true),
            ("vec![:stringify!(tokens)]", true),
            ("vec![prefix:stringify!(impl Default for Shape {})]", true),
            ("vec![0:stringify!(impl Default for Shape {})]", true),
            ("vec![prefix:field stringify!(impl Default for Shape {})]", true),
            ("vec![stringify!($tokens)]", true),
            ("vec![::custom::stringify!(impl Default for Shape {})]", false),
            ("vec![{ #[unknown] struct Local; 0 }]", false),
            ("vec![$tokens]", false),
        ] {
            assert_eq!(nominal_macro(&parse_str(source).unwrap(), &paths), expected, "{source}");
        }
    }

    #[test]
    fn nominal_macro_work_is_bounded_across_flat_and_nested_token_streams() {
        let paths = DefaultPaths::default();
        for (count, expected) in [(2048, true), (2049, false)] {
            let arguments = vec!["0"; count].join(",");
            for source in [format!("vec![{arguments}]"), format!("vec![{{{arguments}}}]")] {
                assert_eq!(nominal_macro(&parse_str(&source).unwrap(), &paths), expected, "{count}");
            }
        }
        for (count, expected) in [(1022, true), (1023, false)] {
            let arguments = vec!["0"; count].join(",");
            let source = format!("vec![vec![{arguments}], vec![{arguments}]]");
            assert_eq!(nominal_macro(&parse_str(&source).unwrap(), &paths), expected, "{count}");
        }
    }

    #[test]
    fn block_local_macro_declarations_only_poison_recognized_builtin_names() {
        for (body, expected) in [
            ("macro_rules! uncalled { () => { impl Default for Shape {} }; }", Some(false)),
            ("macro_rules! vec { () => { impl Default for Shape {} }; }", None),
            ("expand!();", None),
            ("#[cfg(any())] impl Default for Shape {}", Some(false)),
            ("mod nested {}", None),
        ] {
            let source = format!("struct Shape; fn body() {{ {body} }}");
            let identities = Identities::of(&parse_file(&source).unwrap(), &CfgSet::parse(""));
            assert_eq!(identities.default_for(&["Shape".into()]), expected, "{body}");
        }
    }

    #[test]
    fn module_resolution_normalizes_roots_and_bounds_cycles_and_path_growth() {
        let identities = Identities {
            complete_modules: HashSet::from_iter([vec!["target".into()]]),
            links: HashMap::from_iter([(vec!["cycle".into()], Some(vec!["cycle".into(), "child".into()]))]),
            ..Identities::default()
        };
        for root in ["self", "crate"] {
            let mut work = 1;
            assert_eq!(
                identities.module_target(&[root.into(), "target".into()], &mut work),
                Some(vec!["target".into()])
            );
            assert_eq!(work, 0);
            assert_eq!(identities.module_target(&["target".into()], &mut work), None);
        }
        let mut work = 8;
        assert_eq!(identities.module_target(&["cycle".into()], &mut work), None);
        assert_eq!(work, 5);
        assert_eq!(identities.module_target(&vec!["target".into(); 4097], &mut work), None);
        assert_eq!(work, 4);
    }

    #[test]
    fn module_path_length_limit_is_inclusive_even_for_indexed_targets() {
        let boundary = vec!["segment".to_owned(); 4096];
        let excessive = vec!["segment".to_owned(); 4097];
        let identities = Identities {
            complete_modules: HashSet::from_iter([boundary.clone(), excessive.clone()]),
            ..Identities::default()
        };
        let mut work = 2;
        assert_eq!(identities.module_target(&boundary, &mut work), Some(boundary));
        assert_eq!(work, 1);
        assert_eq!(identities.module_target(&excessive, &mut work), None);
        assert_eq!(work, 0);
    }

    #[test]
    fn glob_work_limit_publishes_only_fully_inspected_bindings_at_the_exact_boundary() {
        for (export_count, expected) in [(4094, Some(false)), (4095, None)] {
            let mut exports = HashSet::from_iter([vec!["source".into(), "Shape".into()]]);
            exports.extend((1..export_count).map(|index| vec![format!("unrelated{index}")]));
            let mut identities = Identities {
                declarations: HashMap::from_iter([(vec!["source".into(), "Shape".into()], Some(false))]),
                complete_modules: HashSet::from_iter([vec!["source".into()]]),
                exports,
                globs: vec![(vec!["facade".into()], Some(vec!["source".into()]), false)],
                ..Identities::default()
            };
            identities.finish_globs();
            assert_eq!(
                identities.default_for(&["facade".into(), "Shape".into()]),
                expected,
                "{export_count}"
            );
            assert_eq!(identities.default_for(&["source".into(), "Shape".into()]), Some(false));
        }
    }

    #[test]
    fn glob_resolution_exhaustion_does_not_publish_partial_export_evidence() {
        for (count, expected) in [(1, Some(false)), (4096, None)] {
            let mut identities = Identities {
                declarations: HashMap::from_iter([(vec!["source".into(), "Shape".into()], Some(false))]),
                complete_modules: HashSet::from_iter([vec!["source".into()]]),
                exports: HashSet::from_iter([vec!["source".into(), "Shape".into()]]),
                globs: vec![(vec!["facade".into()], Some(vec!["source".into()]), true); count],
                ..Identities::default()
            };
            identities.finish_globs();
            assert_eq!(identities.default_for(&["facade".into(), "Shape".into()]), expected, "{count}");
            assert_eq!(identities.default_for(&["source".into(), "Shape".into()]), Some(false), "{count}");
        }
    }

    #[test]
    fn duplicate_labeled_declarations_and_aliases_cannot_become_default_proof() {
        let cfg = CfgSet::unconditional();
        let mut root = Pending::of(&parse_file("#[derive(Default)] struct Outer;").unwrap(), &cfg);
        for source in [
            "struct Shape; type Alias = Shape; impl Default for Shape {}",
            "#[derive(Default)] struct Shape; type Alias = Shape;",
        ] {
            root.absorb_at(&Pending::of(&parse_file(source).unwrap(), &cfg), &["child".into()]);
        }
        let identities = root.finish();
        assert_eq!(identities.default_for(&["Outer".into()]), Some(true));
        assert_eq!(identities.default_for(&["child".into(), "Shape".into()]), None);
        assert_eq!(identities.default_for(&["child".into(), "Alias".into()]), None);
    }

    #[test]
    fn p17_decided_cfg_attributes_keep_nominal_evidence_and_unknown_expansions_do_not() {
        let cfg = CfgSet::parse("").with_undecided(["custom".into()]);
        for (attributes, expected) in [
            ("#[cfg_attr(any(), derive(Default))]", Some(false)),
            ("#[cfg_attr(any(), transform)]", Some(false)),
            ("#[cfg_attr(all(), doc = \"nominal\")]", Some(false)),
            ("#[cfg_attr(all(), cfg_attr(any(), transform), derive(Default))]", Some(true)),
            ("#[cfg_attr(feature = \"absent\", derive(Default))]", Some(false)),
            ("#[cfg_attr(custom, derive(Default))]", None),
            ("#[cfg_attr(all(), transform)]", None),
            ("#[cfg_attr(all(), derive(Expand))]", None),
            ("#[cfg(custom)]", None),
        ] {
            let source = format!("{attributes} struct Shape;");
            let identities = Identities::of(&parse_file(&source).unwrap(), &cfg);
            assert_eq!(identities.default_for(&["Shape".into()]), expected, "{source}");
        }
        let identities = Identities::of(&parse_file("#[cfg(custom)] mod conditional { pub struct Shape; }").unwrap(), &cfg);
        assert_eq!(identities.default_for(&["conditional".into(), "Shape".into()]), None);
    }

    #[test]
    fn p17_conditional_implementations_are_unknown_instead_of_positive_or_negative() {
        for implementation in [
            "#[cfg(custom)] impl Default for Shape {}",
            "impl Default for Shape where u8: Marker {}",
            "#[cfg_attr(custom, transform)] impl Default for Shape {}",
        ] {
            let source = format!("struct Shape; {implementation}");
            let identities = Identities::of(&parse_file(&source).unwrap(), &CfgSet::parse("").with_undecided(["custom".into()]));
            assert_eq!(identities.default_for(&["Shape".into()]), None, "{source}");
        }
        let identities = Identities::of(
            &parse_file("struct Shape; #[cfg(any())] impl Default for Shape {}").unwrap(),
            &CfgSet::parse(""),
        );
        assert_eq!(identities.default_for(&["Shape".into()]), Some(false));
    }

    #[test]
    fn p17_local_glob_reexports_preserve_canonical_identity_and_named_precedence() {
        let source = "
            mod options {
                pub(crate) struct ProbesOptions;
                #[derive(Default)] pub struct Ready;
                struct Private;
            }
            mod facade { pub(crate) use super::options::*; }
            pub(crate) use facade::*;
            mod duplicate { pub use crate::options::ProbesOptions; }
            use duplicate::*;
            mod shadow { pub struct Ready; }
            use shadow::Ready;
            impl ::core::default::Default for facade::ProbesOptions {}";
        let identities = Identities::of(&parse_file(source).unwrap(), &CfgSet::parse("").with_undecided(["custom".into()]));
        for (name, canonical, default) in [
            ("ProbesOptions", vec!["options".into(), "ProbesOptions".into()], Some(true)),
            ("Ready", vec!["shadow".into(), "Ready".into()], Some(false)),
        ] {
            assert_eq!(identities.resolve_path(&[name.into()]), Some(canonical), "{name}");
            assert_eq!(identities.default_for(&[name.into()]), default, "{name}");
        }
        assert_eq!(identities.default_for(&["facade".into(), "Ready".into()]), Some(true));
        assert_eq!(identities.default_for(&["Private".into()]), None);
    }

    #[test]
    fn p17_unknown_ambiguous_and_macro_generated_glob_sources_remain_unknown() {
        for source in [
            "mod known { pub struct Shape; } use known::*; use dependency::*;",
            "mod known { pub struct Shape; } use known::*; use ::dependency::*;",
            "mod known { pub struct Shape; } mod absent; use known::*; use absent::*;",
            "mod first { pub struct Shape; } mod second { pub struct Shape; } use first::*; use second::*;",
            "mod known { pub struct Shape; expand!(); } use known::*;",
            "mod known { pub struct Shape; } #[cfg(custom)] use known::*;",
            "mod first { pub use super::second::*; } mod second { pub use super::first::*; } use first::*;",
            "mod first { pub struct Shape; pub use external::*; } mod second { pub use super::first::*; } use second::*;",
        ] {
            let identities = Identities::of(&parse_file(source).unwrap(), &CfgSet::parse(""));
            assert_eq!(identities.default_for(&["Shape".into()]), None, "{source}");
        }
        for source in [
            "struct Shape; implement_default!(Shape);",
            "struct Shape; mod other { implement_default!(crate::Shape); }",
            "struct Shape; #[derive(Expand)] struct Other;",
            "struct Shape; use custom::Debug; #[derive(Debug)] struct Other;",
            "struct Shape; use dependency::*; #[derive(Debug)] struct Other;",
            "use core::default::Default as Debug; #[derive(Debug)] struct Shape;",
            "mod core { pub mod fmt { pub use custom::Debug; } } use core::fmt::Debug; #[derive(Debug)] struct Shape;",
            "struct Shape; impl Default for UnresolvedAlias {}",
            "struct Shape; fn body() { implement_default!(Shape); }",
            "struct Shape; fn body() { impl Default for Shape {} }",
            "struct Shape; fn body() { use custom::vec; vec![]; }",
            "struct Shape; macro_rules! vec { () => { impl Default for Shape {} }; } fn body() { vec!(); }",
            "struct Shape; fn body() { vec![{ impl Default for Shape {} }]; }",
            "struct Shape; #[macro_use] extern crate custom; fn body() { vec![]; }",
        ] {
            let identities = Identities::of(&parse_file(source).unwrap(), &CfgSet::parse(""));
            assert_eq!(identities.default_for(&["Shape".into()]), None, "{source}");
        }
        let identities = Identities::of(
            &parse_file("struct Shape; macro_rules! uncalled { () => { impl Default for Shape {} }; }").unwrap(),
            &CfgSet::parse(""),
        );
        assert_eq!(identities.default_for(&["Shape".into()]), Some(false));
        let identities = Identities::of(
            &parse_file("struct Shape; fn body() { let _ = vec![1, 2]; assert_eq!(1, 1); }").unwrap(),
            &CfgSet::parse(""),
        );
        assert_eq!(identities.default_for(&["Shape".into()]), Some(false));
    }

    #[test]
    fn unrelated_block_items_and_nested_builtin_macros_do_not_erase_default_absence() {
        for body in [
            "use dependency::*;",
            "use dependency::vec;",
            "struct Local; impl Local { fn value(&self) {} }",
            "trait Marker {} impl Marker for Shape {}",
            "trait Marker<T> {} impl<T> Marker<T> for Shape {}",
            "trait Default {} impl Default for Shape {}",
            "#[derive(Debug, Clone, Default)] struct Local;",
            "assert!(!false); assert_ne!(1, 2);",
            "assert!(matches!(Some(1), Some(_)));",
            "let _ = format!(\"{}\", core::stringify!(impl Default for Shape {}));",
            "let _ = vec![vec![1, 2], vec![3]];",
            "if !(false) { assert!(true); }",
        ] {
            let source = format!("struct Shape; fn body() {{ {body} }}");
            let identities = Identities::of(&parse_file(&source).unwrap(), &CfgSet::parse(""));
            assert_eq!(identities.default_for(&["Shape".into()]), Some(false), "{source}");
        }
    }

    #[test]
    fn block_scopes_keep_real_default_providers_and_unknown_nested_expansions_open() {
        for body in [
            "use core::default::Default as D; impl D for Shape {}",
            "use dependency::*; vec![];",
            "use dependency::vec; vec![];",
            "assert!(dependency::vec![]);",
            "assert!({ impl Default for Shape {} true });",
            "assert!(matches!({ implement_default!(Shape); true }, true));",
            "#[derive(Expand)] struct Local;",
            "mod nested { use core::default::Default as Marker; impl Marker for crate::Shape {} }",
            "trait Marker {} { use core::default::Default as Marker; impl Marker for Shape {} }",
            "trait Marker {} { use dependency::Provider as Marker; impl Marker for Shape {} }",
            "trait Marker {} { use dependency::*; impl Marker for Shape {} }",
            "struct Local; impl Local { fn value() { implement_default!(Shape); } }",
        ] {
            let source = format!("struct Shape; fn body() {{ {body} }}");
            let identities = Identities::of(&parse_file(&source).unwrap(), &CfgSet::parse(""));
            assert_eq!(identities.default_for(&["Shape".into()]), None, "{source}");
        }
    }

    #[test]
    fn longest_alias_prefix_wins_and_shorter_value_aliases_cannot_redirect_its_target() {
        let file = parse_file(
            "mod original { pub fn value() {} }
             use original::value as a;
             mod a {
                 pub struct Target;
                 pub mod nested { pub use super::Target as Exported; }
             }",
        )
        .unwrap();
        let identities = Identities::of(&file, &CfgSet::unconditional());
        assert_eq!(
            identities.resolve_path(&["a".into(), "nested".into(), "Exported".into()]),
            Some(vec!["a".into(), "Target".into()])
        );
    }

    #[test]
    fn growing_alias_cycles_obey_the_exact_derived_work_budget_before_declaring_a_match() {
        for (first, accepted) in [("b", true), ("a", false)] {
            let target = vec![first.to_owned(), "c".into(), "c".into()];
            let identities = Identities {
                declarations: HashMap::from_iter([(target.clone(), Some(false))]),
                links: HashMap::from_iter([
                    (vec!["a".into()], Some(vec!["b".into(), "c".into()])),
                    (vec!["b".into()], Some(vec!["a".into()])),
                ]),
                ..Identities::default()
            };
            assert_eq!(identities.resolve_path(&["a".into()]), accepted.then_some(target), "{first}");
        }
    }

    #[test]
    fn empty_self_import_targets_are_unknown_rather_than_panicking() {
        let identities = Identities::of(&parse_file("use self as Root;").unwrap(), &CfgSet::unconditional());
        assert_eq!(identities.resolve_path(&["Root".into()]), None);
        assert_eq!(identities.default_for(&["Root".into()]), None);
    }

    #[test]
    fn configured_out_declarations_and_expanding_enum_attributes_supply_no_identity_proof() {
        let file = parse_file(
            "#[cfg(any())] struct Absent;
             mod nested { #[cfg(any())] enum Hidden { A } struct Present; }
             #[transform] enum Expanded { A }",
        )
        .unwrap();
        let identities = Identities::of(&file, &CfgSet::parse(""));
        assert_eq!(identities.default_for(&["Absent".into()]), None);
        assert_eq!(identities.default_for(&["nested".into(), "Hidden".into()]), None);
        assert_eq!(identities.default_for(&["nested".into(), "Present".into()]), None);
        assert!(!identities.is_enum(&["Expanded".into()]));
    }

    #[test]
    fn labeled_merges_keep_generic_default_uncertainty_and_climb_multiple_missing_parents() {
        let cfg = CfgSet::unconditional();
        let generic = Pending::of(
            &parse_file("struct Generic<T>(T); impl<T> Default for Generic<T> { fn default() -> Self { panic!() } }").unwrap(),
            &cfg,
        );
        let mut root = Pending::default();
        root.absorb_at(&generic, &["child".into()]);
        assert_eq!(root.finish().default_for(&["child".into(), "Generic".into()]), None);
        let file = parse_file("use super::super::Shape as Escaped;").unwrap();
        let identities = Identities::of(&file, &cfg);
        assert_eq!(identities.resolve_path(&["Escaped".into()]), None);
        let identities = Identities::of(
            &parse_file("mod inner { use custom::Default; struct Opaque; } #[derive(Default)] struct Ready;").unwrap(),
            &cfg,
        );
        assert_eq!(identities.default_for(&["Ready".into()]), Some(true));
        assert_eq!(identities.default_for(&["inner".into(), "Opaque".into()]), Some(false));
    }

    #[test]
    fn rebased_double_parent_imports_resolve_the_root_instead_of_a_child_shadow() {
        let cfg = CfgSet::unconditional();
        let mut root = Pending::of(&parse_file("#[derive(Default)] struct Shape;").unwrap(), &cfg);
        let child = Pending::of(&parse_file("struct Shape; use super::super::Shape as Imported;").unwrap(), &cfg);
        root.absorb_at(&child, &["outer".into(), "child".into()]);
        let identities = root.finish();
        assert_eq!(identities.default_for(&["Shape".into()]), Some(true));
        assert_eq!(
            identities.default_for(&["outer".into(), "child".into(), "Shape".into()]),
            Some(false)
        );
        assert_eq!(
            identities.default_for(&["outer".into(), "child".into(), "Imported".into()]),
            Some(true)
        );
    }

    #[test]
    fn nested_nonstandard_default_imports_cannot_supply_a_standard_derive_proof() {
        let source = "mod nested { use custom::Default; #[derive(Default)] struct Local; }
                      #[derive(Default)] struct Outer;";
        let identities = Identities::of(&parse_file(source).unwrap(), &CfgSet::unconditional());
        assert_eq!(identities.default_for(&["nested".into(), "Local".into()]), None);
        assert_eq!(identities.default_for(&["Outer".into()]), Some(true));
    }

    #[test]
    fn absolute_imports_do_not_resolve_through_same_named_source_modules() {
        let source = "mod external { #[derive(Default)] pub struct Shape; }
                      use ::external::Shape as Imported;
                      use external::Shape as Local;";
        let identities = Identities::of(&parse_file(source).unwrap(), &CfgSet::unconditional());
        assert_eq!(identities.default_for(&["external".into(), "Shape".into()]), Some(true));
        assert_eq!(identities.default_for(&["Local".into()]), Some(true));
        assert_eq!(identities.default_for(&["Imported".into()]), None);
    }

    #[test]
    fn campaign_nominal_default_evidence_requires_every_derive_to_be_known() {
        for (attributes, expected) in [
            ("", Some(false)),
            ("#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]", Some(false)),
            ("#[derive(Clone, Default)]", Some(true)),
            ("#[doc = \"source declaration\"] #[repr(C)] #[derive(Debug)]", Some(false)),
            ("#[derive(Clone, Expand)]", None),
            ("#[derive(Default, Expand)]", None),
            ("#[derive(custom::Default)]", None),
            ("#[derive(Default = invalid)]", None),
            ("#[transform] #[derive(Default)]", None),
        ] {
            let source = format!("{attributes} struct Shape;");
            let identities = Identities::of(&parse_file(&source).unwrap(), &CfgSet::unconditional());
            assert_eq!(identities.default_for(&["Shape".into()]), expected, "{source}");
        }
        let source = "use custom::Default; #[derive(Default)] struct Shape;";
        let identities = Identities::of(&parse_file(source).unwrap(), &CfgSet::unconditional());
        assert_eq!(identities.default_for(&["Shape".into()]), None);
    }

    #[test]
    fn campaign_union_and_enum_declarations_keep_distinct_default_and_variant_evidence() {
        let source = "
            union Missing { byte: u8 }
            union Ready { byte: u8 }
            impl Default for Ready { fn default() -> Self { Self { byte: 0 } } }
            #[derive(Default)] enum Choice { #[default] A, B }
            #[transform] enum Opaque { A }";
        let identities = Identities::of(&parse_file(source).unwrap(), &CfgSet::unconditional());
        for (name, expected_default, expected_enum) in [
            ("Missing", None, false),
            ("Ready", Some(true), false),
            ("Choice", Some(true), true),
            ("Opaque", None, false),
        ] {
            assert_eq!(identities.default_for(&[name.into()]), expected_default, "{name}");
            assert_eq!(identities.is_enum(&[name.into()]), expected_enum, "{name}");
        }
    }

    #[test]
    fn campaign_grouped_self_imports_preserve_module_aliases_and_nested_parent_paths() {
        let source = "
            mod data { #[derive(Default)] pub struct Ready; pub struct Missing; }
            use self::data::{self as renamed, Ready, Missing as Absent};
            mod child {
                use super::data::{self, Ready as Present};
                pub mod leaf { use super::super::data::Ready as Deep; }
            }";
        let identities = Identities::of(&parse_file(source).unwrap(), &CfgSet::unconditional());
        for (path, declaration, expected) in [
            ("Ready", "data::Ready", Some(true)),
            ("Absent", "data::Missing", Some(false)),
            ("renamed::Ready", "data::Ready", Some(true)),
            ("child::data::Missing", "data::Missing", Some(false)),
            ("child::Present", "data::Ready", Some(true)),
            ("child::leaf::Deep", "data::Ready", Some(true)),
            ("self::renamed::Ready", "data::Ready", Some(true)),
            ("crate::child::Present", "data::Ready", Some(true)),
        ] {
            let names = path.split("::").map(str::to_owned).collect::<Vec<_>>();
            assert_eq!(
                identities.resolve_path(&names),
                Some(declaration.split("::").map(str::to_owned).collect()),
                "{path}"
            );
            assert_eq!(identities.default_for(&names), expected, "{path}");
        }
        assert!(identities.local_module(&[], "data"));
        assert!(identities.local_module(&["child".into()], "leaf"));
        assert!(!identities.local_module(&[], "renamed"));
        assert_eq!(identities.resolve_path(&["missing".into()]), None);
    }

    #[test]
    fn campaign_alias_ambiguity_cycles_and_external_roots_withhold_nominal_proof() {
        let source = "
            #[derive(Default)] struct Ready;
            struct Missing;
            use self::Ready as Ambiguous;
            use self::Missing as Ambiguous;
            type First = Second;
            type Second = First;
            use ::external::Ready as Foreign;
            type Generic<T> = Ready;
            type Absolute = ::external::Ready;";
        let identities = Identities::of(&parse_file(source).unwrap(), &CfgSet::unconditional());
        for name in ["Ambiguous", "First", "Second", "Foreign", "Generic", "Absolute"] {
            assert_eq!(identities.resolve_path(&[name.into()]), None, "{name}");
            assert_eq!(identities.default_for(&[name.into()]), None, "{name}");
        }
        assert_eq!(identities.default_for(&["Ready".into()]), Some(true));
        assert_eq!(identities.default_for(&["Missing".into()]), Some(false));
    }

    #[test]
    fn campaign_labeled_identity_merges_rebase_parent_links_and_implementations() {
        let cfg = CfgSet::unconditional();
        let mut root = Pending::of(
            &parse_file("struct Root; mod child {} #[derive(Default)] struct Ready;").unwrap(),
            &cfg,
        );
        let child = Pending::of(
            &parse_file(
                "use super::Root as Parent; use crate::Ready as Absolute;
                 struct Local; impl Default for Local { fn default() -> Self { Self } }
                 mod nested { use super::Local as Present; }
                 use super::super::Missing as Escaped;",
            )
            .unwrap(),
            &cfg,
        );
        root.absorb_at(&child, &["child".into()]);
        let identities = root.finish();
        for (path, expected) in [
            ("child::Parent", Some(false)),
            ("child::Absolute", Some(true)),
            ("child::Local", Some(true)),
            ("child::nested::Present", Some(true)),
            ("child::Escaped", None),
            ("Local", None),
        ] {
            assert_eq!(
                identities.default_for(&path.split("::").map(str::to_owned).collect::<Vec<_>>()),
                expected,
                "{path}"
            );
        }
        assert!(identities.local_module(&["child".into()], "nested"));
    }

    #[test]
    fn labeled_default_evidence_rejects_glob_traits_and_shadowed_relative_roots() {
        let cfg = CfgSet::parse("");
        for (source, expected) in [
            (
                "mod custom { pub trait Default {} } use custom::*; struct Shape; impl Default for Shape {}",
                Some(false),
            ),
            (
                "mod core { pub mod default { pub trait Default {} } } struct Shape; impl core::default::Default for Shape {}",
                Some(false),
            ),
            (
                "mod std { pub mod default { pub trait Default {} } } struct Shape; impl std::default::Default for Shape {}",
                Some(false),
            ),
            ("mod core {} struct Shape; impl ::core::default::Default for Shape {}", Some(true)),
            (
                "mod custom { pub trait Default {} } use custom::*; use ::core::default::Default as D; struct Shape; impl D for Shape {}",
                Some(true),
            ),
            (
                "mod custom { pub trait Default {} } use custom::*; use ::core::default as d; struct Shape; impl d::Default for Shape {}",
                Some(true),
            ),
            ("struct Shape; impl Default for Shape {}", Some(true)),
        ] {
            let mut root = Pending::of(&parse_file("mod child;").unwrap(), &cfg);
            root.absorb_at(&Pending::of(&parse_file(source).unwrap(), &cfg), &["child".into()]);
            assert_eq!(root.finish().default_for(&["child".into(), "Shape".into()]), expected, "{source}");
        }
    }

    #[test]
    fn nested_reexports_preserve_root_declaration_default_identity() {
        let file = parse_file(
            "struct Missing; #[derive(Default)] struct Ready;
             mod child { use super::Missing as Absent; use crate::Ready as Present; }",
        )
        .unwrap();
        let identities = Identities::of(&file, &CfgSet::unconditional());
        assert_eq!(identities.default_for(&["child".into(), "Absent".into()]), Some(false));
        assert_eq!(identities.default_for(&["child".into(), "Present".into()]), Some(true));
    }

    #[test]
    fn prefix_expanding_alias_cycles_are_unknown_and_terminate() {
        let file = parse_file("type Alias = Alias::Nested;").unwrap();
        let identities = Identities::of(&file, &CfgSet::unconditional());
        assert_eq!(identities.resolve_path(&["Alias".into()]), None);
    }

    #[test]
    fn a_default_implementation_through_an_alias_targets_the_declaration_regardless_of_order() {
        for source in [
            "impl Default for Alias {} type Alias = Actual; struct Actual;",
            "struct Actual; type Alias = Actual; impl Default for Alias {}",
        ] {
            let identities = Identities::of(&parse_file(source).unwrap(), &CfgSet::unconditional());
            assert_eq!(identities.default_for(&["Actual".into()]), Some(true));
            assert_eq!(identities.default_for(&["Alias".into()]), Some(true));
        }
    }

    #[test]
    fn merged_local_reexports_keep_uniform_negative_evidence_but_unknown_targets_do_not() {
        let mut defaults =
            Defaults::of(&parse_file("mod implementation { pub struct Artifact; } pub use self::implementation::Artifact;").unwrap());
        defaults.absorb(Defaults::of(&parse_file("struct Unrelated;").unwrap()));
        assert!(!defaults.has_single_source_evidence());
        assert!(defaults.lacks_error_default("Artifact"));
        assert!(defaults.lacks_local_path(&["crate".into(), "catalog".into(), "Artifact".into()]));
        defaults.absorb(Defaults::of(&parse_file("pub use dependency::Artifact;").unwrap()));
        assert!(!defaults.lacks_error_default("Artifact"));
    }

    #[test]
    fn alias_implementations_and_conditional_derives_do_not_supply_negative_evidence_when_merged() {
        for sources in [
            ["type Alias = Actual; struct Actual;", "impl Default for Alias {}"],
            ["impl Default for Alias {}", "type Alias = Actual; struct Actual;"],
        ] {
            let mut defaults = Defaults::default();
            for source in sources {
                defaults.absorb(Defaults::of(&parse_file(source).unwrap()));
            }
            assert!(!defaults.lacks_error_default("Actual"));
        }
        let defaults =
            Defaults::of(&parse_file("#[cfg_attr(all(), derive(Default))] struct Conditional; pub use self::Conditional;").unwrap());
        assert!(!defaults.lacks_error_default("Conditional"));
        assert!(!defaults.lacks_local_path(&["Conditional".into()]));
    }
}
