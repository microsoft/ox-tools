// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Idempotent paired scans on a stationary diagonal.

use quote::quote;
use syn::{BinOp, Expr, Pat, Stmt, Type};

use super::{Cursor, local, method, name};

fn ident(expression: &Expr) -> Option<syn::Ident> {
    let Expr::Path(path) = expression else { return None };
    path.path
        .get_ident()
        // #[gamma::skip(logical.and_remove_right, reason = "get_ident already rejects qualified paths through its singleton and leading-colon requirements.", tag = "equivalent")]
        .filter(|_| path.attrs.is_empty() && path.qself.is_none())
        .cloned()
}

fn conjunction<'a>(expression: &'a Expr, parts: &mut Vec<&'a Expr>) -> Option<()> {
    // #[gamma::skip(cond.always_false, relational.ge_to_gt, literal.int_increment, reason = "The sole caller requires exactly four leaves and then compares the complete scan AST with its four-condition template.", tag = "equivalent")]
    if parts.len() >= 4 {
        return None; // #[gamma::skip(option.none_to_some, reason = "A truncated conjunction still fails the caller's complete scan-AST equality check.", tag = "equivalent")]
    }
    if let Expr::Binary(binary) = expression
        && matches!(binary.op, BinOp::And(_))
        && binary.attrs.is_empty()
    {
        conjunction(&binary.left, parts)?;
        conjunction(&binary.right, parts)
    } else {
        parts.push(expression);
        Some(())
    }
}

fn strings(ty: &Type) -> bool {
    let Type::Reference(reference) = ty else { return false };
    let Type::Slice(slice) = &*reference.elem else { return false };
    let Type::Reference(element) = &*slice.elem else { return false };
    reference.mutability.is_none()
        && element.mutability.is_none()
        && matches!(&*element.elem, Type::Path(path) if path.path.is_ident("str"))
}

