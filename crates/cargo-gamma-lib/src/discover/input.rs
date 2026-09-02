// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bounded reads of discovery inputs and optimization artifacts.

use std::io::{self, Read};

/// The largest discovery input or optimization artifact retained in memory.
pub(super) const MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Reads UTF-8 text without retaining more than `limit` bytes.
///
/// `None` means the input exceeded the limit.
pub(super) fn text(input: impl Read) -> io::Result<Option<String>> {
    text_with_limit(input, MAX_BYTES)
}

fn text_with_limit(mut input: impl Read, limit: u64) -> io::Result<Option<String>> {
    let mut bytes = Vec::new();
    let mut capped = input.by_ref().take(limit.saturating_add(1));

    let _read = capped.read_to_end(&mut bytes)?;
    let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);

    if length > limit {
        return Ok(None);
    }

    String::from_utf8(bytes)
        .map(Some)
        .map_err(|cause| io::Error::new(io::ErrorKind::InvalidData, cause))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_limit_is_retained() {
        assert_eq!(text_with_limit(&b"four"[..], 4).expect("read"), Some("four".to_owned()));
    }

    #[test]
    fn oversized_input_is_refused_after_one_extra_byte() {
        assert_eq!(text_with_limit(&b"oversized"[..], 4).expect("read"), None);
    }
}
