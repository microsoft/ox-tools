// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The stable, content-addressed identity of a mutant and its site.

use core::borrow::Borrow;
use core::fmt::{self, Display, Formatter};
use core::ops::Deref;

use blake3::Hasher;
use camino::Utf8Path;
use compact_str::CompactString;
use serde::{Deserialize, Serialize};

/// A mutant's compact, content-addressed identity.
///
/// A newtype over the text rather than an alias for it. This is the key that cached verdicts,
/// shard assignments and configured expectations are all stored under, so a mutator name, a
/// package name or a file path reaching one of those maps by mistake would attach one mutant's
/// history to another and nothing downstream could tell the difference. The distinct type prevents
/// those unrelated strings from being substituted accidentally.
///
/// The wrapper is transparent to Serde, so the identity's wire representation remains its
/// underlying string. It dereferences to `str`, so the identity reads as the text it is.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MutantId(CompactString);

impl MutantId {
    /// Wraps text that is already a rendered identity.
    ///
    /// The text is deliberately opaque rather than restricted to the currently emitted
    /// [`MUTANT_ID_HEX_LEN`]-character hexadecimal form. Tests and external callers may use
    /// readable identities, while every map keyed on this type only needs stable equality.
    #[inline]
    #[must_use]
    pub fn new(text: impl AsRef<str>) -> Self {
        Self(CompactString::new(text))
    }

    /// The identity as a string slice.
    #[inline]
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Whether the identity spilled out of the inline representation onto the heap.
    ///
    /// Exposed for the tests that keep an identity within the inline budget, which is the whole
    /// reason the underlying representation is a compact string rather than a `String`.
    #[must_use]
    pub fn is_heap_allocated(&self) -> bool {
        self.0.is_heap_allocated()
    }
}

impl Deref for MutantId {
    type Target = str;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.0.as_str()
    }
}

impl Borrow<str> for MutantId {
    #[inline]
    fn borrow(&self) -> &str {
        self.0.as_str()
    }
}

impl AsRef<str> for MutantId {
    #[inline]
    fn as_ref(&self) -> &str {
        self.0.as_str()
    }
}

impl Display for MutantId {
    #[expect(clippy::renamed_function_params, reason = "`f` is less clear than `formatter`")]
    #[inline]
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, formatter)
    }
}

impl From<CompactString> for MutantId {
    #[inline]
    fn from(value: CompactString) -> Self {
        Self(value)
    }
}

impl From<String> for MutantId {
    #[inline]
    fn from(value: String) -> Self {
        Self(value.into())
    }
}

impl From<&str> for MutantId {
    #[inline]
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}

impl From<MutantId> for CompactString {
    #[inline]
    fn from(value: MutantId) -> Self {
        value.0
    }
}

impl PartialEq<str> for MutantId {
    #[inline]
    fn eq(&self, other: &str) -> bool {
        self.0.as_str() == other
    }
}

impl PartialEq<&str> for MutantId {
    #[inline]
    fn eq(&self, other: &&str) -> bool {
        self.0.as_str() == *other
    }
}

impl PartialEq<String> for MutantId {
    #[inline]
    fn eq(&self, other: &String) -> bool {
        self.0.as_str() == other.as_str()
    }
}

impl PartialEq<MutantId> for str {
    #[inline]
    fn eq(&self, other: &MutantId) -> bool {
        self == other.0.as_str()
    }
}

impl PartialEq<MutantId> for &str {
    #[inline]
    fn eq(&self, other: &MutantId) -> bool {
        *self == other.0.as_str()
    }
}

impl PartialEq<MutantId> for String {
    #[inline]
    fn eq(&self, other: &MutantId) -> bool {
        self.as_str() == other.0.as_str()
    }
}

/// Which repeat of a mutation site, and which of that site's replacements, an identity names.
///
/// A named structure rather than two adjacent `u32` parameters. The two counts are
/// indistinguishable at a call site, and swapping them yields a different, entirely valid-looking
/// identity — which silently detaches every cached verdict, shard assignment and configured
/// expectation belonging to that mutant, with no error anywhere to say so.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SiteIndex {
    occurrence: u32,
    replacement_index: u32,
}

impl SiteIndex {
    /// Names which repeat of the site, and which of its replacements, this is.
    ///
    /// Both counts are zero-based and unbounded: a site can repeat as often as the enclosing item
    /// contains it, and a mutator may offer any number of replacements, so there is nothing here
    /// to reject — only two meanings to keep apart.
    #[inline]
    #[must_use]
    pub const fn new(occurrence: u32, replacement_index: u32) -> Self {
        Self {
            occurrence,
            replacement_index,
        }
    }

    /// Which repeat of an otherwise identical site within the enclosing item this is.
    #[inline]
    #[must_use]
    pub const fn occurrence(self) -> u32 {
        self.occurrence
    }

    /// Which of the mutator's replacements for that site this is.
    #[inline]
    #[must_use]
    pub const fn replacement_index(self) -> u32 {
        self.replacement_index
    }
}

