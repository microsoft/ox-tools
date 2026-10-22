// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub fn nested(value: Option<&u8>) -> Result<Option<impl Iterator<Item = u8>>, ()> {
    if ::gamma_rt::a(1u32) {
        (Ok(Some(core::iter::empty()))).map(|__gamma_value| __gamma_value.map(::gamma_rt::Either::L))
    } else {
        ({ Ok(Some(core::iter::once(value.copied().unwrap_or(7)))) }).map(|__gamma_value| __gamma_value.map(::gamma_rt::Either::R))
    }
}

pub fn shadow(value: Option<&u8>) -> Result<Option<impl Iterator<Item = u8>>, ()> {
    if ::gamma_rt::a(2u32) {
        ({
            let value: Option<&u8> = Default::default();
            Ok(Some(core::iter::once(value.copied().unwrap_or(7))))
        })
        .map(|__gamma_value| __gamma_value.map(::gamma_rt::Either::L))
    } else {
        ({ Ok(Some(core::iter::once(value.copied().unwrap_or(7)))) }).map(|__gamma_value| __gamma_value.map(::gamma_rt::Either::R))
    }
}

pub async fn asynchronous(value: u8) -> Result<impl Iterator<Item = u8>, ()> {
    if ::gamma_rt::a(3u32) {
        (Ok(core::iter::empty())).map(::gamma_rt::Either::L)
    } else {
        ({ Ok(core::iter::once(value)) }).map(::gamma_rt::Either::R)
    }
}

pub fn case(value: char) -> String {
    (if ::gamma_rt::a(4u32) {
        ::gamma_rt::Either::L(value.to_uppercase())
    } else {
        ::gamma_rt::Either::R(value.to_lowercase())
    })
    .collect()
}

pub fn borrowed_case(value: &char) -> String {
    (if ::gamma_rt::a(5u32) {
        ::gamma_rt::Either::L(value.to_uppercase())
    } else {
        ::gamma_rt::Either::R(value.to_lowercase())
    })
    .collect()
}
