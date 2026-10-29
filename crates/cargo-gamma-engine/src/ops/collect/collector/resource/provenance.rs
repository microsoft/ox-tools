// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Exact declaration provenance for inferred loop-state bindings.

use quote::ToTokens as _;
use syn::spanned::Spanned as _;
use syn::visit::{self, Visit};
use syn::{Expr, Lit, Type};

use super::super::Collector;
use super::{Binding, Primitive, StableCall, scalar_sources};
use crate::HashMap;

struct Calls {
    sites: Vec<syn::ExprPath>,
    remaining: usize,
}

#[expect(clippy::renamed_function_params, reason = "syn names all visitor parameters i")]
impl<'ast> Visit<'ast> for Calls {
    fn visit_expr(&mut self, expression: &'ast Expr) {
        if self.remaining == 0 {
            return;
        }
        self.remaining -= 1;
        visit::visit_expr(self, expression);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let Expr::Path(callee) = &*call.func {
            self.sites.push(callee.clone());
        }
        visit::visit_expr_call(self, call);
    }
}

fn vector_length(expression: &Expr, bindings: &HashMap<String, Binding>) -> bool {
    let Expr::MethodCall(length) = expression else { return false }; // #[gamma::skip(return_value.default, reason = "false is the bool default.", tag = "equivalent")]
    if !length.attrs.is_empty() || length.method != "len" || !length.args.is_empty() || length.turbofish.is_some() {
        return false; // #[gamma::skip(return_value.default, reason = "false is the bool default.", tag = "equivalent")]
    }
    let Expr::Path(receiver) = &*length.receiver else { return false }; // #[gamma::skip(return_value.default, reason = "false is the bool default.", tag = "equivalent")]
    receiver.path.get_ident().is_some_and(|name| {
        matches!(
            bindings.get(&name.to_string()),
            Some(Binding::Vec | Binding::VecChar | Binding::VecString)
        )
    })
}

