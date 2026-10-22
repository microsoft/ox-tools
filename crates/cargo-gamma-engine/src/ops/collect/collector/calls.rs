// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Source-visible call signatures, indexed by declaration scope and calling convention.

use syn::visit::{self, Visit};
use syn::{Expr, FnArg, GenericArgument, Member, PathArguments, Signature, Type};

use crate::cfg::CfgSet;
use crate::ops::collect::collector::indexes::{ABSOLUTE_ROOT, ScopePath, block_id, module_id};
use crate::ops::collect::collector::predicates::{expr_attrs, stmt_attrs};
use crate::ops::collect::defaults::{impl_item_attrs, item_attrs, trait_item_attrs};
use crate::{HashMap, HashSet};

/// Separates incomplete evidence from proven absence and a uniquely known operator output.
pub(super) enum OperatorResult<'a> {
    Unknown,
    Missing,
    Output(&'a Type),
}

/// Source evidence for call results, nominal fields, and operator admission.
///
/// An absent map entry is unindexed; a `None` entry reserves a name whose signature or field type
/// is ambiguous or unavailable, preventing a lookup from falling through to an outer declaration.
#[derive(Default)]
pub(super) struct Calls {
    free: HashMap<(ScopePath, String), Option<Signature>>,
    qualified: HashMap<Vec<String>, Option<Signature>>,
    modules: HashMap<ScopePath, Vec<String>>,
    blocks: HashSet<ScopePath>,
    fields: HashMap<(Vec<String>, String), Option<Type>>,
    noncopy_members: HashMap<Vec<String>, Vec<Type>>,
    named_methods: HashSet<String>,
    enums: HashSet<Vec<String>>,
    structures: HashSet<Vec<String>>,
    block_types: HashMap<ScopePath, HashSet<String>>,
    identities: crate::ops::collect::default_identity::Identities,
    operators: Vec<(Vec<String>, String, Type, Type)>,
    // Negative operator evidence is usable only while the local inventory is complete.
    closed: bool,
}

impl Calls {
    pub(super) fn has_method_named(&self, name: &str) -> bool {
        self.named_methods.contains(name)
    }

    /// Resolves an inherent method through its nominal owner, withholding ambiguous signatures.
    pub(super) fn method_signature(
        &self,
        ty: &Type,
        name: &str,
        scope: &ScopePath,
        imports: &HashMap<String, Option<Vec<String>>>,
    ) -> Option<&Signature> {
        let ty = match super::values::strip(ty) {
            Type::Reference(reference) => &reference.elem,
            other => other,
        };
        let Type::Path(path) = ty else { return None };
        let mut owner = self.nominal_path(&path.path, scope, imports)?;
        owner.push(name.to_owned());
        self.qualified.get(&owner)?.as_ref()
    }

    /// Supplies trusted source fields to the collector's bounded move-sensitivity classifier.
    pub(super) fn has_noncopy_field(
        &self,
        ty: &Type,
        scope: &ScopePath,
        imports: &HashMap<String, Option<Vec<String>>>,
        noncopy: impl Fn(&Type) -> bool,
    ) -> bool {
        let Type::Path(path) = super::values::strip(ty) else { return false };
        let Some(owner) = self.nominal_path(&path.path, scope, imports) else {
            return false;
        };
        self.noncopy_members.get(&owner).is_some_and(|fields| fields.iter().any(noncopy))
    }

    /// Builds declaration-scope evidence under the active cfg and existing lexical indexes.
    pub(super) fn of(file: &syn::File, cfg: &CfgSet, indexes: &super::indexes::Indexes) -> Self {
        let mut walk = Walk {
            calls: Self::default(),
            scope: Vec::new(),
            module: Vec::new(),
            owner: None,
            owner_parameters: Vec::new(),
            structural_trusted: super::super::default_identity::nominal_attributes(&file.attrs),
            cfg,
            indexes,
        };
        let _previous = walk.calls.modules.insert(Vec::new(), Vec::new());
        walk.calls.closed = super::noop::LocalCalls::nontransforming_attributes(&file.attrs);
        walk.visit_file(file);
        walk.calls.identities = crate::ops::collect::default_identity::Identities::of(file, cfg);
        walk.calls
    }

