// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Closed standard operator signatures; an unresolved operand never becomes negative evidence.

use syn::{BinOp, Expr, Lit, Type};

use super::Collector;
use super::calls::OperatorResult;
use super::values::{Kind, primitive_kind, strip};

#[derive(Clone, Copy, PartialEq)]
enum Operand {
    Duration,
    Instant,
    SystemTime,
    DateTime,
    NaiveDateTime,
    TimeDelta,
    String,
    Str,
    Scalar,
    Other,
}

impl Collector<'_> {
    pub(super) fn binary_expression_type(&self, node: &syn::ExprBinary) -> Option<Type> {
        let original = match node.op {
            BinOp::Add(_) => "+",
            BinOp::Sub(_) => "-",
            BinOp::Mul(_) => "*",
            BinOp::Div(_) => "/",
            BinOp::Rem(_) => "%",
            _ => return None,
        };
        let left = self.inferred_expression_type(&node.left)?;
        let right = self.inferred_expression_type(&node.right)?;
        if self.defaults.has_single_source_evidence()
            && let OperatorResult::Output(output) =
                self.calls
                    .operator_result(&left, &right, original, &self.scope_path, self.current_imports())
        {
            return Some(output.clone());
        }
        let left_kind = self.operator_type(&left)?;
        let right_kind = self.operator_type(&right)?;
        let output = result(left_kind, right_kind, original)?;
        if output == left_kind {
            Some(left)
        } else if output == right_kind {
            Some(right)
        } else {
            match output {
                Operand::Duration => Some(syn::parse_quote!(std::time::Duration)),
                Operand::TimeDelta => Some(syn::parse_quote!(chrono::TimeDelta)),
                _ => None,
            }
        }
    }

    fn operator_operand(&self, expression: &Expr) -> Option<Operand> {
        if let Expr::Lit(literal) = expression {
            match literal.lit {
                Lit::Int(_) | Lit::Float(_) => return Some(Operand::Scalar),
                Lit::Str(_) => return Some(Operand::Str),
                _ => {}
            }
        }
        let ty = self.inferred_expression_type(expression)?;
        self.operator_type(&ty)
    }

    fn operator_type(&self, ty: &Type) -> Option<Operand> {
        let types = self.types();
        let ty = types.resolve_alias(ty);
        if let Type::Reference(reference) = strip(ty) {
            if matches!(strip(&reference.elem), Type::Path(path) if path.path.is_ident("str")) {
                return Some(Operand::Str);
            }
            return self.operator_type(&reference.elem);
        }
        let path = types.unshadowed_type_path(ty)?;
        Some(match path.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
            ["std" | "core", "time", "Duration"] => Operand::Duration,
            ["std", "time", "Instant"] => Operand::Instant,
            ["std", "time", "SystemTime"] => Operand::SystemTime,
            ["chrono", "DateTime"] => Operand::DateTime,
            ["chrono", "NaiveDateTime"] => Operand::NaiveDateTime,
            ["chrono", "TimeDelta" | "Duration"] => Operand::TimeDelta,
            ["String"] | ["std" | "alloc", "string", "String"] => Operand::String,
            [name] | ["std" | "core", "primitive", name]
                if matches!(primitive_kind(name), Some(Kind::Signed | Kind::Unsigned | Kind::Float)) =>
            {
                Operand::Scalar
            }
            _ => Operand::Other,
        })
    }

    pub(super) fn unsupported_operator(&self, node: &syn::ExprBinary, replacement: &str) -> bool {
        let original = match node.op {
            BinOp::Add(_) => "+",
            BinOp::AddAssign(_) => "+=",
            BinOp::Sub(_) => "-",
            BinOp::SubAssign(_) => "-=",
            BinOp::Mul(_) => "*",
            BinOp::MulAssign(_) => "*=",
            BinOp::Div(_) => "/",
            BinOp::DivAssign(_) => "/=",
            BinOp::Rem(_) => "%",
            BinOp::RemAssign(_) => "%=",
            _ => return false,
        };
        if self.defaults.has_single_source_evidence()
            && let (Some(left), Some(right)) = (
                self.inferred_expression_type(&node.left),
                self.inferred_expression_type(&node.right),
            )
            && let OperatorResult::Output(output) =
                self.calls
                    .operator_result(&left, &right, original, &self.scope_path, self.current_imports())
        {
            return match self
                .calls
                .operator_result(&left, &right, replacement, &self.scope_path, self.current_imports())
            {
                OperatorResult::Unknown => false,
                OperatorResult::Missing => true,
                OperatorResult::Output(replaced) => {
                    let types = self.types();
                    let output = super::indexes::complete_type(types.canonical_type(output));
                    let replaced = super::indexes::complete_type(types.canonical_type(replaced));
                    !matches!(output, syn::Type::Infer(_)) && !matches!(replaced, syn::Type::Infer(_)) && replaced != output
                }
            };
        }
        let (Some(left), Some(right)) = (self.operator_operand(&node.left), self.operator_operand(&node.right)) else {
            return false;
        };
        let operator = replacement.trim_end_matches('=');
        let original_result = result(left, right, original.trim_end_matches('='));
        let replacement_result = result(left, right, operator);
        match (original_result, replacement_result) {
            (Some(original), Some(replacement)) => original != replacement,
            (Some(_), None) => true,
            _ => left != Operand::Other && right != Operand::Other,
        }
    }

    pub(super) fn unsupported_range_unit(&self, expression: &Expr, operator: &str) -> bool {
        if matches!(
            self.operator_operand(expression),
            Some(
                Operand::Duration
                    | Operand::Instant
                    | Operand::SystemTime
                    | Operand::DateTime
                    | Operand::NaiveDateTime
                    | Operand::TimeDelta
                    | Operand::String
                    | Operand::Str
            )
        ) {
            return true;
        }
        self.defaults.has_single_source_evidence()
            && self.inferred_expression_type(expression).is_some_and(|ty| {
                self.calls
                    .unit_operator_absent(&ty, operator, &self.scope_path, self.current_imports())
            })
    }
}

fn result(left: Operand, right: Operand, operator: &str) -> Option<Operand> {
    match (left, right, operator) {
        (Operand::Duration, Operand::Duration, "+" | "-")
        | (Operand::Duration, Operand::Scalar, "*" | "/")
        | (Operand::Scalar, Operand::Duration, "*")
        | (Operand::Instant, Operand::Instant, "-")
        | (Operand::SystemTime, Operand::SystemTime, "-") => Some(Operand::Duration),
        (Operand::Instant, Operand::Duration, "+" | "-") => Some(Operand::Instant),
        (Operand::SystemTime, Operand::Duration, "+" | "-") => Some(Operand::SystemTime),
        (Operand::DateTime, Operand::TimeDelta | Operand::Duration, "+" | "-") => Some(Operand::DateTime),
        (Operand::NaiveDateTime, Operand::TimeDelta | Operand::Duration, "+" | "-") => Some(Operand::NaiveDateTime),
        (Operand::DateTime, Operand::DateTime, "-")
        | (Operand::NaiveDateTime, Operand::NaiveDateTime, "-")
        | (Operand::TimeDelta, Operand::TimeDelta, "+" | "-")
        | (Operand::TimeDelta, Operand::Scalar, "*" | "/") => Some(Operand::TimeDelta),
        (Operand::String, Operand::Str, "+") => Some(Operand::String),
        (Operand::Scalar, Operand::Scalar, "+" | "-" | "*" | "/" | "%") => Some(Operand::Scalar),
        _ => None,
    }
}
