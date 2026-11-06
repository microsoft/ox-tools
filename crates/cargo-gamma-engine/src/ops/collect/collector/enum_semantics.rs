// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mutations whose validity depends on standard control-flow and comparison enum identity.

use compact_str::format_compact;
use syn::spanned::Spanned as _;
use syn::visit::{self, Visit};
use syn::{Block, Expr, ExprCall, ExprMethodCall, ExprPath, FnArg, Ident, PatIdent, Signature, Stmt, Type, TypePath};

use super::types::Types;
use super::values::strip;
use super::{Collector, Confidence, Shape, noop};

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests;

fn variant(path: &ExprPath, types: &Types<'_>, module: &str, name: &str) -> Option<String> {
    let ty = Type::Path(TypePath {
        attrs: Vec::new(),
        qself: path.qself.clone(),
        path: path.path.clone(),
    });
    let resolved = types.unshadowed_type_path(&ty)?;
    if let [root, namespace, owner, variant] = resolved.as_slice()
        && matches!(root.as_str(), "std" | "core")
        && namespace == module
        && owner == name
    {
        return Some(variant.clone());
    }
    let mut owner = path.path.clone();
    let last = owner.segments.pop().expect("unshadowed_type_path above requires a nonempty path");
    let ty = Type::Path(TypePath {
        attrs: Vec::new(),
        qself: None,
        path: owner,
    });
    let instantiated = types.instantiate_alias(&ty);
    let owner = types.unshadowed_type_path(instantiated.as_ref().unwrap_or(&ty))?;
    let known = matches!(owner.as_slice(), [root, namespace, owner]
        if matches!(root.as_str(), "std" | "core") && namespace == module && owner == name);
    known.then(|| last.ident.to_string())
}

fn variant_path(expression: &Expr) -> Option<&ExprPath> {
    match expression {
        Expr::Path(path) => Some(path),
        Expr::Paren(paren) => variant_path(&paren.expr),
        Expr::Group(group) => variant_path(&group.expr),
        _ => None,
    }
}

pub(super) fn control_flow_variant(expression: &Expr, types: &Types<'_>) -> Option<String> {
    variant_path(expression).and_then(|path| variant(path, types, "ops", "ControlFlow"))
}

fn plain_variant_path(expression: &Expr) -> Option<&ExprPath> {
    match plain_expression(expression) {
        Expr::Path(path) if path.attrs.is_empty() => Some(path),
        _ => None,
    }
}

fn plain_expression(expression: &Expr) -> &Expr {
    match expression {
        Expr::Paren(paren) if paren.attrs.is_empty() => plain_expression(&paren.expr),
        Expr::Group(group) if group.attrs.is_empty() => plain_expression(&group.expr),
        expression => expression,
    }
}

fn constant_return_expression(body: &Block) -> Option<&Expr> {
    let [Stmt::Expr(expression, semicolon)] = body.stmts.as_slice() else {
        return
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "The return type is Option<&Expr>; its default is already None.")]
        None;
    };
    match expression {
        Expr::Return(returned) if returned.attrs.is_empty() => returned.expr.as_deref(),
        expression if semicolon.is_none() => Some(expression),
        _ => None,
    }
}

struct ParameterBinding<'a> {
    name: &'a Ident,
    found: bool,
}

impl<'ast> Visit<'ast> for ParameterBinding<'_> {
    fn visit_pat_ident(&mut self, i: &'ast PatIdent) {
        self.found |= i.ident == *self.name;
        visit::visit_pat_ident(self, i);
    }
}