    /// Looks up one nominal field without treating missing or ambiguous layouts as type evidence.
    pub(super) fn field(
        &self,
        ty: &Type,
        member: &Member,
        scope: &ScopePath,
        imports: &HashMap<String, Option<Vec<String>>>,
    ) -> Option<Type> {
        let ty = match ty {
            Type::Reference(reference) => &reference.elem,
            other => other,
        };
        let Type::Path(path) = ty else { return None };
        let owner = self.nominal_path(&path.path, scope, imports)?;
        let member = match member {
            Member::Named(name) => name.to_string(),
            Member::Unnamed(index) => index.index.to_string(),
        };
        self.fields.get(&(owner, member))?.clone()
    }

    /// Attaches enclosing-module and block-shadow evidence to each lexical import environment.
    pub(super) fn annotate_imports(&self, scopes: &mut HashMap<ScopePath, HashMap<String, Option<Vec<String>>>>) {
        for (scope, imports) in scopes {
            let mut enclosing = scope.clone();
            let mut shadows = Vec::new();
            loop {
                if let Some(names) = self.block_types.get(&enclosing) {
                    shadows.extend(names.iter().cloned());
                }
                if let Some(module) = self.modules.get(&enclosing) {
                    let _old = imports.insert(super::types::LOCAL_MODULE.to_owned(), Some(module.clone()));
                    let _old = imports.insert(super::types::BLOCK_TYPES.to_owned(), Some(shadows));
                    break;
                }
                if enclosing.pop().is_none() {
                    break;
                }
            }
        }
    }

