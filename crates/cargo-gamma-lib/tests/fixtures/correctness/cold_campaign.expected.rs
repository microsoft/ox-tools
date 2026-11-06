// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::ops::ControlFlow;

pub fn block_compare(value: u8) -> bool {
    if (if ::gamma_rt::a(1u32) {
        ({ value }) != (7)
    } else {
        ({ value } == 7)
    }) {
        true
    } else {
        false
    }
}

pub fn callback(callback: Option<impl FnOnce()>, after: impl FnOnce()) {
    if ::gamma_rt::a(2u32) {
        {
            let callback = ::gamma_rt::default_like(&callback);
            if let Some(callback) = callback {
                callback();
            }
            after();
        }
    } else {
        {
            if let Some(callback) = callback {
                callback();
            }
            after();
        }
    }
}

pub fn assign(left: &mut u8, right: &mut u8, values: impl FnOnce() -> [u8; 2]) {
    (if ::gamma_rt::a(3u32) {
        [*right, *left] = values()
    } else {
        [*left, *right] = values()
    });
}

pub fn borrowed(make: impl FnOnce() -> String) -> bool {
    (if ::gamma_rt::a(4u32) {
        None == Some(std::ffi::OsStr::new("x"))
    } else {
        Some(std::ffi::OsStr::new(&make())) == Some(std::ffi::OsStr::new("x"))
    })
}

pub fn slices() -> impl Iterator<Item = &'static [u8]> {
    if ::gamma_rt::a(5u32) {
        ::gamma_rt::Either::L(core::iter::once::<&'static [u8]>(&[]))
    } else {
        ::gamma_rt::Either::R({ core::iter::once(b"ab".as_slice()) })
    }
}

pub fn characters(text: &str) -> String {
    let mut output = String::new();
    for (_, character) in text.chars().enumerate() {
        output.extend(
            (if ::gamma_rt::a(6u32) {
                ::gamma_rt::Either::L(character.to_lowercase())
            } else {
                ::gamma_rt::Either::R(character.to_uppercase())
            }),
        );
    }
    output
}

pub fn flow(value: ControlFlow<u8, u8>) -> ControlFlow<u8, u8> {
    ControlFlow::Continue(
        (if ::gamma_rt::a(7u32) {
            ::gamma_rt::panic_on_failure(value)?
        } else {
            value?
        }),
    )
}

pub fn logical(left: impl FnOnce() -> bool, right: impl FnOnce() -> bool) -> bool {
    if (if ::gamma_rt::a(8u32) {
        ({ left() }) || (right())
    } else {
        ({ left() } && right())
    }) {
        true
    } else {
        false
    }
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
    (if ::gamma_rt::a(9u32) { reader.skip(1) } else { reader.take(1) })
}

pub fn nested_assign(left: &mut u8, right: &mut u8, other: &mut u8, values: impl FnOnce() -> ([u8; 2], u8)) {
    (if ::gamma_rt::a(10u32) {
        ([*right, *left], *other) = values()
    } else {
        ([*left, *right], *other) = values()
    });
}
