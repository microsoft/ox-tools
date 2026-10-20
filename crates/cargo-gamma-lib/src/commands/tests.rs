// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::CanonicalMutantId;

#[test]
fn survivor_canonical_ids_require_exact_length_and_preserve_borrowed_and_owned_identity() {
    for id in ["", "a", "0123456789a", "0123456789abc", "ABCDEFABCDEF", "xyzxyzxyzxyz"] {
        assert!(CanonicalMutantId::parse(id).is_err(), "{id}");
    }
    for id in ["0123456789ab", "abcdefabcdef", "000000000000"] {
        let parsed = CanonicalMutantId::parse(id).unwrap();
        assert_eq!(parsed.as_str(), id);
        assert_eq!(parsed.as_inner().as_str(), id);
        assert_eq!(parsed.into_inner().as_str(), id);
    }
}

#[test]
fn invalid_mutant_ids_encode_terminal_controls() {
    let error = CanonicalMutantId::parse("bad\n\u{1b}[2K").expect_err("control-bearing ID must be rejected");

    assert_eq!(
        error.to_string(),
        "`bad\\n\\e[2K` has an invalid format; expected 12 lowercase hexadecimal characters"
    );
}
