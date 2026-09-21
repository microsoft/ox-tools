// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bounded reads of discovery inputs and optimization artifacts.

use std::io::{self, Read};

/// The largest discovery input or optimization artifact retained in memory.
///
/// 256 MiB leaves ample room for legitimate compiler artifacts while bounding one malformed or
/// hostile input below the memory budget of ordinary development hosts. Raising this value raises
/// peak memory per concurrently read artifact; lowering it makes larger valid hints and records
/// fail with their callers' oversized-input errors.
pub(super) const MAX_BYTES: u64 = 256 * 1024 * 1024;

const fn retention_limit() -> u64 {
    MAX_BYTES
}

/// Reads UTF-8 text without retaining more than [`MAX_BYTES`].
///
/// `None` means the input exceeded that bound.
///
/// # Errors
///
/// Returns the reader's I/O error, or [`io::ErrorKind::InvalidData`] when the retained bytes are
/// not valid UTF-8.
pub(super) fn text(input: impl Read) -> io::Result<Option<String>> {
    text_with_limit(input, retention_limit())
}

fn text_with_limit(mut input: impl Read, limit: u64) -> io::Result<Option<String>> {
    let mut bytes = Vec::new();
    let mut capped = input.by_ref().take(limit.saturating_add(1));

    let _read = capped.read_to_end(&mut bytes)?;
    let length = u64::try_from(bytes.len()).expect("usize cannot be wider than u64 on Rust's supported targets");

    if length > limit {
        return Ok(None);
    }

    String::from_utf8(bytes)
        .map(Some)
        .map_err(|cause| io::Error::new(io::ErrorKind::InvalidData, cause))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn the_discovery_limit_is_exactly_the_documented_budget() {
        assert_eq!(retention_limit(), 256 * 1024 * 1024);
    }

    #[test]
    fn exact_limit_is_retained() {
        assert_eq!(text_with_limit(&b"four"[..], 4).expect("read"), Some("four".to_owned()));
    }

    #[test]
    fn oversized_input_is_refused_after_one_extra_byte() {
        assert_eq!(text_with_limit(&b"oversized"[..], 4).expect("read"), None);
    }

    #[test]
    fn an_oversized_read_consumes_only_the_single_probe_byte() {
        let mut input = Cursor::new(b"abcdef");

        assert_eq!(text_with_limit(&mut input, 4).expect("read"), None);
        assert_eq!(input.position(), 5);
    }
}
