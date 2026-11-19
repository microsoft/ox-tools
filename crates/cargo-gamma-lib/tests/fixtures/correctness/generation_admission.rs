// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::time::{Duration, Instant};

pub struct Timing {
    elapsed: Duration,
}

pub fn field_assignment(value: &mut Timing, delta: Duration) {
    value.elapsed += delta;
}

pub fn tuple_assignment(value: &mut (u8, Duration), delta: Duration) {
    value.1 += delta;
}

pub fn timestamp(value: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<chrono::Utc> {
    value - chrono::Duration::days(90)
}

pub fn closure_clock(budget: Duration) -> Instant {
    let clock = || Instant::now();
    clock() + budget
}

pub fn collected_text(first: char, chars: &std::str::Chars<'_>) -> String {
    first.to_uppercase().collect::<String>() + chars.as_str()
}

pub struct Position(usize);

pub fn nonnumeric_range(start: Position, ends: (Position, Position)) -> std::ops::Range<Position> {
    start..ends.1
}

pub fn borrowed_predicate(values: &[(usize, bool)]) -> Option<(usize, &(usize, bool))> {
    let mut candidates = values.iter().enumerate().skip(1);
    candidates.find(|(_, (_, opens))| !opens)
}

pub fn borrowed_boundary(markers: &[usize]) -> bool {
    markers.iter().any(|offset| (1usize..9usize).contains(offset))
}

pub fn path_arguments(args: &syn::PathArguments) -> bool {
    args.is_none()
}

pub fn path_owner(ty: &mut syn::Type) -> bool {
    if let syn::Type::Path(path) = ty
        && let Some(owner) = path.path.segments.last_mut()
    {
        return owner.arguments.is_none();
    }
    false
}

pub fn option_predicate(value: Option<usize>) -> bool {
    value.is_none()
}

pub fn owned_boundary(value: usize) -> usize {
    value
}

#[expect(clippy::trivially_copy_pass_by_ref, reason = "The fixture exercises a borrowed numeric operand.")]
pub fn dereferenced_boundary(value: &usize) -> usize {
    *value
}

pub fn compiled_predicate(values: &[(usize, bool)]) -> Option<(usize, &(usize, bool))> {
    let mut candidates = values.iter().enumerate().skip(1);
    candidates.find(|(_, (_, opens))| *opens)
}
