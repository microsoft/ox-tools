// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bounded ownership evidence for straight-line continuations and collection elements.

use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use syn::{Expr, Generics, Macro, Type, TypeParamBound, WherePredicate};

use super::{Collector, ScopePath, values};
use crate::HashMap;
use crate::parse::Extent as _;

pub(super) fn unbounded_parameters(generics: &Generics) -> Vec<String> {
    struct Mentions<'a> {
        name: &'a syn::Ident,
        found: bool,
    }
    impl<'ast> Visit<'ast> for Mentions<'_> {
        fn visit_type_path(&mut self, i: &'ast syn::TypePath) {
            self.found |= i.path.is_ident(self.name);
            visit::visit_type_path(self, i);
        }
    }
    generics
        .type_params()
        .filter(|parameter| {
            !parameter.bounds.iter().any(|bound| matches!(bound, TypeParamBound::Trait(_)))
                && !generics.type_params().any(|other| {
                    // #[gamma::skip(cond.always_false, tag = "equivalent", reason = "The outer filter already excludes trait bounds on this parameter; visiting its own lifetime, precise-capture, or verbatim bounds cannot visit a TypePath or set mentions.found.")]
                    if other.ident == parameter.ident {
                        return false;
                    }
                    let mut mentions = Mentions {
                        name: &parameter.ident,
                        found: false,
                    };
                    for bound in &other.bounds {
                        mentions.visit_type_param_bound(bound);
                    }
                    mentions.found
                })
                && generics.where_clause.as_ref().is_none_or(|clause| {
                    !clause.predicates.iter().any(|predicate| {
                        let WherePredicate::Type(predicate) = predicate else { return false };
                        let mut mentions = Mentions {
                            name: &parameter.ident,
                            found: false,
                        };
                        mentions.visit_type(&predicate.bounded_ty);
                        for bound in &predicate.bounds {
                            mentions.visit_type_param_bound(bound);
                        }
                        mentions.found && predicate.bounds.iter().any(|bound| matches!(bound, TypeParamBound::Trait(_)))
                    })
                })
        })
        .map(|parameter| parameter.ident.to_string())
        .collect()
}

