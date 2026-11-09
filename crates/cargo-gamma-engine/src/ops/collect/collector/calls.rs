// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Source-visible call signatures, indexed by declaration scope and calling convention.

use syn::visit::{self, Visit};
use syn::{Expr, FnArg, GenericArgument, Member, PathArguments, Signature, Type};

use crate::cfg::CfgSet;
use crate::ops::collect::collector::indexes::{
    ABSOLUTE_ROOT, EXTERN_ROOT_PREFIX, EXTERNAL_ROOT, ROOT_EXTERN_PREFIX, ScopePath, block_id, module_id,
};
use crate::ops::collect::collector::predicates::{expr_attrs, stmt_attrs};
use crate::ops::collect::defaults::{Defaults, impl_item_attrs, item_attrs, trait_item_attrs};
use crate::{HashMap, HashSet};

type ScopedSignature = (Signature, ScopePath);
type ScopedField = (Type, ScopePath);

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
    free: HashMap<(ScopePath, String), Option<ScopedSignature>>,
    value_types: HashMap<(ScopePath, String), Type>,
    value_imports: HashMap<ScopePath, HashMap<String, Option<Vec<String>>>>,
    type_only: HashSet<(ScopePath, String)>,
    qualified: HashMap<Vec<String>, Option<ScopedSignature>>,
    modules: HashMap<ScopePath, Vec<String>>,
    crate_aliases: HashSet<String>,
    blocks: HashSet<ScopePath>,
    fields: HashMap<(Vec<String>, String), Option<ScopedField>>,
    noncopy_members: HashMap<Vec<String>, Vec<Type>>,
    named_methods: HashSet<String>,
    enums: HashSet<Vec<String>>,
    structures: HashSet<Vec<String>>,
    block_types: HashMap<ScopePath, HashSet<String>>,
    identities: crate::ops::collect::default_identity::Identities,
    operators: Vec<(Vec<String>, String, Type, Type)>,
    // Negative operator evidence is usable only while the local inventory is complete.
    closed: bool,
    open_methods: bool,
}

impl Calls {
    /// Finds a value item before an enclosing binding; an unavailable type still shadows outer names.
    pub(super) fn value_type(
        &self,
        name: &str,
        scope: &ScopePath,
        binding_scope: Option<&ScopePath>,
    ) -> Option<(Option<&Type>, &ScopePath)> {
        #[cfg(test)]
        let mut remaining = scope.len() + 1;
        self.value_type_with(name, scope, binding_scope, || {
            #[cfg(test)]
            {
                assert!(
                    remaining > 0,
                    "value lookup cannot revisit the root after searching its enclosing scopes"
                );
                remaining -= 1;
            }
        })
    }

    fn value_type_with(
        &self,
        name: &str,
        scope: &ScopePath,
        binding_scope: Option<&ScopePath>,
        mut on_scope: impl FnMut(),
    ) -> Option<(Option<&Type>, &ScopePath)> {
        let mut scope = scope.clone();
        loop {
            on_scope();
            if binding_scope == Some(&scope) {
                return
                // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
                None;
            }
            if let Some((key, _)) = self.free.get_key_value(&(scope.clone(), name.to_owned())) {
                return Some((self.value_types.get(key), &key.0));
            }
            if let Some(imported) = self.imported_value_type(name, &scope) {
                return Some(imported);
            }
            if !self.blocks.contains(&scope) {
                return
                // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
                None;
            }
            let _scope = scope.pop();
        }
    }

    fn imported_value_type(&self, name: &str, scope: &ScopePath) -> Option<(Option<&Type>, &ScopePath)> {
        let (import_scope, imports) = self.value_imports.get_key_value(scope)?;
        let import = imports.get(name).or_else(|| imports.get("*"))?;
        let path =
        // #[gamma::skip(try.propagate_to_unwrap, reason = "Both production value_type callers obtain names from Path::get_ident on parsed Rust expressions. Such identifiers, including raw identifiers, always parse as a single-segment Path.", tag = "equivalent")]
        syn::parse_str(name).ok()?;
        let Some(target) = self.source_path(&path, scope, imports).filter(|_| {
            imports.contains_key(name)
                && import.as_deref().is_none_or(|path| match path {
                    [absolute, root, ..] if absolute == ABSOLUTE_ROOT => {
                        matches!(root.as_str(), "crate" | "self" | "super") || self.crate_aliases.contains(root)
                    }
                    _ => true,
                })
        }) else {
            return Some((None, import_scope));
        };
        let Some((name, module)) = target.split_last() else {
            return imports.contains_key("*").then_some((None, import_scope));
        };
        let mut modules = self.modules.iter().filter(|(_, path)| path.as_slice() == module);
        let Some((origin, _)) = modules.next() else {
            return Some((None, import_scope));
        };
        if modules.next().is_some() {
            return Some((None, import_scope));
        }
        let key = (origin.clone(), name.clone());
        if let Some((key, _)) = self.free.get_key_value(&key) {
            return Some((self.value_types.get(key), &key.0));
        }
        if self.type_only.contains(&key) {
            return imports.contains_key("*").then_some((None, import_scope));
        }
        Some((None, import_scope))
    }

    pub(super) fn has_method_named(&self, name: &str) -> bool {
        self.named_methods.contains(name)
    }

