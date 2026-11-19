// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Negative evidence for APIs that share iterator and option spellings.

use syn::{Expr, PathArguments, Type};

use crate::ops::collect::collector::Collector;
use crate::ops::collect::collector::indexes::{ScopePath, import_target};
use crate::ops::collect::collector::values::strip;

impl Collector<'_> {
    pub(super) fn dependency_field_type(&self, field: &syn::ExprField, scope: &ScopePath) -> Option<Type> {
        let receiver = self.inferred_expression_type_in_scope(&field.base, scope)?;
        let types = self.types_in_scope(scope);
        let mut receiver = types.resolve_alias(&receiver);
        while let Type::Reference(reference) = strip(receiver) {
            // #[gamma::skip(stmt.delete_assign, tag = "timeout", reason = "Each iteration must resolve the inner reference element; deleting this assignment repeatedly examines the same outer reference forever.")]
            receiver = types.resolve_alias(&reference.elem);
        }
        let owner = types.unshadowed_type_path(receiver)?;
        let syn::Member::Named(member) = &field.member else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        match (
            owner.iter().map(String::as_str).collect::<Vec<_>>().as_slice(),
            member.to_string().as_str(),
        ) {
            (["syn", "TypePath" | "ExprPath"], "path") => Some(syn::parse_quote!(::syn::Path)),
            (["syn", "Path"], "segments") => Some(syn::parse_quote!(::syn::punctuated::Punctuated<::syn::PathSegment, _>)),
            (["syn", "PathSegment"], "arguments") => Some(syn::parse_quote!(::syn::PathArguments)),
            _ => None,
        }
    }

    pub(super) fn dependency_method_missing(&self, receiver: &Type, replacement: &str) -> bool {
        let types = self.types();
        let mut receiver = types.resolve_alias(receiver);
        while let Type::Reference(reference) = strip(receiver) {
            // #[gamma::skip(stmt.delete_assign, tag = "timeout", reason = "Each iteration must resolve the inner reference element; deleting this assignment repeatedly examines the same outer reference forever.")]
            receiver = types.resolve_alias(&reference.elem);
        }
        if replacement != "is_some"
            || !types
                .unshadowed_type_path(receiver)
                .is_some_and(|path| path.iter().map(String::as_str).eq(["syn", "PathArguments"]))
            || self.calls.has_method_named(replacement)
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "false is already the default bool value.")]
            return false;
        }
        let imports = self.current_imports();
        let inline = imports
            .get(super::types::LOCAL_MODULE)
            .and_then(Option::as_ref)
            .map_or(&[][..], Vec::as_slice);
        let sources = self.defaults.resource_sources();
        sources.syn_contract_for(self.file.path.as_ref(), inline)
            && sources.closed_methods_for(self.file.path.as_ref(), inline)
            && sources.method_imports_closed_for(
                self.file.path.as_ref(),
                inline,
                imports
                    .iter()
                    .filter(|(name, _)| {
                        !matches!(
                            name.as_str(),
                            super::types::LOCAL_MODULE | super::types::BLOCK_TYPES | super::types::SOURCE_FILE
                        ) && !name.starts_with(super::indexes::EXTERN_ROOT_PREFIX)
                    })
                    .map(|(_, path)| path.as_deref().map(import_target)),
            )
    }

    pub(super) fn callable_output(&self, ty: &Type, scope: &ScopePath) -> Option<Type> {
        let types = self.types_in_scope(scope);
        let ty = types.resolve_alias(ty);
        let bounds = match ty {
            Type::ImplTrait(implementation) => &implementation.bounds,
            Type::TraitObject(object) => &object.bounds,
            Type::Reference(reference) => return self.callable_output(&reference.elem, scope),
            _ => {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            }
        };
        bounds.iter().find_map(|bound| {
            let syn::TypeParamBound::Trait(bound) = bound else { return None };
            let path = &bound.path;
            let ty = Type::Path(syn::TypePath {
                attrs: Vec::new(),
                qself: None,
                path: path.clone(),
            });
            let resolved = types.unshadowed_type_path(&ty)?;
            if !matches!(resolved.as_slice(), [name] if self.implicit_prelude && matches!(name.as_str(), "Fn" | "FnMut" | "FnOnce"))
                && !matches!(resolved.as_slice(), [root, module, name]
                    if matches!(root.as_str(), "core" | "std") && module == "ops" && matches!(name.as_str(), "Fn" | "FnMut" | "FnOnce"))
            {
                return None;
            }
            // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "unshadowed_type_path succeeded above and accepts only nonempty paths. The original bound path has not been modified, so its last segment is present.")]
            let PathArguments::Parenthesized(arguments) = &path.segments.last()?.arguments else {
                return None;
            };
            super::return_type(&arguments.output).map(|ty| types.frozen_type(ty))
        })
    }

    pub(super) fn standard_pattern_payload(&self, path: &syn::Path, expected: &Type) -> Option<Type> {
        let types = self.types();
        let expected = types.resolve_alias(expected);
        if let Type::Reference(reference) = strip(expected) {
            let payload = self.standard_pattern_payload(path, &reference.elem)?;
            let mut reference = reference.clone();
            reference.elem = Box::new(payload);
            return Some(Type::Reference(reference));
        }
        let constructor = Type::Path(syn::TypePath {
            attrs: Vec::new(),
            qself: None,
            path: path.clone(),
        });
        let constructor = types.unshadowed_type_path(&constructor)?;
        if constructor.iter().map(String::as_str).eq(["syn", "Type", "Path"])
            && types
                .unshadowed_type_path(expected)
                .is_some_and(|path| path.iter().map(String::as_str).eq(["syn", "Type"]))
        {
            return Some(syn::parse_quote!(::syn::TypePath));
        }
        let payload = match constructor.as_slice() {
            [name] if name == "Some" && self.implicit_prelude && types.supports_option_api(expected) => 0,
            [root, module, ty, name]
                if matches!(root.as_str(), "std" | "core")
                    && module == "option"
                    && ty == "Option"
                    && name == "Some"
                    && types.supports_option_api(expected) =>
            {
                0
            }
            [name] if name == "Ok" && self.implicit_prelude && types.supports_result_api(expected) => 0,
            [name] if name == "Err" && self.implicit_prelude && types.supports_result_api(expected) => 1,
            [root, module, ty, name]
                if matches!(root.as_str(), "std" | "core")
                    && module == "result"
                    && ty == "Result"
                    && name == "Ok"
                    && types.supports_result_api(expected) =>
            {
                0
            }
            [root, module, ty, name]
                if matches!(root.as_str(), "std" | "core")
                    && module == "result"
                    && ty == "Result"
                    && name == "Err"
                    && types.supports_result_api(expected) =>
            {
                1
            }
            _ => {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            }
        };
        types.payload(expected, payload).map(std::borrow::Cow::into_owned)
    }

    pub(super) fn is_read_receiver(&self, expression: &Expr) -> bool {
        if let Expr::MethodCall(call) = expression
            && (call.method == "take" || call.method == "by_ref" && call.args.is_empty())
        {
            // Without full method resolution, any local by_ref implementation could be the
            // selected method. The file-wide blocker deliberately sacrifices recognition.
            if call.method == "by_ref" && self.calls.has_method_named("by_ref") {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "false is already the default bool value.")]
                return false;
            }
            return self.is_read_receiver(&call.receiver);
        }
        if self
            .type_of_expression(expression)
            .is_some_and(|ty| self.is_read_type(ty, self.imports_of_expression(expression)))
        {
            return true;
        }
        let Some(ty) = self.inferred_expression_type(expression) else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "false is already the default bool value.")]
            return false;
        };
        self.is_read_type(&ty, self.current_imports())
    }

    fn read_trait_in_scope(&self) -> bool {
        self.current_imports().iter().any(|(name, path)| {
            !matches!(
                name.as_str(),
                super::types::SOURCE_FILE | super::types::LOCAL_MODULE | super::types::BLOCK_TYPES
            ) && path.as_ref().is_some_and(|path| {
                let mut path = import_target(path).iter().map(String::as_str);
                matches!(
                    (path.next(), path.next(), path.next(), path.next()),
                    (Some("std"), Some("io"), Some("Read" | "BufRead"), None)
                )
            })
        })
    }

    fn is_read_type(&self, ty: &Type, imports: &crate::HashMap<String, Option<Vec<String>>>) -> bool {
        let types = self.types_with(imports);
        let ty = types.resolve_alias(ty);
        if let Type::Reference(reference) = strip(ty) {
            return self.is_read_type(&reference.elem, imports);
        }
        let bounds = match strip(ty) {
            Type::ImplTrait(implementation) => Some(&implementation.bounds),
            Type::TraitObject(object) => Some(&object.bounds),
            _ => None,
        };
        if let Some(bounds) = bounds {
            return bounds.iter().any(|bound| {
                matches!(bound, syn::TypeParamBound::Trait(bound)
                    if super::types::Types::is_standard_reader_path(&bound.path, imports))
            });
        }
        if matches!(strip(ty), Type::Path(path)
            if path.path.get_ident().is_some_and(|name| self.reader_bounded.contains(&name.to_string())))
        {
            return true;
        }
        // Bounds already identify the reader trait; concrete types additionally need a lexical
        // trait import so similarly named methods are not mistaken for standard reader APIs.
        if !self.read_trait_in_scope() {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "false is already the default bool value.")]
            return false;
        }
        let Some(path) = types.unshadowed_type_path(ty) else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "false is already the default bool value.")]
            return false;
        };
        matches!(
            path.as_slice(),
            [root, module, name] if root == "std" && matches!(
                (module.as_str(), name.as_str()),
                ("fs", "File")
                    | ("io", "BufReader" | "Cursor" | "Stdin" | "StdinLock" | "Take" | "Chain" | "Empty" | "Repeat")
                    | ("net", "TcpStream")
            )
        ) || path.iter().map(String::as_str).eq(["std", "os", "unix", "net", "UnixStream"])
            || matches!(path.as_slice(), [root, module, name]
                if root == "flate2" && matches!(module.as_str(), "read" | "bufread")
                    && matches!(name.as_str(), "GzDecoder" | "MultiGzDecoder" | "ZlibDecoder" | "DeflateDecoder"))
    }

    pub(super) fn some_constructor(&self, expression: &Expr) -> bool {
        match expression {
            Expr::Paren(paren) => return self.some_constructor(&paren.expr),
            Expr::Group(group) => return self.some_constructor(&group.expr),
            _ => {}
        }
        let Expr::Path(callee) = expression else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "false is already the default bool value.")]
            return false;
        };
        if callee.qself.is_some() {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "false is already the default bool value.")]
            return false;
        }
        let mut path = callee.path.clone();
        if path.segments.len() == 1
            && let Some(name) = path.get_ident()
            && let Some(Some(imported)) = self.current_imports().get(&name.to_string())
            && let Ok(imported) = syn::parse_str::<syn::Path>(&import_target(imported).join("::"))
        {
            path = imported;
        }
        if path.segments.last().is_none_or(|segment| segment.ident != "Some") {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "false is already the default bool value.")]
            return false;
        }
        if
        // #[gamma::skip(cond.always_false, reason = "The path is source-parsed and nonempty. For a singleton Some, omitting the early true pops that sole segment. The empty path cannot resolve to a standard Option or a nominal declaration in Calls or source identities, so the final !is_enum && !source_is_enum returns true as before. Qualified paths never entered this early branch.", tag = "equivalent")]
        path.segments.len() ==
        // #[gamma::skip(literal.int_decrement, reason = "The path is source-parsed and nonempty. For a singleton Some, omitting the early true pops that sole segment. The empty path cannot resolve to a standard Option or a nominal declaration in Calls or source identities, so the final !is_enum && !source_is_enum returns true as before. Qualified paths never entered this early branch.", tag = "equivalent")]
        1
        {
            return true;
        }
        let _constructor = path.segments.pop();
        let _separator = path.segments.pop_punct();
        let ty = Type::Path(syn::TypePath {
            attrs: Vec::new(),
            qself: None,
            path: path.clone(),
        });
        if
        // #[gamma::skip(cond.always_false, reason = "A positive supports_option_api answer identifies the standard Option, excluding local or imported source enums and module/type shadows. The final fallback's calls.is_enum and source_is_enum therefore both return false for every positive early-return case. Disabling the early return still reaches the same true answer.", tag = "equivalent")]
        self.types().supports_option_api(&ty) {
            return true;
        }
        if let Some(Some(none)) = self.current_imports().get("None")
            && let Ok(mut none) = syn::parse_str::<syn::Path>(&import_target(none).join("::"))
            && none.segments.last().is_some_and(|segment| segment.ident == "None")
        {
            let _variant = none.segments.pop();
            let _separator = none.segments.pop_punct();
            if self.calls.same_type(&path, &none, &self.scope_path, self.current_imports()) || self.types().source_same_type(&path, &none) {
                return true;
            }
        }
        !self.calls.is_enum(&path, &self.scope_path, self.current_imports()) && !self.types().source_is_enum(&ty)
    }

    pub(super) fn standard_constructor_type(&self, call: &syn::ExprCall) -> Option<Type> {
        self.standard_constructor_type_in_scope(call, &self.scope_path)
    }

    pub(super) fn standard_constructor_type_in_scope(&self, call: &syn::ExprCall, scope: &ScopePath) -> Option<Type> {
        let Expr::Path(callee) = &*call.func else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        if callee.qself.is_some() {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let constructor = Type::Path(syn::TypePath {
            attrs: Vec::new(),
            qself: None,
            path: callee.path.clone(),
        });
        if self.types_in_scope(scope).unshadowed_type_path(&constructor).is_some_and(|path| {
            matches!(path.as_slice(),
                [name] if matches!(name.as_str(), "Ok" | "Err") && self.implicit_prelude)
                || matches!(
                    path.as_slice(),
                    [root, module, ty, name] if matches!(root.as_str(), "std" | "core")
                        && module == "result" && ty == "Result" && matches!(name.as_str(), "Ok" | "Err")
                )
        }) {
            return Some(syn::parse_quote!(std::result::Result<_, _>));
        }
        if
        // #[gamma::skip(cond.always_false, reason = "A source-parsed callee path is nonempty. Omitting the <2 guard only admits singleton paths not already handled by the Result constructor special case. Popping their sole segment leaves an empty type path; unshadowed_type_path calls resolved_type_path and then first()?, which returns None. Thus the function still returns None before any factory match.", tag = "equivalent")]
        callee.path.segments.len() <
        // #[gamma::skip(literal.int_decrement, literal.int_to_zero, reason = "A source-parsed callee path is nonempty. Omitting the <2 guard only admits singleton paths not already handled by the Result constructor special case. Popping their sole segment leaves an empty type path; unshadowed_type_path calls resolved_type_path and then first()?, which returns None. Thus the function still returns None before any factory match.", tag = "equivalent")]
        2
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let (ty, method, resolved) = self.constructor_target(call, scope)?;
        if payload_constructor(&resolved, &method) {
            let types = self.types_in_scope(scope);
            let infer_alias_payload = matches!(&ty, Type::Path(path)
                if path.path.get_ident()
                    .and_then(|name| types.aliases?.get(&name.to_string())?.as_ref())
                    .is_some_and(|alias| !alias.parameters.is_empty()));
            let mut ty = types.frozen_type(&ty);
            if let Type::Path(path) = &mut ty
                && let Some(owner) = path.path.segments.last_mut()
                && (owner.arguments.is_none() || infer_alias_payload)
            {
                let inner = self.inferred_expression_type_in_scope(call.args.first()?, scope)?;
                owner.arguments = PathArguments::AngleBracketed(syn::parse_quote!(<#inner>));
            }
            return Some(ty);
        }
        match (resolved.as_slice(), method.as_str()) {
            ([name], "new" | "from") if name == "String" => Some(ty),
            ([root, module, name], "new" | "from")
                if matches!(root.as_str(), "std" | "alloc") && module == "string" && name == "String" =>
            {
                Some(ty)
            }
            ([root, module, name], "open" | "create" | "create_new") if root == "std" && module == "fs" && name == "File" => {
                Some(syn::parse_quote!(std::result::Result<#ty, std::io::Error>))
            }
            ([root, module, name], method)
                if root == "std" && module == "fs" && matches!((name.as_str(), method), ("File", "options") | ("OpenOptions", "new")) =>
            {
                Some(syn::parse_quote!(std::fs::OpenOptions))
            }
            ([root, module, name], "new") if root == "std" && module == "io" && matches!(name.as_str(), "BufReader" | "Cursor") => Some(ty),
            ([root, module, name], "new")
                if root == "flate2"
                    && matches!(module.as_str(), "read" | "bufread")
                    && matches!(name.as_str(), "GzDecoder" | "MultiGzDecoder" | "ZlibDecoder" | "DeflateDecoder") =>
            {
                Some(ty)
            }
            ([root, name], method)
                if root == "chrono"
                    && matches!(name.as_str(), "Duration" | "TimeDelta")
                    && matches!(
                        method,
                        "zero" | "weeks" | "days" | "hours" | "minutes" | "seconds" | "milliseconds" | "microseconds" | "nanoseconds"
                    ) =>
            {
                Some(ty)
            }
            ([root, module, name], "from_secs" | "from_millis" | "from_micros" | "from_nanos" | "new")
                if matches!(root.as_str(), "std" | "core") && module == "time" && name == "Duration" =>
            {
                Some(syn::parse_quote!(std::time::Duration))
            }
            _ => None,
        }
    }

    fn constructor_target(&self, call: &syn::ExprCall, scope: &ScopePath) -> Option<(Type, String, Vec<String>)> {
        let Expr::Path(callee) = &*call.func else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        if
        // #[gamma::skip(cond.always_false, reason = "Factory inference rejects qself before calling this helper. Its other consumer only recognizes verified concrete standard payload-constructor owners: a well-typed qualified trait path cannot name those structs, while qualified inherent paths leave no owner segments. Removing this duplicate guard changes neither supported factory inference nor payload dependencies.", tag = "equivalent")]
        callee.qself.is_some() {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let mut path = callee.path.clone();
        let method =
        // #[gamma::skip(try.propagate_to_unwrap, reason = "Both consumers pass source-parsed Expr::Path callees, whose paths are nonempty. This is the first removal from the cloned segment list, so pop always returns a segment even for a singleton path.", tag = "equivalent")]
        path.segments.pop()?.ident.to_string();
        let _separator = path.segments.pop_punct();
        let ty = Type::Path(syn::TypePath {
            attrs: Vec::new(),
            qself: None,
            path,
        });
        let resolved = self.types_in_scope(scope).unshadowed_type_path(&ty)?;
        Some((ty, method, resolved))
    }

    pub(super) fn constructor_uses_payload(&self, call: &syn::ExprCall, scope: &ScopePath) -> bool {
        self.constructor_target(call, scope)
            .is_some_and(|(_, method, path)| payload_constructor(&path, &method))
    }
}

fn payload_constructor(path: &[String], method: &str) -> bool {
    method == "new"
        && (matches!(path, [name] if name == "Box")
            || matches!(path, [root, module, name]
            if matches!(
                (root.as_str(), module.as_str(), name.as_str()),
                ("std" | "alloc", "boxed", "Box")
                    | ("std" | "alloc", "rc", "Rc")
                    | ("std" | "alloc", "sync", "Arc")
                    | ("std" | "core", "cell", "RefCell" | "Cell")
                    | ("std", "sync", "Mutex" | "RwLock")
            )))
}

#[cfg(test)]
mod tests {
    use super::Collector;
    use crate::cfg::CfgSet;
    use crate::ops::collect::Defaults;
    use crate::ops::collect::collector::types::LOCAL_MODULE;
    use crate::ops::registry::Selection;
    use crate::parse::SourceFile;

    #[test]
    fn reader_named_module_metadata_is_not_a_reader_trait_import() {
        for name in ["Read", "BufRead"] {
            let source = format!("mod std {{ mod io {{ mod {name} {{}} }} }}");
            let file = SourceFile::parse("fixture.rs", source).unwrap();
            let selection = Selection::everything();
            let cfg = CfgSet::parse("");
            let defaults = Defaults::of(&file.ast);
            let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
            let expected = vec!["std".to_owned(), "io".to_owned(), name.to_owned()];
            collector.scope_path = collector
                .scope_imports
                .iter()
                .find(|(_, imports)| imports.get(LOCAL_MODULE).and_then(Option::as_ref) == Some(&expected))
                .map(|(scope, _)| scope.clone())
                .unwrap();
            assert!(!collector.read_trait_in_scope(), "{name}");
        }
    }
}