impl Cursor {
    /// Certifies the closure when declared, before later bindings can shadow its captures.
    pub(super) fn shift(&self, closure: &syn::ExprClosure) -> bool {
        // #[gamma::skip(iter.first_to_last, reason = "The final complete closure equality requires exactly the one expected parameter.", tag = "equivalent")]
        let Some(Pat::Type(parameter)) = closure.inputs.first() else {
            return false;
        };
        let Pat::Ident(parameter) = &*parameter.pat else { return false };
        let Some(outer) = method(&closure.body, "unwrap_or", 1) else {
            return false;
        };
        let Expr::Call(convert) = &*outer.receiver else { return false };
        // #[gamma::skip(iter.first_to_last, reason = "The final complete closure equality requires exactly one argument at this conversion.", tag = "equivalent")]
        let Some(Expr::Binary(sum)) = convert.args.first() else {
            return false;
        };
        let Some(inner) = method(&sum.right, "unwrap_or", 1) else {
            return false;
        };
        let Expr::Call(convert) = &*inner.receiver else { return false };
        // #[gamma::skip(iter.first_to_last, reason = "The final complete closure equality requires exactly one argument at this conversion.", tag = "equivalent")]
        let Some(capture) = convert.args.first().and_then(ident) else {
            return false;
        };
        let parameter = &parameter.ident;
        if self.types.get(&capture.to_string()) != Some(&syn::parse_quote!(usize)) {
            return false;
        }
        let expected = syn::parse2::<syn::ExprClosure>(quote!(
            |#parameter: isize| usize::try_from(#parameter + isize::try_from(#capture).unwrap_or(isize::MAX)).unwrap_or(0)
        ));
        expected.as_ref().is_ok_and(|expected| expected == closure)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one closed scan grammar ties types, lexical names, paired advances and the backedge store together"
    )]
    pub(super) fn diagonal(&self, expression: &Expr) -> Option<()> {
        let Expr::While(outer) = expression else { return None };
        let Expr::Binary(bound) = &*outer.cond else { return None };
        if !outer.attrs.is_empty() || outer.label.is_some() || !bound.attrs.is_empty() || !matches!(bound.op, BinOp::Le(_)) {
            return None;
        }
        let cursor = ident(&bound.left)?;
        let distance = ident(&bound.right)?;
        if self.types.get(&distance.to_string()) != Some(&syn::parse_quote!(isize)) {
            return None;
        }
        let (entry_name, entry) = local(self.entry.as_ref()?)?;
        if cursor != entry_name || *entry != syn::parse_quote!(-#distance) {
            return None;
        }
        let [
            down,
            x,
            y,
            Stmt::Expr(Expr::While(scan), _),
            Stmt::Expr(Expr::Assign(store), _),
            Stmt::Expr(Expr::If(escape), _),
            tail @ ..,
        ] = outer.body.stmts.as_slice()
        else {
            return None;
        };
        let (down_name, down) = local(down)?;
        let Expr::Binary(disjunction) = down else { return None };
        // #[gamma::skip(cond.always_false, logical.or_remove_left, logical.or_remove_right, logical.or_to_and, reason = "The subsequent expected_down equality independently checks this entire unannotated disjunction.", tag = "equivalent")]
        if !disjunction.attrs.is_empty()
            || !matches!(disjunction.op, BinOp::Or(_))
            || *disjunction.left != syn::parse_quote!(#cursor == -#distance)
        {
            return None;
        }
        let (x_name, initial_x) = local(x)?;
        let Expr::If(initial_x) = initial_x else { return None };
        // #[gamma::skip(cond.always_false, logical.or_remove_left, logical.or_remove_right, logical.or_to_and, reason = "The subsequent expected_x equality independently checks the complete if expression.", tag = "equivalent")]
        if !initial_x.attrs.is_empty() || name(&initial_x.cond).as_deref() != Some(&down_name) {
            return None;
        }
        let [Stmt::Expr(Expr::Index(read), None)] = initial_x.then_branch.stmts.as_slice() else {
            return None;
        };
        let frontier = ident(&read.expr)?;
        let Expr::Call(slot) = &*read.index else { return None };
        let shift = ident(&slot.func)?;
        let unsupported_frontier = !self.closures.contains(&shift.to_string())
            || super::argument(self.types.get(&frontier.to_string())?, "Vec") != Some(&syn::parse_quote!(isize));
        if
        // #[gamma::skip(logical.or_remove_right, reason = "The subsequent expected_x equality independently repeats this complete indexed read.", tag = "equivalent")]
        unsupported_frontier || Expr::Index(read.clone()) != syn::parse_quote!(#frontier[#shift(#cursor + 1)]) {
            return None;
        }
        let down_ident = syn::Ident::new(&down_name, proc_macro2::Span::call_site());
        let expected_down: Expr = syn::parse_quote!(
            #cursor == -#distance
                || (#cursor != #distance && #frontier[#shift(#cursor - 1)] < #frontier[#shift(#cursor + 1)])
        );
        let expected_x: Expr = syn::parse_quote!(
            if #down_ident { #frontier[#shift(#cursor + 1)] } else { #frontier[#shift(#cursor - 1)] + 1 }
        );
        if *down != expected_down || Expr::If(initial_x.clone()) != expected_x {
            return None;
        }
        let (y_name, initial_y) = local(y)?;
        let x = syn::Ident::new(&x_name, proc_macro2::Span::call_site());
        let y = syn::Ident::new(&y_name, proc_macro2::Span::call_site());
        // #[gamma::skip(collection.reverse_array, reason = "Membership and duplicate-name checks depend only on this set of names, not its order.", tag = "equivalent")]
        let names = [&down_name, &x_name, &y_name];
        if names
            .iter()
            .any(|name| self.types.contains_key(*name) || self.closures.contains(*name))
            || names.iter().enumerate().any(|(at, name)| names[..at].contains(name))
            || *initial_y != syn::parse_quote!(#x - #cursor)
        {
            return None;
        }
        let mut conditions = Vec::new();
        conjunction(&scan.cond, &mut conditions)?;
        let [
            Expr::Let(binding),
            Expr::Binary(x_bound),
            Expr::Binary(y_bound),
            Expr::Binary(equal),
        ] = conditions.as_slice()
        else {
            return None;
        };
        let Pat::Tuple(patterns) = &*binding.pat else { return None };
        if patterns.elems.len() != 2 {
            return None;
        }
        let (Pat::TupleStruct(x_pattern), Pat::TupleStruct(y_pattern)) = (&patterns.elems[0], &patterns.elems[1]) else {
            return None;
        };
        // #[gamma::skip(iter.first_to_last, reason = "The final complete scan equality requires each Ok pattern to contain exactly one element.", tag = "equivalent")]
        let (Some(Pat::Ident(xi)), Some(Pat::Ident(yi))) = (x_pattern.elems.first(), y_pattern.elems.first()) else {
            return None;
        };
        let xi = &xi.ident;
        let yi = &yi.ident;
        let n = ident(&x_bound.right)?;
        let m = ident(&y_bound.right)?;
        let (Expr::Index(old), Expr::Index(new)) = (&*equal.left, &*equal.right) else {
            return None;
        };
        let old = ident(&old.expr)?;
        let new = ident(&new.expr)?;
        // #[gamma::skip(collection.reverse_array, reason = "Both independent immutable type checks must succeed; their order is unobservable.", tag = "equivalent")]
        for bound in [&n, &m] {
            if self.types.get(&bound.to_string()) != Some(&syn::parse_quote!(usize)) {
                return None;
            }
        }
        // #[gamma::skip(collection.reverse_array, reason = "Both independent immutable string-type checks must succeed; their order is unobservable.", tag = "equivalent")]
        for input in [&old, &new] {
            if !strings(self.types.get(&input.to_string())?) {
                return None;
            }
        }
        let expected: syn::ExprWhile = syn::parse_quote!(
            while let (Ok(#xi), Ok(#yi)) = (usize::try_from(#x), usize::try_from(#y))
                && #xi < #n && #yi < #m && #old[#xi] == #new[#yi] {
                #x += 1;
                #y += 1;
            }
        );
        if *scan != expected || Expr::Assign(store.clone()) != syn::parse_quote!(#frontier[#shift(#cursor)] = #x) {
            return None;
        }
        let expected: Expr = syn::parse_quote!(
            usize::try_from(#x).unwrap_or(0) >= #n && usize::try_from(#y).unwrap_or(0) >= #m
        );
        if !escape.attrs.is_empty()
            || *escape.cond != expected
            || escape.else_branch.is_some()
            || !matches!(escape.then_branch.stmts.as_slice(), [Stmt::Expr(Expr::Return(returned), _)] if returned.attrs.is_empty())
            || tail.len() > 1
        {
            return None;
        }
        for statement in tail {
            let Stmt::Expr(Expr::Binary(update), _) = statement else {
                return None;
            };
            if !update.attrs.is_empty()
                || !matches!(update.op, BinOp::AddAssign(_))
                || ident(&update.left) != Some(cursor.clone())
                || !super::super::integer_zero(&update.right)
            {
                return None;
            }
        }
        // At this backedge the escape was false. A repeated scan of immutable strings either
        // starts from the same source cell or from its own previous stopping point. Both yield
        // the same stopping point, even if the frontier's read/write indices coincide.
        Some(())
    }
}
