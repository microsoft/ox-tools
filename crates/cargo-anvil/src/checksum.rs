// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SHA-256 helpers.
//!
//! All checksums in `cargo-anvil` are stored as the string
//! `sha256:<lowercase hex>`. This module centralizes hashing so the prefix
//! and encoding are guaranteed consistent across the codebase.
//!
//! **Line endings are normalized to LF before hashing** so that a file
//! committed once produces the same checksum regardless of host OS or
//! Git's `core.autocrlf` setting. Without normalization, a region
//! authored on Windows (CRLF source under `autocrlf=true`) and validated
//! on Linux (LF after git normalizes the commit) would always diverge
//! and confuse the three-checksum decision algorithm.

use sha2::{Digest as _, Sha256};

/// Compute the canonical checksum string for a byte slice.
///
/// CRLF byte pairs are replaced with LF before hashing so the result
/// is invariant under line-ending conversion (see module docs).
#[must_use]
pub fn checksum_bytes(data: &[u8]) -> String {
    let normalized = normalize_line_endings(data);
    let digest = Sha256::digest(&normalized);
    let mut s = String::with_capacity(7 + digest.len() * 2);
    s.push_str("sha256:");
    for byte in digest {
        // 0..=15 always fits in the lookup; no panic possible.
        s.push(HEX[(byte >> 4) as usize] as char);
        s.push(HEX[(byte & 0x0f) as usize] as char);
    }
    s
}

/// Compute the canonical checksum string for a UTF-8 string.
///
/// CRLF sequences are replaced with LF before hashing so the result
/// is invariant under line-ending conversion (see module docs).
#[must_use]
pub fn checksum_str(data: &str) -> String {
    checksum_bytes(data.as_bytes())
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Replace every CRLF (`\r\n`) byte pair with a single LF (`\n`).
///
/// Bare CR bytes are left alone — they're vanishingly rare in
/// modern source trees, and treating them specially would risk
/// false-equating distinct content.
fn normalize_line_endings(data: &[u8]) -> Vec<u8> {
    // Pre-allocate optimistically; the output is the same length as
    // the input minus one byte per CRLF.
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        // Progress guard: each iteration must advance `i`. Catches
        // infinite-loop regressions (and infinite-loop mutants generated
        // by `cargo mutants` against the `+=` operators below) in debug
        // builds.
        let prev = i;
        if data[i] == b'\r' && data.get(i + 1) == Some(&b'\n') {
            out.push(b'\n');
            i += 2;
        } else {
            out.push(data[i]);
            i += 1;
        }
        debug_assert!(i > prev, "normalize_line_endings must make progress (prev={prev}, i={i})");
    }
    out
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn empty_input_has_known_digest() {
        // SHA-256 of empty input.
        assert_eq!(
            checksum_bytes(b""),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn abc_has_known_digest() {
        assert_eq!(
            checksum_str("abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn checksum_is_deterministic() {
        let a = checksum_str("hello world");
        let b = checksum_str("hello world");
        assert_eq!(a, b);
    }

    #[test]
    fn different_input_yields_different_checksum() {
        assert_ne!(checksum_str("a"), checksum_str("b"));
    }

    #[test]
    fn prefix_is_always_sha256() {
        assert!(checksum_str("anything").starts_with("sha256:"));
    }

    #[test]
    fn crlf_and_lf_yield_same_checksum() {
        // The core portability invariant: the same logical content
        // hashes identically regardless of line-ending convention.
        let lf = "line one\nline two\nline three\n";
        let crlf = "line one\r\nline two\r\nline three\r\n";
        assert_eq!(checksum_str(lf), checksum_str(crlf));
    }

    #[test]
    fn mixed_line_endings_normalize_consistently() {
        let mixed = "line one\r\nline two\nline three\r\n";
        let lf_only = "line one\nline two\nline three\n";
        assert_eq!(checksum_str(mixed), checksum_str(lf_only));
    }

    #[test]
    fn bare_cr_is_preserved() {
        // A lone CR (not followed by LF) is real content, not a line
        // ending convention. Leave it alone so distinct strings stay
        // distinct.
        let with_cr = "old\rmac\rstyle";
        let with_n = "old\nmac\nstyle";
        assert_ne!(checksum_str(with_cr), checksum_str(with_n));
    }

    #[test]
    fn normalize_keeps_lf_unchanged() {
        assert_eq!(normalize_line_endings(b"a\nb\nc"), b"a\nb\nc");
    }

    #[test]
    fn normalize_collapses_crlf() {
        assert_eq!(normalize_line_endings(b"a\r\nb\r\nc"), b"a\nb\nc");
    }

    #[test]
    fn normalize_preserves_bare_cr() {
        assert_eq!(normalize_line_endings(b"a\rb\rc"), b"a\rb\rc");
    }

    #[test]
    fn normalize_handles_trailing_cr() {
        // A CR at the very end has no LF to pair with, so it stays.
        assert_eq!(normalize_line_endings(b"abc\r"), b"abc\r");
    }
}
