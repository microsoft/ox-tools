// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub fn nested(value: Option<&u8>) -> Result<Option<impl Iterator<Item = u8>>, ()> {
    Ok(Some(core::iter::once(value.copied().unwrap_or(7))))
}

pub fn shadow(value: Option<&u8>) -> Result<Option<impl Iterator<Item = u8>>, ()> {
    Ok(Some(core::iter::once(value.copied().unwrap_or(7))))
}

pub async fn asynchronous(value: u8) -> Result<impl Iterator<Item = u8>, ()> {
    Ok(core::iter::once(value))
}

pub fn case(value: char) -> String {
    value.to_lowercase().collect()
}

pub fn borrowed_case(value: &char) -> String {
    value.to_lowercase().collect()
}
