// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::ops::ControlFlow;

pub fn block_compare(value: u8) -> bool {
    if { value } == 7 { true } else { false }
}

pub fn callback(callback: Option<impl FnOnce()>, after: impl FnOnce()) {
    if let Some(callback) = callback {
        callback();
    }
    after();
}

pub fn assign(left: &mut u8, right: &mut u8, values: impl FnOnce() -> [u8; 2]) {
    [*left, *right] = values();
}

pub fn borrowed(make: impl FnOnce() -> String) -> bool {
    Some(std::ffi::OsStr::new(&make())) == Some(std::ffi::OsStr::new("x"))
}

pub fn slices() -> impl Iterator<Item = &'static [u8]> {
    core::iter::once(b"ab".as_slice())
}

pub fn characters(text: &str) -> String {
    let mut output = String::new();
    for (_, character) in text.chars().enumerate() {
        output.extend(character.to_uppercase());
    }
    output
}

pub fn flow(value: ControlFlow<u8, u8>) -> ControlFlow<u8, u8> {
    ControlFlow::Continue(value?)
}

pub fn logical(left: impl FnOnce() -> bool, right: impl FnOnce() -> bool) -> bool {
    if { left() } && right() { true } else { false }
}

pub struct Reader;

impl Reader {
    fn take(&self, _: usize) -> Result<&'static [u8], ()> {
        Ok(b"x")
    }
    fn skip(&self, _: usize) -> Result<&'static [u8], ()> {
        Ok(b"y")
    }
}

pub fn custom(reader: &Reader) -> Result<&'static [u8], ()> {
    reader.take(1)
}

pub fn nested_assign(left: &mut u8, right: &mut u8, other: &mut u8, values: impl FnOnce() -> ([u8; 2], u8)) {
    ([*left, *right], *other) = values();
}
