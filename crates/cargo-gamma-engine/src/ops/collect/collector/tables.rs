// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The curated substitutions a mutator chooses from.

use syn::{BinOp, Expr};

/// The curated renames for a standard-library method, keyed by name and argument count.
///
/// The argument count is not decoration. Without type resolution the only evidence that a `take`
/// is `Iterator::take` rather than `Option::take` or `Cell::take` is that it was given a count,
/// and swapping the second kind for `skip` would be applying a transformation nobody advertised.
/// Every entry here is a pair of standard-library methods with the same receiver, arity and
/// result *type*, which is a stricter requirement than it sounds. `take` and `skip` ask a genuine
/// question about a chain, but `Take<I>` and `Skip<I>` are different types, and a mutant shares an
/// `if` with the code it replaces, so that swap could never compile. The same rules out
/// `take_while`/`skip_while`. What is left are the methods whose two spellings agree on a type:
/// `bool`, `Option<T>`, `String` and `&str`.
pub(super) fn method_renames(method: &str, arity: usize) -> Option<&'static [(&'static str, &'static str)]> {
    let swaps: &'static [(&'static str, &'static str)] = match (method, arity) {
        ("any", 1) => &[("iter.any_to_all", "all")],
        ("all", 1) => &[("iter.all_to_any", "any")],

        // Zero arguments is `Iterator::min`; one is `Ord::min`. Both are a choice between the
        // extremes, so both are worth swapping.
        ("min", 0 | 1) => &[("iter.min_to_max", "max")],
        ("max", 0 | 1) => &[("iter.max_to_min", "min")],

        ("first", 0) => &[("iter.first_to_last", "last")],
        ("last", 0) => &[("iter.last_to_first", "first")],

        ("starts_with", 1) => &[("string.starts_with_to_ends_with", "ends_with")],
        ("ends_with", 1) => &[("string.ends_with_to_starts_with", "starts_with")],

        ("to_lowercase", 0) => &[("string.lower_to_upper", "to_uppercase")],
        ("to_uppercase", 0) => &[("string.upper_to_lower", "to_lowercase")],
        ("to_ascii_lowercase", 0) => &[("string.lower_to_upper", "to_ascii_uppercase")],
        ("to_ascii_uppercase", 0) => &[("string.upper_to_lower", "to_ascii_lowercase")],

        ("trim_start", 0) => &[("string.trim_start_to_trim_end", "trim_end")],
        ("trim_end", 0) => &[("string.trim_end_to_trim_start", "trim_start")],

        _ => return None,
    };

    Some(swaps)
}

/// The mutator for deleting an in-place ordering or deduplication call.
///
/// These are the counterpart to the adapters above: because they return `()`, the only way to
/// remove one is to delete the whole statement, and the question they ask — does anything observe
/// that this collection was ordered? — is worth asking under its own name rather than folding it
/// into generic statement deletion.
pub(super) fn in_place_reorder(expression: &Expr) -> Option<&'static str> {
    let Expr::MethodCall(call) = expression else {
        return None;
    };

    match call.method.to_string().as_str() {
        "sort" | "sort_by" | "sort_by_key" | "sort_unstable" | "sort_unstable_by" | "sort_unstable_by_key" => Some("iter.remove_sort"),
        "dedup" | "dedup_by" | "dedup_by_key" => Some("iter.remove_dedup"),
        _ => None,
    }
}