    /// Uses a closed inventory to distinguish an absent operator from a unique output type.
    pub(super) fn operator_result(
        &self,
        left: &Type,
        right: &Type,
        operator: &str,
        scope: &ScopePath,
        imports: &HashMap<String, Option<Vec<String>>>,
    ) -> OperatorResult<'_> {
        if !self.closed {
            return OperatorResult::Unknown;
        }
        let Type::Path(left) = left else { return OperatorResult::Unknown };
        let Some(owner) = self.nominal_path(&left.path, scope, imports) else {
            return OperatorResult::Unknown;
        };
        if self.identities.resolve_path(&owner).is_none() {
            return OperatorResult::Unknown;
        }
        let mut found = None;
        for (implemented, operation, rhs, output) in &self.operators {
            if implemented == &owner && operation == operator {
                let same_rhs = rhs == right
                    || match (rhs, right) {
                        (Type::Path(left), Type::Path(right)) => {
                            left.path.segments.last().map(|segment| &segment.arguments)
                                == right.path.segments.last().map(|segment| &segment.arguments)
                                && self.nominal_path(&left.path, scope, imports) == self.nominal_path(&right.path, scope, imports)
                        }
                        _ => false,
                    };
                if same_rhs {
                    if found.is_some() {
                        return OperatorResult::Unknown;
                    }
                    found = Some(output);
                }
            }
        }
        found.map_or(OperatorResult::Missing, OperatorResult::Output)
    }

    /// Proves absence only for a resolved local owner in a complete operator inventory.
    pub(super) fn unit_operator_absent(
        &self,
        ty: &Type,
        operator: &str,
        scope: &ScopePath,
        imports: &HashMap<String, Option<Vec<String>>>,
    ) -> bool {
        if !self.closed {
            return false;
        }
        let Type::Path(path) = ty else { return false };
        let Some(owner) = self.nominal_path(&path.path, scope, imports) else {
            return false;
        };
        self.identities.resolve_path(&owner).is_some()
            && !self
                .operators
                .iter()
                .any(|(left, operation, _, _)| left == &owner && operation == operator)
    }

    pub(super) fn is_enum(&self, path: &syn::Path, scope: &ScopePath, imports: &HashMap<String, Option<Vec<String>>>) -> bool {
        self.nominal_path(path, scope, imports)
            .is_some_and(|path| self.enums.contains(&path))
    }

    pub(super) fn structure(&self, path: &syn::Path, scope: &ScopePath, imports: &HashMap<String, Option<Vec<String>>>) -> bool {
        self.nominal_path(path, scope, imports)
            .is_some_and(|path| self.structures.contains(&path))
    }

    pub(super) fn same_type(
        &self,
        left: &syn::Path,
        right: &syn::Path,
        scope: &ScopePath,
        imports: &HashMap<String, Option<Vec<String>>>,
    ) -> bool {
        self.nominal_path(left, scope, imports)
            .is_some_and(|left| self.nominal_path(right, scope, imports) == Some(left))
    }

    /// Resolves lexical imports and relative modules without crossing an unresolved block shadow.
    fn nominal_path(&self, path: &syn::Path, scope: &ScopePath, imports: &HashMap<String, Option<Vec<String>>>) -> Option<Vec<String>> {
        let mut names = path.segments.iter().map(|segment| segment.ident.to_string()).collect::<Vec<_>>();
        if imports
            .get(super::types::BLOCK_TYPES)
            .and_then(Option::as_ref)
            .is_some_and(|shadows| names.first().is_some_and(|name| shadows.contains(name)))
        {
            return None;
        }
        if let Some(imported) = imports.get(names.first()?) {
            let _old = names.splice(..1, imported.as_ref()?.iter().cloned());
        }
        if names.first().is_some_and(|name| name == ABSOLUTE_ROOT) {
            let _absolute = names.remove(0);
        }
        let mut scope = scope.clone();
        while !self.modules.contains_key(&scope) {
            let _last = scope.pop()?;
        }
        let mut module = self.modules.get(&scope)?.clone();
        match names.first()?.as_str() {
            "crate" => {
                module.clear();
                let _root = names.remove(0);
            }
            "self" => {
                let _root = names.remove(0);
            }
            "super" => {
                while names.first().is_some_and(|name| name == "super") {
                    let _parent = module.pop()?;
                    let _root = names.remove(0);
                }
            }
            ABSOLUTE_ROOT => return None,
            _ if path.leading_colon.is_some() => return None,
            _ => {}
        }
        module.extend(names);
        Some(self.identities.resolve_path(&module).unwrap_or(module))
    }

    /// Resolves free or qualified calls, stopping at bindings and unavailable inner declarations.
    pub(super) fn signature(
        &self,
        callee: &Expr,
        scope: &ScopePath,
        imports: &HashMap<String, Option<Vec<String>>>,
        self_type: Option<&Type>,
        bound: impl Fn(&str) -> bool,
    ) -> Option<&Signature> {
        let Expr::Path(callee) = callee else { return None };
        if callee.qself.is_some() {
            return None;
        }
        let mut names = callee
            .path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>();
        let first = names.first()?.clone();
        if bound(&first) {
            return None;
        }
        if names.len() == 1 && callee.path.leading_colon.is_none() && !imports.contains_key(&first) {
            let mut scope = scope.clone();
            loop {
                if let Some(signature) = self.free.get(&(scope.clone(), first.clone())) {
                    return signature.as_ref();
                }
                if !self.blocks.contains(&scope) {
                    return None;
                }
                let _last = scope.pop();
            }
        }
        if first == "Self" {
            let Type::Path(ty) = self_type? else { return None };
            let _replaced = names.splice(..1, ty.path.segments.iter().map(|segment| segment.ident.to_string()));
        } else if let Some(imported) = imports.get(&first) {
            let _replaced = names.splice(
                ..1,
                imported.as_ref()?.iter().filter(|name| name.as_str() != ABSOLUTE_ROOT).cloned(),
            );
        }
        let mut module_scope = scope.clone();
        while !self.modules.contains_key(&module_scope) {
            let _last = module_scope.pop()?;
        }
        let module = self.modules.get(&module_scope)?;
        let mut prefix = module.clone();
        match names.first()?.as_str() {
            "crate" => {
                prefix.clear();
                let _root = names.remove(0);
            }
            "self" => {
                let _root = names.remove(0);
            }
            "super" => {
                while names.first().is_some_and(|name| name == "super") {
                    let _parent = prefix.pop()?;
                    let _root = names.remove(0);
                }
            }
            _ if callee.path.leading_colon.is_some() => prefix.clear(),
            _ => {}
        }
        prefix.extend(names);
        self.qualified.get(&prefix)?.as_ref()
    }

    pub(super) fn parameters(signature: &Signature, cfg: &CfgSet) -> Vec<Type> {
        signature
            .inputs
            .iter()
            .filter(|input| {
                !cfg.skip_gate(match input {
                    FnArg::Typed(typed) => &typed.attrs,
                    FnArg::Receiver(receiver) => &receiver.attrs,
                })
            })
            .map(|input| match input {
                FnArg::Typed(typed) => (*typed.ty).clone(),
                FnArg::Receiver(receiver) => Self::receiver_type(receiver),
            })
            .collect()
    }

    fn receiver_type(receiver: &syn::Receiver) -> Type {
        match &receiver.kind {
            syn::ReceiverKind::Value => syn::parse_quote!(Self),
            syn::ReceiverKind::Reference(_, lifetime, mutability) => syn::parse_quote!(&#lifetime #mutability Self),
            syn::ReceiverKind::Typed(_, ty) => (**ty).clone(),
            _ => syn::parse_quote!(_),
        }
    }
}

