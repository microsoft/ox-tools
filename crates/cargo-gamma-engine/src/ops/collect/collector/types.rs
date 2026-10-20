// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! What the file's own declarations say about the types a function mentions.

use std::borrow::Cow;

use syn::{GenericArgument, GenericParam, Generics, Path, PathArguments, ReturnType, Type, TypeParamBound, WherePredicate};

use super::super::defaults::{DefaultPaths, standard_defaulted_parameters};
use super::indexes::ABSOLUTE_ROOT;
use super::predicates::payload;
use super::values::{Kind, primitive_kind, resolve_type, strip, type_argument_values, type_arguments, type_name};
use crate::ops::collect::Defaults;
use crate::{HashMap, HashSet};

#[derive(Clone, PartialEq)]
pub(super) struct Alias {
    pub(super) parameters: Vec<String>,
    pub(super) target: Type,
}

fn substitute_type(ty: &Type, substitutions: &HashMap<String, Type>) -> Type {
    if let Type::Path(path) = ty
        && path.qself.is_none()
        && let Some(ident) = path.path.get_ident()
        && let Some(replacement) = substitutions.get(&ident.to_string())
    {
        return replacement.clone();
    }

    let mut substituted = ty.clone();
    match &mut substituted {
        Type::Array(array) => *array.elem = substitute_type(&array.elem, substitutions),
        Type::Group(group) => *group.elem = substitute_type(&group.elem, substitutions),
        Type::Paren(paren) => *paren.elem = substitute_type(&paren.elem, substitutions),
        Type::Path(path) => {
            for segment in &mut path.path.segments {
                if let PathArguments::AngleBracketed(arguments) = &mut segment.arguments {
                    for argument in &mut arguments.args {
                        if let GenericArgument::Type(inner) = argument {
                            *inner = substitute_type(inner, substitutions);
                        }
                    }
                }
            }
        }
        Type::Ptr(pointer) => *pointer.elem = substitute_type(&pointer.elem, substitutions),
        Type::Reference(reference) => *reference.elem = substitute_type(&reference.elem, substitutions),
        Type::Slice(slice) => *slice.elem = substitute_type(&slice.elem, substitutions),
        Type::Tuple(tuple) => {
            for element in &mut tuple.elems {
                *element = substitute_type(element, substitutions);
            }
        }
        _ => {}
    }
    substituted
}

/// What the value-choosing functions know about the file they are reasoning inside.
///
/// The two facts travel together through the whole recursion and neither is ever used without the
/// other, so they are carried as one thing rather than as a growing parameter list.
pub(super) struct Types<'a> {
    /// Type names in scope that cannot be constructed: type parameters without a `Default` bound,
    /// associated types, and trait objects.
    pub(super) abstracts: &'a [String],

    /// Type parameters carrying an applicable standard `Default` bound.
    pub(super) defaulted: &'a [String],

    /// The module path each imported name came from, so a bare type name can be traced to its crate.
    pub(super) imports: &'a HashMap<String, Option<Vec<String>>>,

    /// What the workspace's own sources say about which of their types implement `Default`.
    pub(super) defaults: &'a Defaults,

    /// Locally visible aliases, used to preserve their declared shape.
    pub(super) aliases: Option<&'a HashMap<String, Option<Alias>>>,

    /// The concrete type named by `Self` inside an `impl` block.
    pub(super) self_type: Option<&'a Type>,

    /// Concrete associated types declared by the enclosing `impl`.
    pub(super) self_associated: Option<&'a HashMap<String, Type>>,
}

