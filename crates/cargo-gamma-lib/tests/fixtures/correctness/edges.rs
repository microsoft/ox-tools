// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::cell::{Cell, RefCell};
use std::ops::RangeInclusive;

pub fn early(flag: bool) -> Result<Option<impl Iterator<Item = u8>>, ()> {
    fn inner() -> u8 {
        return 3;
    }
    let nested = || {
        return inner();
    };
    if flag {
        return Ok(Some(core::iter::once(nested())));
    }
    Ok(Some(core::iter::once(7)))
}

pub fn absent() -> Result<Option<impl Iterator<Item = u8>>, ()> {
    Ok(Some(core::iter::once(7)))
}

pub fn direct_early(flag: bool) -> impl Iterator<Item = u8> {
    if flag {
        return core::iter::once(3);
    }
    core::iter::once(7)
}

pub fn shadow_early(value: Option<&u8>) -> Result<impl Iterator<Item = u8>, ()> {
    if let Some(value) = value {
        return Ok(core::iter::once(*value));
    }
    Ok(core::iter::once(7))
}

pub fn initializer_shadow(value: usize, seen: &Cell<usize>) -> usize {
    let value = {
        seen.set(value);
        String::new()
    };
    value.len()
}

pub fn unknown_borrowed(values: &[(u8, bool)]) -> bool {
    values.iter().any(|(_, flag)| !flag)
}

pub fn inferred_range() -> RangeInclusive<f32> {
    let start = 0.5;
    let end = 4.5;
    start..=end
}

pub fn associated_range(hits: u32) -> RangeInclusive<f64> {
    0.0..=f64::from(hits)
}

pub fn nested_sum(values: &[u64]) -> u64 {
    (values.iter().sum())
}

fn check(log: &RefCell<Vec<&'static str>>, text: &str) -> bool {
    log.borrow_mut().push("consume");
    text == "x "
}

pub fn shortcircuited(log: &RefCell<Vec<&'static str>>, flag: bool) -> bool {
    flag && check(log, " x ".to_owned().trim_end())
}

fn hold<T>(_: T) {}

pub fn pattern_scopes(number: i32) -> i32 {
    if let Some(number) = Some(String::from("if")) {
        hold(number);
    }
    match Some(String::from("match")) {
        Some(number) => hold(number),
        None => {}
    }
    let mut words = vec![String::from("while")];
    while let Some(number) = words.pop() {
        hold(number);
    }
    number
}

fn combine(log: &RefCell<Vec<&'static str>>, first: &str, second: &str) -> String {
    log.borrow_mut().push("consume");
    format!("{first}|{second}")
}

pub fn multiple_temporaries(log: &RefCell<Vec<&'static str>>) -> String {
    combine(log, " a ".to_owned().trim_end(), " b ".to_owned().trim_end())
}

pub fn local_methods() -> impl Iterator<Item = u8> {
    struct Local;
    impl Local {
        fn value() -> u8 {
            return 3;
        }
    }
    trait Extra {
        fn provided() -> u8 {
            return 4;
        }
    }
    impl Extra for Local {}
    core::iter::once(Local::value() + Local::provided())
}

struct Text<'a>(&'a RefCell<Vec<&'static str>>, String);

impl std::ops::Deref for Text<'_> {
    type Target = str;
    fn deref(&self) -> &str {
        self.0.borrow_mut().push("borrow");
        &self.1
    }
}

impl Drop for Text<'_> {
    fn drop(&mut self) {
        self.0.borrow_mut().push("drop");
    }
}

fn text<'a>(log: &'a RefCell<Vec<&'static str>>) -> Text<'a> {
    log.borrow_mut().push("make");
    Text(log, String::from(" x "))
}

pub fn matched(log: &RefCell<Vec<&'static str>>) -> bool {
    let result = match text(log).trim_end() {
        " x" => {
            log.borrow_mut().push("left");
            true
        }
        _ => {
            log.borrow_mut().push("right");
            false
        }
    };
    log.borrow_mut().push("after");
    result
}

pub fn match_string() -> bool {
    match String::from(" x ").trim_end() {
        " x" => true,
        _ => false,
    }
}

#[rustfmt::skip]
pub fn match_arm(log: &RefCell<Vec<&'static str>>, flag: bool) -> bool {
    match flag {
        true => check(log, " x ".to_owned().trim_end()),
        false => false,
    }
}
