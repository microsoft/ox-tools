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
    (if ::gamma_rt::a(1u32) {
        ::core::ops::ControlFlow::Continue(::core::default::Default::default())
    } else {
        ControlFlow::Break(7)
    })
}

pub fn proceed() -> ControlFlow<u8, bool> {
    (if ::gamma_rt::a(2u32) {
        ::core::ops::ControlFlow::Break(::core::default::Default::default())
    } else {
        ControlFlow::Continue(true)
    })
}

pub fn order() -> Ordering {
    (if ::gamma_rt::a(3u32) {
        ::core::cmp::Ordering::Greater
    } else {
        Ordering::Less
    })
}

pub fn ordering_predicate(value: Ordering) -> bool {
    (if ::gamma_rt::a(4u32) { value.is_ge() } else { value.is_lt() })
}

pub fn flow_predicate(value: ControlFlow<u8>) -> bool {
    (if ::gamma_rt::a(5u32) {
        value.is_continue()
    } else {
        value.is_break()
    })
}

pub fn comparison(a: u8, b: u8) -> Ordering {
    if ::gamma_rt::a(6u32) {
        ::core::cmp::Ordering::Equal
    } else {
        { a.cmp(&b) }
    }
}

pub fn partial(value: Option<Ordering>) -> Option<Ordering> {
    if ::gamma_rt::a(7u32) {
        Some(::core::cmp::Ordering::Greater)
    } else {
        { value }
    }
}

pub fn flow(value: ControlFlow<u8, bool>) -> ControlFlow<u8, bool> {
    if ::gamma_rt::a(8u32) {
        ::core::ops::ControlFlow::Break(::core::default::Default::default())
    } else {
        { value }
    }
}

pub fn continued_flow(value: ControlFlow<u8, bool>) -> ControlFlow<u8, bool> {
    if ::gamma_rt::a(11u32) {
        ::core::ops::ControlFlow::Continue(::core::default::Default::default())
    } else {
        { value }
    }
}

pub fn parenthesized_stop() -> bool {
    let value = (if ::gamma_rt::a(9u32) {
        ::core::ops::ControlFlow::Continue(::core::default::Default::default())
    } else {
        (ControlFlow::<u8, bool>::Break)(7)
    });
    matches!(value, ControlFlow::Break(7))
}

pub fn parenthesized_proceed() -> bool {
    let value = (if ::gamma_rt::a(10u32) {
        ::core::ops::ControlFlow::Break(::core::default::Default::default())
    } else {
        (ControlFlow::<u8, bool>::Continue)(true)
    });
    matches!(value, ControlFlow::Continue(true))
}
