// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Negative evidence for APIs that share iterator and option spellings.

use syn::{Expr, Type};

use crate::ops::collect::collector::Collector;
use crate::ops::collect::collector::indexes::ABSOLUTE_ROOT;
use crate::ops::collect::collector::values::strip;

impl Collector<'_> {
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
            _ => return None,
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
                let mut path = path.iter().filter(|name| name.as_str() != ABSOLUTE_ROOT).map(String::as_str);
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
            return false;
        }
        let Some(path) = types.unshadowed_type_path(ty) else { return false };
        matches!(
            path.as_slice(),
            [root, module, name] if root == "std" && matches!(
                (module.as_str(), name.as_str()),
                ("fs", "File")
                    | ("io", "BufReader" | "Cursor" | "Stdin" | "StdinLock" | "Take" | "Chain" | "Empty" | "Repeat")
                    | ("net", "TcpStream")
            )
        ) || path.iter().map(String::as_str).eq(["std", "os", "unix", "net", "UnixStream"])
    }

    pub(super) fn some_constructor(&self, expression: &Expr) -> bool {
        match expression {
            Expr::Paren(paren) => return self.some_constructor(&paren.expr),
            Expr::Group(group) => return self.some_constructor(&group.expr),
            _ => {}
        }
        let Expr::Path(callee) = expression else { return false };
        if callee.qself.is_some() {
            return false;
        }
        let mut path = callee.path.clone();
        if path.segments.len() == 1
            && let Some(name) = path.get_ident()
            && let Some(Some(imported)) = self.current_imports().get(&name.to_string())
            && let Ok(imported) = syn::parse_str::<syn::Path>(
                &imported
                    .iter()
                    .filter(|name| name.as_str() != ABSOLUTE_ROOT)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("::"),
            )
        {
            path = imported;
        }
        if path.segments.last().is_none_or(|segment| segment.ident != "Some") {
            return false;
        }
        if path.segments.len() == 1 {
            return true;
        }
        let _constructor = path.segments.pop();
        let _separator = path.segments.pop_punct();
        let ty = Type::Path(syn::TypePath {
            attrs: Vec::new(),
            qself: None,
            path: path.clone(),
        });
        if self.types().supports_option_api(&ty) {
            return true;
        }
        if let Some(Some(none)) = self.current_imports().get("None")
            && let Ok(mut none) = syn::parse_str::<syn::Path>(
                &none
                    .iter()
                    .filter(|name| name.as_str() != ABSOLUTE_ROOT)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("::"),
            )
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
        let Expr::Path(callee) = &*call.func else { return None };
        if callee.qself.is_some() {
            return None;
        }
        let constructor = Type::Path(syn::TypePath {
            attrs: Vec::new(),
            qself: None,
            path: callee.path.clone(),
        });
        if self.types().unshadowed_type_path(&constructor).is_some_and(|path| {
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
        if callee.path.segments.len() < 2 {
            return None;
        }
        let mut path = callee.path.clone();
        let method = path.segments.pop()?.ident.to_string();
        let _separator = path.segments.pop_punct();
        let ty = Type::Path(syn::TypePath {
            attrs: Vec::new(),
            qself: None,
            path,
        });
        let resolved = self.types().unshadowed_type_path(&ty)?;
        match (resolved.as_slice(), method.as_str()) {
            ([name], "new") if name == "Box" => {
                let inner = self.inferred_expression_type(call.args.first()?)?;
                Some(syn::parse_quote!(#ty<#inner>))
            }
            ([root, module, name], "new")
                if matches!(
                    (root.as_str(), module.as_str(), name.as_str()),
                    ("std" | "alloc", "boxed", "Box")
                        | ("std" | "alloc", "rc", "Rc")
                        | ("std" | "alloc", "sync", "Arc")
                        | ("std" | "core", "cell", "RefCell" | "Cell")
                        | ("std", "sync", "Mutex" | "RwLock")
                ) =>
            {
                let inner = self.inferred_expression_type(call.args.first()?)?;
                Some(syn::parse_quote!(#ty<#inner>))
            }
            ([name], "new" | "from") if name == "String" => Some(syn::parse_quote!(String)),
            ([root, module, name], "new" | "from")
                if matches!(root.as_str(), "std" | "alloc") && module == "string" && name == "String" =>
            {
                Some(syn::parse_quote!(String))
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
            ([root, module, name], "from_secs" | "from_millis" | "from_micros" | "from_nanos" | "new")
                if matches!(root.as_str(), "std" | "core") && module == "time" && name == "Duration" =>
            {
                Some(syn::parse_quote!(std::time::Duration))
            }
            _ => None,
        }
    }
}
