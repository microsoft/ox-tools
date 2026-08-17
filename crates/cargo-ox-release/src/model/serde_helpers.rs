// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Deserialization helpers that tolerate the JSON shapes the facts and request
//! producers emit: a single-element array written as a bare scalar, an empty
//! list or empty string written as `null`, and numeric fields that may arrive
//! as a number, a string, or a bool.

use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// A value that arrived either as a single element or as an array — the shape a
/// producer emits when it unwraps a one-element array to a bare scalar.
#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany<T> {
    Many(Vec<T>),
    One(T),
}

impl<T> OneOrMany<T> {
    fn into_vec(self) -> Vec<T> {
        match self {
            Self::Many(items) => items,
            Self::One(item) => vec![item],
        }
    }
}

/// Like [`flexible_vec`], but preserves the absent/`null` case as `None` so a
/// caller can distinguish "the property was omitted or null" (an error for a
/// required macro-contract field) from "present but empty".
pub(crate) fn opt_flexible_vec<'de, D, T>(deserializer: D) -> Result<Option<Vec<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<OneOrMany<T>>::deserialize(deserializer)?.map(OneOrMany::into_vec))
}

/// Deserializes a value that may be absent, `null`, a single scalar/object, or
/// an array, into a `Vec`. A producer may write a single-element array as a
/// bare scalar and an empty list as `null`, so a plain `Vec` field would fail
/// to parse either shape.
pub(crate) fn flexible_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(opt_flexible_vec(deserializer)?.unwrap_or_default())
}

/// Deserializes a string field a producer may emit as `null` (its rendering of
/// an empty string), mapping `null`/absent to an empty string.
pub(crate) fn null_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

/// Deserializes an exit code that may be serialized as an integer, a float
/// (`101.0`), a numeric string, or a bool. Anything else, or absent/`null`,
/// yields `None`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "a float exit code is truncated toward zero, matching integer exit-code semantics"
)]
pub(crate) fn flex_int<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(match Option::<Value>::deserialize(deserializer)? {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        Some(Value::Bool(b)) => Some(i64::from(b)),
        // Absent, null, or any other shape: no usable exit code.
        None | Some(_) => None,
    })
}