    pub(super) fn method_absent(&self, ty: &Type, name: &str, scope: &ScopePath, imports: &HashMap<String, Option<Vec<String>>>) -> bool {
        if !self.closed || self.open_methods || self.named_methods.contains(name) {
            return
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "This return has inferred bool type and is already false, exactly bool::default().")]
            false;
        }
        let ty = match super::values::strip(ty) {
            Type::Reference(reference) => super::values::strip(&reference.elem),
            other => other,
        };
        let Type::Path(path) = ty else {
            return
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "This return has inferred bool type and is already false, exactly bool::default().")]
            false;
        };
        if path
            .path
            .get_ident()
            .is_some_and(|name| super::values::primitive_kind(&name.to_string()).is_some())
        {
            return
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "This return has inferred bool type and is already false, exactly bool::default().")]
            false;
        }
        self.nominal_path(&path.path, scope, imports)
            .is_some_and(|owner| self.structures.contains(&owner) || self.enums.contains(&owner))
    }

    /// Resolves an inherent method through its nominal owner, withholding ambiguous signatures.
    pub(super) fn method_signature(
        &self,
        ty: &Type,
        name: &str,
        scope: &ScopePath,
        imports: &HashMap<String, Option<Vec<String>>>,
    ) -> Option<&Signature> {
        self.scoped_method_signature(ty, name, scope, imports)
            .map(|(signature, _)| signature)
    }

    pub(super) fn scoped_method_signature(
        &self,
        ty: &Type,
        name: &str,
        scope: &ScopePath,
        imports: &HashMap<String, Option<Vec<String>>>,
    ) -> Option<(&Signature, &ScopePath)> {
        let ty = match super::values::strip(ty) {
            Type::Reference(reference) => &reference.elem,
            other => other,
        };
        let Type::Path(path) = ty else {
            return
        // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
        None;
        };
        if path.path.leading_colon.is_some() {
            return None;
        }
        let mut owner = self.nominal_path(&path.path, scope, imports)?;
        owner.push(name.to_owned());
        self.qualified.get(&owner)?.as_ref().map(|(signature, scope)| (signature, scope))
    }

    /// Supplies trusted source fields to the collector's bounded move-sensitivity classifier.
    pub(super) fn has_noncopy_field(
        &self,
        ty: &Type,
        scope: &ScopePath,
        imports: &HashMap<String, Option<Vec<String>>>,
        noncopy: impl Fn(&Type) -> bool,
    ) -> bool {
        let Type::Path(path) = super::values::strip(ty) else {
            return
        // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
        false;
        };
        let Some(owner) = self.nominal_path(&path.path, scope, imports) else {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        };
        self.noncopy_members.get(&owner).is_some_and(|fields| fields.iter().any(noncopy))
    }

    /// Builds declaration-scope evidence under the active cfg and existing lexical indexes.
    pub(super) fn of(file: &syn::File, cfg: &CfgSet, indexes: &super::indexes::Indexes) -> Self {
        let mut walk = Walk {
            calls: Self::default(),
            scope: Vec::new(),
            module: Vec::new(),
            owner:
            // #[gamma::skip(option.none_to_some, reason = "Calls::of's initial owner is used for associated recording only after visit_item_impl replaces it with that impl's nominal owner. Free function records pass associated=false and do not inspect owner. Replacing the initial None with Some(empty) cannot affect any recorded associated or free signature.", tag = "equivalent")]
            None,
            owner_parameters: Vec::new(),
            structural_trusted: super::super::default_identity::nominal_attributes(&file.attrs),
            cfg,
            indexes,
        };
        let _previous = walk.calls.modules.insert(Vec::new(), Vec::new());
        walk.calls.crate_aliases = indexes
            .root_imports
            .iter()
            .filter_map(|(name, path)| match path.as_deref() {
                Some([_, target]) if target == "crate" => name.strip_prefix(ROOT_EXTERN_PREFIX).map(str::to_owned),
                _ => None,
            })
            .collect();
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
        self.scoped_field(ty, member, scope, imports).map(|(ty, _)| ty.clone())
    }

    pub(super) fn scoped_field(
        &self,
        ty: &Type,
        member: &Member,
        scope: &ScopePath,
        imports: &HashMap<String, Option<Vec<String>>>,
    ) -> Option<&ScopedField> {
        let ty = match ty {
            Type::Reference(reference) => &reference.elem,
            other => other,
        };
        let Type::Path(path) = ty else {
            return
        // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
        None;
        };
        let owner = self.nominal_path(&path.path, scope, imports)?;
        let member = match member {
            Member::Named(name) => name.to_string(),
            Member::Unnamed(index) => index.index.to_string(),
        };
        self.fields.get(&(owner, member))?.as_ref()
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
                if
                // #[gamma::skip(cond.always_false, reason = "Calls::of unconditionally installs the root scope in modules before traversal. Searching ancestor scope prefixes therefore reaches a known module no later than the empty root; the subsequent checked get is present, and popping an already-empty unknown scope is unreachable. These fallback/unwrap mutations change no Calls::of-produced inventory. Campaign outcome was inconclusive: timed out: the test run exceeded its budget; not a confirmed semantic survivor.", tag = "equivalent")]
                enclosing.pop().is_none() {
                    // #[gamma::skip(loop.delete_break, reason = "Calls::of unconditionally installs the root scope in modules before traversal. Searching ancestor scope prefixes therefore reaches a known module no later than the empty root; the subsequent checked get is present, and popping an already-empty unknown scope is unreachable. These fallback/unwrap mutations change no Calls::of-produced inventory.", tag = "equivalent")]
                    // #[gamma::skip(loop.break_to_continue, reason = "Calls::of unconditionally installs the root scope in modules before traversal. Searching ancestor scope prefixes therefore reaches a known module no later than the empty root; the subsequent checked get is present, and popping an already-empty unknown scope is unreachable. These fallback/unwrap mutations change no Calls::of-produced inventory.", tag = "equivalent")]
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
        if left.path.leading_colon.is_some() {
            return OperatorResult::Unknown;
        }
        let Some(owner) = self.nominal_path(&left.path, scope, imports) else {
            return OperatorResult::Unknown;
        };
        if self.identities.resolve_path(&owner).is_none() {
            return OperatorResult::Unknown;
        }
        let right_identity = super::types::frozen_type_identity(right);
        if super::types::contains_unknown_type(&right_identity) {
            return OperatorResult::Unknown;
        }
        let mut found = None;
        for (implemented, operation, rhs, output) in &self.operators {
            if implemented == &owner && operation == operator {
                let rhs_identity = super::types::frozen_type_identity(rhs);
                if super::types::contains_unknown_type(&rhs_identity) {
                    return OperatorResult::Unknown;
                }
                let same_rhs = rhs_identity == right_identity
                    || match (rhs, right) {
                        (Type::Path(left), Type::Path(right)) => {
                            left.path.leading_colon.is_none()
                                && right.path.leading_colon.is_none()
                                && left.path.segments.last().map(|segment| &segment.arguments)
                                    == right.path.segments.last().map(|segment| &segment.arguments)
                                && self.same_type(&left.path, &right.path, scope, imports)
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
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        }
        let Type::Path(path) = ty else {
            return
        // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
        false;
        };
        let Some(owner) = self.nominal_path(&path.path, scope, imports) else {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
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
        let module = self.source_path(path, scope, imports)?;
        Some(self.identities.resolve_path(&module).unwrap_or(module))
    }

    fn source_path(&self, path: &syn::Path, scope: &ScopePath, imports: &HashMap<String, Option<Vec<String>>>) -> Option<Vec<String>> {
        let mut names = path.segments.iter().map(|segment| segment.ident.to_string()).collect::<Vec<_>>();
        let root =
            // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "The input is a source-parsed or canonicalized Path, both of which have nonempty segments. This first access precedes import expansion, so names cannot be empty.")]
            names.first()?;
        if path.leading_colon.is_some() {
            if !matches!(root.as_str(), "crate" | "self" | "super") {
                if !self.crate_aliases.contains(root) {
                    return
                    // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
                    None;
                }
                "crate".clone_into(&mut names[0]);
            }
        } else {
            if imports
                .get(super::types::BLOCK_TYPES)
                .and_then(Option::as_ref)
                .is_some_and(|shadows| shadows.contains(root))
            {
                return
                // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
                None;
            }
            if let Some(imported) = imports.get(root) {
                let _old = names.splice(..1, imported.as_ref()?.iter().cloned());
            }
        }
        if names.first().is_some_and(|name| name == ABSOLUTE_ROOT) {
            let _absolute = names.remove(0);
            if names.first().is_some_and(|name| name == EXTERNAL_ROOT) {
                return None;
            }
            if let Some(root) = names.first_mut()
                && self.crate_aliases.contains(root)
            {
                "crate".clone_into(root);
            }
        }
        let mut scope = scope.clone();
        while !self.modules.contains_key(&scope) {
            let _last =
            // #[gamma::skip(try.propagate_to_unwrap, reason = "Calls::of unconditionally installs the root scope in modules before traversal. Searching ancestor scope prefixes therefore reaches a known module no later than the empty root; the subsequent checked get is present, and popping an already-empty unknown scope is unreachable. These fallback/unwrap mutations change no Calls::of-produced inventory.", tag = "equivalent")]
            scope.pop()?;
        }
        let mut module =
        // #[gamma::skip(try.propagate_to_unwrap, reason = "Calls::of unconditionally installs the root scope in modules before traversal. Searching ancestor scope prefixes therefore reaches a known module no later than the empty root; the subsequent checked get is present, and popping an already-empty unknown scope is unreachable. These fallback/unwrap mutations change no Calls::of-produced inventory.", tag = "equivalent")]
        self.modules.get(&scope)?.clone();
        match
        // #[gamma::skip(try.propagate_to_unwrap, reason = "The queried parsed path is nonempty, and every published import map comes through Indexes::canonicalize_imports/canonical_import. Successful empty targets become None at canonical_import's path.first()?; external absolute targets retain a nonempty imported name after their root marker. No published import can reach this access with an empty target.", tag = "equivalent")]
        names.first()?.as_str() {
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
            _ => {}
        }
        module.extend(names);
        Some(module)
    }

    /// Resolves free or qualified calls, stopping at bindings and unavailable inner declarations.
    #[cfg(test)]
    pub(super) fn signature(
        &self,
        callee: &Expr,
        scope: &ScopePath,
        imports: &HashMap<String, Option<Vec<String>>>,
        self_type: Option<&Type>,
        bound: impl Fn(&str) -> bool,
    ) -> Option<&Signature> {
        self.scoped_signature(callee, scope, imports, self_type, bound)
            .map(|(signature, _)| signature)
    }

    pub(super) fn scoped_signature(
        &self,
        callee: &Expr,
        scope: &ScopePath,
        imports: &HashMap<String, Option<Vec<String>>>,
        self_type: Option<&Type>,
        bound: impl Fn(&str) -> bool,
    ) -> Option<(&Signature, &ScopePath)> {
        #[cfg(test)]
        let mut remaining = scope.len() + 1;
        self.signature_with(callee, scope, imports, self_type, bound, |_| {
            #[cfg(test)]
            {
                assert!(
                    remaining > 0,
                    "free call lookup cannot revisit the root after searching its enclosing scopes"
                );
                remaining -= 1;
            }
        })
        .map(|(signature, scope)| (signature, scope))
    }

    /// Observes every free-call lexical-scope probe before lookup, so tests can detect repeated
    /// root probes without substituting the resolver. Qualified and method resolution are unobserved.
    fn signature_with(
        &self,
        callee: &Expr,
        scope: &ScopePath,
        imports: &HashMap<String, Option<Vec<String>>>,
        self_type: Option<&Type>,
        bound: impl Fn(&str) -> bool,
        mut on_scope: impl FnMut(&ScopePath),
    ) -> Option<&ScopedSignature> {
        let Expr::Path(callee) = callee else {
            return
        // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
        None;
        };
        if callee.qself.is_some() {
            return
            // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
            None;
        }
        let mut names = callee
            .path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>();
        let first =
        // #[gamma::skip(try.propagate_to_unwrap, reason = "These names are copied from the nonempty source-parsed callee path before import expansion.", tag = "equivalent")]
        names.first()?.clone();
        if bound(&first) {
            return
            // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
            None;
        }
        if names.len() == 1 && callee.path.leading_colon.is_none() && !imports.contains_key(&first) {
            let mut scope = scope.clone();
            loop {
                on_scope(&scope);
                if let Some(signature) = self.free.get(&(scope.clone(), first.clone())) {
                    return signature.as_ref();
                }
                if !self.blocks.contains(&scope) {
                    return
                    // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
                    None;
                }
                let _last = scope.pop();
            }
        }
        if first == "Self" {
            let Type::Path(ty) = self_type? else {
                return
            // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
            None;
            };
            let _replaced = names.splice(..1, ty.path.segments.iter().map(|segment| segment.ident.to_string()));
        } else if let Some(imported) = imports.get(&first) {
            let _replaced = names.splice(
                ..1,
                imported.as_ref()?.iter().filter(|name| name.as_str() != ABSOLUTE_ROOT).cloned(),
            );
        }
        let mut module_scope = scope.clone();
        while !self.modules.contains_key(&module_scope) {
            let _last =
            // #[gamma::skip(try.propagate_to_unwrap, reason = "Calls::of installs the root module unconditionally, so ancestor search stops before popping an empty scope.", tag = "equivalent")]
            module_scope.pop()?;
        }
        let module =
        // #[gamma::skip(try.propagate_to_unwrap, reason = "The preceding loop stops only after contains_key succeeds on this unchanged modules map.", tag = "equivalent")]
        self.modules.get(&module_scope)?;
        let mut prefix = module.clone();
        match
        // #[gamma::skip(try.propagate_to_unwrap, reason = "Canonical imports reject empty successful targets; parsed paths and Self owner paths are nonempty.", tag = "equivalent")]
        names.first()?.as_str() {
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
        right = self.freeze_canonical_type(&right);
        output = self.freeze_canonical_type(&output);
        self.calls.operators.push((owner, operation.to_owned(), right, output));
    }

    fn freeze_canonical_type(&self, ty: &Type) -> Type {
        // Local names and aliases are already canonical; freeze intrinsic identities before leaving this scope.
        let defaults = Defaults::default();
        let types = super::types::Types {
            abstracts: &[],
            defaulted: &[],
            imports: self.indexes.scope_imports.get(&self.scope).unwrap_or(&self.indexes.root_imports),
            defaults: &defaults,
            aliases: None,
            self_type: None,
            self_associated: None,
        };
        types.frozen_canonical_type(ty)
    }

    fn record_value(&mut self, ident: &syn::Ident, ty: &Type) {
        let key = (self.scope.clone(), ident.to_string());
        if self.calls.free.insert(key.clone(), None).is_none() {
            let mut ty = ty.clone();
            self.canonical_type(&mut ty);
            let ty = self.freeze_canonical_type(&ty);
            let _previous = self.calls.value_types.insert(key, ty);
        } else {
            let _previous = self.calls.value_types.remove(&key);
        }
    }

    fn record_imports(&mut self, tree: &syn::UseTree) {
        let name = match tree {
            syn::UseTree::Path(path) => return self.record_imports(&path.tree),
            syn::UseTree::Group(group) => {
                for item in &group.items {
                    self.record_imports(item);
                }
                return;
            }
            syn::UseTree::Name(name) => (name.ident != "self").then(|| name.ident.to_string()),
            syn::UseTree::Rename(rename) => (rename.ident != "self").then(|| rename.rename.to_string()),
            syn::UseTree::Glob(_) => Some("*".to_owned()),
        };
        if let Some(name) = name {
            let imports = self.indexes.scope_imports.get(&self.scope).unwrap_or(&self.indexes.root_imports);
            let _previous = self
                .calls
                .value_imports
                .entry(self.scope.clone())
                .or_default()
                .insert(name.clone(), imports.get(&name).cloned().flatten());
        }
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
            let _previous = self.calls.value_types.remove(&key);
            let _entry = self
                .calls
                .free
                .entry(key)
                .and_modify(|entry| *entry = None)
                .or_insert_with(|| Some((signature.clone(), self.scope.clone())));
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
            .or_insert_with(|| Some((signature.clone(), self.scope.clone())));
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
        self.canonical_type_with_root(ty, remaining, budget, true);
    }

    fn canonical_type_with_root(&self, ty: &mut Type, remaining: usize, budget: &mut usize, source: bool) {
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
            && let Some(first) =
                // #[gamma::skip(iter.first_to_last, reason = "This path.segments.first() is evaluated under path.segments.len()==1. The single segment is simultaneously first and last, including its generic arguments.", tag = "equivalent")]
                path.path.segments.first()
            && let Some(Some(alias)) = self
                .indexes
                .scope_aliases
                .get(&self.scope)
                .and_then(|aliases| aliases.get(&first.ident.to_string()))
        {
            self.canonical_type_arguments(&mut path.path, remaining - 1, budget, source);
            let arguments = super::values::type_argument_values(ty);
            if arguments.len() != alias.parameters.len() {
                *ty = syn::parse_quote!(_);
                return;
            }
            let substitutions = alias.parameters.iter().cloned().zip(arguments.into_iter().cloned()).collect();
            *ty = super::types::substitute_type_with_budget(&alias.target, &substitutions, budget);
            self.canonical_type_with_root(ty, remaining - 1, budget, false);
            return;
        }
        match ty {
            Type::Path(path) if path.qself.is_none() => {
                self.canonical_type_arguments(&mut path.path, remaining, budget, source);
                if source {
                    self.canonical_local_root(&mut path.path);
                }
                let imports = self.indexes.scope_imports.get(&self.scope).unwrap_or(&self.indexes.root_imports);
                if path.path.leading_colon.is_none()
                    && let Some(first) = path.path.segments.first()
                    && let Some(Some(names)) = imports.get(&first.ident.to_string())
                    && !names.is_empty()
                {
                    self.qualify_imported_type_path(names, &mut path.path);
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
                if path.path.segments.first().is_some_and(|first| first.ident == "crate") {
                    path.path.leading_colon = None;
                }
            }
            Type::Reference(reference) => self.canonical_type_with_root(&mut reference.elem, remaining, budget, source),
            Type::Tuple(tuple) => {
                for ty in &mut tuple.elems {
                    self.canonical_type_with_root(ty, remaining, budget, source);
                }
            }
            Type::Array(array) => self.canonical_type_with_root(&mut array.elem, remaining, budget, source),
            Type::Slice(slice) => self.canonical_type_with_root(&mut slice.elem, remaining, budget, source),
            Type::Ptr(pointer) => self.canonical_type_with_root(&mut pointer.elem, remaining, budget, source),
            Type::Paren(paren) => self.canonical_type_with_root(&mut paren.elem, remaining, budget, source),
            Type::Group(group) => self.canonical_type_with_root(&mut group.elem, remaining, budget, source),
            _ => {}
        }
    }

    fn qualify_imported_type_path(&self, names: &[String], original: &mut syn::Path) {
        let mut qualified = super::indexes::import_target(names).join("::");
        if names.first().is_some_and(|name| name == ABSOLUTE_ROOT) {
            qualified.insert_str(0, "::");
        }
        if let Ok(mut prefix) = syn::parse_str::<syn::Path>(&qualified) {
            if let (Some(last), Some(first)) = (prefix.segments.last_mut(), original.segments.first()) {
                last.arguments = first.arguments.clone();
            }
            prefix.segments.extend(original.segments.iter().skip(1).cloned());
            if !names.iter().any(|name| name == EXTERNAL_ROOT) {
                self.canonical_local_root(&mut prefix);
            }
            *original = prefix;
        }
    }

    fn canonical_type_arguments(&self, path: &mut syn::Path, remaining: usize, budget: &mut usize, source: bool) {
        for segment in &mut path.segments {
            if let PathArguments::AngleBracketed(arguments) = &mut segment.arguments {
                for argument in &mut arguments.args {
                    if let GenericArgument::Type(ty) = argument {
                        self.canonical_type_with_root(ty, remaining, budget, source);
                    }
                }
            }
        }
    }

    fn canonical_local_root(&self, path: &mut syn::Path) {
        let Some(first) = path.segments.first_mut() else { return };
        let root = first.ident.to_string();
        let imports = self.indexes.scope_imports.get(&self.scope).unwrap_or(&self.indexes.root_imports);
        let local = if path.leading_colon.is_some() {
            self.calls.crate_aliases.contains(&root)
        } else {
            !imports.contains_key(&root)
                && matches!(imports.get(&format!("{EXTERN_ROOT_PREFIX}{root}")).and_then(Option::as_deref),
                    Some([_, target]) if target == "crate")
        };
        if local {
            first.ident = syn::Ident::new("crate", first.ident.span());
            path.leading_colon = None;
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
                && field.attrs.iter().all(|attribute|
                    // #[gamma::skip(logical.and_remove_right, reason = "The same field/structure trust conjunction independently calls default_identity::nominal_attributes, whose allowlist rejects cfg_attr. Removing/broadening the duplicate cfg_attr check cannot admit it. The previous attributed-field oracle still fails the independent nominal-attribute gate.", tag = "equivalent")]
                    !attribute.path().is_ident("cfg") && !attribute.path().is_ident(
                    // #[gamma::skip(literal.str_to_empty, literal.str_to_xyzzy, reason = "The same field/structure trust conjunction independently calls default_identity::nominal_attributes, whose allowlist rejects cfg_attr. Removing/broadening the duplicate cfg_attr check cannot admit it. The previous attributed-field oracle still fails the independent nominal-attribute gate.", tag = "equivalent")]
                    "cfg_attr"))
                && super::super::default_identity::nominal_attributes(&field.attrs)
            {
                self.calls.noncopy_members.entry(owner.clone()).or_default().push(ty.clone());
            }
            let ty = Some((self.freeze_canonical_type(&ty), self.scope.clone()));
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
        if !matches!(node.fields, syn::Fields::Named(_)) {
            let key = (self.scope.clone(), node.ident.to_string());
            let _previous = self.calls.value_types.remove(&key);
            let _previous = self.calls.free.insert(key, None);
        }
        if self.calls.blocks.contains(&self.scope) {
            let _inserted = self
                .calls
                .block_types
                .entry(self.scope.clone())
                .or_default()
                .insert(node.ident.to_string());
        } else {
            let trusted = (
                // #[gamma::skip(logical.and_remove_right, reason = "Walk::visit_item intersects structural_trusted with nominal_attributes(item.attrs) before dispatching to visit_item_struct. Repeating that same nominal-attribute conjunct in the struct handler cannot add any rejection not already carried in structural_trusted.", tag = "equivalent")]
                self.structural_trusted && super::super::default_identity::nominal_attributes(&node.attrs)
            ) && node.attrs.iter().all(|attribute|
                    // #[gamma::skip(logical.and_remove_right, reason = "The same field/structure trust conjunction independently calls default_identity::nominal_attributes, whose allowlist rejects cfg_attr. Removing/broadening the duplicate cfg_attr check cannot admit it. The previous attributed-field oracle still fails the independent nominal-attribute gate.", tag = "equivalent")]
                    !attribute.path().is_ident("cfg") && !attribute.path().is_ident(
                    // #[gamma::skip(literal.str_to_empty, literal.str_to_xyzzy, reason = "The same field/structure trust conjunction independently calls default_identity::nominal_attributes, whose allowlist rejects cfg_attr. Removing/broadening the duplicate cfg_attr check cannot admit it. The previous attributed-field oracle still fails the independent nominal-attribute gate.", tag = "equivalent")]
                    "cfg_attr"));
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
            let type_only = match node {
                syn::Item::Type(item) => Some(&item.ident),
                syn::Item::Trait(item) => Some(&item.ident),
                syn::Item::Enum(item) => Some(&item.ident),
                syn::Item::Mod(item) => Some(&item.ident),
                syn::Item::Union(item) => Some(&item.ident),
                syn::Item::Struct(item) => Some(&item.ident),
                _ => None,
            };
            if let Some(name) = type_only {
                let _inserted = self.calls.type_only.insert((self.scope.clone(), name.to_string()));
            }
            // #[gamma::skip(assign.or_to_and, stmt.delete_assign, tag = "equivalent", reason = "Only derive/cfg_attr can set open_methods here. Both are rejected by the immediately following nontransforming_attributes check, permanently clearing closed. method_absent is the only reader of open_methods and already rejects every !closed inventory. The separate trait-impl assignment is not covered.")]
            self.calls.open_methods |= item_attrs(node).iter().any(|attribute|
                    // #[gamma::skip(logical.or_remove_left, logical.or_remove_right, logical.or_to_and, tag = "equivalent", reason = "Either recognized attribute independently clears closed in the following nontransforming_attributes check. Narrowing only this open_methods predicate cannot change method_absent, which rejects !closed first.")]
                    attribute.path().is_ident(
                        // #[gamma::skip(literal.str_to_empty, literal.str_to_xyzzy, tag = "equivalent", reason = "Removing this derive recognition only narrows open_methods; the original derive attribute still permanently clears closed in the following nontransforming_attributes check, so method_absent rejects the inventory regardless.")]
                        "derive")
                        || attribute.path().is_ident(
                            // #[gamma::skip(literal.str_to_empty, literal.str_to_xyzzy, tag = "equivalent", reason = "Removing this cfg_attr recognition only narrows open_methods; the original cfg_attr attribute still permanently clears closed in the following nontransforming_attributes check, so method_absent rejects the inventory regardless.")]
                            "cfg_attr"));
            self.calls.closed &= super::noop::LocalCalls::nontransforming_attributes(item_attrs(node));
            let outer = self.structural_trusted;
            self.structural_trusted &= super::super::default_identity::nominal_attributes(item_attrs(node));
            visit::visit_item(self, node);
            self.structural_trusted = outer;
        }
    }
    fn visit_impl_item(&mut self, node: &'ast syn::ImplItem) {
        if !self.cfg.skip_gate(impl_item_attrs(node)) {
            self.calls.open_methods |= matches!(node, syn::ImplItem::Macro(_));
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
        self.calls.open_methods |= node.trait_.is_some();
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
    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        let _inserted = self.calls.named_methods.insert(node.sig.ident.to_string());
        visit::visit_trait_item_fn(self, node);
    }
    fn visit_item_const(&mut self, node: &'ast syn::ItemConst) {
        self.record_value(&node.ident, &node.ty);
        visit::visit_item_const(self, node);
    }
    fn visit_item_use(&mut self, node: &'ast syn::ItemUse) {
        self.record_imports(&node.tree);
    }
    fn visit_item_static(&mut self, node: &'ast syn::ItemStatic) {
        self.record_value(&node.ident, &node.ty);
        visit::visit_item_static(self, node);
    }
    fn visit_foreign_item_fn(&mut self, node: &'ast syn::ForeignItemFn) {
        let key = (self.scope.clone(), node.sig.ident.to_string());
        let _previous = self.calls.value_types.remove(&key);
        let _previous = self.calls.free.insert(key, None);
        visit::visit_foreign_item_fn(self, node);
    }

    fn visit_item_macro(&mut self, _: &'ast syn::ItemMacro) {
        self.calls.closed = false;
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use syn::{Item, Path, TypeGroup, parse_file, parse_quote, parse_str};

    use super::super::indexes::indexes_in;
    use super::*;
    use crate::ops::registry::Selection;

    #[test]
    fn value_lookup_stops_at_the_enclosing_module() {
        let file = parse_file("const VALUE: () = (); mod child {}").unwrap();
        let cfg = CfgSet::unconditional();
        let indexes = indexes_in(&file, &Selection::everything(), &cfg);
        let calls = Calls::of(&file, &cfg, &indexes);
        let Item::Mod(child) = &file.items[1] else {
            panic!("fixture declares child as its second item");
        };
        let mut visits = 0;
        assert_eq!(
            calls.value_type_with("ABSENT", &Vec::new(), None, || {
                visits += 1;
                assert_eq!(visits, 1, "a missing root value cannot revisit its module scope");
            }),
            None
        );
        assert_eq!(visits, 1);
        assert_eq!(calls.value_type("VALUE", &vec![module_id(&child.ident)], None), None);
    }

    #[test]
    fn ambiguous_and_reexported_value_imports_withhold_outer_evidence() {
        for modules in [
            "#[cfg(feature = \"first\")] mod values { pub const VALUE: bool = true; }
             #[cfg(not(feature = \"first\"))] mod values { pub const VALUE: bool = false; }",
            "mod actual { pub const VALUE: bool = true; }
             mod values { pub use crate::actual::VALUE; }",
        ] {
            let file = parse_file(&format!(
                "const VALUE: bool = true; {modules} fn f() {{ use crate::values::VALUE; }}"
            ))
            .unwrap();
            let cfg = CfgSet::unconditional();
            let indexes = indexes_in(&file, &Selection::everything(), &cfg);
            let calls = Calls::of(&file, &cfg, &indexes);
            let Item::Fn(function) = file.items.last().unwrap() else {
                panic!("fixture ends with a function containing the import");
            };
            let scope = vec![block_id(&function.block)];
            assert_eq!(calls.value_type("VALUE", &scope, None), Some((None, &scope)), "{modules}");
        }
    }

    #[test]
    fn type_only_declarations_and_imports_preserve_outer_values_but_constructors_shadow_them() {
        for (declaration, type_only) in [
            ("pub type VALUE = ();", true),
            ("pub trait VALUE {}", true),
            ("pub enum VALUE { One }", true),
            ("pub mod VALUE {}", true),
            ("pub union VALUE { pub field: u8 }", true),
            ("pub struct VALUE { pub field: u8 }", true),
            ("pub struct VALUE;", false),
            ("pub struct VALUE(pub u8);", false),
        ] {
            for (module, body) in [
                (format!("mod values {{ {declaration} }}"), "use crate::values::VALUE;".to_owned()),
                (String::new(), declaration.to_owned()),
            ] {
                let file = parse_file(&format!("const VALUE: bool = true; {module} fn f() {{ {body} }}")).unwrap();
                let cfg = CfgSet::unconditional();
                let indexes = indexes_in(&file, &Selection::everything(), &cfg);
                let calls = Calls::of(&file, &cfg, &indexes);
                let Item::Fn(function) = file.items.last().unwrap() else {
                    panic!("fixture ends with a function containing the declaration or import");
                };
                let expected = type_only.then(|| parse_quote!(::core::primitive::bool));
                let found = calls.value_type("VALUE", &vec![block_id(&function.block)], None);
                assert!(found.is_some_and(|(ty, _)| ty == expected.as_ref()), "{declaration}: {found:?}");
            }
        }
    }

    #[test]
    fn self_associated_calls_require_a_path_owner() {
        let file = parse_file("struct Owner; impl Owner { fn helper() -> u8 { 1 } }").unwrap();
        let cfg = CfgSet::unconditional();
        let indexes = indexes_in(&file, &Selection::everything(), &cfg);
        let calls = Calls::of(&file, &cfg, &indexes);
        let callee = parse_quote!(Self::helper);
        let owner = parse_quote!(Owner);
        assert!(
            calls
                .signature(&callee, &Vec::new(), &indexes.root_imports, Some(&owner), |_| false)
                .is_some()
        );
        for owner in [parse_quote!((Owner,)), parse_quote!(&Owner), parse_quote!([Owner; 1])] {
            assert!(
                calls
                    .signature(&callee, &Vec::new(), &indexes.root_imports, Some(&owner), |_| false)
                    .is_none()
            );
        }
    }

    #[test]
    fn missing_free_call_stops_at_the_root_scope_without_repeating_it() {
        let file = parse_file("").unwrap();
        let cfg = CfgSet::unconditional();
        let indexes = indexes_in(&file, &Selection::everything(), &cfg);
        let calls = Calls::of(&file, &cfg, &indexes);
        let root = Vec::new();
        let mut visits = 0;
        assert!(
            calls
                .signature_with(
                    &parse_quote!(missing),
                    &root,
                    &indexes.root_imports,
                    None,
                    |_| false,
                    |scope| {
                        assert_eq!(scope, &root);
                        visits += 1;
                        assert_eq!(visits, 1, "a module scope cannot fall through to an enclosing block");
                    }
                )
                .is_none()
        );
        assert_eq!(visits, 1);
    }

    #[test]
    fn source_operator_rhs_aliases_are_canonicalized_and_only_active_output_types_are_selected() {
        let file = parse_file(
            "struct Owner;
             type Pair = (u8, bool);
             impl core::ops::Add<Pair, char> for Owner {
                 type Other = String;
                 #[cfg(any())] type Output = bool;
                 type Output = Owner;
             }",
        )
        .unwrap();
        let cfg = CfgSet::parse("");
        let indexes = indexes_in(&file, &Selection::everything(), &cfg);
        let calls = Calls::of(&file, &cfg, &indexes);
        assert_eq!(calls.operators.len(), 1);
        assert_eq!(calls.operators[0].2, parse_quote!((::core::primitive::u8, ::core::primitive::bool)));
        assert_eq!(calls.operators[0].3, parse_quote!(crate::Owner));
    }

    #[test]
    fn signature_traversal_reaches_nested_functions_in_constant_static_and_foreign_types() {
        let file = parse_file(
            "const VALUE: u8 = { fn from_const() -> bool { true } 0 };
             static STORED: u8 = { fn from_static() -> bool { true } 0 };
             unsafe extern \"C\" {
                 fn external(value: [u8; { fn from_foreign() -> bool { true } 1 }]);
             }",
        )
        .unwrap();
        let cfg = CfgSet::parse("");
        let indexes = indexes_in(&file, &Selection::everything(), &cfg);
        let calls = Calls::of(&file, &cfg, &indexes);
        for name in ["from_const", "from_static", "from_foreign"] {
            let (signature, _) = calls
                .free
                .values()
                .flatten()
                .find(|(signature, _)| signature.ident == name)
                .unwrap();
            assert_eq!(signature.output, parse_quote!(-> bool), "{name}");
        }
    }

    #[test]
    fn typed_receivers_resolve_aliases_while_recursive_aliases_stay_unknown() {
        let file = parse_file(
            "type Recursive = Vec<Recursive>;
             fn recursive(value: Recursive) {}
             struct Owner;
             type Receiver = Box<Owner>;
             impl Owner { fn boxed(self: Receiver) {} }",
        )
        .unwrap();
        let cfg = CfgSet::parse("");
        let indexes = indexes_in(&file, &Selection::everything(), &cfg);
        let calls = Calls::of(&file, &cfg, &indexes);
        let signature = calls
            .signature(&parse_quote!(recursive), &Vec::new(), &indexes.root_imports, None, |_| false)
            .unwrap();
        assert_eq!(Calls::parameters(signature, &cfg), [parse_quote!(_)]);
        let signature = calls
            .method_signature(&parse_quote!(Owner), "boxed", &Vec::new(), &indexes.root_imports)
            .unwrap();
        assert_eq!(Calls::parameters(signature, &cfg), [parse_quote!(Box<self::Owner>)]);
    }

    #[test]
    fn repeated_instantiations_of_one_alias_obey_the_exact_expansion_allowance() {
        let file = parse_file("type Identity<T> = T; fn nested(value: Identity<Identity<bool>>) {}").unwrap();
        let cfg = CfgSet::parse("");
        let indexes = indexes_in(&file, &Selection::everything(), &cfg);
        let calls = Calls::of(&file, &cfg, &indexes);
        let signature = calls
            .signature(&parse_quote!(nested), &Vec::new(), &indexes.root_imports, None, |_| false)
            .unwrap();
        assert_eq!(Calls::parameters(signature, &cfg), [parse_quote!(_)]);
    }

    #[test]
    fn nominal_paths_resolve_import_heads_self_and_absolute_roots_without_guessing_missing_parents() {
        let cfg = CfgSet::unconditional();
        let file =
            parse_file("extern crate self as this; fn method() -> bool { true } struct Root; impl Root { fn method() -> bool { true } } mod child { struct Local; }")
                .unwrap();
        let indexes = indexes_in(&file, &Selection::everything(), &cfg);
        let calls = Calls::of(&file, &cfg, &indexes);
        let empty = HashMap::default();
        let imports = HashMap::from_iter([
            ("Renamed".into(), Some(vec!["crate".into(), "Root".into()])),
            (
                "Outside".into(),
                Some(vec![ABSOLUTE_ROOT.into(), "dependency".into(), "Record".into()]),
            ),
            ("Blocked".into(), None),
        ]);
        for (ty, expected) in [
            ("Renamed", Some(vec!["Root".to_owned()])),
            ("::Renamed", None),
            ("Renamed::Member", Some(vec!["Root".to_owned(), "Member".to_owned()])),
            ("Outside", Some(vec!["dependency".to_owned(), "Record".to_owned()])),
            ("::Outside", None),
            ("Blocked", None),
            ("self::Root", Some(vec!["Root".to_owned()])),
            ("::self::Root", Some(vec!["Root".to_owned()])),
            ("::crate::Root", Some(vec!["Root".to_owned()])),
            ("::this::Root", Some(vec!["Root".to_owned()])),
            ("super::Root", None),
            ("::super::Root", None),
            ("::dependency::Record", None),
        ] {
            let path: Path = parse_str(ty).unwrap();
            assert_eq!(calls.nominal_path(&path, &Vec::new(), &imports), expected);
        }
        assert!(
            calls
                .signature(&parse_quote!(<Root as Contract>::method), &Vec::new(), &empty, None, |_| false)
                .is_none()
        );
        assert!(
            calls
                .signature(&parse_quote!(<Root>::method), &Vec::new(), &empty, None, |_| false)
                .is_none()
        );
        let Item::Mod(child) = file.items.last().unwrap() else {
            unreachable!()
        };
        let child_scope = vec![super::super::indexes::module_id(&child.ident)];
        assert_eq!(
            calls.nominal_path(&parse_quote!(self::Local), &child_scope, &empty),
            Some(vec!["child".into(), "Local".into()])
        );
        assert_eq!(
            calls.nominal_path(&parse_quote!(::self::Local), &child_scope, &empty),
            Some(vec!["child".into(), "Local".into()])
        );
        assert_eq!(
            calls.nominal_path(&parse_quote!(::super::Root), &child_scope, &empty),
            Some(vec!["Root".into()])
        );
        let blocked_scope = (1, 1);
        let shadows = HashMap::from_iter([(super::super::types::BLOCK_TYPES.into(), Some(vec!["Root".into()]))]);
        assert!(calls.nominal_path(&parse_quote!(Root), &vec![blocked_scope], &shadows).is_none());
    }

    #[test]
    fn canonical_alias_depth_is_exact_and_containers_do_not_receive_extra_expansion_steps() {
        let cfg = CfgSet::unconditional();
        let file = parse_file("struct Terminal; type First = Terminal;").unwrap();
        let indexes = indexes_in(&file, &Selection::everything(), &cfg);
        let walk = Walk {
            calls: Calls::default(),
            scope: Vec::new(),
            module: Vec::new(),
            owner: None,
            owner_parameters: Vec::new(),
            structural_trusted: true,
            cfg: &cfg,
            indexes: &indexes,
        };
        for (text, low, high) in [
            ("First", "_", "self::Terminal"),
            ("Vec<First>", "Vec<_>", "Vec<self::Terminal>"),
            ("&First", "&_", "&self::Terminal"),
            ("(First, First)", "(_, _)", "(self::Terminal, self::Terminal)"),
            ("[First; 1]", "[_; 1]", "[self::Terminal; 1]"),
            ("[First]", "[_]", "[self::Terminal]"),
            ("*const First", "*const _", "*const self::Terminal"),
            ("*mut First", "*mut _", "*mut self::Terminal"),
            ("(First)", "(_)", "(self::Terminal)"),
        ] {
            for (remaining, expected) in [(1, low), (2, high)] {
                let mut ty = parse_str(text).unwrap();
                let mut budget = 4096;
                walk.canonical_type_at(&mut ty, remaining, &mut budget);
                assert_eq!(ty, parse_str::<Type>(expected).unwrap(), "{text}: {remaining}");
            }
        }
        let mut ty = parse_quote!(bool);
        walk.canonical_type_at(&mut ty, 0, &mut 4096);
        assert_eq!(ty, parse_quote!(_));
        for (remaining, expected) in [(1, parse_quote!(_)), (2, parse_quote!(self::Terminal))] {
            let mut ty = Type::Group(TypeGroup {
                attrs: Vec::new(),
                group_token: syn::token::Group::default(),
                elem: Box::new(parse_quote!(First)),
            });
            walk.canonical_type_at(&mut ty, remaining, &mut 4096);
            let Type::Group(group) = ty else {
                panic!("canonicalization must preserve a compiler-inserted group")
            };
            assert_eq!(*group.elem, expected);
        }
    }

    #[test]
    fn canonicalization_preserves_qualified_self_and_generic_arguments_when_splicing_imports() {
        let cfg = CfgSet::unconditional();
        let file = parse_file("extern crate self as this; struct Root; use external::Trait; use std::vec::Vec as Renamed;").unwrap();
        let indexes = indexes_in(&file, &Selection::everything(), &cfg);
        let walk = Walk {
            calls: Calls::of(&file, &cfg, &indexes),
            scope: Vec::new(),
            module: Vec::new(),
            owner: None,
            owner_parameters: Vec::new(),
            structural_trusted: true,
            cfg: &cfg,
            indexes: &indexes,
        };
        let mut ty = parse_quote!(<u8 as Trait>::Value);
        walk.canonical_type_at(&mut ty, 3, &mut 4096);
        assert_eq!(ty, parse_quote!(<u8 as Trait>::Value));
        let mut ty = parse_quote!(Renamed<bool>::IntoIter);
        walk.canonical_type_at(&mut ty, 3, &mut 4096);
        assert_eq!(ty, parse_quote!(std::vec::Vec<bool>::IntoIter));
        let mut ty = parse_quote!(::this::Root);
        walk.canonical_type_at(&mut ty, 3, &mut 4096);
        assert_eq!(ty, parse_quote!(crate::Root));
    }
}