impl Collector<'_> {
    pub(super) fn ownership_noncopy_type(
        &self,
        ty: &Type,
        remaining: usize,
        scope: &ScopePath,
        imports: &HashMap<String, Option<Vec<String>>>,
    ) -> bool {
        if remaining == 0 {
            return false;
        }
        if let Type::Path(path) = values::strip(ty)
            && let Some(name) = path.path.get_ident().map(ToString::to_string)
            && self.generics.iter().chain(&self.defaulted).any(|generic| *generic == name)
        {
            return self.ownership_generics.contains(&name)
                && !imports.contains_key(&name)
                && !self.scope_aliases.get(scope).unwrap_or(&self.aliases).contains_key(&name);
        }
        let types = self.types_with_in_scope(scope, imports);
        let ty = types.resolve_alias(ty);
        match values::strip(ty) {
            Type::Reference(reference) => return reference.mutability.is_some(),
            Type::Tuple(tuple) => {
                return tuple
                    .elems
                    .iter()
                    .any(|ty| self.ownership_noncopy_type(ty, remaining - 1, scope, imports));
            }
            Type::Array(array) => return self.ownership_noncopy_type(&array.elem, remaining - 1, scope, imports),
            _ => {}
        }
        types.supports_string_api(ty)
            || types.supports_collection_api(ty)
            || types.supports_wrapper_api(ty)
            || types.unshadowed_type_path(ty).is_some_and(|path| {
                matches!(path.as_slice(), [root, module, name]
                    if root == "std" && matches!((module.as_str(), name.as_str()),
                        ("fs", "File") | ("path", "PathBuf") | ("ffi", "OsString")
                            | ("process", "Child" | "Command" | "Stdio" | "Output")
                            | ("thread", "JoinHandle")
                            | ("sync", "Mutex" | "RwLock" | "MutexGuard" | "RwLockReadGuard" | "RwLockWriteGuard"))
                        || root == "tokio" && module == "sync" && name == "OwnedSemaphorePermit")
            })
            || self.calls.has_noncopy_field(ty, scope, imports, |field| {
                self.ownership_noncopy_type(field, remaining - 1, scope, imports)
            })
    }

    pub(super) fn ownership_known_place(&self, expression: &Expr) -> Option<bool> {
        let (ty, scope, imports) = if let Some(ty) = self.numeric_evidence.returned_types.get(&expression.extent().byte_range()) {
            (ty, &self.parameter_scope, self.imports_in_scope(&self.parameter_scope))
        } else {
            self.type_of_place_in_scope(expression, &self.scope_path)?
        };
        let types = self.types_with_in_scope(scope, imports);
        if let Type::Reference(reference) = values::strip(types.resolve_alias(ty)) {
            return Some(reference.mutability.is_some() && !self.numeric_evidence.reborrowable.contains(&expression.extent().byte_range()));
        }
        // Eight nested fields/containers bound recursive non-Copy inspection. This conservative
        // default is not a Rust type limit: exhaustion proves no ownership invalidity.
        Some(self.ownership_noncopy_type(ty, 8, scope, imports))
    }

    pub(super) fn ownership_receiver_consumes(&self, receiver: &Expr, method: &str) -> bool {
        let fallback = self
            .ownership_type(receiver)
            .cloned()
            .or_else(|| self.inferred_expression_type(receiver));
        let Some((ty, scope, imports)) = self
            .type_of_place_in_scope(receiver, &self.scope_path)
            .or_else(|| fallback.as_ref().map(|ty| (ty, &self.scope_path, self.current_imports())))
        else {
            return false;
        };
        let types = self.types_with_in_scope(scope, imports);
        let ty = types.resolve_alias(ty);
        match method {
            "into_iter" => {
                self.implicit_prelude
                    && !self.current_imports().contains_key("IntoIterator")
                    && !self.current_imports().contains_key("*")
                    && types.supports_collection_api(ty)
            }
            "into_bytes" | "into_boxed_str" => types.supports_string_api(ty),
            "into_string" => types
                .unshadowed_type_path(ty)
                .is_some_and(|path| path.iter().map(String::as_str).eq(["std", "ffi", "OsString"])),
            "into_os_string" => types
                .unshadowed_type_path(ty)
                .is_some_and(|path| path.iter().map(String::as_str).eq(["std", "path", "PathBuf"])),
            _ => false,
        }
    }

    pub(super) fn ownership_falls_through(&self, expression: &Expr, remaining: usize) -> bool {
        if remaining == 0 {
            return false;
        }
        let next = remaining - 1;
        match expression {
            Expr::Lit(_) | Expr::Path(_) => true,
            Expr::Paren(paren) => self.ownership_falls_through(&paren.expr, next),
            Expr::Group(group) => self.ownership_falls_through(&group.expr, next),
            Expr::Field(field) => self.ownership_falls_through(&field.base, next),
            Expr::Reference(reference) => self.ownership_falls_through(&reference.expr, next),
            Expr::Assign(assign) => self.ownership_falls_through(&assign.left, next) && self.ownership_falls_through(&assign.right, next),
            Expr::Tuple(tuple) => tuple.elems.iter().all(|element| self.ownership_falls_through(element, next)),
            Expr::Array(array) => array.elems.iter().all(|element| self.ownership_falls_through(element, next)),
            Expr::Macro(mac) => self.format_captures(&mac.mac).is_some(),
            Expr::Call(call) => {
                self.inferred_expression_type(expression)
                    .is_some_and(|ty| !matches!(values::strip(&ty), Type::Never(_) | Type::Infer(_)))
                    && self.ownership_falls_through(&call.func, next)
                    && call.args.iter().all(|argument| self.ownership_falls_through(argument, next))
            }
            Expr::MethodCall(call) => {
                self.ownership_expression_type(expression)
                    .is_some_and(|ty| !matches!(values::strip(&ty), Type::Never(_) | Type::Infer(_)))
                    && self.ownership_falls_through(&call.receiver, next)
                    && call.args.iter().all(|argument| self.ownership_falls_through(argument, next))
            }
            _ => false,
        }
    }

    fn ownership_expression_type(&self, expression: &Expr) -> Option<Type> {
        if let Some(ty) = self.inferred_expression_type(expression) {
            return Some(ty);
        }
        if let Expr::MethodCall(call) = expression
            && call.method == "join"
            && call.args.len() == 1
        {
            let receiver = self.ownership_expression_type(&call.receiver)?;
            let mut receiver = values::strip(&receiver);
            while let Type::Reference(reference) = receiver {
                // #[gamma::skip(stmt.delete_assign, tag = "timeout", reason = "Without advancing to the reference element, this while loop repeats forever on any reference receiver.")]
                receiver = values::strip(&reference.elem);
            }
            if self.types().unshadowed_type_path(receiver).is_some_and(|path| {
                matches!(path.as_slice(), [root, module, name]
                    if root == "std" && module == "path" && matches!(name.as_str(), "Path" | "PathBuf"))
            }) {
                return Some(syn::parse_quote!(::std::path::PathBuf));
            }
        }
        None
    }

    fn format_captures(&self, mac: &Macro) -> Option<Vec<Expr>> {
        let names = mac
            .path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>();
        let standard = match names.as_slice() {
            [name] if name == "format" => {
                self.implicit_prelude
                    && !self.format_macro_shadowed
                    && !self.current_imports().contains_key("format")
                    && !self.current_imports().contains_key("*")
            }
            [root, name] if root == "std" && name == "format" => self
                .types()
                .unshadowed_type_path(&syn::parse_quote!(std::string::String))
                .is_some_and(|path| path.iter().map(String::as_str).eq(["std", "string", "String"])),
            _ => false,
        };
        if !standard {
            return None;
        }
        let literal = syn::parse2::<syn::LitStr>(mac.tokens.clone()).ok()?;
        captured_places(&literal.value())
    }

    pub(super) fn invalid_ownership_reversal(&self, elements: &Punctuated<Expr, syn::token::Comma>) -> bool {
        // Conservative defaults bound separate work dimensions: 32 element effects to compare,
        // and 64 expression levels per fall-through check. Neither is a measured capacity;
        // exhaustion keeps the reversal eligible instead of claiming an invalid move.
        if elements.len() > 32 || !elements.iter().all(|element| self.ownership_falls_through(element, 64)) {
            return false;
        }
        let mut all_effects = Vec::with_capacity(elements.len());
        for element in elements {
            let mut effects = Effects {
                collector: self,
                uses: Vec::new(),
                moves: Vec::new(),
                supported: true,
            };
            effects.visit_expr(element);
            // #[gamma::skip(cond.always_true, tag = "equivalent", reason = "Adding a non-place to moves cannot change the result: overlapping_places rejects every expression without a place_path.")]
            if place_path(element).is_some() {
                effects.moves.push(element.clone());
            }
            if !effects.supported {
                return false;
            }
            all_effects.push(effects);
        }
        let mut earlier = Vec::new();
        for effects in all_effects {
            if effects.moves.iter().any(|moved| {
                self.noncopy_place(moved)
                    && !self
                        .ownership_expression_type(moved)
                        .is_some_and(|ty| matches!(values::strip(&ty), Type::Reference(_)))
                    && earlier.iter().any(|used| overlapping_places(used, moved))
            }) {
                return true;
            }
            earlier.extend(effects.uses);
        }
        false
    }
}

