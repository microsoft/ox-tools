// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Closed unsigned intervals and finite positive floating-point bounds.

use super::{BinOp, Binding, Primitive, Value, float, identity};

pub(super) fn same_invariant(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::UnsignedRange(a, b, ty, _), Value::UnsignedRange(c, d, other, _)) => a == c && b == d && ty == other,
        (Value::FiniteRange(a, b, ty, _), Value::FiniteRange(c, d, other, _)) => a == c && b == d && ty == other,
        _ => left == right,
    }
}

fn unsigned(value: &Value) -> Option<(u128, u128, Primitive)> {
    match value {
        Value::Int(value, ty @ Primitive::Unsigned(_)) => Some((*value, *value, *ty)),
        Value::UnsignedRange(low, high, ty, _) => Some((*low, *high, *ty)),
        Value::Symbol(_, Binding::Primitive(ty @ Primitive::Unsigned(_))) => Some((0, ty.mask()?, *ty)),
        _ => None,
    }
}

fn unsigned_value(low: u128, high: u128, ty: Primitive, identity: String) -> Option<Value> {
    if low > high || high > ty.mask()? {
        return None;
    }
    Some(if low == high {
        Value::Int(low, ty)
    } else {
        Value::UnsignedRange(low, high, ty, identity)
    })
}

fn finite(low: f64, high: f64, ty: Primitive, identity: String) -> Option<Value> {
    if !low.is_finite() || !high.is_finite() || low < 0.0 || low > high {
        return None;
    }
    if low.to_bits() == high.to_bits() {
        return float(low, ty);
    }
    Some(Value::FiniteRange(low.to_bits(), high.to_bits(), ty, identity))
}

pub(super) fn wrapping(method: &str, left: &Value, right: &Value) -> Option<Value> {
    if matches!(left, Value::Int(..)) && matches!(right, Value::Int(..)) {
        return None;
    }
    let (low, high, ty) = unsigned(left)?;
    let Value::Int(amount, same) = right else { return None };
    if *same != ty {
        return None;
    }
    let mask = ty.mask()?;
    let bounds = match method {
        "wrapping_add" if high.checked_add(*amount).is_some_and(|sum| sum <= mask) => (low + amount, high + amount),
        "wrapping_sub" if low >= *amount => (low - amount, high - amount),
        "wrapping_mul" if high.checked_mul(*amount).is_some_and(|product| product <= mask) => (low * amount, high * amount),
        "wrapping_add" | "wrapping_sub" | "wrapping_mul" => (0, mask),
        _ => return None,
    };
    unsigned_value(bounds.0, bounds.1, ty, identity(format_args!("{method}({left:?},{right:?})"))?)
}

pub(super) fn binary(op: &BinOp, left: &Value, right: &Value) -> Option<Value> {
    if let BinOp::Shr(_) = op
        && !matches!(left, Value::Int(..))
    {
        let (low, high, ty @ Primitive::Unsigned(width)) = unsigned(left)? else {
            return None;
        };
        let Value::Int(shift, _) = right else { return None };
        if *shift >= u128::from(width) {
            return None;
        }
        return unsigned_value(low >> shift, high >> shift, ty, identity(format_args!("{left:?}>>{right:?}"))?);
    }
    let (Value::Float(bits, ty), Value::FiniteRange(low, high, other, _)) = (left, right) else {
        return None;
    };
    let (value, low, high) = (f64::from_bits(*bits), f64::from_bits(*low), f64::from_bits(*high));
    if ty != other || low <= 0.0 || *ty != Primitive::Float(64) || !value.is_finite() || value < 0.0 {
        return None;
    }
    match op {
        BinOp::Div(_) => finite(value / high, value / low, *ty, identity(format_args!("{left:?}/{right:?}"))?),
        BinOp::Mul(_) => finite(value * low, value * high, *ty, identity(format_args!("{left:?}*{right:?}"))?),
        _ => None,
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "monotone primitive endpoint casts bound all representable interior values"
)]
pub(super) fn cast(value: &Value, target: Primitive) -> Option<Value> {
    let (low, high, _) = unsigned(value)?;
    if target != Primitive::Float(64) {
        return None;
    }
    finite(low as f64, high as f64, target, identity(format_args!("{value:?} as f64"))?)
}