impl Collector<'_> {
    pub(in super::super) fn resource_stable_calls(&self, expression: &Expr) -> HashMap<String, StableCall> {
        let imports = self.current_imports();
        let inline = imports
            .get(super::super::types::LOCAL_MODULE)
            .and_then(Option::as_ref)
            .map_or(&[][..], Vec::as_slice);
        let sources = self.defaults.resource_sources();
        let mut result = HashMap::default();
        if imports.contains_key("*") || !sources.closed_methods_for(self.file.path.as_ref(), inline) {
            return result;
        }
        if sources.width_contract_for(self.file.path.as_ref(), inline) {
            let _old = result.insert(
                super::CHAR_WIDTH.into(),
                StableCall {
                    parameters: vec![Binding::Char],
                    output: Binding::Integer,
                    char_option: false, // #[gamma::skip(literal.bool_flip, reason = "The char-width sentinel is consumed only by contains_key; its StableCall fields are never read.", tag = "equivalent")]
                },
            );
        }
        let mut calls = Calls {
            sites: Vec::new(),
            remaining: 256,
        };
        calls.visit_expr(expression);
        if calls.remaining == 0 {
            return result;
        }
        let Some(shadows) = super::enclosing_pattern_shadows(&self.file.ast, &expression.span().byte_range()) else {
            return result;
        };
        for callee in calls.sites {
            if callee.path.leading_colon.is_some()
                || callee.qself.is_some()
                || callee
                    .path
                    .segments
                    .iter()
                    .any(|part| !matches!(part.arguments, syn::PathArguments::None))
            {
                continue;
            }
            let mut path = callee.path.segments.iter().map(|part| part.ident.to_string()).collect::<Vec<_>>();
            let Some(first) = path.first() else {
                // #[gamma::skip(loop.continue_to_break, reason = "Parsed callee paths contain a segment, and mapping segments preserves nonempty length.", tag = "equivalent")]
                continue;
            };
            if self.bindings.contains_key(first)
                || shadows.contains(first)
                || self.generics.contains(first)
                || self.defaulted.contains(first)
            {
                continue;
            }
            if path.as_slice() == ["Some"] && sources.primitive_scope_for(self.file.path.as_ref(), inline) {
                let _old = result.insert(
                    callee.to_token_stream().to_string(),
                    StableCall {
                        parameters: vec![Binding::CharReference],
                        output: Binding::MaybeChar,
                        char_option: true,
                    },
                );
                // #[gamma::skip(loop.delete_continue, reason = "Primitive-scope validation excludes a resolvable declaration named Some, so the extra lookup cannot overwrite this entry.", tag = "equivalent")]
                continue;
            }
            if let Some(import) = imports.get(first) {
                let Some(import) = import else { continue };
                path = import.iter().chain(&path[1..]).cloned().collect();
            }
            if let Some(function) = sources.function(self.file.path.as_ref(), inline, &path)
                && let Some(call) = super::certify(sources, function)
            {
                let _old = result.insert(callee.to_token_stream().to_string(), call);
            }
        }
        result
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one bounded provenance pass handles direct initializer forms without widening inference"
    )]
    pub(in super::super) fn resource_source_bindings(&self, expression: &Expr, bindings: &mut HashMap<String, Binding>) {
        let Some(sources) = scalar_sources(&self.file.ast, &expression.span().byte_range()) else {
            return;
        };
        for (name, initializer) in sources.locals {
            if matches!(&initializer, Expr::Lit(literal) if matches!(literal.lit, Lit::Int(_))) {
                let _entry = bindings.entry(name).or_insert(Binding::Integer);
                continue;
            }
            if let Expr::MethodCall(maximum) = &initializer
                && maximum.attrs.is_empty()
                && maximum.method == "max"
                && maximum.args.len() == 1
                && maximum.turbofish.is_none()
                && vector_length(&maximum.receiver, bindings)
                && vector_length(&maximum.args[0], bindings)
                && self.defaults.resource_sources().closed_methods_for(
                    self.file.path.as_ref(),
                    self.current_imports()
                        .get(super::super::types::LOCAL_MODULE)
                        .and_then(Option::as_ref)
                        .map_or(&[][..], Vec::as_slice),
                )
            {
                let _prior = bindings.insert(name, Binding::Integer);
                continue;
            }
            if let Expr::MethodCall(collect) = &initializer
                && collect.attrs.is_empty()
                && collect.method == "collect"
                && collect.args.is_empty()
                && collect.turbofish.is_none()
                && bindings.get(&name) == Some(&Binding::Vec)
                && let Expr::MethodCall(chars) = &*collect.receiver
                && chars.attrs.is_empty()
                && chars.method == "chars"
                && chars.args.is_empty()
                && chars.turbofish.is_none()
                && let Expr::Path(receiver) = &*chars.receiver
                && let Some(receiver) = receiver.path.get_ident()
                && let Some(binding) = self
                    .binding_types
                    .get(&receiver.to_string())
                    .filter(|binding| binding.resource_trusted)
                && self.types_with(&binding.imports).supports_str_reference(&binding.ty)
            {
                let _prior = bindings.insert(name, Binding::VecChar);
                continue;
            }
            let Expr::Call(call) = initializer else { continue };
            if let Expr::Path(callee) = &*call.func
                && callee.qself.is_none()
                && call.attrs.is_empty()
            {
                let mut path = callee.path.clone();
                if let Some(method) = path.segments.pop()
                    && matches!(method.ident.to_string().as_str(), "new" | "with_capacity")
                    && matches!(method.arguments, syn::PathArguments::None)
                    && call.args.len() == usize::from(method.ident == "with_capacity")
                {
                    let _separator = path.segments.pop_punct();
                    let ty = Type::Path(syn::TypePath {
                        attrs: Vec::new(),
                        qself: None,
                        path,
                    });
                    let types = self.types();
                    if types.standard_constructor_root(&ty)
                        && types.supports_collection_api(&ty)
                        && types.standard_type_name(&ty).as_deref() == Some("Vec")
                    {
                        let _prior = bindings.insert(name, Binding::Vec);
                        continue;
                    }
                }
            }
            if let Some(ty) = self.standard_constructor_type(&call)
                && let Some(kind @ (Binding::Vec | Binding::VecChar)) = self.resource_binding(&ty, &self.scope_path)
            {
                let _prior = bindings.insert(name, kind);
                continue;
            }
            let Some(signature) = self.call_signature(&call.func) else {
                continue;
            };
            let syn::ReturnType::Type(_, ty) = &signature.output else {
                continue;
            };
            let Type::Path(path) = &**ty else { continue };
            let Some(primitive) = path.path.get_ident().and_then(|ident| Primitive::named(&ident.to_string())) else {
                continue;
            };
            if signature.asyncness.is_none() && signature.generics.params.is_empty() {
                let _prior = bindings.insert(name, Binding::Primitive(primitive));
            }
        }
        if let Some((name, ty)) = sources.nonzero_get {
            let types = self.types_with(&self.root_imports);
            let abstract_root = matches!(&ty, Type::Path(path) if path.path.segments.first().is_some_and(|segment| {
                let name = segment.ident.to_string();
                self.generics.contains(&name) || self.defaulted.contains(&name)
            }));
            if !abstract_root
                && types.standard_constructor_root(&ty)
                && types.supports_nonzero_api(&ty)
                && types.standard_type_name(&ty).as_deref() == Some("NonZeroU32")
            {
                let _prior = bindings.insert(name, Binding::Primitive(Primitive::Unsigned(32)));
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use syn::PathArguments;

    use super::*;
    use crate::cfg::CfgSet;
    use crate::ops::collect::Defaults;
    use crate::ops::collect::collector::indexes::block_id;
    use crate::ops::registry::Selection;
    use crate::parse::SourceFile;

    #[test]
    fn qualified_self_cannot_reuse_a_resolvable_module_helper() {
        with_loop(
            "mod implementation { pub fn inspect(value:char)->bool{true} }
             fn run(){while ready {
                 <T as crate::implementation>::inspect('x');
                 crate::implementation::inspect('x');
             }}",
            |collector, expression| {
                let mut sites = Calls {
                    sites: Vec::new(),
                    remaining: 256,
                };
                sites.visit_expr(expression);
                assert_eq!(sites.sites.len(), 2);
                assert_eq!(sites.sites[0].qself.as_ref().map(|qualified| qualified.position), Some(2));
                assert!(sites.sites[0].path.leading_colon.is_none());
                assert_eq!(
                    sites.sites[0].path, sites.sites[1].path,
                    "only qualified-self metadata distinguishes the otherwise resolvable helper"
                );
                assert_eq!(
                    calls(collector, expression),
                    HashMap::from_iter([(
                        "crate :: implementation :: inspect".into(),
                        (vec![Binding::Char], Binding::Bool, false),
                    )])
                );
            },
        );
    }

    #[test]
    fn a_generic_segment_cannot_reuse_a_resolvable_module_helper() {
        with_loop(
            "mod implementation { pub fn inspect(value:char)->bool{true} }
             fn run(){while ready {
                 crate::implementation::inspect::<u8>('x');
                 crate::implementation::inspect('x');
             }}",
            |collector, expression| {
                let mut sites = Calls {
                    sites: Vec::new(),
                    remaining: 256,
                };
                sites.visit_expr(expression);
                assert_eq!(sites.sites.len(), 2);
                assert!(sites.sites[0].qself.is_none());
                assert!(sites.sites[0].path.leading_colon.is_none());
                assert_eq!(sites.sites[0].path.segments.len(), 3);
                assert_eq!(sites.sites[0].path.segments[0].arguments, PathArguments::None);
                assert!(matches!(
                    sites.sites[0].path.segments[2].arguments,
                    PathArguments::AngleBracketed(_)
                ));
                assert_eq!(
                    calls(collector, expression),
                    HashMap::from_iter([(
                        "crate :: implementation :: inspect".into(),
                        (vec![Binding::Char], Binding::Bool, false),
                    )])
                );
            },
        );
    }

    #[test]
    fn exact_maximum_length_provenance_does_not_stop_later_initializer_analysis() {
        with_loop(
            "fn run() { let bound=left.len().max(right.len()); let later=7; while ready{} }",
            |collector, expression| {
                let mut bindings = HashMap::from_iter([("left".into(), Binding::Vec), ("right".into(), Binding::Vec)]);
                collector.resource_source_bindings(expression, &mut bindings);
                assert_eq!(
                    bindings,
                    HashMap::from_iter([
                        ("left".into(), Binding::Vec),
                        ("right".into(), Binding::Vec),
                        ("bound".into(), Binding::Integer),
                        ("later".into(), Binding::Integer),
                    ])
                );
            },
        );
    }

    #[test]
    fn stable_calls_resolve_import_aliases_without_accepting_qualified_self_or_type_arguments() {
        with_loop(
            "mod implementation { pub fn inspect(value: char) -> bool { true } }
             use crate::implementation::inspect as renamed;
             fn inspect(value: char) -> bool { true }
             fn other(value: char) -> bool { false }
             fn run() { while ready { renamed('x'); inspect::<u8>('x'); <u8>::inspect('x'); other('x'); } }",
            |collector, expression| {
                assert_eq!(
                    calls(collector, expression),
                    HashMap::from_iter([("other".into(), (vec![Binding::Char], Binding::Bool, false))])
                );
                collector.scope_imports.entry(collector.scope_path.clone()).or_default().insert(
                    "renamed".into(),
                    Some(vec!["crate".into(), "implementation".into(), "inspect".into()]),
                );
                assert_eq!(
                    calls(collector, expression),
                    HashMap::from_iter([
                        ("renamed".into(), (vec![Binding::Char], Binding::Bool, false)),
                        ("other".into(), (vec![Binding::Char], Binding::Bool, false)),
                    ])
                );
                collector
                    .scope_imports
                    .entry(collector.scope_path.clone())
                    .or_default()
                    .insert("renamed".into(), None);
                assert_eq!(
                    calls(collector, expression),
                    HashMap::from_iter([("other".into(), (vec![Binding::Char], Binding::Bool, false)),])
                );
            },
        );
    }

    #[test]
    fn vector_length_evidence_rejects_attributes_and_near_match_methods() {
        let bindings = HashMap::from_iter([("rows".into(), Binding::Vec)]);
        for source in ["#[custom] rows.len()", "rows.capacity()", "rows.len(0)", "rows", "0"] {
            assert!(!vector_length(&syn::parse_str(source).unwrap(), &bindings), "{source}");
        }
    }

    #[test]
    fn independent_helper_shadows_do_not_stop_a_later_resolved_call() {
        for shadow in 0..3 {
            with_loop(
                "fn inspect(value:char)->bool{true} fn other(value:char)->bool{false} fn run(){while ready{inspect('x');other('x');}}",
                |collector, expression| {
                    match shadow {
                        0 => collector.bind("inspect".into(), false),
                        1 => collector.generics.push("inspect".into()),
                        2 => collector.defaulted.push("inspect".into()),
                        _ => unreachable!(),
                    }
                    assert_eq!(
                        calls(collector, expression),
                        HashMap::from_iter([("other".into(), (vec![Binding::Char], Binding::Bool, false)),])
                    );
                },
            );
        }
    }

    #[test]
    fn absolute_nonzero_type_still_requires_unshadowed_standard_root_evidence() {
        with_loop(
            "fn run(n: ::std::num::NonZeroU32){let n=n.get();while ready{}}",
            |collector, expression| {
                let mut bindings = HashMap::default();
                collector.resource_source_bindings(expression, &mut bindings);
                assert_eq!(
                    bindings,
                    HashMap::from_iter([("n".into(), Binding::Primitive(Primitive::Unsigned(32)))])
                );
                collector.root_imports.insert("std".into(), None);
                let mut bindings = HashMap::default();
                collector.resource_source_bindings(expression, &mut bindings);
                assert_eq!(bindings, HashMap::default());
            },
        );
    }

    #[test]
    fn character_collection_provenance_continues_to_later_independent_initializers() {
        with_loop(
            "fn run(value: &str) {
                let characters = value.chars().collect();
                let later = 7;
                while ready {}
             }",
            |collector, expression| {
                let mut bindings = HashMap::from_iter([("characters".into(), Binding::Vec)]);
                collector.resource_source_bindings(expression, &mut bindings);
                assert_eq!(
                    bindings,
                    HashMap::from_iter([("characters".into(), Binding::VecChar), ("later".into(), Binding::Integer)])
                );
            },
        );
    }

    #[test]
    fn source_constructors_distinguish_vec_from_other_standard_collections_and_custom_names() {
        for (source, expected) in [
            ("let rows = Vec::new();", Some(Binding::Vec)),
            ("let rows = Vec::with_capacity(7);", Some(Binding::Vec)),
            ("let rows = std::collections::HashMap::new();", None),
            ("let rows = String::new();", None),
            ("let rows = custom::Vec::new();", None),
        ] {
            let prefix = if source.contains("custom::") {
                "mod custom {pub struct Vec;}"
            } else {
                ""
            };
            with_loop(
                &format!("{prefix} fn run() {{ {source} let later = 7; while ready {{}} }}"),
                |collector, expression| {
                    let mut bindings = HashMap::default();
                    collector.resource_source_bindings(expression, &mut bindings);
                    let mut expected_bindings = HashMap::from_iter([("later".into(), Binding::Integer)]);
                    if let Some(kind) = expected {
                        expected_bindings.insert("rows".into(), kind);
                    }
                    assert_eq!(bindings, expected_bindings, "{source}");
                },
            );
        }
    }

    #[test]
    fn stable_calls_require_explicit_imports_and_unshadowed_some_identity() {
        with_loop(
            "fn helper(value:char)->bool{true} fn run(){while ready {helper('x');}}",
            |collector, expression| {
                assert_eq!(calls(collector, expression).len(), 1);
                collector
                    .scope_imports
                    .entry(collector.scope_path.clone())
                    .or_default()
                    .insert("*".into(), None);
                assert_eq!(calls(collector, expression), HashMap::default());
            },
        );
        with_loop("struct Some(char); fn run(){while ready {Some(&'x');}}", |collector, expression| {
            assert_eq!(calls(collector, expression), HashMap::default());
        });
    }

    #[test]
    fn absolute_vector_constructor_does_not_bypass_shadowed_standard_root_evidence() {
        with_loop(
            "fn run(){let rows=::std::vec::Vec::new();let later=7;while ready{}}",
            |collector, expression| {
                collector
                    .scope_imports
                    .entry(collector.scope_path.clone())
                    .or_default()
                    .insert("std".into(), None);
                let mut bindings = HashMap::default();
                collector.resource_source_bindings(expression, &mut bindings);
                assert_eq!(bindings, HashMap::from_iter([("later".into(), Binding::Integer)]));
            },
        );
    }

    #[test]
    fn source_constructor_provenance_keeps_later_initializers_after_qualified_standard_calls() {
        with_loop(
            "fn run(){type String=Vec<char>;let rows=std::string::String::new();let later=7;while ready{}}",
            |collector, expression| {
                let mut bindings = HashMap::default();
                collector.resource_source_bindings(expression, &mut bindings);
                assert_eq!(bindings.get("later"), Some(&Binding::Integer));
            },
        );
    }

    fn with_loop(source: &str, check: impl FnOnce(&mut Collector<'_>, &Expr)) {
        struct Target(Vec<Expr>);
        impl<'ast> Visit<'ast> for Target {
            fn visit_expr_while(&mut self, i: &'ast syn::ExprWhile) {
                self.0.push(Expr::While(i.clone()));
                visit::visit_expr_while(self, i);
            }
        }
        let file = SourceFile::parse("provenance.rs", source.into()).expect("the fixed provenance fixture is valid Rust syntax");
        let function = file
            .ast
            .items
            .iter()
            .find_map(|item| match item {
                syn::Item::Fn(function) if function.sig.ident == "run" => Some(function),
                _ => None,
            })
            .expect("each fixture declares its run function");
        let mut target = Target(Vec::new());
        target.visit_block(&function.block);
        assert_eq!(target.0.len(), 1, "each fixture has one target loop");
        let expression = target.0.pop().expect("the target count was checked above");
        let selection = Selection::everything();
        let cfg = CfgSet::unconditional();
        let defaults = Defaults::of(&file.ast);
        let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
        collector.with_function_parameters(&function.sig.generics, |collector| {
            collector.in_function(&function.sig, |collector| {
                collector.scope_path.push(block_id(&function.block));
                check(collector, &expression);
            });
        });
    }

    fn calls(collector: &Collector<'_>, expression: &Expr) -> HashMap<String, (Vec<Binding>, Binding, bool)> {
        collector
            .resource_stable_calls(expression)
            .into_iter()
            .map(|(name, call)| (name, (call.parameters, call.output, call.char_option)))
            .collect()
    }

    #[test]
    fn vector_lengths_require_bare_known_vectors_and_unmodified_len_calls() {
        let bindings = HashMap::from_iter([("rows".into(), Binding::Vec)]);
        for (expression, accepted) in [
            ("rows.len()", true),
            ("rows.len(1)", false),
            ("rows.len::<u8>()", false),
            ("state.rows.len()", false),
            ("unknown.len()", false),
        ] {
            assert_eq!(
                vector_length(&syn::parse_str(expression).unwrap(), &bindings),
                accepted,
                "{expression}"
            );
        }
        let mut calls = Calls {
            sites: Vec::new(),
            remaining: 0,
        };
        calls.visit_expr(&syn::parse_quote!(helper()));
        assert!(calls.sites.is_empty());
    }

    #[test]
    fn stable_calls_preserve_qualified_declarations_and_continue_after_an_unsupported_call() {
        with_loop(
            "mod implementation { pub fn inspect(value: char) -> bool { matches!(value, 'x') } }
             fn run() { while ready { ::implementation::inspect('x'); Some(&'x'); crate::implementation::inspect('x'); } }",
            |collector, expression| {
                assert_eq!(
                    calls(collector, expression),
                    HashMap::from_iter([
                        (
                            "crate :: implementation :: inspect".into(),
                            (vec![Binding::Char], Binding::Bool, false)
                        ),
                        ("Some".into(), (vec![Binding::CharReference], Binding::MaybeChar, true)),
                    ])
                );
            },
        );
        with_loop(
            "fn identity(value: bool) -> bool { value }
             fn inspect(value: char) -> bool { matches!(value, 'x') }
             fn run() { while ready { identity(inspect('x')); } }",
            |collector, expression| {
                assert_eq!(
                    calls(collector, expression),
                    HashMap::from_iter([
                        ("identity".into(), (vec![Binding::Bool], Binding::Bool, false)),
                        ("inspect".into(), (vec![Binding::Char], Binding::Bool, false)),
                    ])
                );
            },
        );
    }

    #[test]
    fn stable_call_search_requires_the_complete_expression_within_its_work_allowance() {
        with_loop(
            "fn inspect(value: char) -> bool { true } fn run() { while ready { inspect('x'); } }",
            |collector, expression| {
                for (padding, expected) in [(250, true), (251, false), (252, false)] {
                    let mut expression = expression.clone();
                    let Expr::While(loop_) = &mut expression else {
                        panic!("with_loop selects a while expression");
                    };
                    loop_
                        .body
                        .stmts
                        .splice(0..0, core::iter::repeat_n(syn::parse_quote!(() ;), padding));
                    assert_eq!(
                        calls(collector, &expression),
                        if expected {
                            HashMap::from_iter([("inspect".into(), (vec![Binding::Char], Binding::Bool, false))])
                        } else {
                            HashMap::default()
                        },
                        "{padding} inert expressions before the helper call"
                    );
                }
            },
        );
    }

    #[test]
    fn stable_helper_identity_rejects_parameter_pattern_and_generic_shadows() {
        for source in [
            "fn inspect(value: char) -> bool { true } fn other(value: char) -> bool { false }
             fn run(inspect: fn(char) -> bool) { while ready { inspect('x'); other('x'); } }",
            "fn inspect(value: char) -> bool { true } fn other(value: char) -> bool { false }
             fn run(value: Option<fn(char) -> bool>) { match value { Some(inspect) => { while ready { inspect('x'); other('x'); } }, _ => {} } }",
            "mod helper { pub fn inspect(value: char) -> bool { true } } fn other(value: char) -> bool { false }
             trait Api { fn inspect(value: char) -> bool; }
             fn run<helper: Api>() { while ready { helper::inspect('x'); other('x'); } }",
            "mod helper { pub fn inspect(value: char) -> bool { true } } fn other(value: char) -> bool { false }
             trait Api { fn inspect(value: char) -> bool; }
             fn run<helper: Api + Default>() { while ready { helper::inspect('x'); other('x'); } }",
        ] {
            with_loop(source, |collector, expression| {
                assert_eq!(
                    calls(collector, expression),
                    HashMap::from_iter([("other".into(), (vec![Binding::Char], Binding::Bool, false))]),
                    "{source}"
                );
            });
        }
        with_loop(
            "fn inspect(value: char) -> bool { true }
             fn run(value: Option<fn(char) -> bool>) { match value { Some(other) => { while ready { inspect('x'); } }, _ => {} } }",
            |collector, expression| {
                assert_eq!(
                    calls(collector, expression),
                    HashMap::from_iter([("inspect".into(), (vec![Binding::Char], Binding::Bool, false))])
                );
            },
        );
    }

    #[test]
    fn method_namespace_uncertainty_invalidates_otherwise_closed_helper_evidence() {
        for (source, expected) in [
            (
                "trait Extension { fn ordinary(&self); } fn inspect(value: char) -> bool { true }
                 fn run() { while ready { inspect('x'); } }",
                true,
            ),
            (
                "fn inspect(value: char) -> bool { true }
                 fn run() { use std::fmt::*; while ready { inspect('x'); } }",
                false,
            ),
            (
                "fn inspect(value: char) -> bool { true }
                 trait Extension { fn len(&self); }
                 fn run() { while ready { inspect('x'); } }",
                false,
            ),
        ] {
            with_loop(source, |collector, expression| {
                assert_eq!(
                    calls(collector, expression),
                    if expected {
                        HashMap::from_iter([("inspect".into(), (vec![Binding::Char], Binding::Bool, false))])
                    } else {
                        HashMap::default()
                    }
                );
            });
        }
    }

    #[test]
    fn nonzero_get_requires_the_standard_u32_declaration_not_just_a_matching_method() {
        for (source, expected) in [
            ("fn run(n: std::num::NonZeroU32) { let n = n.get(); while ready {} }", true),
            (
                "use std::num::NonZeroU32 as Bound; fn run(n: Bound) { let n = n.get(); while ready {} }",
                true,
            ),
            ("fn run(n: std::num::NonZeroU64) { let n = n.get(); while ready {} }", false),
            (
                "struct NonZeroU32; impl NonZeroU32 { fn get(&self) -> u32 { 0 } } fn run(n: NonZeroU32) { let n = n.get(); while ready {} }",
                false,
            ),
            (
                "trait Read { fn get(&self) -> u32; } fn run<NonZeroU32: Read>(n: NonZeroU32) { let n = n.get(); while ready {} }",
                false,
            ),
            (
                "trait Read { fn get(&self) -> u32; } fn run<NonZeroU32: Read + Default>(n: NonZeroU32) { let n = n.get(); while ready {} }",
                false,
            ),
        ] {
            with_loop(source, |collector, expression| {
                let mut bindings = HashMap::default();
                collector.resource_source_bindings(expression, &mut bindings);
                assert_eq!(
                    bindings,
                    if expected {
                        HashMap::from_iter([("n".into(), Binding::Primitive(Primitive::Unsigned(32)))])
                    } else {
                        HashMap::default()
                    },
                    "{source}"
                );
            });
        }
    }

    #[test]
    fn unsupported_initializer_shapes_do_not_stop_later_independent_provenance() {
        with_loop(
            "fn unit() {} fn nominal() -> String { String::new() }
             fn borrowed() -> &'static u8 { &0 }
             async fn deferred() -> u8 { 0 }
             fn generic<T>() -> u8 { 0 }
             fn number() -> u16 { 7 }
             fn run() {
                 let unknown = external(); let a = unit(); let b = nominal();
                 let c = borrowed(); let d = deferred(); let e = generic::<u8>();
                 let known = number(); let rows = Vec::new(); let typed = 0;
                 while ready {}
             }",
            |collector, expression| {
                let mut bindings = HashMap::from_iter([("typed".into(), Binding::Primitive(Primitive::Unsigned(16)))]);
                collector.resource_source_bindings(expression, &mut bindings);
                assert_eq!(
                    bindings,
                    HashMap::from_iter([
                        ("known".into(), Binding::Primitive(Primitive::Unsigned(16))),
                        ("rows".into(), Binding::Vec),
                        ("typed".into(), Binding::Primitive(Primitive::Unsigned(16))),
                    ])
                );
            },
        );
    }
}