impl Types<'_> {
    /// Resolves a source-visible path to a declaration name in the current package.
    ///
    /// Bare unimported names and unrenamed imports rooted at `crate`, `self`, or `super` are local.
    /// Written qualified paths and renamed imports remain unknown: the name-only index cannot
    /// distinguish declarations in different modules with the same final identifier.
    fn package_type_name(&self, ty: &Type) -> Option<String> {
        if self.imports.contains_key("*") {
            return None;
        }

        let written_name = match strip(ty) {
            Type::Path(path) if path.qself.is_none() && path.path.segments.len() == 1 => {
                path.path.segments.first().map(|segment| segment.ident.to_string())
            }
            _ => None,
        };
        let path = resolved_type_path(ty, self.imports)?;
        let last = path.last()?.clone();

        let local = path.len() == 1
            || (written_name.as_deref() == Some(last.as_str())
                && matches!(path.first().map(String::as_str), Some("crate" | "self" | "super")));
        local.then_some(last)
    }

    fn package_has_default(&self, ty: &Type) -> bool {
        self.package_type_name(ty).is_some_and(|name| self.defaults.has_default_name(&name))
    }

    fn package_declares_default(&self, ty: &Type) -> bool {
        self.package_type_name(ty)
            .is_some_and(|name| self.defaults.declares_default_name(&name))
    }

    fn package_lacks_default(&self, ty: &Type) -> bool {
        let name = self.package_type_name(ty);
        name.is_some_and(|name| self.defaults.lacks_default_name(&name))
    }

    fn package_defines(&self, ty: &Type) -> bool {
        self.package_type_name(ty).is_some_and(|name| self.defaults.defines(&name))
    }

    fn instantiate_alias(&self, ty: &Type) -> Option<Type> {
        let aliases = self.aliases?;
        let mut resolved = ty.clone();
        let mut visited = HashSet::default();
        let mut changed = false;

        loop {
            let Type::Path(path) = strip(&resolved) else {
                return changed.then_some(resolved);
            };
            if path.qself.is_some() || path.path.segments.len() != 1 {
                return changed.then_some(resolved);
            }

            let name = path.path.segments[0].ident.to_string();
            if self.imports.contains_key(&name) || self.defaults.defines(&name) {
                return changed.then_some(resolved);
            }
            // #[gamma::skip(cond.always_false, tag = "timeout", reason = "removing cycle detection makes mutually recursive generic aliases resolve forever")]
            if !visited.insert(name.clone()) {
                return None;
            }
            let Some(alias) = aliases.get(&name).and_then(Option::as_ref) else {
                return changed.then_some(resolved);
            };
            let arguments = super::values::type_argument_values(&resolved);
            let substitutions = alias
                .parameters
                .iter()
                .zip(arguments)
                .map(|(parameter, argument)| (parameter.clone(), argument.clone()))
                .collect();
            resolved = substitute_type(&alias.target, &substitutions);
            changed = true;
        }
    }

    pub(super) fn resolve_alias<'a>(&'a self, ty: &'a Type) -> &'a Type {
        let Some(aliases) = self.aliases else {
            return ty;
        };
        let mut resolved = ty;
        let mut visited = HashSet::default();

        loop {
            let Type::Path(path) = strip(resolved) else {
                return resolved;
            };
            let Some(name) = (path.path.segments.len() == 1).then(|| path.path.segments[0].ident.to_string()) else {
                return resolved;
            };
            if self.imports.contains_key(&name) || self.defaults.defines(&name) {
                return resolved;
            }
            // #[gamma::skip(cond.always_false, tag = "timeout", reason = "removing cycle detection makes mutually recursive aliases resolve forever")]
            if !visited.insert(name.clone()) {
                return ty;
            }
            let Some(target) = aliases.get(&name).and_then(Option::as_ref) else {
                return resolved;
            };
            resolved = &target.target;
        }
    }

    pub(super) fn primitive_kind(&self, ty: &Type) -> Kind {
        let mut resolved = ty;
        let mut visited = HashSet::default();
        loop {
            if !visited.insert(core::ptr::from_ref(resolved)) {
                return Kind::Unknown;
            }
            resolved = self.resolve_alias(resolved);
            match strip(resolved) {
                Type::Reference(reference) => resolved = &reference.elem,
                concrete => return resolve_type(concrete),
            }
        }
    }

    pub(super) fn payload<'a>(&'a self, ty: &'a Type, index: usize) -> Option<Cow<'a, Type>> {
        if let Some(resolved) = self.instantiate_alias(ty) {
            return payload(&resolved, index).map(|payload| Cow::Owned(payload.clone()));
        }

        let resolved = self.resolve_alias(ty);
        let payload = payload(resolved, index)?;
        Some(Cow::Borrowed(payload))
    }

    /// Returns positive source-visible evidence that a value of `ty` can be defaulted.
    pub(super) fn has_default(&self, ty: &Type) -> bool {
        let ty = self.resolve_alias(ty);
        let concrete = self
            .concrete_self_type(ty)
            .or_else(|| self.concrete_self_associated_type(ty))
            .unwrap_or(ty);
        let kind = resolve_type(concrete);
        if matches!(kind, Kind::Collection | Kind::Map) && !self.supports_collection_api(concrete) {
            return self.renamed_collection_has_default(concrete);
        }

        match kind {
            Kind::Unit | Kind::Bool | Kind::Signed | Kind::Unsigned | Kind::Float | Kind::String => true,
            Kind::Option => {
                self.supports_option_api(concrete) || self.package_has_default(concrete) || self.local_generic_default(concrete)
            }
            Kind::Collection => {
                if self.package_defines(concrete) {
                    return !self.defaults.is_complete() || self.local_generic_default(concrete);
                }

                let name = type_name(concrete).map(ToString::to_string).unwrap_or_default();
                if name == "HashSet" {
                    payload(concrete, 1).is_none_or(|hasher| self.has_default(hasher))
                } else {
                    true
                }
            }
            Kind::Map => {
                if self.package_defines(concrete) {
                    return !self.defaults.is_complete() || self.local_generic_default(concrete);
                }
                let name = type_name(concrete).map(ToString::to_string).unwrap_or_default();
                if name == "HashMap" {
                    payload(concrete, 2).is_none_or(|hasher| self.has_default(hasher))
                } else {
                    true
                }
            }

            Kind::Wrapper => payload(concrete, 0).is_some_and(|inner| self.has_default(inner)),
            Kind::Array => {
                let array = array_type(concrete);
                let syn::Expr::Lit(length) = &array.len else {
                    return false;
                };
                let syn::Lit::Int(length) = &length.lit else {
                    return false;
                };
                let Ok(length) = length.base10_parse::<usize>() else {
                    return false;
                };

                length == 0 || length <= 32 && (self.has_source_independent_reference_default(&array.elem) || self.has_default(&array.elem))
            }
            Kind::Tuple => {
                matches!(strip(concrete), Type::Tuple(tuple) if tuple.elems.iter().all(|element| self.has_default(element)))
            }
            Kind::Unknown => {
                let Type::Path(path) = strip(concrete) else {
                    return false;
                };
                if path.path.is_ident("str") {
                    return false;
                }
                if resolved_type_path(concrete, self.imports).is_some_and(|segments| {
                    matches!(segments.as_slice(), [root, module, name]
                        if matches!(root.as_str(), "std" | "core") && module == "fmt" && name == "Error")
                }) {
                    return true;
                }
                if path.path.segments.last().is_some_and(|segment| segment.ident == "RandomState") {
                    return true;
                }
                path.path
                    .get_ident()
                    .is_some_and(|ident| self.defaulted.contains(&ident.to_string()))
                    || self.package_has_default(concrete)
            }
            Kind::Result | Kind::StaticStr | Kind::MutStr | Kind::NonZero | Kind::Cow | Kind::Iterator | Kind::Reference => false,
        }
    }

    pub(super) fn has_proven_default(&self, ty: &Type) -> bool {
        let ty = self.resolve_alias(ty);
        let concrete = self
            .concrete_self_type(ty)
            .or_else(|| self.concrete_self_associated_type(ty))
            .unwrap_or(ty);
        let kind = resolve_type(concrete);
        if matches!(strip(concrete), Type::Path(path)
            if path.path.get_ident().is_some_and(|ident| self.defaulted.contains(&ident.to_string())))
        {
            return true;
        }
        if self.package_declares_default(concrete) || self.local_generic_default(concrete) {
            return true;
        }
        if matches!(kind, Kind::Collection | Kind::Map) && !self.supports_collection_api(concrete) {
            let Some(path) = resolved_type_path(concrete, self.imports) else {
                return false;
            };

            return matches!(
                path.as_slice(),
                [root, name]
                    if root == "rustc_hash"
                        && matches!(name.as_str(), "FxHashMap" | "FxHashSet")
            ) || self.package_declares_default(concrete)
                || self.local_generic_default(concrete);
        }

        match kind {
            kind @ (Kind::Bool | Kind::Signed | Kind::Unsigned | Kind::Float) => self.supports_primitive_kind(concrete, kind),
            Kind::Option => {
                self.supports_option_api(concrete) || self.package_declares_default(concrete) || self.local_generic_default(concrete)
            }
            Kind::String => self.supports_string_api(concrete) || self.package_declares_default(concrete),
            Kind::Wrapper => {
                self.supports_wrapper_api(concrete) && payload(concrete, 0).is_some_and(|inner| self.has_proven_default(inner))
            }
            Kind::Array => {
                let array = array_type(concrete);
                let syn::Expr::Lit(length) = &array.len else {
                    return false;
                };
                let syn::Lit::Int(length) = &length.lit else {
                    return false;
                };
                let Ok(length) = length.base10_parse::<usize>() else {
                    return false;
                };

                length == 0
                    || length <= 32 && (self.has_source_independent_reference_default(&array.elem) || self.has_proven_default(&array.elem))
            }
            Kind::Tuple => {
                matches!(strip(concrete), Type::Tuple(tuple)
                    if tuple.elems.iter().all(|element| self.has_proven_default(element)))
            }
            Kind::Unknown if self.package_type_name(concrete).is_some() => {
                self.package_declares_default(concrete) || self.local_generic_default(concrete)
            }
            _ => self.has_default(concrete),
        }
    }

    /// Whether a collection-shaped local type must use the trait constructor.
    ///
    /// Standard collections have stable inherent `new` constructors. A local type may deliberately
    /// reuse one of their names without providing that constructor. An imported alias may likewise
    /// expose a standard collection name while resolving to another concrete type. Positive
    /// `Default` evidence is then the only constructor syntax the source proves.
    pub(super) fn prefers_default_constructor(&self, ty: &Type) -> bool {
        let ty = self.resolve_alias(ty);
        let concrete = self
            .concrete_self_type(ty)
            .or_else(|| self.concrete_self_associated_type(ty))
            .unwrap_or(ty);
        let imported_alias = self.import_changes_name(concrete);

        self.has_default(concrete)
            && (imported_alias || self.package_defines(concrete) && (self.local_generic_default(concrete) || !self.defaults.is_complete()))
    }

    pub(super) fn supports_primitive_kind(&self, ty: &Type, expected: Kind) -> bool {
        self.supports_primitive_kind_with(ty, expected, &mut HashSet::default())
    }

    fn supports_primitive_kind_with(&self, ty: &Type, expected: Kind, visited: &mut HashSet<*const Type>) -> bool {
        if !visited.insert(core::ptr::from_ref(ty)) {
            return false;
        }
        let ty = self.resolve_alias(ty);
        if let Type::Reference(reference) = strip(ty) {
            return self.supports_primitive_kind_with(&reference.elem, expected, visited);
        }
        let Some(path) = resolved_type_path(ty, self.imports) else {
            return false;
        };
        let name = match path.as_slice() {
            [name] if !self.defaults.defines(name) && !self.imports.contains_key("*") => name,
            [root, module, name] if matches!(root.as_str(), "std" | "core") && module == "primitive" => name,
            _ => return false,
        };

        primitive_kind(name) == Some(expected)
    }

    pub(super) fn import_changes_name(&self, ty: &Type) -> bool {
        type_name(ty).is_some_and(|written| {
            resolved_type_path(ty, self.imports)
                .and_then(|path| path.last().cloned())
                .is_some_and(|resolved| *written != resolved)
        })
    }

    pub(super) fn supports_collection_api(&self, ty: &Type) -> bool {
        let ty = self.resolve_alias(ty);
        if self.package_defines(ty) {
            return false;
        }

        let Some(path) = resolved_type_path(ty, self.imports) else {
            return false;
        };
        let collection_kind = |name: &str| match name {
            "Vec" | "VecDeque" | "HashSet" | "BTreeSet" | "BinaryHeap" | "LinkedList" | "FxHashSet" => Some(Kind::Collection),
            "HashMap" | "BTreeMap" | "FxHashMap" => Some(Kind::Map),
            _ => None,
        };
        let collection_module = |name: &str| match name {
            "VecDeque" => Some("vec_deque"),
            "HashSet" => Some("hash_set"),
            "BTreeSet" => Some("btree_set"),
            "BinaryHeap" => Some("binary_heap"),
            "LinkedList" => Some("linked_list"),
            "HashMap" => Some("hash_map"),
            "BTreeMap" => Some("btree_map"),
            _ => None,
        };
        let written_kind = path
            .last()
            .and_then(|name| collection_kind(name))
            .unwrap_or_else(|| resolve_type(ty));

        match path.as_slice() {
            [name] => !self.imports.contains_key("*") && collection_kind(name) == Some(written_kind),
            [root, module, name]
                if matches!(root.as_str(), "std" | "alloc")
                    && matches!(module.as_str(), "vec" | "collections")
                    && collection_kind(name) == Some(written_kind) =>
            {
                true
            }
            [root, collections, module, name]
                if matches!(root.as_str(), "std" | "alloc")
                    && collections == "collections"
                    && collection_module(name) == Some(module.as_str())
                    && collection_kind(name) == Some(written_kind) =>
            {
                true
            }
            [root, name] if root == "rustc_hash" && collection_kind(name) == Some(written_kind) => true,
            _ => false,
        }
    }

    pub(super) fn supports_collection_constructor(&self, ty: &Type) -> bool {
        if !self.supports_collection_api(ty) {
            return false;
        }
        let ty = self.resolve_alias(ty);
        let Type::Path(path) = strip(ty) else {
            return false;
        };
        let Some(segment) = path.path.segments.last() else {
            return false;
        };
        let base_arity = if resolve_type(ty) == Kind::Map { 2 } else { 1 };

        type_arguments(segment) == base_arity
    }

    pub(super) fn supports_option_api(&self, ty: &Type) -> bool {
        self.supports_standard_type(ty, "Option", "option", &["std", "core"])
    }

    pub(super) fn supports_result_api(&self, ty: &Type) -> bool {
        self.supports_standard_type(ty, "Result", "result", &["std", "core"])
    }

    pub(super) fn supports_string_api(&self, ty: &Type) -> bool {
        self.supports_standard_type(ty, "String", "string", &["std", "alloc"])
    }

    pub(super) fn supports_peekable_chars(&self, ty: &Type) -> bool {
        self.supports_peekable_chars_with(ty, &mut HashSet::default())
    }

    fn supports_peekable_chars_with(&self, ty: &Type, visited: &mut HashSet<*const Type>) -> bool {
        if !visited.insert(core::ptr::from_ref(ty)) {
            return false;
        }
        if matches!(strip(ty), Type::Path(path) if path.path.segments.first().is_some_and(|segment| {
            let name = segment.ident.to_string();
            self.abstracts.contains(&name) || self.defaulted.contains(&name)
        })) {
            return false;
        }
        let ty = self.resolve_alias(ty);
        if let Type::Reference(reference) = strip(ty) {
            return reference.mutability.is_some() && self.supports_peekable_chars_with(&reference.elem, visited);
        }
        let Type::Path(path) = strip(ty) else {
            return false;
        };
        let Some(resolved) = resolved_type_path(ty, self.imports) else {
            return false;
        };
        if !self.standard_root_unshadowed(&resolved)
            || !matches!(resolved.as_slice(), [root, module, name]
                if matches!(root.as_str(), "std" | "core") && module == "iter" && name == "Peekable")
        {
            return false;
        }
        let Some(segment) = path.path.segments.last() else {
            return false;
        };
        let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
            return false;
        };
        if arguments.args.len() != 1 {
            return false;
        }
        let Some(GenericArgument::Type(element)) = arguments.args.first() else {
            return false;
        };
        if matches!(strip(element), Type::Path(path) if path.path.segments.first().is_some_and(|segment| {
            let name = segment.ident.to_string();
            self.abstracts.contains(&name) || self.defaulted.contains(&name)
        })) {
            return false;
        }
        let Some(resolved) = resolved_type_path(element, self.imports) else {
            return false;
        };
        self.standard_root_unshadowed(&resolved)
            && matches!(resolved.as_slice(), [root, module, name]
                if matches!(root.as_str(), "std" | "core") && module == "str" && name == "Chars")
    }

    pub(super) fn supports_char_api(&self) -> bool {
        let ty: Type = syn::parse_quote!(char);
        let Some(path) = resolved_type_path(&ty, self.imports) else {
            return false;
        };
        !self.abstracts.iter().chain(self.defaulted).any(|name| name == "char")
            && !self.defaults.defines("char")
            && !self.imports.contains_key("*")
            && self.standard_root_unshadowed(&path)
            && (matches!(path.as_slice(), [name] if name == "char")
                || matches!(path.as_slice(), [root, module, name]
                    if matches!(root.as_str(), "std" | "core") && module == "primitive" && name == "char"))
    }

    pub(super) fn supports_str_reference(&self, ty: &Type) -> bool {
        if matches!(strip(ty), Type::Path(path) if path.path.segments.first().is_some_and(|segment| {
            let name = segment.ident.to_string();
            self.abstracts.contains(&name) || self.defaulted.contains(&name)
        })) {
            return false;
        }
        let Type::Reference(reference) = strip(self.resolve_alias(ty)) else {
            return false;
        };
        let Some(path) = resolved_type_path(&reference.elem, self.imports) else {
            return false;
        };
        !self.abstracts.iter().chain(self.defaulted).any(|name| name == "str")
            && !self.defaults.defines("str")
            && !self.imports.contains_key("*")
            && self.standard_root_unshadowed(&path)
            && (matches!(path.as_slice(), [name] if name == "str")
                || matches!(path.as_slice(), [root, module, name]
                    if matches!(root.as_str(), "std" | "core") && module == "primitive" && name == "str"))
    }

    /// Constructors whose empty value has the same allocation and state as their trait default.
    pub(super) fn supports_empty_default_constructor(&self, ty: &Type) -> bool {
        let Type::Path(path) = strip(ty) else {
            return false;
        };
        if path.qself.is_some() {
            return false;
        }
        let Some(first) = path.path.segments.first() else {
            return false;
        };
        let name = first.ident.to_string();
        if self.abstracts.contains(&name)
            || self.defaulted.contains(&name)
            || self.defaults.defines(&name)
            || self.aliases.is_some_and(|aliases| aliases.contains_key(&name))
        {
            return false;
        }
        let Some(resolved) = resolved_type_path(ty, self.imports) else {
            return false;
        };
        if !self.standard_root_unshadowed(&resolved) {
            return false;
        }
        let Some(name) = resolved.last() else {
            return false;
        };
        if resolved.len() == 1 && !matches!(name.as_str(), "Vec" | "String") {
            return false;
        }
        let Some(arguments) = constructor_argument_count(&path.path) else {
            return false;
        };
        if name == "String" {
            return self.supports_string_api(ty) && arguments == 0;
        }
        // HashMap/HashSet defaults may construct observable hasher state; allocator parameters
        // likewise make an empty collection's default more than an empty element list.
        let max_arguments = if name == "BTreeMap" { 2 } else { 1 };
        matches!(
            name.as_str(),
            "Vec" | "VecDeque" | "LinkedList" | "BinaryHeap" | "BTreeMap" | "BTreeSet"
        ) && self.supports_collection_api(ty)
            && arguments <= max_arguments
    }

    pub(super) fn standard_type_name(&self, ty: &Type) -> Option<String> {
        resolved_type_path(ty, self.imports)?.last().cloned()
    }

    pub(super) fn supports_default_trait(&self, ty: &Type) -> bool {
        let Type::Path(written) = ty else {
            return false;
        };
        if written
            .path
            .segments
            .iter()
            .any(|segment| !matches!(segment.arguments, PathArguments::None))
            || written.path.segments.first().is_some_and(|segment| {
                let name = segment.ident.to_string();
                self.abstracts.contains(&name) || self.defaulted.contains(&name)
            })
        {
            return false;
        }
        let Some(path) = resolved_type_path(ty, self.imports) else {
            return false;
        };
        if !self.standard_root_unshadowed(&path) {
            return false;
        }
        matches!(path.as_slice(), [name]
            if name == "Default"
                && !self.imports.contains_key("*")
                && !self.defaults.defines(name)
                && !self.abstracts.contains(name)
                && !self.defaulted.contains(name))
            || matches!(path.as_slice(), [root, module, name]
                if matches!(root.as_str(), "std" | "core") && module == "default" && name == "Default")
    }

    pub(super) fn supports_bare_default_trait(&self) -> bool {
        if let Some(imported) = self.imports.get("Default") {
            return imported.as_ref().is_some_and(|path| {
                let path = if path.first().is_some_and(|root| root == ABSOLUTE_ROOT) {
                    &path[1..]
                } else {
                    path.as_slice()
                };
                matches!(path, [root, module, name]
                    if matches!(root.as_str(), "std" | "core") && module == "default" && name == "Default")
                    && self.standard_root_unshadowed(path)
            });
        }
        !self.imports.contains_key("*") && !self.defaults.defines("Default")
    }

    pub(super) fn standard_constructor_root(&self, ty: &Type) -> bool {
        resolved_type_path(ty, self.imports).is_some_and(|path| self.standard_root_unshadowed(&path))
    }

    fn standard_root_unshadowed(&self, path: &[String]) -> bool {
        let Some(root) = path.first() else {
            return false;
        };
        if !matches!(root.as_str(), "std" | "core" | "alloc") {
            return true;
        }
        self.imports.get(root).is_none_or(|imported| {
            imported.as_ref().is_some_and(|imported| {
                let imported = if imported.first().is_some_and(|root| root == ABSOLUTE_ROOT) {
                    &imported[1..]
                } else {
                    imported.as_slice()
                };
                matches!(imported, [name] if name == root)
            })
        })
    }

    pub(super) fn supports_nonzero_api(&self, ty: &Type) -> bool {
        let ty = self.resolve_alias(ty);
        if self.package_defines(ty) {
            return false;
        }
        let Some(path) = resolved_type_path(ty, self.imports) else {
            return false;
        };
        let is_nonzero = |name: &str| {
            matches!(
                name,
                "NonZeroI8"
                    | "NonZeroI16"
                    | "NonZeroI32"
                    | "NonZeroI64"
                    | "NonZeroI128"
                    | "NonZeroIsize"
                    | "NonZeroU8"
                    | "NonZeroU16"
                    | "NonZeroU32"
                    | "NonZeroU64"
                    | "NonZeroU128"
                    | "NonZeroUsize"
            )
        };

        matches!(path.as_slice(), [name] if is_nonzero(name) && !self.imports.contains_key("*"))
            || matches!(
                path.as_slice(),
                [root, module, name]
                    if matches!(root.as_str(), "std" | "core")
                        && module == "num"
                        && is_nonzero(name)
            )
    }

    fn supports_standard_type(&self, ty: &Type, expected_name: &str, expected_module: &str, roots: &[&str]) -> bool {
        let ty = self.resolve_alias(ty);
        if self.package_defines(ty) {
            return false;
        }

        let Some(path) = resolved_type_path(ty, self.imports) else {
            return false;
        };

        matches!(path.as_slice(), [name] if name == expected_name && !self.imports.contains_key("*"))
            || matches!(
                path.as_slice(),
                [root, module, name]
                    if roots.contains(&root.as_str())
                        && module == expected_module
                        && name == expected_name
            )
    }

    pub(super) fn supports_iterator_trait(&self, ty: &Type, iterator_bounded: &[String]) -> bool {
        self.supports_iterator_trait_with(ty, iterator_bounded, &mut HashSet::default())
    }

    fn supports_iterator_trait_with(&self, ty: &Type, iterator_bounded: &[String], visited: &mut HashSet<*const Type>) -> bool {
        if !visited.insert(core::ptr::from_ref(ty)) {
            return false;
        }
        let ty = self.resolve_alias(ty);
        if let Type::Reference(reference) = strip(ty) {
            return reference.mutability.is_some() && self.supports_iterator_trait_with(&reference.elem, iterator_bounded, visited);
        }
        if matches!(strip(ty), Type::Path(path)
            if path.path.get_ident().is_some_and(|ident| iterator_bounded.contains(&ident.to_string())))
        {
            return true;
        }
        let Type::ImplTrait(implementation) = strip(ty) else {
            return false;
        };

        implementation.bounds.iter().any(|bound| {
            let TypeParamBound::Trait(trait_bound) = bound else {
                return false;
            };
            Self::is_standard_iterator_path(&trait_bound.path, self.imports)
        })
    }

    pub(super) fn supports_range_iterator(&self, ty: &Type) -> bool {
        let ty = self.resolve_alias(ty);
        if !self.supports_standard_type(ty, "Range", "ops", &["std", "core"]) {
            return false;
        }
        let arguments = type_argument_values(ty);
        let [element] = arguments.as_slice() else {
            return false;
        };

        self.supports_primitive_kind(element, Kind::Signed) || self.supports_primitive_kind(element, Kind::Unsigned)
    }

    pub(super) fn array_may_default(&self, ty: &Type) -> bool {
        let array = array_type(ty);
        let supported_length = if let syn::Expr::Lit(length) = &array.len
            && let syn::Lit::Int(length) = &length.lit
        {
            length.base10_parse::<usize>().is_ok_and(|length| length <= 32)
        } else {
            true
        };

        supported_length
            && (self.has_source_independent_reference_default(&array.elem)
                || !is_abstract_type(&array.elem, self.abstracts)
                    && !self.lacks_default(&array.elem)
                    && !matches!(
                        resolve_type(self.resolve_alias(&array.elem)),
                        Kind::Result | Kind::Reference | Kind::Iterator | Kind::NonZero | Kind::Cow | Kind::StaticStr | Kind::MutStr
                    ))
    }

    fn has_source_independent_reference_default(&self, ty: &Type) -> bool {
        let concrete = self.resolve_alias(ty);
        if matches!(resolve_type(concrete), Kind::StaticStr) {
            return true;
        }

        matches!(
            strip(concrete),
            Type::Reference(reference)
                if reference.mutability.is_none() && matches!(strip(&reference.elem), Type::Slice(_))
        )
    }

    pub(super) fn collection_hasher_index(&self, ty: &Type) -> Option<usize> {
        if !self.supports_collection_api(ty) {
            return None;
        }
        match resolved_type_path(ty, self.imports)?.last()?.as_str() {
            "HashSet" => Some(1),
            "HashMap" => Some(2),
            _ => None,
        }
    }

    pub(super) fn supports_wrapper_api(&self, ty: &Type) -> bool {
        let ty = self.resolve_alias(ty);
        if self.package_defines(ty) {
            return false;
        }
        let Some(path) = resolved_type_path(ty, self.imports) else {
            return false;
        };

        matches!(path.as_slice(), [name]
            if matches!(name.as_str(), "Box" | "Rc" | "Arc")
                && !self.imports.contains_key("*"))
            || matches!(
                path.as_slice(),
                [root, module, name]
                    if matches!(root.as_str(), "std" | "alloc")
                        && matches!(
                            (module.as_str(), name.as_str()),
                            ("boxed", "Box") | ("rc", "Rc") | ("sync", "Arc")
                        )
            )
    }

    pub(super) fn supports_cow_api(&self, ty: &Type) -> bool {
        self.supports_standard_type(ty, "Cow", "borrow", &["std", "alloc"])
    }

    fn is_standard_iterator_path(path: &Path, imports: &HashMap<String, Option<Vec<String>>>) -> bool {
        let mut segments = path.segments.iter().map(|segment| segment.ident.to_string()).collect::<Vec<_>>();
        if path.leading_colon.is_none()
            && let Some(first) = segments.first()
            && let Some(Some(imported)) = imports.get(first)
        {
            let _replaced = segments.splice(..1, imported.iter().cloned());
        }
        if segments.first().is_some_and(|segment| segment == ABSOLUTE_ROOT) {
            let _absolute = segments.remove(0);
        }
        let iterator_name = |name: &str| matches!(name, "Iterator" | "DoubleEndedIterator" | "ExactSizeIterator" | "FusedIterator");

        matches!(segments.as_slice(), [name]
            if iterator_name(name)
                && !imports.contains_key(name)
                && !imports.contains_key("*"))
            || matches!(
                segments.as_slice(),
                [root, module, name]
                    if matches!(root.as_str(), "std" | "core")
                        && module == "iter"
                        && iterator_name(name)
            )
    }

    pub(super) fn standard_iterator_bounded_parameters(generics: &Generics, imports: &HashMap<String, Option<Vec<String>>>) -> Vec<String> {
        let mut bounded = HashSet::default();
        for parameter in &generics.params {
            let GenericParam::Type(parameter) = parameter else {
                continue;
            };
            if parameter.bounds.iter().any(|bound| {
                matches!(bound, TypeParamBound::Trait(bound)
                    if Self::is_standard_iterator_path(&bound.path, imports))
            }) {
                let _added = bounded.insert(parameter.ident.to_string());
            }
        }
        if let Some(clause) = &generics.where_clause {
            for predicate in &clause.predicates {
                let WherePredicate::Type(predicate) = predicate else {
                    continue;
                };
                let Type::Path(path) = &predicate.bounded_ty else {
                    continue;
                };
                let Some(ident) = path.path.get_ident() else {
                    continue;
                };
                if predicate.bounds.iter().any(|bound| {
                    matches!(bound, TypeParamBound::Trait(bound)
                        if Self::is_standard_iterator_path(&bound.path, imports))
                }) {
                    let _added = bounded.insert(ident.to_string());
                }
            }
        }
        bounded.into_iter().collect()
    }

    fn renamed_collection_has_default(&self, ty: &Type) -> bool {
        let Some(path) = resolved_type_path(ty, self.imports) else {
            return false;
        };

        matches!(
            path.as_slice(),
            [root, name]
                if root == "rustc_hash" && matches!(name.as_str(), "FxHashMap" | "FxHashSet")
        ) || self.package_has_default(ty)
            || self.package_defines(ty) && (!self.defaults.is_complete() || self.local_generic_default(ty))
    }

    pub(super) fn is_local_type(&self, ty: &Type) -> bool {
        let ty = self.resolve_alias(ty);
        let concrete = self
            .concrete_self_type(ty)
            .or_else(|| self.concrete_self_associated_type(ty))
            .unwrap_or(ty);
        self.package_defines(concrete)
    }

    fn local_generic_default(&self, ty: &Type) -> bool {
        let Some(name) = self.package_type_name(ty) else {
            return false;
        };

        self.defaults.declares_generic_default_name(&name)
            && matches!(
                strip(ty),
                Type::Path(path)
                    if path.path.segments.last().is_some_and(|segment| match &segment.arguments {
                        syn::PathArguments::AngleBracketed(arguments) => arguments.args.iter().all(|argument| {
                            !matches!(argument, syn::GenericArgument::Type(argument) if !self.has_proven_default(argument))
                        }),
                        syn::PathArguments::None | syn::PathArguments::Parenthesized(_) => false,
                    })
            )
    }

    /// Returns whether a type has no `Default` to reach for.
    ///
    /// Two independent readings say so, and either is enough: an error type from another crate,
    /// which effectively never has one, or a type this workspace defines and gives none.
    pub(super) fn lacks_default(&self, ty: &Type) -> bool {
        let concrete = self
            .concrete_self_type(ty)
            .or_else(|| self.concrete_self_associated_type(ty))
            .unwrap_or(ty);
        let concrete = self.resolve_alias(concrete);

        matches!(strip(concrete), Type::Path(path) if path.path.is_ident("str"))
            || is_foreign_error(concrete, self.imports)
            || Self::is_standard_without_default(concrete, self.imports)
            || self.package_lacks_default(concrete)
    }

    fn is_standard_without_default(ty: &Type, imports: &HashMap<String, Option<Vec<String>>>) -> bool {
        resolved_type_path(ty, imports).is_some_and(|segments| {
            matches!(segments.as_slice(), [root, module, name]
                if matches!(root.as_str(), "std" | "core")
                    && module == "time"
                    && matches!(name.as_str(), "Instant" | "SystemTime"))
                || matches!(segments.as_slice(), [root, module, name]
                    if matches!(root.as_str(), "std" | "core") && module == "cmp" && name == "Ordering")
                || matches!(segments.as_slice(), [root, module, name]
                    if root == "std" && module == "process" && matches!(name.as_str(), "Command" | "Stdio"))
                || matches!(segments.as_slice(), [root, module, name]
                    if root == "std" && module == "fs" && name == "File")
                || matches!(segments.as_slice(), [root, module, name]
                    if root == "std" && module == "sync" && name == "MutexGuard")
        })
    }

    /// Returns whether this is the standard library's zero-argument `fmt::Result` alias.
    pub(super) fn is_fmt_result(&self, ty: &Type) -> bool {
        resolved_type_path(ty, self.imports).is_some_and(|segments| {
            matches!(segments.as_slice(), [root, module, result]
                if matches!(root.as_str(), "std" | "core") && module == "fmt" && result == "Result")
        })
    }

    pub(super) fn concrete_self_type_or_associated<'a>(&'a self, ty: &Type) -> Option<&'a Type> {
        self.concrete_self_type(ty).or_else(|| self.concrete_self_associated_type(ty))
    }

    fn concrete_self_type<'a>(&'a self, ty: &Type) -> Option<&'a Type> {
        let Type::Path(path) = strip(ty) else {
            return None;
        };

        (path.qself.is_none() && path.path.is_ident("Self"))
            .then_some(self.self_type)
            .flatten()
    }

    fn concrete_self_associated_type<'a>(&'a self, ty: &Type) -> Option<&'a Type> {
        let Type::Path(path) = strip(ty) else {
            return None;
        };
        let mut segments = path.path.segments.iter();
        let (Some(root), Some(name), None) = (segments.next(), segments.next(), segments.next()) else {
            return None;
        };

        (path.qself.is_none() && root.ident == "Self")
            .then(|| self.self_associated?.get(&name.ident.to_string()))
            .flatten()
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn array_type(ty: &Type) -> &syn::TypeArray {
    let Type::Array(array) = strip(ty) else {
        unreachable!("resolve_type classifies only Type::Array syntax as Kind::Array")
    };
    array
}

