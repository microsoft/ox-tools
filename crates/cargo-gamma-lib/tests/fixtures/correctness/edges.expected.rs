// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::cell::{Cell, RefCell};
use std::ops::RangeInclusive;

pub fn early(flag: bool) -> Result<Option<impl Iterator<Item = u8>>, ()> {
    if ::gamma_rt::a(1u32) {
        (Ok(Some(core::iter::empty()))).map(|__gamma_value| __gamma_value.map(::gamma_rt::Either::L))
    } else {
        ('__gamma_return_1: {
            {
                fn inner() -> u8 {
                    return 3;
                }
                let nested = || {
                    return inner();
                };
                if flag {
                    break '__gamma_return_1 Ok(Some(core::iter::once(nested())));
                }
                Ok(Some(core::iter::once(7)))
            }
        })
        .map(|__gamma_value| __gamma_value.map(::gamma_rt::Either::R))
    }
}

pub fn absent() -> Result<Option<impl Iterator<Item = u8>>, ()> {
    if ::gamma_rt::a(2u32) {
        Ok(None)
    } else {
        { Ok(Some(core::iter::once(7))) }
    }
}

pub fn direct_early(flag: bool) -> impl Iterator<Item = u8> {
    if ::gamma_rt::a(3u32) {
        ::gamma_rt::Either::L(core::iter::empty())
    } else {
        ::gamma_rt::Either::R('__gamma_return_3: {
            {
                if flag {
                    break '__gamma_return_3 core::iter::once(3);
                }
                core::iter::once(7)
            }
        })
    }
}

pub fn shadow_early(value: Option<&u8>) -> Result<impl Iterator<Item = u8>, ()> {
    if ::gamma_rt::a(4u32) {
        ('__gamma_return_4: {
            {
                let value: Option<&u8> = Default::default();
                if let Some(value) = value {
                    break '__gamma_return_4 Ok(core::iter::once(*value));
                }
                Ok(core::iter::once(7))
            }
        })
        .map(::gamma_rt::Either::L)
    } else {
        ('__gamma_return_4: {
            {
                if let Some(value) = value {
                    break '__gamma_return_4 Ok(core::iter::once(*value));
                }
                Ok(core::iter::once(7))
            }
        })
        .map(::gamma_rt::Either::R)
    }
}

pub fn initializer_shadow(value: usize, seen: &Cell<usize>) -> usize {
    let value = {
        seen.set((if ::gamma_rt::a(5u32) { (value) + 1 } else { value }));
        String::new()
    };
    value.len()
}

pub fn unknown_borrowed(values: &[(u8, bool)]) -> bool {
    values.iter().any(|(_, flag)| (if ::gamma_rt::a(6u32) { *(flag) } else { !flag }))
}

pub fn inferred_range() -> RangeInclusive<f32> {
    let start = 0.5;
    let end = 4.5;
    (if ::gamma_rt::a(7u32) {
        (start)..=((end) - 1.0)
    } else {
        start..=end
    })
}

pub fn associated_range(hits: u32) -> RangeInclusive<f64> {
    (if ::gamma_rt::a(8u32) {
        (0.0)..=((f64::from(hits)) - 1.0)
    } else {
        0.0..=f64::from(hits)
    })
}

pub fn nested_sum(values: &[u64]) -> u64 {
    (if ::gamma_rt::a(9u32) {
        ::gamma_rt::increment((values.iter().sum()))
    } else {
        (values.iter().sum())
    })
}

fn check(log: &RefCell<Vec<&'static str>>, text: &str) -> bool {
    log.borrow_mut().push("consume");
    text == "x "
}

pub fn shortcircuited(log: &RefCell<Vec<&'static str>>, flag: bool) -> bool {
    flag && (if ::gamma_rt::a(10u32) {
        check(log, " x ".to_owned().trim_start())
    } else {
        check(log, " x ".to_owned().trim_end())
    })
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
    (if ::gamma_rt::a(11u32) { (number) + 1 } else { number })
}

fn combine(log: &RefCell<Vec<&'static str>>, first: &str, second: &str) -> String {
    log.borrow_mut().push("consume");
    format!("{first}|{second}")
}

pub fn multiple_temporaries(log: &RefCell<Vec<&'static str>>) -> String {
    (if ::gamma_rt::a(12u32) {
        combine(log, " a ".to_owned().trim_start(), " b ".to_owned().trim_end())
    } else {
        (if ::gamma_rt::a(13u32) {
            combine(log, " a ".to_owned().trim_end(), " b ".to_owned().trim_start())
        } else {
            combine(log, " a ".to_owned().trim_end(), " b ".to_owned().trim_end())
        })
    })
}

pub fn local_methods() -> impl Iterator<Item = u8> {
    if ::gamma_rt::a(14u32) {
        ::gamma_rt::Either::L(core::iter::empty())
    } else {
        ::gamma_rt::Either::R({
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
        })
    }
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
    let result = (if ::gamma_rt::a(15u32) {
        match text(log).trim_start() {
            " x" => {
                log.borrow_mut().push("left");
                true
            }
            _ => {
                log.borrow_mut().push("right");
                false
            }
        }
    } else {
        match text(log).trim_end() {
            " x" => {
                log.borrow_mut().push("left");
                true
            }
            _ => {
                log.borrow_mut().push("right");
                false
            }
        }
    });
    log.borrow_mut().push("after");
    result
}

pub fn match_string() -> bool {
    (if ::gamma_rt::a(16u32) {
        match String::from(" x ").trim_start() {
            " x" => true,
            _ => false,
        }
    } else {
        match String::from(" x ").trim_end() {
            " x" => true,
            _ => false,
        }
    })
}

#[rustfmt::skip]
pub fn match_arm(log: &RefCell<Vec<&'static str>>, flag: bool) -> bool {
    match flag {
        true => (if ::gamma_rt::a(17u32) {
                check(log, " x ".to_owned().trim_start())
            } else {
                check(log, " x ".to_owned().trim_end())
            }),
        false => false,
    }
}