/// Collects declarations in their lexical environments and tracks the inventory's completeness.
///
/// Transforming attributes, item macros, external modules, and unclassified trait implementations
/// can hide operators. They invalidate negative evidence even when other indexed facts remain useful.
struct Walk<'a> {
    calls: Calls,
    scope: ScopePath,
    module: Vec<String>,
    owner: Option<Vec<String>>,
    owner_parameters: Vec<String>,
    structural_trusted: bool,
    cfg: &'a CfgSet,
    indexes: &'a super::indexes::Indexes,
}

impl Walk<'_> {
    /// Records supported operator implementations; unclassifiable shapes prevent absence proofs.
    fn operator(&mut self, node: &syn::ItemImpl) {
        let Some((path, _)) = &node.trait_ else { return };
        if !node.generics.params.is_empty() {
            self.calls.closed = false;
            return;
        }
        let mut trait_type: Type = syn::parse_quote!(#path);
        self.canonical_type(&mut trait_type);
        let Type::Path(trait_type) = trait_type else { return };
        let names = trait_type
            .path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>();
        let [root, module, name] = names.as_slice() else { return };
        if !matches!(root.as_str(), "std" | "core") || module != "ops" {
            return;
        }
        let operation = match name.as_str() {
            "Add" => "+",
            "Sub" => "-",
            "Mul" => "*",
            "Div" => "/",
            "Rem" => "%",
            "AddAssign" => "+=",
            "SubAssign" => "-=",
            "MulAssign" => "*=",
            "DivAssign" => "/=",
            "RemAssign" => "%=",
            _ => return,
        };
        let mut left = (*node.self_ty).clone();
        self.canonical_type(&mut left);
        let Type::Path(left_path) = &left else {
            self.calls.closed = false;
            return;
        };
        let imports = self.indexes.scope_imports.get(&self.scope).unwrap_or(&self.indexes.root_imports);
        let Some(owner) = self.calls.nominal_path(&left_path.path, &self.scope, imports) else {
            return;
        };
        let mut right = super::values::type_argument_values(&Type::Path(trait_type))
            .first()
            .map_or_else(|| left.clone(), |ty| (*ty).clone());
        if matches!(&right, Type::Path(path) if path.path.is_ident("Self")) {
            right = left.clone();
        }
        self.canonical_type(&mut right);
        let Some(mut output) = node
            .items
            .iter()
            .find_map(|item| match item {
                syn::ImplItem::Type(ty) if ty.ident == "Output" && !self.cfg.skip_gate(&ty.attrs) => Some(ty.ty.clone()),
                _ => None,
            })
            .or_else(|| operation.ends_with('=').then(|| syn::parse_quote!(())))
        else {
            self.calls.closed = false;
            return;
        };
        if matches!(&output, Type::Path(path) if path.path.is_ident("Self")) {
            output = left;
        }
        self.canonical_type(&mut output);
        self.calls.operators.push((owner, operation.to_owned(), right, output));
    }

    /// Freezes a signature in its declaration environment, marking duplicate names unavailable.
    fn record(&mut self, signature: &Signature, associated: bool) {
        let mut signature = signature.clone();
        let mut substitutions: HashMap<String, Type> = signature
            .generics
            .type_params()
            .map(|parameter| parameter.ident.to_string())
            .chain(self.owner_parameters.iter().cloned())
            .map(|name| (name, syn::parse_quote!(_)))
            .collect();
        if associated && let Some(owner) = &self.owner {
            let names = core::iter::once("crate".to_owned())
                .chain(owner.iter().cloned())
                .collect::<Vec<_>>();
            if let Ok(owner) = syn::parse_str::<Type>(&names.join("::")) {
                let _old = substitutions.insert("Self".into(), owner);
            }
        }
        for input in &mut signature.inputs {
            match input {
                FnArg::Typed(typed) => {
                    *typed.ty = super::types::substitute_type(&typed.ty, &substitutions);
                    self.canonical_type(&mut typed.ty);
                }
                FnArg::Receiver(receiver) => {
                    let mut ty = super::types::substitute_type(&Calls::receiver_type(receiver), &substitutions);
                    self.canonical_type(&mut ty);
                    receiver.kind = syn::ReceiverKind::Typed(syn::token::Colon::default(), Box::new(ty));
                }
            }
        }
        if let syn::ReturnType::Type(_, ty) = &mut signature.output {
            **ty = super::types::substitute_type(ty, &substitutions);
            self.canonical_type(ty);
        }
        let signature = &signature;
        let name = signature.ident.to_string();
        if !associated {
            let key = (self.scope.clone(), name.clone());
            let _entry = self
                .calls
                .free
                .entry(key)
                .and_modify(|entry| *entry = None)
                .or_insert_with(|| Some(signature.clone()));
        }

        if self.calls.blocks.contains(&self.scope) {
            return;
        }
        let mut qualified = if associated {
            self.owner.clone().unwrap_or_default()
        } else {
            self.module.clone()
        };
        qualified.push(name);
        let _entry = self
            .calls
            .qualified
            .entry(qualified)
            .and_modify(|entry| *entry = None)
            .or_insert_with(|| Some(signature.clone()));
    }

    /// Normalizes bounded alias/import evidence, replacing unresolved expansions with unknown types.
    fn canonical_type(&self, ty: &mut Type) {
        let mut budget = super::types::TYPE_EXPANSION_LIMIT;
        if !super::types::reserve_type(ty, &mut budget) {
            *ty = syn::parse_quote!(_);
            return;
        }
        self.canonical_type_at(ty, self.indexes.aliases.len().saturating_add(1), &mut budget);
    }

    fn canonical_type_at(&self, ty: &mut Type, remaining: usize, budget: &mut usize) {
        if remaining == 0 {
            *ty = syn::parse_quote!(_);
            return;
        }
        if let Type::Path(path) = ty
            && path.path.leading_colon.is_none()
            && let Some(first) = path.path.segments.first()
            && self
                .indexes
                .scope_imports
                .get(&self.scope)
                .unwrap_or(&self.indexes.root_imports)
                .get(&first.ident.to_string())
                .is_some_and(Option::is_none)
        {
            *ty = syn::parse_quote!(_);
            return;
        }
        if let Type::Path(path) = ty
            && path.qself.is_none()
            && path.path.leading_colon.is_none()
            && path.path.segments.len() == 1
            && let Some(first) = path.path.segments.first()
            && let Some(Some(alias)) = self
                .indexes
                .scope_aliases
                .get(&self.scope)
                .and_then(|aliases| aliases.get(&first.ident.to_string()))
        {
            let arguments = super::values::type_argument_values(ty);
            if arguments.len() != alias.parameters.len() {
                *ty = syn::parse_quote!(_);
                return;
            }
            let substitutions = alias.parameters.iter().cloned().zip(arguments.into_iter().cloned()).collect();
            *ty = super::types::substitute_type_with_budget(&alias.target, &substitutions, budget);
            self.canonical_type_at(ty, remaining - 1, budget);
            return;
        }
        match ty {
            Type::Path(path) if path.qself.is_none() => {
                for segment in &mut path.path.segments {
                    if let PathArguments::AngleBracketed(arguments) = &mut segment.arguments {
                        for argument in &mut arguments.args {
                            if let GenericArgument::Type(ty) = argument {
                                self.canonical_type_at(ty, remaining, budget);
                            }
                        }
                    }
                }
                let imports = self.indexes.scope_imports.get(&self.scope).unwrap_or(&self.indexes.root_imports);
                if path.path.leading_colon.is_none()
                    && let Some(first) = path.path.segments.first()
                    && let Some(Some(names)) = imports.get(&first.ident.to_string())
                    && !names.is_empty()
                {
                    let arguments = first.arguments.clone();
                    let mut qualified = names
                        .iter()
                        .filter(|name| name.as_str() != ABSOLUTE_ROOT)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("::");
                    if names.first().is_some_and(|name| name == ABSOLUTE_ROOT) {
                        qualified.insert_str(0, "::");
                    }
                    if let Ok(mut prefix) = syn::parse_str::<syn::Path>(&qualified) {
                        if let Some(last) = prefix.segments.last_mut() {
                            last.arguments = arguments;
                        }
                        prefix.segments.extend(path.path.segments.iter().skip(1).cloned());
                        path.path = prefix;
                    }
                } else if path.path.leading_colon.is_none()
                    && let Some(first) = path.path.segments.first()
                    && self
                        .indexes
                        .scope_aliases
                        .get(&self.scope)
                        .is_some_and(|aliases| aliases.contains_key(&first.ident.to_string()))
                {
                    let mut prefix = syn::parse_str::<syn::Path>(&format!("crate::{}", self.module.join("::")))
                        .unwrap_or_else(|_| syn::parse_quote!(crate));
                    prefix.segments.extend(path.path.segments.iter().cloned());
                    path.path = prefix;
                }
            }
            Type::Reference(reference) => self.canonical_type_at(&mut reference.elem, remaining, budget),
            Type::Tuple(tuple) => {
                for ty in &mut tuple.elems {
                    self.canonical_type_at(ty, remaining, budget);
                }
            }
            Type::Array(array) => self.canonical_type_at(&mut array.elem, remaining, budget),
            Type::Paren(paren) => self.canonical_type_at(&mut paren.elem, remaining, budget),
            Type::Group(group) => self.canonical_type_at(&mut group.elem, remaining, budget),
            _ => {}
        }
    }

    /// Indexes field types, retaining move evidence only for structurally trusted declarations.
    fn fields(&mut self, name: &syn::Ident, fields: &syn::Fields, generics: &syn::Generics, trusted: bool) {
        let mut owner = self.module.clone();
        owner.push(name.to_string());
        for (index, field) in fields.iter().enumerate() {
            if self.cfg.skip_gate(&field.attrs) {
                continue;
            }
            let mut ty = field.ty.clone();
            let substitutions = generics
                .type_params()
                .map(|parameter| (parameter.ident.to_string(), syn::parse_quote!(_)))
                .collect();
            ty = super::types::substitute_type(&ty, &substitutions);
            self.canonical_type(&mut ty);
            if trusted
                && field
                    .attrs
                    .iter()
                    .all(|attribute| !attribute.path().is_ident("cfg") && !attribute.path().is_ident("cfg_attr"))
                && super::super::default_identity::nominal_attributes(&field.attrs)
            {
                self.calls.noncopy_members.entry(owner.clone()).or_default().push(ty.clone());
            }
            let ty = Some(ty);
            let member = field.ident.as_ref().map_or_else(|| index.to_string(), ToString::to_string);
            let _entry = self
                .calls
                .fields
                .entry((owner.clone(), member))
                .and_modify(|entry| *entry = None)
                .or_insert(ty);
        }
    }
}