/// The one type outside this workspace whose name ends in `Error` and that does implement `Default`.
///
/// `core::fmt::Error` is a unit struct standing for "formatting failed", and it derives everything.
/// Every other error type in `std` that was checked does not implement `Default`, so it is cheaper
/// to name the exception than to enumerate the rule.
pub(super) const DEFAULTABLE_ERROR: &str = "fmt";

pub(super) fn constructor_argument_count(path: &Path) -> Option<usize> {
    let mut segments = path.segments.iter().peekable();
    while let Some(segment) = segments.next() {
        if segments.peek().is_some() {
            if !matches!(segment.arguments, PathArguments::None) {
                return None;
            }
            continue;
        }
        return match &segment.arguments {
            PathArguments::None => Some(0),
            PathArguments::AngleBracketed(arguments)
                if arguments.args.iter().all(|argument| matches!(argument, GenericArgument::Type(_))) =>
            {
                Some(arguments.args.len())
            }
            _ => None,
        };
    }
    None
}

fn resolved_type_path(ty: &Type, imports: &HashMap<String, Option<Vec<String>>>) -> Option<Vec<String>> {
    let Type::Path(path) = strip(ty) else {
        return None;
    };
    if path.qself.is_some() {
        return None;
    }

    let mut segments = path
        .path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>();

    if path.path.leading_colon.is_none() {
        for _ in 0..imports.len().saturating_add(1) {
            let first = segments.first()?;
            if first == ABSOLUTE_ROOT {
                let _absolute = segments.remove(0);
                break;
            }
            let Some(imported) = imports.get(first) else {
                break;
            };
            let Some(prefix) = imported else {
                return None;
            };
            // #[gamma::skip(iter.first_to_last, reason = "the comparison is reached only for a one-element prefix, whose first and last element are identical")]
            if prefix.len() == 1 && prefix.first() == Some(first) {
                break;
            }
            let _replaced = segments.splice(..1, prefix.iter().cloned());
        }
    }

    Some(segments)
}

/// Returns whether a type is an error type from outside this workspace, which will have no `Default`.
///
/// Error types are the largest single source of mutants that cannot compile: `Err(Default::default())`
/// wants an error value, and `std::io::Error`, `anyhow::Error`, `serde_json::Error` and their kind
/// have no `Default` and are not going to acquire one. Withholding the mutant there costs no signal,
/// because there was never a mutant to lose.
///
/// The rule is deliberately confined to types from *other* crates. A workspace error type may well
/// be an enum with a `#[default]` variant, and the collector cannot see the definition to find out,
/// so `crate::`, `self::`, `super::` and any name this file does not import stay optimistic.
pub(super) fn is_foreign_error(ty: &Type, imports: &HashMap<String, Option<Vec<String>>>) -> bool {
    let Some(resolved) = resolved_type_path(ty, imports) else {
        return false;
    };

    let Some(last) = resolved.last() else {
        return false;
    };

    if !last.ends_with("Error") {
        return false;
    }

    let prefix = &resolved[..resolved.len().saturating_sub(1)];

    let (Some(root), Some(qualifier)) = (prefix.first(), prefix.last()) else {
        return false;
    };

    if matches!(root.as_str(), "crate" | "self" | "super") {
        return false;
    }

    qualifier != DEFAULTABLE_ERROR
}

/// Returns whether a signature's `Result` fixes its error type to one with no `Default`.
///
/// `Ok(v)` becoming `Err(Default::default())` needs a value of the error type, and the call site
/// does not name it — the signature does. When the error type is written out, it is read straight
/// off the return type. When the signature uses a crate-wide `type Result<T>` alias instead, which
/// is close to universal in real Rust, the alias is resolved through the workspace index.
pub(super) fn returns_undefaultable_error(output: &ReturnType, types: &Types<'_>) -> bool {
    let ReturnType::Type(_arrow, ty) = output else {
        return false;
    };

    let Type::Path(path) = strip(ty) else {
        return false;
    };
    if resolve_type(ty) != Kind::Result {
        return false;
    }

    if let Some(error) = payload(ty, 1) {
        return types.lacks_default(error);
    }

    // No second argument, so the error type is whatever an alias fixed it to. The alias is named by
    // the return type's last segment, and the index answers for the name it resolved to.
    let alias = path
        .path
        .segments
        .last()
        .expect("a type resolved as Result must have a final path segment");

    types
        .defaults
        .aliased_error(&alias.ident.to_string())
        .is_some_and(|error| types.defaults.lacks_error_default(error))
}

/// Returns whether a type is one no concrete `Default` can be assumed for.
///
/// `Default::default()` is the fallback for an unknown concrete type, but a caller's type
/// parameter, an associated type projected from one, and a trait object or `impl Trait` name no
/// constructible type unless the signature supplies a `Default` bound.
///
/// `Self::Value` is deliberately excluded. Inside an `impl` block it resolves to the type chosen
/// by that block and may implement `Default`.
pub(super) fn is_abstract_type(ty: &Type, abstracts: &[String]) -> bool {
    match ty {
        Type::TraitObject(_) | Type::ImplTrait(_) => true,

        Type::Paren(paren) => is_abstract_type(&paren.elem, abstracts),

        Type::Path(path) if path.qself.is_none() => {
            let Some(last) = path.path.segments.last() else {
                return false;
            };

            if path.path.segments.len() > 1 {
                let root = &path.path.segments[0].ident;
                return root == "Self" || abstracts.contains(&root.to_string());
            }

            // `Box<dyn Reader>` is exactly as unconstructable as the `dyn Reader` inside it.
            if last.ident == "Box" {
                return payload(ty, 0).is_some_and(|inner| is_abstract_type(inner, abstracts));
            }

            abstracts.contains(&last.ident.to_string())
        }

        _ => false,
    }
}

