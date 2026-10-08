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
    /// Bare unimported names are local. Qualified paths remain unknown rather than borrowing
    /// evidence from an unrelated local declaration or alias with the same final segment.
    fn package_type_name(&self, ty: &Type) -> Option<String> {
        let path = resolved_type_path(ty, self.imports)?;
        let last = path.last()?.clone();

        (path.len() == 1 && !self.imports.contains_key("*")).then_some(last)
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
        loop {
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
        let ty = self.resolve_alias(ty);
        if let Type::Reference(reference) = strip(ty) {
            return self.supports_primitive_kind(&reference.elem, expected);
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
        let ty = self.resolve_alias(ty);
        if let Type::Reference(reference) = strip(ty) {
            return reference.mutability.is_some() && self.supports_iterator_trait(&reference.elem, iterator_bounded);
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