#[expect(clippy::renamed_function_params, reason = "visitor parameters use descriptive names")]
impl<'ast> Visit<'ast> for Walk<'_> {
    fn visit_item_struct(&mut self, node: &'ast syn::ItemStruct) {
        if self.calls.blocks.contains(&self.scope) {
            let _inserted = self
                .calls
                .block_types
                .entry(self.scope.clone())
                .or_default()
                .insert(node.ident.to_string());
        } else {
            let trusted = self.structural_trusted
                && super::super::default_identity::nominal_attributes(&node.attrs)
                && node
                    .attrs
                    .iter()
                    .all(|attribute| !attribute.path().is_ident("cfg") && !attribute.path().is_ident("cfg_attr"));
            self.fields(&node.ident, &node.fields, &node.generics, trusted);
            let mut path = self.module.clone();
            path.push(node.ident.to_string());
            let _inserted = self.calls.structures.insert(path);
        }
    }
    fn visit_item_enum(&mut self, node: &'ast syn::ItemEnum) {
        if self.calls.blocks.contains(&self.scope) {
            let _inserted = self
                .calls
                .block_types
                .entry(self.scope.clone())
                .or_default()
                .insert(node.ident.to_string());
        } else if super::super::default_identity::nominal_attributes(&node.attrs) {
            let mut path = self.module.clone();
            path.push(node.ident.to_string());
            let _inserted = self.calls.enums.insert(path);
        }
    }
    fn visit_item(&mut self, node: &'ast syn::Item) {
        if !self.cfg.skip_gate(item_attrs(node)) {
            self.calls.closed &= super::noop::LocalCalls::nontransforming_attributes(item_attrs(node));
            let outer = self.structural_trusted;
            self.structural_trusted &= super::super::default_identity::nominal_attributes(item_attrs(node));
            visit::visit_item(self, node);
            self.structural_trusted = outer;
        }
    }
    fn visit_impl_item(&mut self, node: &'ast syn::ImplItem) {
        if !self.cfg.skip_gate(impl_item_attrs(node)) {
            visit::visit_impl_item(self, node);
        }
    }
    fn visit_trait_item(&mut self, node: &'ast syn::TraitItem) {
        if !self.cfg.skip_gate(trait_item_attrs(node)) {
            visit::visit_trait_item(self, node);
        }
    }
    fn visit_stmt(&mut self, node: &'ast syn::Stmt) {
        if !self.cfg.skip_gate(stmt_attrs(node)) {
            visit::visit_stmt(self, node);
        }
    }
    fn visit_expr(&mut self, node: &'ast syn::Expr) {
        if !self.cfg.skip_gate(expr_attrs(node)) {
            visit::visit_expr(self, node);
        }
    }
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        self.calls.closed &= node.content.is_some();
        self.scope.push(module_id(&node.ident));
        self.module.push(node.ident.to_string());
        let _previous = self.calls.modules.insert(self.scope.clone(), self.module.clone());
        visit::visit_item_mod(self, node);
        let _name = self.module.pop();
        let _scope = self.scope.pop();
    }
    fn visit_block(&mut self, node: &'ast syn::Block) {
        self.scope.push(block_id(node));
        let _inserted = self.calls.blocks.insert(self.scope.clone());
        visit::visit_block(self, node);
        let _scope = self.scope.pop();
    }
    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        self.operator(node);
        let owner = match &*node.self_ty {
            Type::Path(path) if node.trait_.is_none() => self.calls.nominal_path(
                &path.path,
                &self.scope,
                self.indexes.scope_imports.get(&self.scope).unwrap_or(&self.indexes.root_imports),
            ),
            _ => None,
        };
        let outer = core::mem::replace(&mut self.owner, owner);
        let parameters = core::mem::replace(
            &mut self.owner_parameters,
            node.generics.type_params().map(|parameter| parameter.ident.to_string()).collect(),
        );
        visit::visit_item_impl(self, node);
        self.owner = outer;
        self.owner_parameters = parameters;
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        self.record(&node.sig, false);
        visit::visit_item_fn(self, node);
    }
    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        let _inserted = self.calls.named_methods.insert(node.sig.ident.to_string());
        if self.owner.is_some() {
            self.record(&node.sig, true);
        }
        visit::visit_impl_item_fn(self, node);
    }
    fn visit_item_const(&mut self, node: &'ast syn::ItemConst) {
        let _previous = self.calls.free.insert((self.scope.clone(), node.ident.to_string()), None);
        visit::visit_item_const(self, node);
    }
    fn visit_item_static(&mut self, node: &'ast syn::ItemStatic) {
        let _previous = self.calls.free.insert((self.scope.clone(), node.ident.to_string()), None);
        visit::visit_item_static(self, node);
    }
    fn visit_foreign_item_fn(&mut self, node: &'ast syn::ForeignItemFn) {
        let _previous = self.calls.free.insert((self.scope.clone(), node.sig.ident.to_string()), None);
        visit::visit_foreign_item_fn(self, node);
    }

    fn visit_item_macro(&mut self, _: &'ast syn::ItemMacro) {
        self.calls.closed = false;
    }
}
