// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

pub fn increment(remaining: &mut usize) -> usize {
    *remaining
}

#[rustfmt::skip]
pub fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map_or_else(String::new, |first| first.to_uppercase().collect::<String>() + chars.as_str())
}

pub fn slug(text: &str) -> String {
    let mut result = String::new();
    for character in text.chars() {
        result.extend(character.to_lowercase());
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
        || vec![directory.join(format!("{name}.rs")), directory.join(name).join("mod.rs")],
        |path| vec![directory.join(path)],
    )
}

pub fn spawned() -> io::Result<JoinHandle<()>> {
    let joined = std::thread::Builder::new().spawn(move || {})?;
    Ok(joined)
}