pub(super) fn from(value: Value, target: Primitive) -> Option<Value> {
    let Binding::Primitive(source) = value.kind() else { return None };
    let lossless = match (source, target) {
        (Primitive::Unsigned(a), Primitive::Unsigned(b)) | (Primitive::Signed(a), Primitive::Signed(b)) => a <= b,
        (Primitive::Unsigned(a), Primitive::Signed(b)) => a < b,
        (Primitive::Unsigned(a) | Primitive::Signed(a), Primitive::Float(64)) => a <= 32,
        (Primitive::Unsigned(a) | Primitive::Signed(a), Primitive::Float(32)) => a <= 16,
        (Primitive::Float(32), Primitive::Float(64)) => true,
        _ => false,
    };
    if !lossless {
        return None;
    }
    if let Value::Symbol(_, _) = value {
        return Some(Value::Symbol(
            identity(format_args!("{target:?}::from({value:?})"))?,
            Binding::Primitive(target),
        ));
    }
    super::cast(value, target)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn bounds(value: Value) -> (u128, u128) {
        match value {
            Value::Int(value, _) => (value, value),
            Value::UnsignedRange(low, high, _, _) => (low, high),
            other => panic!("expected unsigned bounds, got {other:?}"),
        }
    }

    #[test]
    fn wrapping_intervals_bound_every_native_result_including_wrap_edges() {
        let ty = Primitive::Unsigned(8);
        for (low, high) in [(0_u8, 1_u8), (0, 255), (1, 2), (7, 127), (127, 128), (254, 255)] {
            for amount in [0_u8, 1, 2, 127, 128, 255] {
                let interval = Value::UnsignedRange(u128::from(low), u128::from(high), ty, "input".into());
                for method in ["wrapping_add", "wrapping_sub", "wrapping_mul"] {
                    let result = wrapping(method, &interval, &Value::Int(u128::from(amount), ty)).unwrap();
                    let actual = bounds(result);
                    let values = (low..=high)
                        .map(|value| match method {
                            "wrapping_add" => value.wrapping_add(amount),
                            "wrapping_sub" => value.wrapping_sub(amount),
                            _ => value.wrapping_mul(amount),
                        })
                        .collect::<Vec<_>>();
                    let exact = (u128::from(*values.iter().min().unwrap()), u128::from(*values.iter().max().unwrap()));
                    assert!(
                        actual.0 <= exact.0 && exact.1 <= actual.1,
                        "{low}..={high} {method} {amount}: {actual:?}"
                    );
                    let no_wrap = match method {
                        "wrapping_add" => high.checked_add(amount).is_some(),
                        "wrapping_sub" => low >= amount,
                        _ => high.checked_mul(amount).is_some(),
                    };
                    assert_eq!(actual, if no_wrap { exact } else { (0, 255) });
                }
            }
        }
        assert!(wrapping("wrapping_add", &Value::Int(1, ty), &Value::Int(2, ty)).is_none());
        assert!(
            wrapping(
                "saturating_add",
                &Value::Symbol("x".into(), Binding::Primitive(ty)),
                &Value::Int(1, ty)
            )
            .is_none()
        );
        assert!(
            wrapping(
                "wrapping_add",
                &Value::Symbol("x".into(), Binding::Primitive(ty)),
                &Value::Int(1, Primitive::Unsigned(16))
            )
            .is_none()
        );
    }

    #[test]
    fn unsigned_shift_collapses_singletons_and_rejects_out_of_width_counts() {
        let ty = Primitive::Unsigned(8);
        let value = Value::UnsignedRange(128, 255, ty, "input".into());
        for (shift, expected) in [
            (0, Some((128, 255))),
            (1, Some((64, 127))),
            (7, Some((1, 1))),
            (8, None),
            (128, None),
        ] {
            assert_eq!(binary(&syn::parse_quote!(>>), &value, &Value::Int(shift, ty)).map(bounds), expected);
        }
        assert!(binary(&syn::parse_quote!(<<), &value, &Value::Int(1, ty)).is_none());
        assert!(binary(&syn::parse_quote!(>>), &value, &Value::Bool(true)).is_none());
        assert!(unsigned_value(2, 1, ty, "bad".into()).is_none());
        assert!(unsigned_value(0, 256, ty, "bad".into()).is_none());
        assert_eq!(unsigned_value(255, 255, ty, "singleton".into()), Some(Value::Int(255, ty)));
        assert_eq!(unsigned(&Value::Int(7, ty)), Some((7, 7, ty)));
        assert!(cast(&Value::UnsignedRange(0, 7, ty, "input".into()), Primitive::Float(32)).is_none());
    }

    #[test]
    fn finite_float_interval_operations_reverse_divisor_endpoints() {
        let ty = Primitive::Float(64);
        let left = Value::Float(12.0_f64.to_bits(), ty);
        let right = finite(2.0, 4.0, ty, "denominator".into()).unwrap();
        for (op, low, high) in [(syn::parse_quote!(/), 3.0_f64, 6.0_f64), (syn::parse_quote!(*), 24.0, 48.0)] {
            let Some(Value::FiniteRange(actual_low, actual_high, actual_ty, _)) = binary(&op, &left, &right) else {
                panic!("expected finite bounds");
            };
            assert_eq!((actual_low, actual_high, actual_ty), (low.to_bits(), high.to_bits(), ty));
        }
        assert!(binary(&syn::parse_quote!(/), &left, &finite(0.0, 4.0, ty, "zero".into()).unwrap()).is_none());
        assert!(binary(&syn::parse_quote!(+), &left, &right).is_none());
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            assert!(finite(invalid, 4.0, ty, "bad".into()).is_none());
            assert!(finite(0.0, invalid, ty, "bad".into()).is_none());
        }
        assert!(finite(4.0, 2.0, ty, "reversed".into()).is_none());
        assert_eq!(finite(2.0, 2.0, ty, "point".into()), Some(Value::Float(2.0_f64.to_bits(), ty)));
        assert!(binary(&syn::parse_quote!(*), &Value::Float(f64::INFINITY.to_bits(), ty), &right).is_none());
        assert!(binary(&syn::parse_quote!(*), &Value::Float((-1.0_f64).to_bits(), ty), &right).is_none());
    }

    #[test]
    fn invariant_equality_ignores_only_provenance_not_bounds_or_types() {
        let a = Value::UnsignedRange(1, 7, Primitive::Unsigned(8), "first".into());
        assert!(same_invariant(
            &a,
            &Value::UnsignedRange(1, 7, Primitive::Unsigned(8), "second".into())
        ));
        for other in [
            Value::UnsignedRange(0, 7, Primitive::Unsigned(8), "first".into()),
            Value::UnsignedRange(1, 8, Primitive::Unsigned(8), "first".into()),
            Value::UnsignedRange(1, 7, Primitive::Unsigned(16), "first".into()),
            Value::Int(1, Primitive::Unsigned(8)),
        ] {
            assert!(!same_invariant(&a, &other));
        }
        let a = finite(1.0, 7.0, Primitive::Float(64), "first".into()).unwrap();
        assert!(same_invariant(
            &a,
            &finite(1.0, 7.0, Primitive::Float(64), "second".into()).unwrap()
        ));
        assert!(!same_invariant(
            &a,
            &finite(1.0, 8.0, Primitive::Float(64), "first".into()).unwrap()
        ));
        assert!(!same_invariant(
            &Value::Symbol("a".into(), Binding::Bool),
            &Value::Symbol("b".into(), Binding::Bool)
        ));
    }

    #[test]
    fn from_symbol_requires_lossless_width_and_signedness_conversion() {
        for (source, target, accepted) in [
            (Primitive::Unsigned(8), Primitive::Unsigned(8), true),
            (Primitive::Unsigned(16), Primitive::Unsigned(8), false),
            (Primitive::Signed(8), Primitive::Signed(16), true),
            (Primitive::Signed(16), Primitive::Signed(8), false),
            (Primitive::Unsigned(8), Primitive::Signed(8), false),
            (Primitive::Unsigned(8), Primitive::Signed(16), true),
            (Primitive::Signed(8), Primitive::Unsigned(16), false),
            (Primitive::Unsigned(32), Primitive::Float(64), true),
            (Primitive::Unsigned(64), Primitive::Float(64), false),
            (Primitive::Signed(16), Primitive::Float(32), true),
            (Primitive::Signed(32), Primitive::Float(32), false),
            (Primitive::Float(32), Primitive::Float(64), true),
            (Primitive::Float(64), Primitive::Float(32), false),
        ] {
            let converted = from(Value::Symbol("input".into(), Binding::Primitive(source)), target);
            assert_eq!(converted.is_some(), accepted, "{source:?} -> {target:?}");
            if let Some(value) = converted {
                assert_eq!(value.kind(), Binding::Primitive(target));
                assert_ne!(value, Value::Symbol("input".into(), Binding::Primitive(source)));
            }
        }
        assert!(from(Value::Bool(true), Primitive::Unsigned(8)).is_none());
    }
}