struct Effects<'a, 'source> {
    collector: &'a Collector<'source>,
    uses: Vec<Expr>,
    moves: Vec<Expr>,
    supported: bool,
}

impl<'ast> Visit<'ast> for Effects<'_, '_> {
    fn visit_expr_assign(&mut self, _: &'ast syn::ExprAssign) {
        self.supported = false;
    }

    fn visit_expr_path(&mut self, i: &'ast syn::ExprPath) {
        if i.path.get_ident().is_some() {
            self.uses.push(Expr::Path(i.clone()));
        }
    }

    fn visit_expr_field(&mut self, i: &'ast syn::ExprField) {
        self.uses.push(Expr::Field(i.clone()));
    }

    fn visit_expr_call(&mut self, i: &'ast syn::ExprCall) {
        visit::visit_expr_call(self, i);
        self.moves
            .extend(i.args.iter().filter(|argument| place_path(argument).is_some()).cloned());
    }

    fn visit_expr_method_call(&mut self, i: &'ast syn::ExprMethodCall) {
        visit::visit_expr_method_call(self, i);
        self.moves
            .extend(i.args.iter().filter(|argument| place_path(argument).is_some()).cloned());
    }

    fn visit_macro(&mut self, i: &'ast Macro) {
        if let Some(captures) = self.collector.format_captures(i) {
            self.uses.extend(captures);
        }
    }
}

