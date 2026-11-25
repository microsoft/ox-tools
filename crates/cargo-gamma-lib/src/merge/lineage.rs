// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::cmp::Ordering;
use core::fmt;
use core::ops::Deref;

/// Persisted opaque lineage text or a computed lowercase hexadecimal digest.
#[derive(Clone)]
pub(super) enum Lineage<'report> {
    Borrowed(&'report str),
    Inline([u8; 64]),
}

impl Deref for Lineage<'_> {
    type Target = str;

    fn deref(&self) -> &str {
        match self {
            Self::Borrowed(value) => value,
            Self::Inline(bytes) => core::str::from_utf8(bytes).expect("inline lineage bytes are copied from blake3's ASCII hex encoding"),
        }
    }
}

impl fmt::Display for Lineage<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self)
    }
}

impl fmt::Debug for Lineage<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl PartialEq for Lineage<'_> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for Lineage<'_> {}

impl PartialOrd for Lineage<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Lineage<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.deref().cmp(other)
    }
}

impl<'report> From<&'report str> for Lineage<'report> {
    fn from(value: &'report str) -> Self {
        Self::Borrowed(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_storage_variants_debug_as_the_decoded_text() {
        let text = "a".repeat(64);
        for lineage in [Lineage::Borrowed(&text), Lineage::Inline([b'a'; 64])] {
            assert_eq!(format!("{lineage:?}"), format!("{text:?}"));
        }
        let escaped = "line\n\"quoted\"";
        assert_eq!(format!("{:?}", Lineage::Borrowed(escaped)), format!("{escaped:?}"));
    }

    #[test]
    fn relational_ordering_is_total_across_borrowed_and_inline_lineages() {
        let text = "a".repeat(64);
        let borrowed = Lineage::from(text.as_str());
        let inline = Lineage::Inline([b'a'; 64]);
        let later = Lineage::Inline([b'b'; 64]);
        assert!(Lineage::from("") < inline);
        assert!(inline <= borrowed);
        assert!(borrowed >= inline);
        assert!(later > inline);
    }
}