/// The identity normalization contract.
///
/// Caller-supplied error replacement text, normalized stated-value expressions, and normalized
/// composed return replacements participate in those mutants' identities. Other registry-provided
/// replacements remain independent of replacement text.
///
/// The current contract builds an implementation's item-path scope from the self type's complete
/// token-aware source representation. Qualification, generic arguments, reference syntax, and
/// lexical token boundaries therefore remain distinct. The preceding contract used only the final
/// path segment and could fall back to source-order occurrence for otherwise distinct self types.
/// Source-order occurrence slots are reserved before confidence filtering, so withholding an
/// optimistic site cannot renumber the surviving sites.
/// Composed return replacements include their text so pruning unproven nested defaults cannot
/// assign an old replacement slot's verdict to a different value.
pub const MUTANT_ID_VERSION: u32 = 8;

/// Computes the stable, content-addressed identity of a mutant.
///
/// Deliberately *not* keyed on line and column. Inserting a line at the top of a file would
/// renumber every mutant below it, which would reshuffle every shard, orphan every cached verdict
/// and silently detach every configured expectation. The enclosing item path survives code motion
/// within a file, and the occurrence index handles two textually identical sites in one function.
/// Comment edits preserve identity when normalized site text stays unchanged. Inserting or removing
/// an interior comment between adjacent tokens can add or remove a normalized separator and change
/// the identity. Trait implementation
/// paths include both the self type and the implemented trait, so same-named methods from two
/// traits never depend on source order for that disambiguation.
#[must_use]
pub fn mutant_id(file: &Utf8Path, item_path: &str, mutator: &str, normalized_site_text: &str, site: SiteIndex) -> MutantId {
    mutant_id_with_discriminator(file, item_path, mutator, normalized_site_text, site, None)
}

/// Computes an identity with additional caller-supplied replacement content.
#[must_use]
pub(crate) fn mutant_id_with_discriminator(
    file: &Utf8Path,
    item_path: &str,
    mutator: &str,
    normalized_site_text: &str,
    site: SiteIndex,
    discriminator: Option<&str>,
) -> MutantId {
    let mut hasher = Hasher::new();

    // Length-prefix every field so that no two different field splits can hash alike.
    for field in [file.as_str(), item_path, mutator, normalized_site_text] {
        let _ = hasher.update(&(field.len() as u64).to_le_bytes());
        let _ = hasher.update(field.as_bytes());
    }

    let _ = hasher.update(&site.occurrence().to_le_bytes());
    let _ = hasher.update(&site.replacement_index().to_le_bytes());
    if let Some(discriminator) = discriminator {
        let _ = hasher.update(&(discriminator.len() as u64).to_le_bytes());
        let _ = hasher.update(discriminator.as_bytes());
    }

    let digest = hasher.finalize();
    let bytes = digest.as_bytes();
    let mut out = CompactString::with_capacity(MUTANT_ID_HEX_LEN);

    for byte in bytes.iter().take(MUTANT_ID_BYTES) {
        out.push(HEX[usize::from(byte >> 4)]);
        out.push(HEX[usize::from(byte & 0x0f)]);
    }

    MutantId(out)
}

/// How much of the digest a mutant identifier keeps.
///
/// Six bytes is forty-eight bits, which by the birthday bound gives roughly even odds of one
/// collision only once a run holds about `2^24` — sixteen million — distinct mutants. This
/// workspace generates on the order of ten thousand, so the margin is three orders of magnitude,
/// and at a hundred thousand mutants the chance of any collision is still under one in three
/// thousand. Widening this is cheap and safe; narrowing it is not, because the identifier is what
/// cached verdicts, shard assignments and configured expectations are keyed on, so a collision
/// silently attaches one mutant's history to another.
const MUTANT_ID_BYTES: usize = 6;

/// The width of a rendered mutant identifier: two lowercase hex characters per kept digest byte.
pub const MUTANT_ID_HEX_LEN: usize = MUTANT_ID_BYTES * 2;

/// The lowercase hex alphabet, indexed by nibble.
const HEX: [char; 16] = ['0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f'];

/// Identifies a mutation site for the purpose of counting repeats of it within one item.
///
/// A digest rather than the text itself, because the counter has to be kept for every distinct
/// site in a file and holding the item path and the normalized source of each one costs two owned
/// strings per mutant that nothing ever reads back. At 128 bits a collision between two real sites
/// is not a thing that happens.
///
/// The exact encoded value is stable for persisted `SiteIdentity` digests and reach-cluster records.
#[must_use]
pub fn site_key(item_path: &str, mutator: &str, normalized_site_text: &str) -> u128 {
    let mut hasher = Hasher::new();

    for field in [item_path, mutator, normalized_site_text] {
        let _ = hasher.update(&(field.len() as u64).to_le_bytes());
        let _ = hasher.update(field.as_bytes());
    }

    let digest = hasher.finalize();
    let key = digest
        .as_bytes()
        .first_chunk::<16>()
        .expect("a BLAKE3 digest is always 32 bytes, so its first 16 bytes always exist");

    u128::from_le_bytes(*key)
}