fn place_path(expression: &Expr) -> Option<Vec<String>> {
    match expression {
        Expr::Path(path) => Some(vec![path.path.get_ident()?.to_string()]),
        Expr::Field(field) => {
            let mut path = place_path(&field.base)?;
            path.push(match &field.member {
                syn::Member::Named(name) => name.to_string(),
                syn::Member::Unnamed(index) => index.index.to_string(),
            });
            Some(path)
        }
        Expr::Paren(paren) => place_path(&paren.expr),
        Expr::Group(group) => place_path(&group.expr),
        _ => None,
    }
}

fn overlapping_places(left: &Expr, right: &Expr) -> bool {
    match (place_path(left), place_path(right)) {
        (Some(left), Some(right)) => left.starts_with(&right) || right.starts_with(&left),
        _ => false,
    }
}

fn captured_places(format: &str) -> Option<Vec<Expr>> {
    let mut captures = Vec::new();
    let mut characters = format.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '{' {
            if characters.peek() == Some(&'{') {
                let _escaped = characters.next();
                continue;
            }
            let mut capture = String::new();
            loop {
                match characters.next()? {
                    '}' => break,
                    '{' => {
                        return None;
                    }
                    character => capture.push(character),
                }
            }
            // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "str::split always yields at least one piece, including for an empty capture.")]
            let name = capture.split(':').next()?;
            let expression = syn::parse_str::<Expr>(name).ok()?;
            if !matches!(&expression, Expr::Path(path) if path.path.get_ident().is_some()) {
                return None;
            }
            // Dynamic format widths introduce further captures; leave those formats unknown.
            if capture.contains('$') {
                return None;
            }
            captures.push(expression);
        } else if character == '}' && characters.next() != Some('}') {
            return None;
        }
        // Bound separately parsed capture expressions to 32, a conservative analysis default.
        // An oversized format stays unknown, so it cannot prove an ownership-invalid reversal.
        if captures.len() > 32 {
            return None;
        }
    }
    Some(captures)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod ownership_regressions {
    use super::*;
    use crate::cfg::CfgSet;
    use crate::ops::collect::Defaults;
    use crate::ops::registry::Selection;
    use crate::parse::SourceFile;

    fn with_collector(source: &str, check: impl FnOnce(&mut Collector<'_>)) {
        let file = SourceFile::parse("ownership.rs", source.into()).unwrap();
        let selection = Selection::parse("collection.reverse_array").unwrap();
        let cfg = CfgSet::unconditional();
        let defaults = Defaults::default();
        check(&mut Collector::new(&file, &selection, &[], &cfg, &defaults));
    }

    fn expression(source: &str) -> Expr {
        syn::parse_str(source).unwrap()
    }

    fn elements(source: &str) -> Punctuated<Expr, syn::token::Comma> {
        let Expr::Array(array) = expression(source) else {
            panic!("fixture must be an array")
        };
        array.elems
    }

    #[test]
    fn ownership_captures_preserve_names_escapes_and_format_specs() {
        for (format, expected) in [
            ("", vec![]),
            ("plain text", vec![]),
            ("{{ignored}}", vec![]),
            ("{{{name}}}", vec!["name"]),
            ("{first} {{escaped}} {second:?}", vec!["first", "second"]),
            ("{name:08x}", vec!["name"]),
            ("{name}{name}", vec!["name", "name"]),
        ] {
            let actual = captured_places(format).unwrap();
            let names: Vec<_> = actual.iter().map(|value| place_path(value).unwrap().join(".")).collect();
            assert_eq!(names, expected, "{format:?}");
        }
        for format in [
            "{",
            "{name",
            "{name{nested}}",
            "{name/*{*/}",
            "}",
            "}x",
            "}}}",
            "{}",
            "{0}",
            "{pair.field}",
            "{mod::name}",
            "{name:width$}",
            "{invalid!}",
        ] {
            assert!(captured_places(format).is_none(), "{format:?}");
        }
        assert!(captured_places(&"{name}".repeat(32)).is_some());
        assert!(captured_places(&"{name}".repeat(33)).is_none());
    }

    #[test]
    fn ownership_places_preserve_projection_order_and_overlap_direction() {
        for (source, expected) in [("name", vec!["name"]), ("(name.left.0)", vec!["name", "left", "0"])] {
            assert_eq!(place_path(&expression(source)).unwrap(), expected);
        }
        let grouped = Expr::Group(syn::ExprGroup {
            attrs: Vec::new(),
            group_token: syn::token::Group::default(),
            expr: Box::new(expression("name.left")),
        });
        assert_eq!(place_path(&grouped).unwrap(), ["name", "left"]);
        for source in ["module::name", "call().field", "&name", "name[0]", "42"] {
            assert!(place_path(&expression(source)).is_none(), "{source}");
        }
        for (left, right, expected) in [
            ("pair", "pair.left", true),
            ("pair.left", "pair", true),
            ("pair.left", "pair.left", true),
            ("pair.left", "pair.right", false),
            ("left.pair", "pair", false),
            ("pair", "other.pair", false),
            ("pair.left", "other.left", false),
            ("&pair", "pair", false),
            ("pair", "call()", false),
        ] {
            assert_eq!(
                overlapping_places(&expression(left), &expression(right)),
                expected,
                "{left}, {right}"
            );
        }
    }

    #[test]
    fn ownership_fallthrough_depth_is_per_edge_and_checks_every_child() {
        with_collector("", |collector| {
            let grouped = Expr::Group(syn::ExprGroup {
                attrs: Vec::new(),
                group_token: syn::token::Group::default(),
                expr: Box::new(expression("value")),
            });
            let mut cases: Vec<Expr> = [
                "(value)",
                "value.field",
                "&value",
                "value = other",
                "(value, other)",
                "[value, other]",
            ]
            .into_iter()
            .map(expression)
            .collect();
            cases.push(grouped);
            for value in cases {
                assert!(!collector.ownership_falls_through(&value, 1), "{value:?}");
                assert!(collector.ownership_falls_through(&value, 2), "{value:?}");
            }
            assert!(!collector.ownership_falls_through(&expression("value"), 0));
            assert!(collector.ownership_falls_through(&expression("value"), 1));
            assert!(collector.ownership_falls_through(&expression("0"), 1));
            for source in ["()", "[]", "(0, value)", "[value, 0]", "value = other", "format!(\"{value}\")"] {
                assert!(collector.ownership_falls_through(&expression(source), 8), "{source}");
            }
            for source in [
                "(return, value)",
                "(value, return)",
                "[return, value]",
                "[value, return]",
                "value = return",
                "(return) = value",
                "unknown()",
                "unknown.method()",
                "panic!()",
                "{ value }",
            ] {
                assert!(!collector.ownership_falls_through(&expression(source), 8), "{source}");
            }
            for source in ["(value) = other", "value = (other)"] {
                let value = expression(source);
                assert!(!collector.ownership_falls_through(&value, 2), "{source}");
                assert!(collector.ownership_falls_through(&value, 3), "{source}");
            }
        });
    }

    #[test]
    fn ownership_fallthrough_calls_require_known_return_and_all_operands() {
        with_collector(
            "fn known(a: u32, b: u32) -> u32 { a + b }
             fn zero() -> u32 { 0 }
             fn stop() -> ! { loop {} }
             struct Owner;
             impl Owner {
                 fn method(&self, a: u32, b: u32) -> u32 { a + b }
                 fn zero(&self) -> u32 { 0 }
             }",
            |collector| {
                collector.declared_types.insert("owner".into(), Some(syn::parse_quote!(Owner)));
                for source in ["known(1, 2)", "zero()", "owner.method(1, 2)", "owner.zero()"] {
                    let value = expression(source);
                    assert!(!collector.ownership_falls_through(&value, 1), "{source}");
                    assert!(collector.ownership_falls_through(&value, 2), "{source}");
                }
                for source in ["known((1), 2)", "(owner).method(1, 2)", "owner.method((1), 2)"] {
                    let value = expression(source);
                    assert!(!collector.ownership_falls_through(&value, 2), "{source}");
                    assert!(collector.ownership_falls_through(&value, 3), "{source}");
                }
                for source in [
                    "known(return, 2)",
                    "known(1, return)",
                    "owner.method(return, 2)",
                    "owner.method(1, return)",
                    "unknown.method(1, 2)",
                    "({ owner }).method(1, 2)",
                    "stop()",
                ] {
                    assert!(!collector.ownership_falls_through(&expression(source), 8), "{source}");
                }
            },
        );
    }

    #[test]
    fn ownership_receiver_consumption_is_specific_to_type_method_and_trait_scope() {
        for (ty, methods) in [
            ("Vec<u8>", vec!["into_iter"]),
            ("String", vec!["into_bytes", "into_boxed_str"]),
            ("std::ffi::OsString", vec!["into_string"]),
            ("std::path::PathBuf", vec!["into_os_string"]),
        ] {
            with_collector("", |collector| {
                collector.declared_types.insert("value".into(), Some(syn::parse_str(ty).unwrap()));
                for method in [
                    "into_iter",
                    "into_bytes",
                    "into_boxed_str",
                    "into_string",
                    "into_os_string",
                    "inspect",
                ] {
                    assert_eq!(
                        collector.ownership_receiver_consumes(&expression("value"), method),
                        methods.contains(&method),
                        "{ty}.{method}"
                    );
                }
                assert!(!collector.ownership_receiver_consumes(&expression("unknown"), "into_iter"));
            });
        }

        for prefix in ["#![no_implicit_prelude]", "use custom::IntoIterator;", "use custom::*;"] {
            with_collector(prefix, |collector| {
                collector.declared_types.insert("value".into(), Some(syn::parse_quote!(Vec<u8>)));
                assert!(
                    !collector.ownership_receiver_consumes(&expression("value"), "into_iter"),
                    "{prefix}"
                );
            });
        }
        with_collector("", |collector| {
            assert!(collector.ownership_receiver_consumes(&expression("String::new()"), "into_bytes"));
        });
    }

    #[test]
    fn ownership_receiver_wildcard_trait_shadow_does_not_depend_on_collection_type_identity() {
        with_collector("use custom::*;", |collector| {
            let ty: Type = syn::parse_quote!(::std::vec::Vec<u8>);
            assert!(collector.types().supports_collection_api(&ty));
            collector.binding_types.insert(
                "value".into(),
                super::super::BindingType {
                    ty,
                    imports: collector.current_imports().clone(),
                    scope: collector.scope_path.clone(),
                    resource_trusted: false,
                },
            );
            assert!(collector.implicit_prelude);
            assert!(!collector.current_imports().contains_key("IntoIterator"));
            assert!(collector.current_imports().contains_key("*"));
            let value = expression("value");
            let (ty, scope, imports) = collector.type_of_place_in_scope(&value, &collector.scope_path).unwrap();
            assert!(collector.types_with_in_scope(scope, imports).supports_collection_api(ty));
            assert!(!collector.ownership_receiver_consumes(&value, "into_iter"));
        });
    }

    #[test]
    fn ownership_known_place_distinguishes_unknown_shared_mutable_and_reborrowed() {
        with_collector("", |collector| {
            let value = expression("value");
            let span = value.extent().byte_range();
            assert_eq!(collector.ownership_known_place(&value), None);
            for (ty, expected) in [("String", true), ("u32", false), ("&String", false), ("&mut String", true)] {
                collector.declared_types.insert("value".into(), Some(syn::parse_str(ty).unwrap()));
                assert_eq!(collector.ownership_known_place(&value), Some(expected), "{ty}");
                collector
                    .numeric_evidence
                    .returned_types
                    .insert(span.clone(), syn::parse_str(ty).unwrap());
                assert_eq!(collector.ownership_known_place(&value), Some(expected), "returned {ty}");
                collector.numeric_evidence.returned_types.clear();
            }
            collector
                .numeric_evidence
                .returned_types
                .insert(span.clone(), syn::parse_quote!(&mut String));
            collector.numeric_evidence.reborrowable.insert(span);
            assert_eq!(collector.ownership_known_place(&value), Some(false));
        });
    }

    #[test]
    fn ownership_noncopy_generic_evidence_respects_import_and_alias_shadows() {
        with_collector("type Alias = String;", |collector| {
            for defaulted in [false, true] {
                collector.generics.clear();
                collector.defaulted.clear();
                if defaulted {
                    collector.defaulted.push("T".into());
                } else {
                    collector.generics.push("T".into());
                }
                collector.ownership_generics = vec!["T".into()];
                let ty: Type = syn::parse_quote!(T);
                assert!(collector.ownership_noncopy_type(&ty, 1, &Vec::new(), &HashMap::default()));
                let imports = HashMap::from_iter([("T".into(), None)]);
                assert!(!collector.ownership_noncopy_type(&ty, 1, &Vec::new(), &imports));
                let aliases = collector.scope_aliases.get_mut(&Vec::new()).unwrap();
                let alias = aliases.get("Alias").unwrap().clone();
                aliases.insert("T".into(), alias);
                assert!(!collector.ownership_noncopy_type(&ty, 1, &Vec::new(), &HashMap::default()));
                collector.scope_aliases.get_mut(&Vec::new()).unwrap().remove("T");
            }
            assert!(!collector.ownership_noncopy_type(&syn::parse_quote!(String), 0, &Vec::new(), &HashMap::default()));
        });
    }

    #[test]
    fn ownership_path_join_requires_path_identity_and_known_receiver() {
        for (ty, expected) in [
            ("std::path::Path", true),
            ("std::path::PathBuf", true),
            ("&&std::path::Path", true),
            ("std::ffi::OsString", false),
            ("custom::path::Path", false),
            ("std::other::Path", false),
        ] {
            with_collector("", |collector| {
                collector
                    .declared_types
                    .insert("directory".into(), Some(syn::parse_str(ty).unwrap()));
                let actual = collector.ownership_expression_type(&expression("directory.join(\"name\")"));
                assert_eq!(actual.is_some(), expected, "{ty}");
                if let Some(actual) = actual {
                    assert!(matches!(actual, Type::Path(path) if path.path.segments.last().unwrap().ident == "PathBuf"));
                }
            });
        }
        with_collector("", |collector| {
            assert!(collector.ownership_expression_type(&expression("unknown.join(\"name\")")).is_none());
        });
    }

    #[test]
    fn ownership_format_macros_require_standard_name_scope_and_literal_input() {
        with_collector("", |collector| {
            for source in ["format!(\"{value}\")", "std::format!(\"{value}\")"] {
                let Expr::Macro(mac) = expression(source) else { unreachable!() };
                assert_eq!(collector.format_captures(&mac.mac).unwrap().len(), 1);
            }
            for source in [
                "other!(\"{value}\")",
                "std::other!(\"{value}\")",
                "custom::format!(\"{value}\")",
                "custom::other!(\"{value}\")",
                "format!(input)",
                "format!(\"{}\", value)",
            ] {
                let Expr::Macro(mac) = expression(source) else { unreachable!() };
                assert!(collector.format_captures(&mac.mac).is_none(), "{source}");
            }
        });
        for prefix in [
            "use custom::*;",
            "use custom::format;",
            "#![no_implicit_prelude]",
            "macro_rules! format { () => {} }",
        ] {
            with_collector(prefix, |collector| {
                let Expr::Macro(mac) = expression("format!(\"{value}\")") else {
                    unreachable!()
                };
                assert!(collector.format_captures(&mac.mac).is_none(), "{prefix}");
            });
        }
    }

    #[test]
    fn ownership_effects_record_only_places_and_reject_assignments() {
        with_collector("", |collector| {
            let mut effects = Effects {
                collector,
                uses: Vec::new(),
                moves: Vec::new(),
                supported: true,
            };
            effects.visit_expr(&expression("value"));
            assert_eq!(effects.uses.len(), 1);
            effects.visit_expr(&expression("module::constant"));
            assert_eq!(effects.uses.len(), 1);
            effects.visit_expr(&expression("value = 0"));
            assert!(!effects.supported);
        });
    }

    #[test]
    fn ownership_reversal_checks_moves_references_assignments_and_size_boundaries() {
        with_collector(
            "fn observe(_: &String) -> String { String::new() }
             fn take(value: String) -> String { value }",
            |collector| {
                collector.declared_types.insert("value".into(), Some(syn::parse_quote!(String)));
                assert!(collector.invalid_ownership_reversal(&elements("[observe(&value), value]")));
                assert!(!collector.invalid_ownership_reversal(&elements("[value, observe(&value)]")));
                assert!(!collector.invalid_ownership_reversal(&elements("[observe(&value), value = String::new()]")));
                assert!(!collector.invalid_ownership_reversal(&elements(
                    "[((), observe(&value)), (value = String::new(), String::new()), ((), take(value))]"
                )));
                assert!(!collector.invalid_ownership_reversal(&elements("[observe(&value), unknown()]")));
                for size in [31, 32, 33, 34] {
                    let source = format!("[observe(&value), {}value]", "0, ".repeat(size - 2));
                    assert_eq!(collector.invalid_ownership_reversal(&elements(&source)), size <= 32, "{size}");
                }
                for depth in [60, 61, 62, 63] {
                    let source = format!("[observe({}&value{}), value]", "(".repeat(depth), ")".repeat(depth));
                    // observe -> parens -> reference -> value uses three edges besides the parens.
                    assert_eq!(collector.invalid_ownership_reversal(&elements(&source)), depth <= 61, "{depth}");
                }
                collector
                    .declared_types
                    .insert("value".into(), Some(syn::parse_quote!(&mut String)));
                assert!(!collector.invalid_ownership_reversal(&elements("[observe(&value), value]")));
            },
        );
    }
}
