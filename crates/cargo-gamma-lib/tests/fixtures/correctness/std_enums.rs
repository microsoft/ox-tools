// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::cmp::Ordering;
use core::ops::ControlFlow;

trait Default {
    fn default() -> Self;
}

impl Default for u8 {
    fn default() -> Self {
        99
    }
}

impl Default for bool {
    fn default() -> Self {
        true
    }
}

pub fn shadowed_defaults() -> (u8, bool) {
    (Default::default(), Default::default())
}

pub fn stop() -> ControlFlow<u8, bool> {
    ControlFlow::Break(7)
}

pub fn proceed() -> ControlFlow<u8, bool> {
    ControlFlow::Continue(true)
}

pub fn order() -> Ordering {
    Ordering::Less
}

pub fn ordering_predicate(value: Ordering) -> bool {
    value.is_lt()
}

pub fn flow_predicate(value: ControlFlow<u8>) -> bool {
    value.is_break()
}

pub fn comparison(a: u8, b: u8) -> Ordering {
    a.cmp(&b)
}

pub fn partial(value: Option<Ordering>) -> Option<Ordering> {
    value
}

pub fn flow(value: ControlFlow<u8, bool>) -> ControlFlow<u8, bool> {
    value
}

pub fn continued_flow(value: ControlFlow<u8, bool>) -> ControlFlow<u8, bool> {
    value
}

pub fn parenthesized_stop() -> bool {
    let value = (ControlFlow::<u8, bool>::Break)(7);
    matches!(value, ControlFlow::Break(7))
}

pub fn parenthesized_proceed() -> bool {
    let value = (ControlFlow::<u8, bool>::Continue)(true);
    matches!(value, ControlFlow::Continue(true))
}
