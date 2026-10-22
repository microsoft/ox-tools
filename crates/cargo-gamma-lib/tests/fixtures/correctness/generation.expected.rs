// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

pub fn increment(remaining: &mut usize) -> usize {
    (if ::gamma_rt::a(1u32) { (*remaining) + 1 } else { *remaining })
}

#[rustfmt::skip]
pub fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first|
        (if ::gamma_rt::a(2u32) {
            ::gamma_rt::Either::L(first.to_lowercase())
        } else {
            ::gamma_rt::Either::R(first.to_uppercase())
        })
        .collect::<String>() + chars.as_str())
}

pub fn slug(text: &str) -> String {
    let mut result = String::new();
    for character in text.chars() {
        result.extend(
            (if ::gamma_rt::a(3u32) {
                ::gamma_rt::Either::L(character.to_uppercase())
            } else {
                ::gamma_rt::Either::R(character.to_lowercase())
            }),
        );
    }
    result
}

fn opened(bytes: &[u8]) -> io::Result<Cursor<&[u8]>> {
    Ok(Cursor::new(bytes))
}

pub fn reader(bytes: &[u8]) -> io::Result<Vec<u8>> {
    match opened(bytes) {
        Ok(mut reader) => {
            let mut result = Vec::new();
            (&mut reader).take(3).read_to_end(&mut result)?;
            Ok(result)
        }
        Err(error) => Err(error),
    }
}

#[rustfmt::skip]
pub fn vectors(directory: &Path, name: &str, attribute: Option<PathBuf>) -> Vec<PathBuf> {
    attribute.map_or_else(
        || (if ::gamma_rt::a(4u32) {
            vec![directory.join(name).join("mod.rs"), directory.join(format!("{name}.rs"))]
        } else {
            vec![directory.join(format!("{name}.rs")), directory.join(name).join("mod.rs")]
        }),
        |path| vec![directory.join(path)],
    )
}

pub fn spawned() -> io::Result<JoinHandle<()>> {
    let joined = (if ::gamma_rt::a(5u32) {
        ::gamma_rt::panic_on_failure(std::thread::Builder::new().spawn(move || {}))?
    } else {
        std::thread::Builder::new().spawn(move || {})?
    });
    Ok(joined)
}