/// The mutators and replacement operators available for a binary operator.
pub(super) const fn binary_replacements(op: &BinOp) -> &'static [(&'static str, &'static str)] {
    match op {
        BinOp::Lt(_) => &[("relational.lt_to_le", "<="), ("relational.lt_to_gt", ">")],
        BinOp::Le(_) => &[("relational.le_to_lt", "<"), ("relational.le_to_ge", ">=")],
        BinOp::Gt(_) => &[("relational.gt_to_ge", ">="), ("relational.gt_to_lt", "<")],
        BinOp::Ge(_) => &[("relational.ge_to_gt", ">"), ("relational.ge_to_le", "<=")],
        BinOp::Eq(_) => &[("relational.eq_to_ne", "!=")],
        BinOp::Ne(_) => &[("relational.ne_to_eq", "==")],

        BinOp::Add(_) => &[("arith.add_to_sub", "-"), ("arith.add_to_mul", "*")],
        BinOp::Sub(_) => &[("arith.sub_to_add", "+"), ("arith.sub_to_div", "/")],
        BinOp::Mul(_) => &[("arith.mul_to_div", "/"), ("arith.mul_to_add", "+")],
        BinOp::Div(_) => &[("arith.div_to_mul", "*"), ("arith.div_to_rem", "%")],
        BinOp::Rem(_) => &[("arith.rem_to_div", "/"), ("arith.rem_to_mul", "*")],

        BinOp::BitAnd(_) => &[("bitwise.and_to_or", "|"), ("bitwise.and_to_xor", "^")],
        BinOp::BitOr(_) => &[("bitwise.or_to_and", "&")],
        BinOp::BitXor(_) => &[("bitwise.xor_to_and", "&")],
        BinOp::Shl(_) => &[("shift.shl_to_shr", ">>")],
        BinOp::Shr(_) => &[("shift.shr_to_shl", "<<")],

        BinOp::And(_) => &[("logical.and_to_or", "||")],
        BinOp::Or(_) => &[("logical.or_to_and", "&&")],

        BinOp::AddAssign(_) => &[("assign.add_to_sub", "-=")],
        BinOp::SubAssign(_) => &[("assign.sub_to_add", "+=")],
        BinOp::MulAssign(_) => &[("assign.mul_to_div", "/=")],
        BinOp::DivAssign(_) => &[("assign.div_to_mul", "*=")],
        BinOp::RemAssign(_) => &[("assign.rem_to_div", "/=")],
        BinOp::BitAndAssign(_) => &[("assign.and_to_or", "|=")],
        BinOp::BitOrAssign(_) => &[("assign.or_to_and", "&=")],
        BinOp::BitXorAssign(_) => &[("assign.xor_to_and", "&=")],
        BinOp::ShlAssign(_) => &[("assign.shl_to_shr", ">>=")],
        BinOp::ShrAssign(_) => &[("assign.shr_to_shl", "<<=")],

        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use syn::parse_quote;

    use super::*;

    #[test]
    fn method_renames_depend_on_both_name_and_arity() {
        assert_eq!(method_renames("min", 0), Some(&[("iter.min_to_max", "max")][..]));
        assert_eq!(method_renames("min", 2), None);
        assert_eq!(
            method_renames("to_ascii_uppercase", 0),
            Some(&[("string.upper_to_lower", "to_ascii_lowercase")][..])
        );
    }

    #[test]
    fn in_place_reorder_distinguishes_sort_dedup_and_other_calls() {
        let sort: Expr = parse_quote!(values.sort_unstable_by_key(key));
        let dedup: Expr = parse_quote!(values.dedup_by_key(key));
        let other: Expr = parse_quote!(values.reserve(8));
        let not_a_call: Expr = parse_quote!(values[0]);

        assert_eq!(in_place_reorder(&sort), Some("iter.remove_sort"));
        assert_eq!(in_place_reorder(&dedup), Some("iter.remove_dedup"));
        assert_eq!(in_place_reorder(&other), None);
        assert_eq!(in_place_reorder(&not_a_call), None);
    }

    #[test]
    fn binary_replacements_cover_supported_operator_families() {
        let relational: syn::ExprBinary = parse_quote!(left < right);
        let arithmetic: syn::ExprBinary = parse_quote!(left + right);
        let logical: syn::ExprBinary = parse_quote!(left && right);
        let shift_assign: syn::ExprBinary = parse_quote!(left <<= right);

        assert_eq!(
            binary_replacements(&relational.op),
            &[("relational.lt_to_le", "<="), ("relational.lt_to_gt", ">")]
        );
        assert_eq!(
            binary_replacements(&arithmetic.op),
            &[("arith.add_to_sub", "-"), ("arith.add_to_mul", "*")]
        );
        assert_eq!(binary_replacements(&logical.op), &[("logical.and_to_or", "||")]);
        assert_eq!(binary_replacements(&shift_assign.op), &[("assign.shl_to_shr", ">>=")]);
    }
}