/// Normalizes the source text of a site for hashing.
///
/// Whitespace runs collapse to a single space and comments disappear; everything else is
/// preserved verbatim, including identifiers, literal values, literal suffixes and integer bases.
/// Adding or removing inter-token whitespace or punctuation (including rustfmt-added trailing
/// commas) can change the identity. This is not a token-level or general `cargo fmt` invariant.
/// Preserving those distinctions avoids silently reattaching stale verdicts to genuine edits.
#[must_use]
pub fn normalize_site_text(text: &str) -> CompactString {
    let mut out = CompactString::with_capacity(text.len());
    let comments = crate::parse::comment_spans(text);
    let mut comments = comments.iter().peekable();
    let mut offset = 0;
    let mut pending_space = false;

    while offset < text.len() {
        if let Some(comment) = comments.peek()
            && comment.start == offset
        {
            offset = comment.end;
            let _ = comments.next();
            pending_space = !out.is_empty();
            continue;
        }

        if let Some(end) = crate::parse::literal_end(text, offset) {
            if pending_space {
                out.push(' ');
                pending_space =
                // #[gamma::skip(assign_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
                false;
            }

            let literal = text
                .get(offset..end)
                .expect("literal_end returns an in-bounds UTF-8 boundary in this same text");
            out.push_str(literal);
            // #[gamma::skip(stmt.delete_assign, tag = "outofmem", reason = "Without advancing past a literal, this loop repeatedly appends the same literal until allocation fails.")]
            offset = end;
            continue;
        }

        let character = text
            .get(offset..)
            .and_then(|rest| rest.chars().next())
            .expect("the loop keeps the UTF-8 boundary below the text length");
        // #[gamma::skip(stmt.delete_assign, tag = "timeout", reason = "removing the character advance revisits the same nonliteral character indefinitely")]
        // #[gamma::skip(assign.add_to_mul, tag = "timeout", reason = "At offset zero, multiplying by the character width leaves offset zero and revisits the same character indefinitely.")]
        offset += character.len_utf8();

        if character.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }

        if pending_space {
            out.push(' ');
            pending_space =
            // #[gamma::skip(assign_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        }

        out.push(character);
    }

    out
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// The public `mutant_id` entry point is the discriminator-less case of
    /// `mutant_id_with_discriminator`, and must produce exactly the same identity as calling that
    /// function directly with `None`.
    #[test]
    fn mutant_id_delegates_to_the_discriminated_form_with_no_discriminator() {
        let file = Utf8Path::new("src/lib.rs");

        let direct = mutant_id(file, "subject::f", "arith.add_to_sub", "1 + 1", SiteIndex::default());
        let via_discriminator = mutant_id_with_discriminator(file, "subject::f", "arith.add_to_sub", "1 + 1", SiteIndex::default(), None);

        assert_eq!(direct, via_discriminator);
        assert_eq!(direct.len(), MUTANT_ID_HEX_LEN);
        assert!(direct.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    /// Changing any one field of the identity — including whether a discriminator is present —
    /// changes the identity, and an absent discriminator is not the same as an empty one.
    #[test]
    fn each_field_and_the_discriminator_affect_the_identity() {
        let file = Utf8Path::new("src/lib.rs");
        let baseline = mutant_id_with_discriminator(file, "subject::f", "arith.add_to_sub", "1 + 1", SiteIndex::default(), None);

        let other_file = mutant_id_with_discriminator(
            Utf8Path::new("src/other.rs"),
            "subject::f",
            "arith.add_to_sub",
            "1 + 1",
            SiteIndex::default(),
            None,
        );
        let other_occurrence = mutant_id_with_discriminator(file, "subject::f", "arith.add_to_sub", "1 + 1", SiteIndex::new(1, 0), None);
        let other_replacement_index =
            mutant_id_with_discriminator(file, "subject::f", "arith.add_to_sub", "1 + 1", SiteIndex::new(0, 1), None);
        let with_discriminator =
            mutant_id_with_discriminator(file, "subject::f", "arith.add_to_sub", "1 + 1", SiteIndex::default(), Some("panic"));
        let with_empty_discriminator =
            mutant_id_with_discriminator(file, "subject::f", "arith.add_to_sub", "1 + 1", SiteIndex::default(), Some(""));
        let with_other_discriminator = mutant_id_with_discriminator(
            file,
            "subject::f",
            "arith.add_to_sub",
            "1 + 1",
            SiteIndex::default(),
            Some("overflow"),
        );

        for other in [other_file, other_occurrence, other_replacement_index, with_discriminator.clone()] {
            assert_ne!(baseline, other, "a field change must not collide with the baseline identity");
        }

        assert_ne!(with_discriminator, with_other_discriminator);
        assert_ne!(baseline, with_empty_discriminator);
    }
}
