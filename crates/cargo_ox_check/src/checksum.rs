// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SHA-256 helpers.
//!
//! All checksums in `cargo-ox-check` are stored as the string
//! `sha256:<lowercase hex>`. This module centralizes hashing so the prefix
//! and encoding are guaranteed consistent across the codebase.

use sha2::{Digest as _, Sha256};

/// Compute the canonical checksum string for a byte slice.
#[must_use]
pub fn checksum_bytes(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
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
#[must_use]
pub fn checksum_str(data: &str) -> String {
    checksum_bytes(data.as_bytes())
}

const HEX: &[u8; 16] = b"0123456789abcdef";

#[cfg(test)]
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
}