impl Collector<'_> {
    pub(super) fn constant_ordering_return(&self, signature: &Signature, body: &Block, types: &Types<'_>) -> Option<String> {
        let path = plain_variant_path(constant_return_expression(body)?)?;
        self.constant_return_variant(signature, path, types, "cmp", "Ordering")
    }

    pub(super) fn constant_default_control_flow_return(&self, signature: &Signature, body: &Block, types: &Types<'_>) -> Option<String> {
        let Expr::Call(call) = plain_expression(constant_return_expression(body)?) else {
            return
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "The return type is Option<String>; its default is already None.")]
            None;
        };
        if !call.attrs.is_empty() || call.args.len() != 1 {
            return
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "The return type is Option<String>; its default is already None.")]
            None;
        }
        // #[gamma::skip(iter.first_to_last, try.propagate_to_unwrap, tag = "equivalent", reason = "The preceding call.args.len()==1 check guarantees that first exists and is the same element as last.")]
        let payload = plain_expression(call.args.first()?);
        let known_default = match payload {
            Expr::Tuple(tuple) => tuple.attrs.is_empty() && tuple.elems.is_empty(),
            Expr::Lit(literal) => literal.attrs.is_empty() && self.expression_is_known_default(payload),
            Expr::Call(default) => {
                let path = plain_variant_path(&default.func)?;
                // A bounded type can have an inherent `default` unrelated to the standard trait.
                !path
                    .path
                    .segments
                    .first()
                    .is_some_and(|segment| types.defaulted.contains(&segment.ident.to_string()))
                    && noop::is_resolved_trait_default(payload, types)
            }
            _ => false,
        };
        if !known_default {
            return
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "The return type is Option<String>; its default is already None.")]
            None;
        }
        let path = plain_variant_path(&call.func)?;
        self.constant_return_variant(signature, path, types, "ops", "ControlFlow")
    }

    fn constant_return_variant(
        &self,
        signature: &Signature,
        path: &ExprPath,
        types: &Types<'_>,
        module: &str,
        name: &str,
    ) -> Option<String> {
        if self.variant_is_shadowed(path) {
            return
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "The return type is Option<String>; its default is already None.")]
            None;
        }
        if let Some(name) = path.path.get_ident() {
            let mut binding = ParameterBinding { name, found: false };
            for input in &signature.inputs {
                if let FnArg::Typed(input) = input {
                    binding.visit_pat(&input.pat);
                }
            }
            if binding.found {
                return
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "The return type is Option<String>; its default is already None.")]
                None;
            }
        }
        variant(path, types, module, name)
    }

    pub(super) fn control_flow_constructor(&mut self, node: &ExprCall) {
        if node.args.len() != 1 || self.callee_is_binding(&node.func) {
            return;
        }

        let Some(variant) = control_flow_variant(&node.func, &self.types()) else {
            return;
        };
        let (mutator, replacement, destination) = match variant.as_str() {
            "Break" => ("control_flow.break_to_continue", "Continue", 1),
            "Continue" => ("control_flow.continue_to_break", "Break", 0),
            _ => return,
        };
        let root = self.types().standard_enum_root();
        let replacement = format_compact!("::{root}::ops::ControlFlow::{replacement}(::{root}::default::Default::default())");
        let explicit_default = if let Some(path) = variant_path(&node.func) {
            let mut owner = path.path.clone();
            owner.segments.pop();
            let ty = Type::Path(TypePath {
                attrs: Vec::new(),
                qself: None,
                path: owner,
            });
            let types = self.types();
            types
                .control_flow_payload(&ty, destination)
                .is_some_and(|payload| types.has_proven_default(&payload))
        } else {
            // #[gamma::skip(literal.bool_flip, tag = "equivalent", reason = "The earlier successful control_flow_variant call already required variant_path on this same immutable callee to return Some, so this None branch is unreachable.")]
            false
        };
        self.emit_at_conditionally(
            mutator,
            node.span().byte_range(),
            replacement,
            0,
            Shape::Expr,
            Confidence::Proven,
            self.admissible_defaults.contains(&node.span().byte_range()) || explicit_default,
        );
    }

    pub(super) fn ordering_variant(&mut self, node: &ExprPath) {
        if self.variant_is_shadowed(node) {
            return;
        }
        let Some(original) = variant(node, &self.types(), "cmp", "Ordering") else {
            return;
        };
        let root = self.types().standard_enum_root();
        let replacements = match original.as_str() {
            // #[gamma::skip(collection.reverse_array, tag = "equivalent", reason = "Each alternative has a distinct mutator name and index zero; traversal sorts candidates by span and mutator name before returning them.")]
            "Less" => [("ordering.less_to_equal", "Equal"), ("ordering.less_to_greater", "Greater")],
            // #[gamma::skip(collection.reverse_array, tag = "equivalent", reason = "Each alternative has a distinct mutator name and index zero; traversal sorts candidates by span and mutator name before returning them.")]
            "Equal" => [("ordering.equal_to_less", "Less"), ("ordering.equal_to_greater", "Greater")],
            // #[gamma::skip(collection.reverse_array, tag = "equivalent", reason = "Each alternative has a distinct mutator name and index zero; traversal sorts candidates by span and mutator name before returning them.")]
            "Greater" => [("ordering.greater_to_less", "Less"), ("ordering.greater_to_equal", "Equal")],
            _ => return,
        };
        for (mutator, replacement) in replacements {
            self.emit(mutator, node.span(), format_compact!("::{root}::cmp::Ordering::{replacement}"), 0);
        }
    }

    pub(super) fn enum_predicate(&mut self, node: &ExprMethodCall, method: &str) {
        let (control_flow, mutator, replacement) = match method {
            "is_break" => (true, "control_flow.is_break_to_is_continue", "is_continue"),
            "is_continue" => (true, "control_flow.is_continue_to_is_break", "is_break"),
            "is_lt" => (false, "ordering.is_lt_to_is_ge", "is_ge"),
            "is_ge" => (false, "ordering.is_ge_to_is_lt", "is_lt"),
            "is_le" => (false, "ordering.is_le_to_is_gt", "is_gt"),
            "is_gt" => (false, "ordering.is_gt_to_is_le", "is_le"),
            "is_eq" => (false, "ordering.is_eq_to_is_ne", "is_ne"),
            "is_ne" => (false, "ordering.is_ne_to_is_eq", "is_eq"),
            _ => return,
        };
        if !node.args.is_empty() {
            return;
        }
        let Some(ty) = self.inferred_expression_type(&node.receiver) else {
            return;
        };
        let mut ty = strip(&ty);
        while let Type::Reference(reference) = ty {
            // #[gamma::skip(stmt.delete_assign, tag = "timeout", reason = "Deleting the sole reference-peeling update leaves the same reference type in the loop forever; the campaign confirmed a timeout rather than a different discovery result.")]
            ty = strip(&reference.elem);
        }
        let types = self.types();
        let supported = if control_flow {
            types.supports_control_flow_api(ty)
        } else {
            types.supports_ordering_api(ty)
        };
        if supported {
            self.rename_method_as(node, mutator, replacement, Shape::Expr);
        }
    }

    fn callee_is_binding(&self, expression: &Expr) -> bool {
        variant_path(expression).is_some_and(|path| self.variant_is_shadowed(path))
    }

    fn variant_is_shadowed(&self, path: &ExprPath) -> bool {
        path.path.leading_colon.is_none() && path.path.segments.len() == 1 && {
            let name = path.path.segments[0].ident.to_string();
            self.const_parameters.contains(&name)
                || self.bindings.contains_key(&name)
                || self
                    .scope_value_items
                    .get(&self.scope_path)
                    .is_some_and(|items| items.contains(&name))
        }
    }
}