/// Names every type parameter a generics list declares without a `Default` bound.
///
/// A parameter written `T: Default` is excluded, because there the promise this is looking for was
/// made explicitly and the mutant it would otherwise withhold compiles.
pub(super) fn undefaulted_parameters(generics: &Generics, defaults: &DefaultPaths) -> Vec<String> {
    let defaulted = standard_defaulted_parameters(generics, defaults);

    generics
        .params
        .iter()
        .filter_map(|param| match param {
            GenericParam::Type(ty) => (!defaulted.iter().any(|name| name == &ty.ident.to_string())).then(|| ty.ident.to_string()),

            _ => None,
        })
        .collect()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use syn::punctuated::Punctuated;
    use syn::{PathSegment, TypePath, parse_quote};

    use super::*;

    fn facts<'a>(defaults: &'a Defaults, imports: &'a HashMap<String, Option<Vec<String>>>) -> Types<'a> {
        Types {
            abstracts: &[],
            defaulted: &[],
            imports,
            defaults,
            aliases: None,
            self_type: None,
            self_associated: None,
        }
    }

    #[test]
    fn alias_resolution_preserves_imported_and_nominal_shadows_at_every_hop() {
        let aliases = HashMap::from_iter([
            (
                "Alias".into(),
                Some(Alias {
                    parameters: vec![],
                    target: parse_quote!(Option<u8>),
                }),
            ),
            (
                "Outer".into(),
                Some(Alias {
                    parameters: vec![],
                    target: parse_quote!(Alias),
                }),
            ),
        ]);
        for imported in [false, true] {
            let defaults = if imported {
                Defaults::default()
            } else {
                Defaults::of(&syn::parse_file("struct Alias;").unwrap())
            };
            let imports = if imported {
                HashMap::from_iter([("Alias".into(), Some(vec!["foreign".into(), "Alias".into()]))])
            } else {
                HashMap::default()
            };
            let types = Types {
                aliases: Some(&aliases),
                ..facts(&defaults, &imports)
            };
            let alias: Type = parse_quote!(Alias);
            assert_eq!(types.instantiate_alias(&alias), None, "imported={imported}");
            assert_eq!(types.resolve_alias(&alias), &alias, "imported={imported}");
            let outer: Type = parse_quote!(Outer);
            assert_eq!(types.instantiate_alias(&outer), Some(alias.clone()), "imported={imported}");
            assert_eq!(types.resolve_alias(&outer), &alias, "imported={imported}");
            assert_eq!(types.payload(&alias, 0), None, "imported={imported}");
            assert_eq!(types.payload(&outer, 0), None, "imported={imported}");
        }
    }

    #[test]
    fn associated_paths_do_not_instantiate_an_alias_with_the_same_first_segment() {
        let defaults = Defaults::default();
        let imports = HashMap::default();
        let aliases = HashMap::from_iter([(
            "Alias".into(),
            Some(Alias {
                parameters: vec!["T".into()],
                target: parse_quote!(Option<T>),
            }),
        )]);
        let types = Types {
            aliases: Some(&aliases),
            ..facts(&defaults, &imports)
        };
        assert_eq!(types.instantiate_alias(&parse_quote!(Alias<u8>)), Some(parse_quote!(Option<u8>)));
        for ty in [parse_quote!(Alias<u8>::Associated), parse_quote!(<Alias<u8>>::Associated)] {
            assert_eq!(types.instantiate_alias(&ty), None, "{ty:?}");
            assert_eq!(types.resolve_alias(&ty), &ty, "{ty:?}");
            assert_eq!(types.payload(&ty, 0), None, "{ty:?}");
        }
    }

    #[test]
    fn absolute_import_expansion_stops_before_a_shadowed_standard_root() {
        let imports = HashMap::from_iter([
            (
                "Clock".into(),
                Some(vec![ABSOLUTE_ROOT.into(), "std".into(), "time".into(), "Duration".into()]),
            ),
            ("std".into(), Some(vec!["foreign".into(), "standard".into()])),
        ]);
        assert_eq!(
            resolved_type_path(&parse_quote!(Clock), &imports),
            Some(vec!["std".into(), "time".into(), "Duration".into()])
        );
        assert_eq!(
            resolved_type_path(&parse_quote!(std::time::Duration), &imports),
            Some(vec!["foreign".into(), "standard".into(), "time".into(), "Duration".into()])
        );
    }

    #[test]
    fn qualified_package_names_and_self_projections_do_not_borrow_bare_evidence() {
        let defaults = Defaults::default();
        let imports = HashMap::from_iter([("nested".into(), Some(vec!["crate".into(), "nested".into()]))]);
        let owner = parse_quote!(u32);
        let associated = HashMap::from_iter([("Item".into(), parse_quote!(u16))]);
        let types = Types {
            self_type: Some(&owner),
            self_associated: Some(&associated),
            ..facts(&defaults, &imports)
        };
        assert_eq!(types.package_type_name(&parse_quote!(nested::nested)), None);
        assert_eq!(types.package_type_name(&parse_quote!(nested)), Some("nested".into()));
        assert_eq!(types.concrete_self_type(&parse_quote!(Self)), Some(&owner));
        assert_eq!(
            types.concrete_self_associated_type(&parse_quote!(Self::Item)),
            associated.get("Item")
        );
        for text in ["<Other>::Self", "<Other as Self>::Item"] {
            let ty: Type = syn::parse_str(text).unwrap();
            assert_eq!(types.concrete_self_type_or_associated(&ty), None, "{text}");
        }
    }

    #[test]
    fn empty_absolute_imports_stay_unknown_instead_of_panicking_on_their_type_name() {
        let file = syn::parse_file("use ::{self as Root};").unwrap();
        let indexes = super::super::indexes::indexes_in(
            &file,
            &crate::ops::registry::Selection::everything(),
            &crate::cfg::CfgSet::unconditional(),
        );
        let defaults = Defaults::default();
        let types = facts(&defaults, &indexes.root_imports);
        assert_eq!(resolved_type_path(&parse_quote!(Root), types.imports), Some(Vec::new()));
        assert_eq!(types.package_type_name(&parse_quote!(Root)), None);
    }

    #[test]
    fn hasher_constraints_do_not_apply_to_nonhash_collection_allocators() {
        let defaults = Defaults::of(&syn::parse_file("struct Missing; #[derive(Default)] struct str;").unwrap());
        let imports = HashMap::default();
        let types = facts(&defaults, &imports);
        for text in ["Vec<u8, Missing>", "BTreeMap<u8, u16, Missing>"] {
            assert!(types.has_default(&syn::parse_str(text).unwrap()), "{text}");
        }
        for text in ["HashSet<u8, Missing>", "HashMap<u8, u16, Missing>", "str"] {
            assert!(!types.has_default(&syn::parse_str(text).unwrap()), "{text}");
        }
        assert!(defaults.declares_default_name("str"));
    }

    #[test]
    fn qualified_where_predicates_do_not_hide_later_iterator_bounds() {
        let function: syn::ItemFn = parse_quote!(
            fn f<T, U>()
            where
                T::Item: Clone,
                U: iter::Iterator,
            {
            }
        );
        let imports = HashMap::from_iter([("iter".into(), Some(vec!["std".into(), "iter".into()]))]);
        assert_eq!(Types::standard_iterator_bounded_parameters(&function.sig.generics, &imports), ["U"]);
        let function: syn::ItemFn = parse_quote!(
            fn f<T: iter::Iterator>() {}
        );
        assert_eq!(Types::standard_iterator_bounded_parameters(&function.sig.generics, &imports), ["T"]);
    }

    #[test]
    fn renamed_collection_defaults_keep_nongeneric_local_derive_evidence() {
        let imports = HashMap::default();
        let derived = Defaults::of(&syn::parse_file("#[derive(Default)] struct HashMap;").unwrap());
        let plain = Defaults::of(&syn::parse_file("struct HashMap;").unwrap());
        assert!(facts(&derived, &imports).renamed_collection_has_default(&parse_quote!(HashMap)));
        assert!(!facts(&plain, &imports).renamed_collection_has_default(&parse_quote!(HashMap)));
        assert!(!facts(&derived, &imports).renamed_collection_has_default(&parse_quote!(foreign::HashMap)));
    }

    #[test]
    fn resource_type_proofs_require_independent_unshadowed_primitive_and_iterator_names() {
        let defaults = Defaults::default();
        let imports = HashMap::default();
        let types = facts(&defaults, &imports);
        for (text, expected) in [
            ("std::iter::Peekable<std::str::Chars<'static>>", true),
            ("core::iter::Peekable<core::str::Chars<'static>>", true),
            ("&mut std::iter::Peekable<std::str::Chars<'static>>", true),
            ("&std::iter::Peekable<std::str::Chars<'static>>", false),
            ("std::iter::Peekable", false),
            ("std::iter::Peekable<>", false),
            ("std::iter::Peekable<'static>", false),
            ("std::iter::Peekable<std::str::Chars<'static>, u8>", false),
            ("std::iter::Peekable<u8, std::str::Chars<'static>>", false),
            ("std::iter::Peekable<other::str::Chars<'static>>", false),
            ("std::iter::Peekable<std::wrong::Chars<'static>>", false),
            ("std::iter::Peekable<std::str::Bytes<'static>>", false),
            ("other::iter::Peekable<std::str::Chars<'static>>", false),
            ("std::wrong::Peekable<std::str::Chars<'static>>", false),
            ("std::iter::Other<std::str::Chars<'static>>", false),
            ("()", false),
            ("<T as Trait>::Peekable", false),
        ] {
            assert_eq!(types.supports_peekable_chars(&syn::parse_str(text).unwrap()), expected, "{text}");
        }
        for (text, expected) in [
            ("&str", true),
            ("&mut str", true),
            ("&core::primitive::str", true),
            ("&std::primitive::str", true),
            ("str", false),
            ("&u8", false),
            ("&other::primitive::str", false),
            ("&std::wrong::str", false),
            ("&std::primitive::char", false),
            ("&<T as Trait>::Text", false),
            ("()", false),
        ] {
            assert_eq!(types.supports_str_reference(&syn::parse_str(text).unwrap()), expected, "{text}");
        }
        assert!(types.supports_char_api());
        for primitive in ["str", "char"] {
            for bounded in [false, true] {
                let names = [primitive.to_owned()];
                let shadowed = if bounded {
                    Types {
                        defaulted: &names,
                        ..types
                    }
                } else {
                    Types {
                        abstracts: &names,
                        ..types
                    }
                };
                if primitive == "str" {
                    assert!(!shadowed.supports_str_reference(&parse_quote!(&str)));
                } else {
                    assert!(!shadowed.supports_char_api());
                }
            }
            let local = Defaults::of(&syn::parse_file(&format!("struct {primitive};")).unwrap());
            let shadowed = facts(&local, &imports);
            if primitive == "str" {
                assert!(!shadowed.supports_str_reference(&parse_quote!(&str)));
            } else {
                assert!(!shadowed.supports_char_api());
            }
        }
        for (name, target) in [
            ("*", None),
            ("char", None),
            ("char", Some(vec!["other", "char"])),
            ("str", Some(vec!["other", "str"])),
            ("std", None),
            ("std", Some(vec!["other"])),
            ("core", Some(vec!["other"])),
        ] {
            let imports = HashMap::from_iter([(name.to_owned(), target.map(|p| p.into_iter().map(str::to_owned).collect()))]);
            let shadowed = facts(&defaults, &imports);
            if matches!(name, "*" | "char") {
                assert!(!shadowed.supports_char_api(), "{name}");
            }
            if matches!(name, "*" | "str") {
                assert!(!shadowed.supports_str_reference(&parse_quote!(&str)), "{name}");
            }
            if matches!(name, "std" | "core") {
                let text = format!("{name}::iter::Peekable<{name}::str::Chars<'static>>");
                assert!(!shadowed.supports_peekable_chars(&syn::parse_str(&text).unwrap()), "{name}");
                let text = format!("&{name}::primitive::str");
                assert!(!shadowed.supports_str_reference(&syn::parse_str(&text).unwrap()), "{name}");
            }
        }
    }

    #[test]
    fn imported_primitives_and_generic_lookalikes_keep_resource_proofs_conservative() {
        let defaults = Defaults::default();
        let imports = HashMap::from_iter([
            ("char".into(), Some(vec!["core".into(), "primitive".into(), "char".into()])),
            ("str".into(), Some(vec!["std".into(), "primitive".into(), "str".into()])),
            ("P".into(), Some(vec!["core".into(), "iter".into(), "Peekable".into()])),
            ("C".into(), Some(vec!["std".into(), "str".into(), "Chars".into()])),
        ]);
        let types = facts(&defaults, &imports);
        assert!(types.supports_char_api());
        assert!(types.supports_str_reference(&parse_quote!(&str)));
        assert!(types.supports_peekable_chars(&parse_quote!(P<C<'static>>)));
        for names in [vec!["P".into()], vec!["C".into()]] {
            let abstracted = Types {
                abstracts: &names,
                ..types
            };
            let defaulted = Types {
                defaulted: &names,
                ..types
            };
            assert!(!abstracted.supports_peekable_chars(&parse_quote!(P<C<'static>>)));
            assert!(!defaulted.supports_peekable_chars(&parse_quote!(P<C<'static>>)));
        }
        let aliases = HashMap::from_iter([(
            "Text".into(),
            Some(Alias {
                parameters: vec![],
                target: parse_quote!(&str),
            }),
        )]);
        let resolved = Types {
            aliases: Some(&aliases),
            ..types
        };
        assert!(resolved.supports_str_reference(&parse_quote!(Text)));
        let names = ["Text".into()];
        assert!(
            !Types {
                abstracts: &names,
                ..resolved
            }
            .supports_str_reference(&parse_quote!(Text))
        );
        assert!(
            !Types {
                defaulted: &names,
                ..resolved
            }
            .supports_str_reference(&parse_quote!(Text))
        );
    }

    #[test]
    fn absolute_import_provenance_does_not_hide_a_shadowed_standard_root() {
        let defaults = Defaults::default();
        for shadow in ["std", "core"] {
            let imports = HashMap::from_iter([
                (shadow.into(), None),
                (
                    "char".into(),
                    Some(vec!["<absolute>".into(), shadow.into(), "primitive".into(), "char".into()]),
                ),
                (
                    "str".into(),
                    Some(vec!["<absolute>".into(), shadow.into(), "primitive".into(), "str".into()]),
                ),
                (
                    "Default".into(),
                    Some(vec!["<absolute>".into(), shadow.into(), "default".into(), "Default".into()]),
                ),
            ]);
            let types = facts(&defaults, &imports);
            assert!(!types.supports_char_api(), "{shadow}");
            assert!(!types.supports_str_reference(&parse_quote!(&str)), "{shadow}");
            assert!(!types.supports_bare_default_trait(), "{shadow}");
            let text = format!("::{shadow}::default::Default");
            assert!(!types.supports_default_trait(&syn::parse_str(&text).unwrap()), "{shadow}");
            let text = format!("::{shadow}::iter::Peekable<::std::str::Chars<'static>>");
            assert!(!types.supports_peekable_chars(&syn::parse_str(&text).unwrap()), "{shadow}");
            let text = format!("::std::iter::Peekable<::{shadow}::str::Chars<'static>>");
            assert!(!types.supports_peekable_chars(&syn::parse_str(&text).unwrap()), "{shadow}");
            assert!(!types.standard_root_unshadowed(&[]));
            assert!(types.standard_root_unshadowed(&["unrelated".into()]));
        }
    }

    #[test]
    fn alias_instantiation_does_not_strip_qualified_self_or_qualified_targets() {
        let defaults = Defaults::default();
        let imports = HashMap::default();
        let aliases = HashMap::from_iter([
            (
                "Wrap".into(),
                Some(Alias {
                    parameters: vec!["T".into()],
                    target: parse_quote!(Option<T>),
                }),
            ),
            (
                "Qualified".into(),
                Some(Alias {
                    parameters: vec![],
                    target: parse_quote!(foreign::Thing),
                }),
            ),
            (
                "Tuple".into(),
                Some(Alias {
                    parameters: vec![],
                    target: parse_quote!((u8, bool)),
                }),
            ),
        ]);
        let types = Types {
            aliases: Some(&aliases),
            ..facts(&defaults, &imports)
        };
        assert_eq!(types.instantiate_alias(&parse_quote!(<T>::Wrap<u8>)), None);
        assert_eq!(types.instantiate_alias(&parse_quote!(foreign::Wrap<u8>)), None);
        assert_eq!(
            types.instantiate_alias(&parse_quote!(Qualified)),
            Some(parse_quote!(foreign::Thing))
        );
        assert_eq!(types.instantiate_alias(&parse_quote!(Tuple)), Some(parse_quote!((u8, bool))));
    }

    #[test]
    fn collection_proofs_cover_nested_standard_modules_but_not_unknown_reference_or_ambiguous_constructors() {
        let defaults = Defaults::default();
        let imports = HashMap::default();
        let types = facts(&defaults, &imports);
        for text in [
            "std::collections::vec_deque::VecDeque<u8>",
            "alloc::collections::vec_deque::VecDeque<u8>",
            "std::collections::hash_set::HashSet<u8>",
            "alloc::collections::hash_set::HashSet<u8>",
        ] {
            assert!(types.supports_collection_api(&syn::parse_str(text).unwrap()), "{text}");
        }
        assert!(!types.supports_collection_api(&parse_quote!(Unknown)));
        assert!(!types.supports_collection_constructor(&parse_quote!(Unknown)));
        assert!(!types.supports_collection_constructor(&parse_quote!(&Vec<u8>)));
        let imports = HashMap::from_iter([("Vec".into(), Some(vec!["foreign".into(), "Thing".into()]))]);
        assert!(!facts(&defaults, &imports).prefers_default_constructor(&parse_quote!(Vec<u8>)));
        let imports = HashMap::from_iter([("Unknown".into(), None), ("HashMap".into(), None)]);
        let types = facts(&defaults, &imports);
        assert_eq!(types.standard_type_name(&parse_quote!(Unknown)), None);
        assert!(!types.has_proven_default(&parse_quote!(HashMap<u8, u8>)));
        let local = Defaults::of(&syn::parse_file("#[derive(Default)] struct Vec; #[derive(Default)] struct Option;").unwrap());
        let local_imports = HashMap::default();
        let types = facts(&local, &local_imports);
        assert!(!types.prefers_default_constructor(&parse_quote!(Vec)));
        assert!(types.has_default(&parse_quote!(Option)));
        let mut partial = Defaults::of(&syn::parse_file("struct Option;").unwrap());
        partial.mark_incomplete();
        let types = facts(&partial, &imports);
        assert!(types.has_default(&parse_quote!(Option)));
        assert!(!types.has_proven_default(&parse_quote!(Option)));
    }

    #[test]
    fn empty_constructor_proofs_exclude_allocator_hasher_alias_and_trait_shadows() {
        let defaults = Defaults::default();
        let imports = HashMap::default();
        let types = facts(&defaults, &imports);
        for (text, expected) in [
            ("String", true),
            ("String<u8>", false),
            ("Vec<u8>", true),
            ("Vec<u8, Allocator>", false),
            ("std::collections::vec_deque::VecDeque<u8>", true),
            ("std::collections::hash_set::HashSet<u8>", false),
            ("std::collections::btree_map::BTreeMap<u8,u16>", true),
            ("std::collections::btree_map::BTreeMap<u8,u16,A>", false),
            ("BTreeSet<u8>", false),
            ("std::collections::BTreeSet<u8>", true),
            ("other::String", false),
            ("<T as Trait>::String", false),
            ("String<'static>", false),
            ("std::<u8>::vec::Vec<u8>", false),
            ("()", false),
        ] {
            assert_eq!(
                types.supports_empty_default_constructor(&syn::parse_str(text).unwrap()),
                expected,
                "{text}"
            );
        }
        for bounded in [false, true] {
            let names = ["std".into(), "String".into()];
            let shadowed = if bounded {
                Types {
                    defaulted: &names,
                    ..types
                }
            } else {
                Types {
                    abstracts: &names,
                    ..types
                }
            };
            assert!(!shadowed.supports_empty_default_constructor(&parse_quote!(String)));
            assert!(!shadowed.supports_empty_default_constructor(&parse_quote!(std::vec::Vec<u8>)));
        }
        let aliases = HashMap::from_iter([("String".into(), None)]);
        assert!(
            !Types {
                aliases: Some(&aliases),
                ..types
            }
            .supports_empty_default_constructor(&parse_quote!(String))
        );
        let imports = HashMap::from_iter([("std".into(), Some(vec!["other".into()]))]);
        assert!(!facts(&defaults, &imports).supports_empty_default_constructor(&parse_quote!(std::vec::Vec<u8>)));
        let local = Defaults::of(&syn::parse_file("struct String;").unwrap());
        assert!(!facts(&local, &HashMap::default()).supports_empty_default_constructor(&parse_quote!(String)));
    }

    #[test]
    fn default_trait_identity_survives_absolute_reexports_but_not_independent_shadows() {
        let defaults = Defaults::default();
        for absolute in [false, true] {
            let mut path: Vec<String> = ["core", "default", "Default"].map(str::to_owned).into();
            if absolute {
                path.insert(0, ABSOLUTE_ROOT.into());
            }
            let imports = HashMap::from_iter([("Default".into(), Some(path))]);
            let types = facts(&defaults, &imports);
            assert!(types.supports_bare_default_trait());
            assert!(types.supports_default_trait(&parse_quote!(Default)));
        }
        for imports in [
            HashMap::from_iter([("*".into(), None)]),
            HashMap::from_iter([("Default".into(), None)]),
            HashMap::from_iter([("Default".into(), Some(vec!["other".into(), "Default".into()]))]),
            HashMap::from_iter([
                ("Default".into(), Some(vec!["std".into(), "default".into(), "Default".into()])),
                ("std".into(), None),
            ]),
        ] {
            let types = facts(&defaults, &imports);
            assert!(!types.supports_bare_default_trait());
            assert!(!types.supports_default_trait(&parse_quote!(Default)));
        }
        let imports = HashMap::default();
        let types = facts(&defaults, &imports);
        for (text, expected) in [
            ("Default", true),
            ("core::default::Default", true),
            ("std::default::Default", true),
            ("Default<u8>", false),
            ("std::<u8>::default::Default", false),
            ("std::default::Default<u8>", false),
            ("other::default::Default", false),
            ("()", false),
        ] {
            assert_eq!(types.supports_default_trait(&syn::parse_str(text).unwrap()), expected, "{text}");
        }
        for bounded in [false, true] {
            let names = ["std".into()];
            let shadowed = if bounded {
                Types {
                    defaulted: &names,
                    ..types
                }
            } else {
                Types {
                    abstracts: &names,
                    ..types
                }
            };
            assert!(!shadowed.supports_default_trait(&parse_quote!(std::default::Default)));
        }
        let local = Defaults::of(&syn::parse_file("struct Default;").unwrap());
        assert!(!facts(&local, &imports).supports_bare_default_trait());
        for root in ["std", "core", "alloc"] {
            for absolute in [false, true] {
                let mut prefix = vec![root.to_owned()];
                if absolute {
                    prefix.insert(0, ABSOLUTE_ROOT.into());
                }
                let imports = HashMap::from_iter([(root.into(), Some(prefix))]);
                assert!(facts(&defaults, &imports).standard_root_unshadowed(&[root.into()]));
            }
        }
    }

    #[test]
    fn nested_default_evidence_preserves_reference_arrays_and_foreign_tuple_payloads() {
        let defaults = Defaults::of(&syn::parse_file("struct Missing; #[derive(Default)] struct Generic<T>(T);").unwrap());
        let imports = HashMap::default();
        let types = facts(&defaults, &imports);
        for (text, expected) in [
            ("[&str; 32]", true),
            ("[&[u8]; 32]", true),
            ("[&str; 33]", false),
            ("[&mut str; 1]", false),
            ("[u8; \"length\"]", false),
            ("[u8; 340282366920938463463374607431768211456]", false),
            ("Generic<Missing>", false),
            ("(bool, u8)", true),
            ("(bool, Missing)", false),
            ("core::fmt::Error", true),
            ("str", false),
        ] {
            let ty = syn::parse_str(text).unwrap();
            assert_eq!(types.has_default(&ty), expected, "{text}");
            assert_eq!(types.has_proven_default(&ty), expected, "{text}");
        }
        let imports = HashMap::from_iter([("Mystery".into(), Some(vec!["foreign".into(), "Thing".into()]))]);
        let types = facts(&defaults, &imports);
        assert!(!types.has_proven_default(&parse_quote!(Mystery)));
        let imports = HashMap::from_iter([("HashMap".into(), Some(vec!["rustc_hash".into(), "FxHashMap".into()]))]);
        let types = facts(&defaults, &imports);
        assert!(types.has_proven_default(&parse_quote!(HashMap<u8, u8>)));
        assert!(types.has_proven_default(&parse_quote!(Generic<u8>)));
        assert!(types.has_default(&parse_quote!((foreign::bool, bool))));
        assert!(!types.has_proven_default(&parse_quote!((foreign::bool, bool))));
        assert!(!types.prefers_default_constructor(&parse_quote!(Unknown)));
    }

    #[test]
    fn iterator_where_predicates_continue_after_irrelevant_lifetimes_and_composite_types() {
        let imports = HashMap::from_iter([(
            "Iter".into(),
            Some(vec![ABSOLUTE_ROOT.into(), "core".into(), "iter".into(), "Iterator".into()]),
        )]);
        let item: syn::ItemFn = parse_quote!(
            fn f<'a, T, U, V, W>()
            where
                'a: 'static,
                (T,): Clone,
                U: Clone + Iter<Item = u8>,
                V: 'a + Iterator,
                W: Clone,
            {
            }
        );
        let mut names = Types::standard_iterator_bounded_parameters(&item.sig.generics, &imports);
        names.sort();
        assert_eq!(names, ["U", "V"]);
        let defaults = Defaults::default();
        let types = facts(&defaults, &imports);
        assert!(!types.supports_iterator_trait(&parse_quote!(&mut u8), &["I".into()]));
        assert!(!types.supports_iterator_trait(&parse_quote!(&I), &["I".into()]));
        assert!(types.supports_iterator_trait(&parse_quote!(&mut I), &["I".into()]));
        assert!(types.supports_iterator_trait(&parse_quote!(impl Iter<Item = u8>), &[]));
        assert!(!types.supports_iterator_trait(&parse_quote!(impl foreign::Iterator<Item = u8>), &[]));
    }

    #[test]
    fn foreign_error_provenance_keeps_absolute_imports_and_local_nested_names_distinct() {
        let imports = HashMap::from_iter([
            (
                "Remote".into(),
                Some(vec![ABSOLUTE_ROOT.into(), "std".into(), "io".into(), "Error".into()]),
            ),
            ("std".into(), None),
        ]);
        assert_eq!(
            resolved_type_path(&parse_quote!(Remote), &imports),
            Some(vec!["std".into(), "io".into(), "Error".into()])
        );
        assert!(is_foreign_error(&parse_quote!(Remote), &imports));
        for text in [
            "crate::nested::Error",
            "self::nested::Error",
            "super::nested::Error",
            "<T as Trait>::Error",
            "u8",
        ] {
            assert!(!is_foreign_error(&syn::parse_str(text).unwrap(), &imports), "{text}");
        }
        assert!(is_foreign_error(&parse_quote!(foreign::nested::Error), &imports));
        assert!(!is_foreign_error(&parse_quote!(core::fmt::Error), &imports));
        let defaults = Defaults::default();
        let types = facts(&defaults, &imports);
        assert!(!returns_undefaultable_error(&parse_quote!(-> (bool, bool)), &types));
        assert!(!returns_undefaultable_error(
            &parse_quote!(-> Wrapper<bool, foreign::Error>),
            &types
        ));
        assert!(returns_undefaultable_error(&parse_quote!(-> Result<bool, foreign::Error>), &types));
    }

    #[test]
    fn package_names_retain_only_unambiguous_unrenamed_local_bindings() {
        let defaults = Defaults::of(&syn::parse_file("#[derive(Default)] struct Local;").unwrap());
        for (prefix, written, expected) in [
            (vec!["crate", "Local"], "Local", Some("Local")),
            (vec!["self", "Local"], "Local", Some("Local")),
            (vec!["super", "Local"], "Local", Some("Local")),
            (vec!["other", "Local"], "Local", None),
            (vec!["crate", "Local"], "Renamed", None),
        ] {
            let imports = HashMap::from_iter([(written.into(), Some(prefix.into_iter().map(str::to_owned).collect()))]);
            let types = Types {
                abstracts: &[],
                defaulted: &[],
                imports: &imports,
                defaults: &defaults,
                aliases: None,
                self_type: None,
                self_associated: None,
            };
            assert_eq!(
                types.package_type_name(&syn::parse_str(written).unwrap()),
                expected.map(str::to_owned),
                "{written}"
            );
        }
        for wildcard in [false, true] {
            let mut imports = HashMap::default();
            if wildcard {
                let _previous = imports.insert("*".into(), None);
            }
            let types = Types {
                abstracts: &[],
                defaulted: &[],
                imports: &imports,
                defaults: &defaults,
                aliases: None,
                self_type: None,
                self_associated: None,
            };
            assert_eq!(
                types.package_type_name(&parse_quote!(Local)),
                (!wildcard).then(|| "Local".to_owned())
            );
            for text in ["other::Local", "<T as Trait>::Local", "()", "&Local"] {
                assert_eq!(types.package_type_name(&syn::parse_str(text).unwrap()), None, "{text}");
            }
        }
    }

    #[test]
    fn resolved_path_expansion_preserves_full_prefixes_absolute_paths_and_iteration_bound() {
        let imports = HashMap::from_iter([
            ("A".into(), Some(vec!["B".into()])),
            ("B".into(), Some(vec!["core".into(), "option".into(), "Option".into()])),
            ("Stable".into(), Some(vec!["Stable".into()])),
            ("Unknown".into(), None),
            ("Empty".into(), Some(vec![])),
            ("Absolute".into(), Some(vec![ABSOLUTE_ROOT.into(), "A".into()])),
        ]);
        for (text, expected) in [
            ("A<u8>", Some(vec!["core", "option", "Option"])),
            ("A::Value", Some(vec!["core", "option", "Option", "Value"])),
            ("Stable", Some(vec!["Stable"])),
            ("Unknown", None),
            ("Empty", None),
            ("::A", Some(vec!["A"])),
            ("Absolute", Some(vec!["A"])),
            ("()", None),
            ("<T as Trait>::Value", None),
        ] {
            assert_eq!(
                resolved_type_path(&syn::parse_str(text).unwrap(), &imports),
                expected.map(|path| path.into_iter().map(str::to_owned).collect()),
                "{text}"
            );
        }
        let cycle = HashMap::from_iter([("A".into(), Some(vec!["B".into()])), ("B".into(), Some(vec!["A".into()]))]);
        assert_eq!(resolved_type_path(&parse_quote!(A), &cycle), Some(vec!["B".into()]));
        assert_eq!(resolved_type_path(&parse_quote!(B), &cycle), Some(vec!["A".into()]));
    }

    #[test]
    fn standard_api_evidence_requires_exact_root_module_name_and_arity() {
        let defaults = Defaults::default();
        let imports = HashMap::default();
        let types = Types {
            abstracts: &[],
            defaulted: &[],
            imports: &imports,
            defaults: &defaults,
            aliases: None,
            self_type: None,
            self_associated: None,
        };
        for (text, option, result, string, wrapper, cow, nonzero) in [
            ("Option<u8>", true, false, false, false, false, false),
            ("core::option::Option<u8>", true, false, false, false, false, false),
            ("other::option::Option<u8>", false, false, false, false, false, false),
            ("std::wrong::Option<u8>", false, false, false, false, false, false),
            ("core::result::Result<u8, bool>", false, true, false, false, false, false),
            ("alloc::string::String", false, false, true, false, false, false),
            ("core::string::String", false, false, false, false, false, false),
            ("alloc::boxed::Box<u8>", false, false, false, true, false, false),
            ("std::rc::Rc<u8>", false, false, false, true, false, false),
            ("alloc::sync::Arc<u8>", false, false, false, true, false, false),
            ("std::rc::Box<u8>", false, false, false, false, false, false),
            ("std::borrow::Cow<'static, str>", false, false, false, false, true, false),
            ("core::num::NonZeroU8", false, false, false, false, false, true),
            ("other::num::NonZeroU8", false, false, false, false, false, false),
            ("()", false, false, false, false, false, false),
        ] {
            let ty = syn::parse_str::<Type>(text).unwrap();
            assert_eq!(
                (
                    types.supports_option_api(&ty),
                    types.supports_result_api(&ty),
                    types.supports_string_api(&ty),
                    types.supports_wrapper_api(&ty),
                    types.supports_cow_api(&ty),
                    types.supports_nonzero_api(&ty),
                ),
                (option, result, string, wrapper, cow, nonzero),
                "{text}",
            );
        }
        for (text, api, constructor, hasher) in [
            ("std::vec::Vec<u8>", true, true, None),
            ("alloc::collections::VecDeque<u8>", true, true, None),
            ("std::collections::HashMap<u8, bool>", true, true, Some(2)),
            ("std::collections::HashMap<u8, bool, H>", true, false, Some(2)),
            ("std::collections::HashSet<u8>", true, true, Some(1)),
            ("std::collections::HashSet<u8, H>", true, false, Some(1)),
            ("std::collections::btree_map::BTreeMap<u8, bool>", true, true, None),
            ("std::collections::btree_set::BTreeSet<u8>", true, true, None),
            ("alloc::collections::binary_heap::BinaryHeap<u8>", true, true, None),
            ("std::collections::linked_list::LinkedList<u8>", true, true, None),
            ("std::collections::hash_set::HashMap<u8, bool>", false, false, None),
            ("other::collections::HashSet<u8>", false, false, None),
            ("rustc_hash::FxHashSet<u8>", true, true, None),
            ("()", false, false, None),
        ] {
            let ty = syn::parse_str::<Type>(text).unwrap();
            assert_eq!(types.supports_collection_api(&ty), api, "{text}");
            assert_eq!(types.supports_collection_constructor(&ty), constructor, "{text}");
            assert_eq!(types.collection_hasher_index(&ty), hasher, "{text}");
        }
        for name in [
            "NonZeroU8",
            "NonZeroU16",
            "NonZeroU32",
            "NonZeroU64",
            "NonZeroU128",
            "NonZeroUsize",
            "NonZeroI8",
            "NonZeroI16",
            "NonZeroI32",
            "NonZeroI64",
            "NonZeroI128",
            "NonZeroIsize",
        ] {
            assert!(types.supports_nonzero_api(&syn::parse_str::<Type>(name).unwrap()), "{name}");
        }
    }

    #[test]
    fn imported_and_shadowed_api_names_do_not_borrow_standard_evidence() {
        let defaults = Defaults::of(&syn::parse_file("struct Option<T>(T); struct Vec<T>(T);").unwrap());
        let imports = HashMap::from_iter([
            ("Opt".to_owned(), Some(vec!["core".into(), "option".into(), "Option".into()])),
            ("Foreign".to_owned(), Some(vec!["other".into(), "Option".into()])),
            ("Ambiguous".to_owned(), None),
            (
                "Absolute".to_owned(),
                Some(vec![ABSOLUTE_ROOT.into(), "core".into(), "option".into(), "Option".into()]),
            ),
        ]);
        let types = Types {
            abstracts: &[],
            defaulted: &[],
            imports: &imports,
            defaults: &defaults,
            aliases: None,
            self_type: None,
            self_associated: None,
        };
        assert!(types.supports_option_api(&parse_quote!(Opt<u8>)));
        assert!(types.supports_option_api(&parse_quote!(Absolute<u8>)));
        assert!(!types.supports_option_api(&parse_quote!(Option<u8>)));
        assert!(!types.supports_collection_api(&parse_quote!(Vec<u8>)));
        assert!(!types.supports_option_api(&parse_quote!(Foreign<u8>)));
        assert!(!types.supports_option_api(&parse_quote!(Ambiguous<u8>)));
        assert!(types.import_changes_name(&parse_quote!(Opt<u8>)));
        assert!(!types.import_changes_name(&parse_quote!(Unknown)));
        assert_eq!(
            resolved_type_path(&parse_quote!(::core::option::Option<u8>), &imports),
            Some(vec!["core".into(), "option".into(), "Option".into()])
        );
    }

    #[test]
    fn proven_default_matrix_requires_every_payload_bound_and_array_boundary() {
        let defaults = Defaults::of(
            &syn::parse_file("struct Missing; #[derive(Default)] struct Present; #[derive(Default)] struct Generic<T>(T);").unwrap(),
        );
        let imports = HashMap::default();
        let types = Types {
            abstracts: &["T".into()],
            defaulted: &["D".into()],
            imports: &imports,
            defaults: &defaults,
            aliases: None,
            self_type: None,
            self_associated: None,
        };
        for (text, expected) in [
            ("bool", true),
            ("core::primitive::u32", true),
            ("other::u32", false),
            ("D", true),
            ("T", false),
            ("Present", true),
            ("Missing", false),
            ("Generic<u8>", true),
            ("Generic<Missing>", false),
            ("Option<Missing>", true),
            ("Box<u8>", true),
            ("Box<Missing>", false),
            ("Box<T>", false),
            ("Box<D>", true),
            ("[Missing; 0]", true),
            ("[u8; 1]", true),
            ("[u8; 32]", true),
            ("[u8; 33]", false),
            ("[Missing; 1]", false),
            ("[T; 1]", false),
            ("[D; 1]", true),
            ("[u8; N]", false),
            ("(u8, Present)", true),
            ("(u8, Missing)", false),
            ("(Missing, u8)", false),
            ("&str", false),
            ("&[u8]", false),
            ("&u8", false),
            ("Result<u8,u8>", false),
        ] {
            assert_eq!(types.has_proven_default(&syn::parse_str(text).unwrap()), expected, "{text}");
        }
    }

    #[test]
    fn alias_instantiation_and_self_projection_preserve_concrete_types_and_cycles() {
        let defaults = Defaults::default();
        let imports = HashMap::default();
        let aliases = HashMap::from_iter([
            (
                "Byte".into(),
                Some(Alias {
                    parameters: vec![],
                    target: parse_quote!(u8),
                }),
            ),
            (
                "Next".into(),
                Some(Alias {
                    parameters: vec![],
                    target: parse_quote!(Byte),
                }),
            ),
            (
                "Wrap".into(),
                Some(Alias {
                    parameters: vec!["T".into()],
                    target: parse_quote!(Option<T>),
                }),
            ),
            (
                "Pair".into(),
                Some(Alias {
                    parameters: vec!["T".into()],
                    target: parse_quote!((T, [T; 2])),
                }),
            ),
            (
                "CycleA".into(),
                Some(Alias {
                    parameters: vec![],
                    target: parse_quote!(CycleB),
                }),
            ),
            (
                "CycleB".into(),
                Some(Alias {
                    parameters: vec![],
                    target: parse_quote!(CycleA),
                }),
            ),
            ("Ambiguous".into(), None),
        ]);
        let self_type: Type = parse_quote!(Vec<u8>);
        let associated = HashMap::from_iter([("Value".into(), parse_quote!(bool))]);
        let types = Types {
            abstracts: &[],
            defaulted: &[],
            imports: &imports,
            defaults: &defaults,
            aliases: Some(&aliases),
            self_type: Some(&self_type),
            self_associated: Some(&associated),
        };
        for (text, expected) in [
            ("Next", Some(parse_quote!(u8))),
            ("Wrap<u8>", Some(parse_quote!(Option<u8>))),
            ("Pair<bool>", Some(parse_quote!((bool, [bool; 2])))),
            ("CycleA", None),
            ("CycleB", None),
            ("Unknown", None),
            ("Ambiguous", None),
            ("()", None),
            ("other::Wrap<u8>", None),
            ("<T as Trait>::Wrap", None),
        ] {
            assert_eq!(types.instantiate_alias(&syn::parse_str(text).unwrap()), expected, "{text}");
        }
        for (text, expected) in [("Next", "u8"), ("CycleA", "CycleA"), ("other::Byte", "other::Byte")] {
            assert_eq!(
                types.resolve_alias(&syn::parse_str(text).unwrap()),
                &syn::parse_str::<Type>(expected).unwrap(),
                "{text}"
            );
        }
        assert_eq!(types.primitive_kind(&parse_quote!(&Next)), Kind::Unsigned);
        for (text, expected) in [
            ("Self", Some(&self_type)),
            ("Self::Value", associated.get("Value")),
            ("Other", None),
            ("Other::Value", None),
            ("Self::Missing", None),
            ("Self::Value::Extra", None),
            ("<Other as Trait>::Value", None),
            ("<Self as Trait>::Value", None),
            ("()", None),
        ] {
            assert_eq!(
                types.concrete_self_type_or_associated(&syn::parse_str(text).unwrap()),
                expected,
                "{text}"
            );
        }
    }

    #[test]
    fn iterator_reference_range_and_partial_array_evidence_has_exact_boundaries() {
        let defaults = Defaults::of(&syn::parse_file("struct Missing;").unwrap());
        let imports = HashMap::default();
        let types = Types {
            abstracts: &["T".into()],
            defaulted: &[],
            imports: &imports,
            defaults: &defaults,
            aliases: None,
            self_type: None,
            self_associated: None,
        };
        for (text, expected) in [
            ("I", true),
            ("&mut I", true),
            ("&I", false),
            ("impl Iterator<Item=u8>", true),
            ("impl core::iter::Iterator<Item=u8>", true),
            ("impl other::Iterator<Item=u8>", false),
            ("impl Clone", false),
            ("u8", false),
        ] {
            assert_eq!(
                types.supports_iterator_trait(&syn::parse_str(text).unwrap(), &["I".into()]),
                expected,
                "{text}"
            );
        }
        for (text, expected) in [
            ("Range<u8>", true),
            ("Range<i8>", true),
            ("core::ops::Range<i32>", true),
            ("Range<f64>", false),
            ("Range<bool>", false),
            ("Range<u8,u8>", false),
            ("other::ops::Range<u8>", false),
            ("RangeInclusive<u8>", false),
            ("u8", false),
        ] {
            assert_eq!(types.supports_range_iterator(&syn::parse_str(text).unwrap()), expected, "{text}");
        }
        for (text, expected) in [
            ("[u8; 32]", true),
            ("[u8; 33]", false),
            ("[u8; N]", true),
            ("[T; 1]", false),
            ("[Missing; 1]", false),
            ("[&u8; 1]", false),
            ("[Result<u8,u8>; 1]", false),
            ("[NonZeroU8; 1]", false),
            ("[&str; 1]", true),
            ("[&[u8]; 1]", true),
        ] {
            assert_eq!(types.array_may_default(&syn::parse_str(text).unwrap()), expected, "{text}");
        }
    }

    #[test]
    fn collection_default_evidence_distinguishes_custom_hashers_and_partial_local_shapes() {
        let mut defaults = Defaults::of(
            &syn::parse_file("struct Missing; struct Vec<T>(T); struct HashMap<K,V>(K,V); #[derive(Default)] struct Option<T>(T);")
                .unwrap(),
        );
        let imports = HashMap::default();
        for partial in [false, true] {
            if partial {
                defaults.mark_incomplete();
            }
            let types = Types {
                abstracts: &[],
                defaulted: &[],
                imports: &imports,
                defaults: &defaults,
                aliases: None,
                self_type: None,
                self_associated: None,
            };
            for (text, complete) in [
                ("Vec<u8>", false),
                ("HashMap<u8,u8>", false),
                ("Option<u8>", true),
                ("HashSet<u8,Missing>", false),
                ("std::collections::HashMap<u8,u8,Missing>", false),
                ("HashSet<u8>", true),
                ("std::collections::BTreeMap<u8,u8>", true),
                ("[Missing;0]", true),
                ("[u8;32]", true),
                ("str", false),
            ] {
                let expected = if text == "Vec<u8>" || text == "HashMap<u8,u8>" {
                    partial
                } else if text == "Option<u8>" {
                    !partial
                } else if text.ends_with("Missing>") {
                    partial
                } else {
                    complete
                };
                assert_eq!(
                    types.has_default(&syn::parse_str(text).unwrap()),
                    expected,
                    "{text}, partial={partial}"
                );
                if text == "Vec<u8>" || text == "HashMap<u8,u8>" {
                    assert_eq!(
                        types.prefers_default_constructor(&syn::parse_str(text).unwrap()),
                        partial,
                        "{text}, partial={partial}"
                    );
                }
            }
        }
    }

    #[test]
    fn local_standard_name_collisions_require_their_own_explicit_default_evidence() {
        for derives in [false, true] {
            let derive = if derives { "#[derive(Default)]" } else { "" };
            let defaults = Defaults::of(
                &syn::parse_file(&format!(
                    "{derive} struct String; {derive} struct Option<T>(T); {derive} struct Vec<T>(T);
                 {derive} struct HashMap<K,V>(K,V); struct Missing;"
                ))
                .unwrap(),
            );
            let imports = HashMap::default();
            let types = Types {
                abstracts: &[],
                defaulted: &[],
                imports: &imports,
                defaults: &defaults,
                aliases: None,
                self_type: None,
                self_associated: None,
            };
            for text in ["String", "Option<u8>", "Vec<u8>", "HashMap<u8,u8>"] {
                assert_eq!(
                    types.has_proven_default(&syn::parse_str(text).unwrap()),
                    derives,
                    "{text}, derives={derives}"
                );
                if text.starts_with("Vec") || text.starts_with("HashMap") {
                    assert_eq!(
                        types.has_default(&syn::parse_str(text).unwrap()),
                        derives,
                        "{text}, derives={derives}"
                    );
                }
            }
            for text in ["Option<Missing>", "Vec<Missing>", "HashMap<u8,Missing>", "HashMap<Missing,u8>"] {
                assert!(
                    !types.has_proven_default(&syn::parse_str(text).unwrap()),
                    "{text}, derives={derives}"
                );
            }
        }
    }

    #[test]
    fn renamed_collection_and_undefaulted_generic_evidence_has_exact_oracles() {
        let defaults = Defaults::default();
        let imports = HashMap::default();
        let types = Types {
            abstracts: &[],
            defaulted: &[],
            imports: &imports,
            defaults: &defaults,
            aliases: None,
            self_type: None,
            self_associated: None,
        };
        assert!(types.renamed_collection_has_default(&parse_quote!(rustc_hash::FxHashMap<u8,u8>)));
        assert!(types.renamed_collection_has_default(&parse_quote!(rustc_hash::FxHashSet<u8>)));
        assert!(!types.renamed_collection_has_default(&parse_quote!(other::FxHashSet<u8>)));
        let generic: Generics = parse_quote!(<T, U: Default, V: Clone>);
        assert_eq!(undefaulted_parameters(&generic, &DefaultPaths::default()), ["T", "V"]);
    }

    #[test]
    fn foreign_error_and_iterator_bound_matrices_require_full_paths_and_bounds() {
        let imports = HashMap::from_iter([
            ("IoError".into(), Some(vec!["std".into(), "io".into(), "Error".into()])),
            ("FormatError".into(), Some(vec!["core".into(), "fmt".into(), "Error".into()])),
            ("Unknown".into(), None),
            ("I".into(), Some(vec!["core".into(), "iter".into(), "Iterator".into()])),
        ]);
        for (text, expected) in [
            ("std::io::Error", true),
            ("IoError", true),
            ("core::fmt::Error", false),
            ("FormatError", false),
            ("crate::Error", false),
            ("self::Error", false),
            ("super::Error", false),
            ("Error", false),
            ("Unknown", false),
            ("std::io::Other", false),
            ("()", false),
        ] {
            assert_eq!(is_foreign_error(&syn::parse_str(text).unwrap(), &imports), expected, "{text}");
        }
        let item: syn::ItemFn = parse_quote!(
            fn f<'a, const N: usize, A: Clone, B: I<Item = u8>, C, D, E>()
            where
                C: core::iter::Iterator<Item = u8>,
                D: other::Iterator<Item = u8>,
                E: Clone,
            {
            }
        );
        assert_eq!(
            Types::standard_iterator_bounded_parameters(&item.sig.generics, &imports),
            ["B", "C"]
        );
        let defaults = Defaults::of(&syn::parse_file("struct Missing;").unwrap());
        let types = Types {
            abstracts: &[],
            defaulted: &[],
            imports: &imports,
            defaults: &defaults,
            aliases: None,
            self_type: None,
            self_associated: None,
        };
        for (output, expected) in [
            (parse_quote!(-> Result<u8, Missing>), true),
            (parse_quote!(-> Result<u8, u8>), false),
            (parse_quote!(-> Result<u8, std::io::Error>), true),
            (parse_quote!(-> u8), false),
            (syn::ReturnType::Default, false),
        ] {
            assert_eq!(returns_undefaultable_error(&output, &types), expected, "{output:?}");
        }
    }

    fn defaults(source: &str) -> Defaults {
        Defaults::of(&syn::parse_file(source).expect("test source should parse"))
    }

    fn empty_path_type() -> Type {
        Type::Path(TypePath {
            attrs: Vec::new(),
            qself: None,
            path: Path {
                leading_colon: None,
                segments: Punctuated::<PathSegment, syn::token::PathSep>::new(),
            },
        })
    }

    #[test]
    fn foreign_error_detection_rejects_empty_paths_and_empty_import_prefixes() {
        let mut imports = HashMap::default();
        let imported_error: Type = parse_quote!(TheirError);

        let _old = imports.insert("TheirError".to_owned(), Some(Vec::new()));
        let _old = imports.insert("Local".to_owned(), Some(vec!["Local".to_owned()]));

        assert!(!is_foreign_error(&empty_path_type(), &HashMap::default()));
        assert!(!is_foreign_error(&imported_error, &imports));
        assert!(!is_foreign_error(&parse_quote!(<Thing as Trait>::Error), &imports));
        assert!(!is_foreign_error(&parse_quote!(Local), &imports));
        assert!(is_foreign_error(&parse_quote!(std::num::ParseIntError), &HashMap::default()));
    }

    #[test]
    fn standard_non_default_detection_handles_unknown_and_imported_names() {
        let defaults = Defaults::default();
        let abstracts = Vec::new();
        let defaulted = Vec::new();
        let aliases = HashMap::default();
        let mut imports = HashMap::default();
        let _old = imports.insert(
            "Instant".to_owned(),
            Some(vec!["std".to_owned(), "time".to_owned(), "Instant".to_owned()]),
        );
        let types = Types {
            abstracts: &abstracts,
            defaulted: &defaulted,
            imports: &imports,
            defaults: &defaults,
            aliases: Some(&aliases),
            self_type: None,
            self_associated: None,
        };

        assert!(!types.lacks_default(&empty_path_type()));
        assert!(types.lacks_default(&parse_quote!(Instant)));
        assert!(types.lacks_default(&parse_quote!(std::time::SystemTime)));
        assert!(types.lacks_default(&parse_quote!(std::cmp::Ordering)));
        assert!(types.lacks_default(&parse_quote!(std::process::Command)));
        assert!(types.lacks_default(&parse_quote!(std::process::Stdio)));
        assert!(types.lacks_default(&parse_quote!(std::fs::File)));
        assert!(types.lacks_default(&parse_quote!(std::sync::MutexGuard<'static, ()>)));
        assert!(!types.lacks_default(&parse_quote!(SystemTime)));
        assert!(!types.lacks_default(&parse_quote!(&'static str)));
    }

    #[test]
    fn positive_default_evidence_covers_containers_wrappers_and_tuples() {
        let defaults = Defaults::default();
        let abstracts = Vec::new();
        let defaulted = Vec::new();
        let aliases = HashMap::default();
        let imports = HashMap::default();
        let types = Types {
            abstracts: &abstracts,
            defaulted: &defaulted,
            imports: &imports,
            defaults: &defaults,
            aliases: Some(&aliases),
            self_type: None,
            self_associated: None,
        };

        for ty in [
            parse_quote!(std::collections::BTreeMap<u8, u8>),
            parse_quote!(Box<bool>),
            parse_quote!((bool, String)),
            parse_quote!([bool; 32]),
            parse_quote!([Unknown; 0]),
        ] {
            assert!(types.has_default(&ty), "{ty:?}");
        }

        assert!(!types.has_default(&parse_quote!([bool; 33])));
        assert!(!types.has_default(&parse_quote!([Unknown; 1])));
        assert!(!types.has_default(&parse_quote!([bool; LENGTH])));
        assert!(!types.has_default(&parse_quote!([bool; true])));
        assert!(!types.has_default(&parse_quote!([bool; 999999999999999999999999999999999999])));
    }

    #[test]
    fn default_and_iterator_evidence_cover_conservative_edge_shapes() {
        let defaults = defaults("#[derive(Default)] struct NonZeroU8(u8); struct NoDefault;");
        let abstracts = Vec::new();
        let defaulted = vec!["T".to_owned()];
        let aliases = HashMap::from_iter([
            (
                "PayloadAlias".to_owned(),
                Some(Alias {
                    parameters: vec!["T".to_owned()],
                    target: parse_quote!(Option<T>),
                }),
            ),
            (
                "QualifiedAlias".to_owned(),
                Some(Alias {
                    parameters: Vec::new(),
                    target: parse_quote!(crate::Value),
                }),
            ),
        ]);
        let imports = HashMap::default();
        let types = Types {
            abstracts: &abstracts,
            defaulted: &defaulted,
            imports: &imports,
            defaults: &defaults,
            aliases: Some(&aliases),
            self_type: None,
            self_associated: None,
        };

        assert!(!types.has_default(&parse_quote!(std::collections::HashSet<bool, NoDefault>)));
        assert!(!types.has_default(&parse_quote!(std::collections::HashMap<bool, bool, NoDefault>)));
        assert!(types.has_default(&parse_quote!(std::fmt::Error)));
        assert!(types.has_default(&parse_quote!(std::collections::hash_map::RandomState)));
        assert!(types.has_default(&parse_quote!(T)));
        let bool_type: Type = parse_quote!(bool);
        assert_eq!(types.payload(&parse_quote!(PayloadAlias<bool>), 0).as_deref(), Some(&bool_type));
        assert!(types.payload(&parse_quote!(QualifiedAlias), 0).is_none());
        assert!(types.supports_range_iterator(&parse_quote!(core::ops::Range<u8>)));
        assert!(!types.supports_range_iterator(&parse_quote!(core::ops::Range<bool>)));
        assert!(!types.supports_range_iterator(&parse_quote!(core::ops::Range<u8, u16>)));
        assert!(!types.has_default(&parse_quote!(impl Send)));
        assert!(types.has_proven_default(&parse_quote!(T)));
        assert!(types.has_proven_default(&parse_quote!(Option<bool>)));
        assert!(types.has_proven_default(&parse_quote!(String)));
        assert!(types.has_proven_default(&parse_quote!(Box<bool>)));
        assert!(!types.has_proven_default(&parse_quote!(dependency::Box<bool>)));
        assert!(!types.has_proven_default(&parse_quote!([dependency::Box<bool>; 1])));
        assert!(types.has_proven_default(&parse_quote!(NonZeroU8)));
        assert!(types.has_proven_default(&parse_quote!([NonZeroU8; 1])));
        assert!(types.has_proven_default(&parse_quote!([bool; 0])));
        assert!(!types.has_proven_default(&parse_quote!([bool; LENGTH])));
        assert!(!types.has_proven_default(&parse_quote!([bool; true])));
        assert!(!types.has_proven_default(&parse_quote!([bool; 999999999999999999999999999999999999])));
        assert!(types.has_proven_default(&parse_quote!((bool, String))));
        assert!(!types.has_proven_default(&parse_quote!((bool, NoDefault))));
        assert!(types.supports_primitive_kind(&parse_quote!(&u8), Kind::Unsigned));
        assert!(!types.supports_primitive_kind(&parse_quote!(crate::u8), Kind::Unsigned));
        assert!(!types.has_proven_default(&parse_quote!(dependency::u8)));
        assert!(!types.import_changes_name(&parse_quote!(bool)));
        assert!(types.supports_collection_api(&parse_quote!(rustc_hash::FxHashSet<bool>)));
        assert!(types.supports_collection_api(&parse_quote!(
            std::collections::hash_map::HashMap<bool, bool>
        )));
        assert!(types.supports_collection_api(&parse_quote!(
            alloc::collections::btree_map::BTreeMap<bool, bool>
        )));
        assert!(!types.supports_collection_api(&parse_quote!(
            std::collections::hash_set::HashMap<bool, bool>
        )));
        assert!(!types.supports_collection_constructor(&parse_quote!(std::collections::HashSet<bool, NoDefault>)));
        assert!(!types.supports_nonzero_api(&parse_quote!(NonZeroU8)));
        assert!(types.supports_nonzero_api(&parse_quote!(std::num::NonZeroU8)));
        assert!(types.supports_nonzero_api(&parse_quote!(core::num::NonZeroI16)));
        assert!(!types.supports_nonzero_api(&parse_quote!(crate::NonZeroU8)));
        let mut empty_impl_trait: Type = parse_quote!(impl Iterator);
        let Type::ImplTrait(implementation) = &mut empty_impl_trait else {
            unreachable!("the fixture is explicitly parsed as an impl-trait type");
        };
        implementation.bounds.clear();
        assert!(!types.supports_iterator_trait(&empty_impl_trait, &[]));
        assert!(types.supports_iterator_trait(&parse_quote!(&mut T), &["T".to_owned()]));
        assert!(!types.supports_iterator_trait(&parse_quote!(&T), &["T".to_owned()]));
        assert!(types.supports_iterator_trait(&parse_quote!(&mut impl Iterator<Item = u8>), &[]));

        let probe: syn::ItemFn = parse_quote!(
            fn probe<'a, const N: usize, T: Iterator, U>()
            where
                U: core::iter::Iterator,
                Vec<U>: Iterator,
                T: 'a,
            {
            }
        );
        let generics: Generics = probe.sig.generics;
        let mut bounded = Types::standard_iterator_bounded_parameters(&generics, &imports);
        bounded.sort();
        assert_eq!(bounded, ["T", "U"]);
    }

    #[test]
    fn wildcard_imports_do_not_borrow_package_default_evidence() {
        let defaults = Defaults::of(&syn::parse_file("struct Widget;").expect("the defaults fixture parses"));
        let abstracts = Vec::new();
        let defaulted = Vec::new();
        let aliases = HashMap::default();
        let imports = HashMap::from_iter([("*".to_owned(), None)]);
        let types = Types {
            abstracts: &abstracts,
            defaulted: &defaulted,
            imports: &imports,
            defaults: &defaults,
            aliases: Some(&aliases),
            self_type: None,
            self_associated: None,
        };

        assert!(!types.has_default(&parse_quote!(Widget)));
        assert!(!types.lacks_default(&parse_quote!(Widget)));
        assert!(!types.lacks_default(&parse_quote!(crate::Widget)));
    }

    #[test]
    fn an_explicit_external_import_does_not_borrow_package_default_evidence() {
        let defaults = defaults("#[derive(Default)] struct Config;");
        let abstracts = Vec::new();
        let defaulted = Vec::new();
        let aliases = HashMap::default();
        let imports = HashMap::from_iter([("Config".to_owned(), Some(vec!["external".to_owned(), "Config".to_owned()]))]);
        let types = Types {
            abstracts: &abstracts,
            defaulted: &defaulted,
            imports: &imports,
            defaults: &defaults,
            aliases: Some(&aliases),
            self_type: None,
            self_associated: None,
        };

        assert!(!types.has_proven_default(&parse_quote!(Config)));
    }

    #[test]
    fn a_same_crate_qualified_import_borrows_package_default_evidence() {
        let defaults = defaults("struct Artifact;");
        let abstracts = Vec::new();
        let defaulted = Vec::new();
        let aliases = HashMap::default();
        let imports = HashMap::from_iter([(
            "Artifact".to_owned(),
            Some(vec!["crate".to_owned(), "catalog".to_owned(), "Artifact".to_owned()]),
        )]);
        let types = Types {
            abstracts: &abstracts,
            defaulted: &defaulted,
            imports: &imports,
            defaults: &defaults,
            aliases: Some(&aliases),
            self_type: None,
            self_associated: None,
        };

        assert!(types.lacks_default(&parse_quote!(Artifact)));
    }

    #[test]
    fn recursive_reference_queries_share_alias_cycle_detection() {
        let defaults = Defaults::default();
        let imports = HashMap::default();
        let aliases = HashMap::from_iter([
            (
                "A".to_owned(),
                Some(Alias {
                    parameters: Vec::new(),
                    target: parse_quote!(&'static mut B),
                }),
            ),
            (
                "B".to_owned(),
                Some(Alias {
                    parameters: Vec::new(),
                    target: parse_quote!(&'static mut A),
                }),
            ),
        ]);
        let types = Types {
            abstracts: &[],
            defaulted: &[],
            imports: &imports,
            defaults: &defaults,
            aliases: Some(&aliases),
            self_type: None,
            self_associated: None,
        };
        let original = parse_quote!(A);
        assert_eq!(types.primitive_kind(&original), Kind::Unknown);
        assert!(!types.supports_primitive_kind(&original, Kind::Unsigned));
        assert!(!types.supports_peekable_chars(&original));
        assert!(!types.supports_iterator_trait(&original, &[]));
    }

    #[test]
    fn resource_type_proofs_require_exact_standard_paths_and_generic_shapes() {
        let defaults = Defaults::default();
        let imports = HashMap::default();
        let types = Types {
            abstracts: &[],
            defaulted: &[],
            imports: &imports,
            defaults: &defaults,
            aliases: None,
            self_type: None,
            self_associated: None,
        };
        for (text, expected) in [
            ("std::iter::Peekable<std::str::Chars<'static>>", true),
            ("core::iter::Peekable<core::str::Chars<'static>>", true),
            ("&mut std::iter::Peekable<std::str::Chars<'static>>", true),
            ("&std::iter::Peekable<std::str::Chars<'static>>", false),
            ("u8", false),
            ("()", false),
            ("std::iter::Peekable", false),
            ("std::iter::Peekable<'static>", false),
            ("std::iter::Peekable<std::str::Chars<'static>, u8>", false),
            ("std::iter::Peekable<other::Chars<'static>>", false),
            ("std::iter::Peekable<(u8,)>", false),
            ("other::Peekable<std::str::Chars<'static>>", false),
        ] {
            assert_eq!(types.supports_peekable_chars(&syn::parse_str(text).unwrap()), expected, "{text}");
        }
        for (text, expected) in [
            ("&str", true),
            ("&mut str", true),
            ("&std::primitive::str", true),
            ("&core::primitive::str", true),
            ("str", false),
            ("&u8", false),
            ("&()", false),
            ("&other::str", false),
        ] {
            assert_eq!(types.supports_str_reference(&syn::parse_str(text).unwrap()), expected, "{text}");
        }
        let shadowed = Types {
            abstracts: &["str".to_owned()],
            ..types
        };
        assert!(!shadowed.supports_str_reference(&parse_quote!(&str)));
        assert!(!shadowed.supports_peekable_chars(&parse_quote!(std::iter::Peekable<str>)));
        let generic = Types {
            abstracts: &["T".to_owned()],
            ..types
        };
        assert!(!generic.supports_peekable_chars(&parse_quote!(T)));
        assert!(!generic.supports_str_reference(&parse_quote!(T)));
        assert!(!generic.supports_peekable_chars(&parse_quote!(std::iter::Peekable<T>)));
        assert!(types.supports_char_api());
        let shadowed = Types {
            abstracts: &["char".to_owned()],
            ..types
        };
        assert!(!shadowed.supports_char_api());
    }

    #[test]
    fn standard_trait_and_constructor_proofs_reject_opaque_imports() {
        let defaults = Defaults::default();
        let imports = HashMap::default();
        let types = Types {
            abstracts: &[],
            defaulted: &[],
            imports: &imports,
            defaults: &defaults,
            aliases: None,
            self_type: None,
            self_associated: None,
        };
        for (text, expected) in [
            ("Default", true),
            ("std::default::Default", true),
            ("core::default::Default", true),
            ("Default<u8>", false),
            ("&Default", false),
            ("other::Default", false),
        ] {
            assert_eq!(types.supports_default_trait(&syn::parse_str(text).unwrap()), expected, "{text}");
        }
        let mut empty: syn::TypePath = parse_quote!(Vec);
        empty.path.segments.clear();
        assert!(!types.supports_empty_default_constructor(&Type::Path(empty)));
        for (text, expected) in [
            ("&str", false),
            ("<T as Trait>::Value", false),
            ("std::collections::VecDeque<u8>", true),
            ("std::collections::vec_deque::VecDeque<u8>", true),
            ("std::collections::linked_list::LinkedList<u8>", true),
            ("std::collections::hash_set::HashSet<u8>", false),
            ("Vec<u8, Allocator>", false),
        ] {
            assert_eq!(
                types.supports_empty_default_constructor(&syn::parse_str(text).unwrap()),
                expected,
                "{text}"
            );
        }
        for (path, expected) in [
            (None, false),
            (Some(vec!["other".to_owned(), "Default".to_owned()]), false),
            (Some(vec!["core".to_owned(), "default".to_owned(), "Default".to_owned()]), true),
            (
                Some(vec![
                    ABSOLUTE_ROOT.to_owned(),
                    "std".to_owned(),
                    "default".to_owned(),
                    "Default".to_owned(),
                ]),
                true,
            ),
        ] {
            let imports = HashMap::from_iter([("Default".to_owned(), path)]);
            let shadowed = Types {
                imports: &imports,
                ..types
            };
            assert_eq!(shadowed.supports_bare_default_trait(), expected);
        }
        let imports = HashMap::from_iter([("std".to_owned(), None)]);
        let shadowed = Types {
            imports: &imports,
            ..types
        };
        assert!(!shadowed.supports_default_trait(&parse_quote!(std::default::Default)));
        assert!(!shadowed.supports_empty_default_constructor(&parse_quote!(std::vec::Vec<u8>)));
        let imports = HashMap::from_iter([("char".to_owned(), None)]);
        assert!(
            !Types {
                imports: &imports,
                ..types
            }
            .supports_char_api()
        );
    }

    #[test]
    fn cyclic_file_wide_aliases_stop_at_the_original_type() {
        let defaults = Defaults::default();
        let abstracts = Vec::new();
        let defaulted = Vec::new();
        let imports = HashMap::default();
        let aliases = HashMap::from_iter([
            (
                "A".to_owned(),
                Some(Alias {
                    parameters: Vec::new(),
                    target: parse_quote!(B),
                }),
            ),
            (
                "B".to_owned(),
                Some(Alias {
                    parameters: Vec::new(),
                    target: parse_quote!(A),
                }),
            ),
        ]);
        let types = Types {
            abstracts: &abstracts,
            defaulted: &defaulted,
            imports: &imports,
            defaults: &defaults,
            aliases: Some(&aliases),
            self_type: None,
            self_associated: None,
        };
        let original: Type = parse_quote!(A);

        assert_eq!(types.resolve_alias(&original), &original);
        assert!(types.payload(&original, 0).is_none());
    }

    #[test]
    fn chained_generic_aliases_substitute_each_declared_parameter_role() {
        let defaults = Defaults::default();
        let abstracts = Vec::new();
        let defaulted = Vec::new();
        let imports = HashMap::default();
        let aliases = HashMap::from_iter([
            (
                "Outer".to_owned(),
                Some(Alias {
                    parameters: vec!["T".to_owned()],
                    target: parse_quote!(Inner<Option<T>>),
                }),
            ),
            (
                "Inner".to_owned(),
                Some(Alias {
                    parameters: vec!["U".to_owned()],
                    target: parse_quote!(Result<U, Error>),
                }),
            ),
        ]);
        let types = Types {
            abstracts: &abstracts,
            defaulted: &defaulted,
            imports: &imports,
            defaults: &defaults,
            aliases: Some(&aliases),
            self_type: None,
            self_associated: None,
        };

        assert_eq!(
            types.payload(&parse_quote!(Outer<bool>), 0).as_deref(),
            Some(&parse_quote!(Option<bool>))
        );
        assert_eq!(types.payload(&parse_quote!(Outer<bool>), 1).as_deref(), Some(&parse_quote!(Error)));
    }

    #[test]
    fn generic_substitution_descends_through_every_supported_type_container() {
        let substitutions = HashMap::from_iter([("T".to_owned(), parse_quote!(bool))]);

        for (input, expected) in [
            (parse_quote!([T; 1]), parse_quote!([bool; 1])),
            (parse_quote!((T)), parse_quote!((bool))),
            (parse_quote!(*const T), parse_quote!(*const bool)),
            (parse_quote!(&T), parse_quote!(&bool)),
            (parse_quote!([T]), parse_quote!([bool])),
            (parse_quote!((T, Option<T>)), parse_quote!((bool, Option<bool>))),
            (parse_quote!(impl Send), parse_quote!(impl Send)),
        ] {
            assert_eq!(substitute_type(&input, &substitutions), expected);
        }

        let grouped = Type::Group(syn::TypeGroup {
            attrs: Vec::new(),
            group_token: syn::token::Group::default(),
            elem: Box::new(parse_quote!(T)),
        });
        let expected = Type::Group(syn::TypeGroup {
            attrs: Vec::new(),
            group_token: syn::token::Group::default(),
            elem: Box::new(parse_quote!(bool)),
        });
        assert_eq!(substitute_type(&grouped, &substitutions), expected);
    }

    #[test]
    fn defensive_type_shapes_withhold_unsupported_standard_apis() {
        let defaults = defaults(
            "
            struct Vec<T>(T);
            struct HashMap<K, V>(K, V);
            struct Box<T>(T);
            ",
        );
        let abstracts = Vec::new();
        let defaulted = Vec::new();
        let aliases = HashMap::default();
        let imports = HashMap::default();
        let types = Types {
            abstracts: &abstracts,
            defaulted: &defaulted,
            imports: &imports,
            defaults: &defaults,
            aliases: Some(&aliases),
            self_type: None,
            self_associated: None,
        };
        let opaque: Type = parse_quote!(impl Send);

        assert!(!types.has_default(&parse_quote!(Vec<bool>)));
        assert!(!types.has_default(&parse_quote!(HashMap<bool, bool>)));
        assert!(!types.supports_primitive_kind(&opaque, Kind::Unsigned));
        assert!(!types.supports_nonzero_api(&opaque));
        assert!(!types.supports_iterator_trait(&parse_quote!(bool), &[]));
        assert!(!types.supports_wrapper_api(&parse_quote!(Box<bool>)));
        assert!(!types.supports_wrapper_api(&opaque));
        assert!(!types.renamed_collection_has_default(&opaque));

        let function: syn::ItemFn = parse_quote!(
            fn probe<'a, const N: usize, T: 'a + ::std::iter::Iterator>()
            where
                'a: 'static,
                [T; N]: ::std::iter::Iterator,
            {
            }
        );
        assert_eq!(
            Types::standard_iterator_bounded_parameters(&function.sig.generics, &imports),
            vec!["T".to_owned()]
        );
    }

    #[test]
    fn result_aliases_and_direct_errors_can_both_be_screened() {
        let defaults = defaults(
            "
            struct Error;
            type Result<T> = core::result::Result<T, Error>;
            ",
        );
        let abstracts = Vec::new();
        let defaulted = Vec::new();
        let aliases = HashMap::default();
        let imports = HashMap::default();
        let types = Types {
            abstracts: &abstracts,
            defaulted: &defaulted,
            imports: &imports,
            defaults: &defaults,
            aliases: Some(&aliases),
            self_type: None,
            self_associated: None,
        };
        let alias_output: ReturnType = parse_quote!(-> Result<bool>);
        let direct: ReturnType = parse_quote!(-> Result<bool, std::io::Error>);

        assert!(returns_undefaultable_error(&alias_output, &types));
        assert!(returns_undefaultable_error(&direct, &types));
    }

    #[test]
    fn abstract_type_detection_handles_empty_paths_and_boxed_trait_objects() {
        assert!(!is_abstract_type(&empty_path_type(), &[]));
        assert!(is_abstract_type(&parse_quote!(Box<dyn core::fmt::Debug>), &[]));
        assert!(!is_abstract_type(
            &parse_quote!(<Concrete as Abstract>::Item),
            &[String::from("Abstract")]
        ));
    }
}
