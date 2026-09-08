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
/// Caller-supplied error replacement text participates in those mutants' identities. Every
/// identity whose replacement comes from the registry remains independent of replacement text.
///
/// The current contract builds an implementation's item-path scope from the self type's complete
/// token-aware source representation. Qualification, generic arguments, reference syntax, and
/// lexical token boundaries therefore remain distinct. The preceding contract used only the final
/// path segment and could fall back to source-order occurrence for otherwise distinct self types.
pub const MUTANT_ID_VERSION: u32 = 5;

/// Computes the stable, content-addressed identity of a mutant.
///
/// Deliberately *not* keyed on line and column. Inserting a line at the top of a file would
/// renumber every mutant below it, which would reshuffle every shard, orphan every cached verdict
/// and silently detach every configured expectation. The enclosing item path provides the same
/// disambiguation while surviving both reformatting and code motion within a file, and the
/// occurrence index handles two textually identical sites in one function. Trait implementation
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
#[must_use]
pub fn site_key(item_path: &str, mutator: &str, normalized_site_text: &str) -> u128 {
    let mut hasher = Hasher::new();

    for field in [item_path, mutator, normalized_site_text] {
        let _ = hasher.update(&(field.len() as u64).to_le_bytes());
        let _ = hasher.update(field.as_bytes());
    }

    let mut key = [0_u8; 16];

    key.copy_from_slice(hasher.finalize().as_bytes().get(..16).unwrap_or(&[0; 16]));
    u128::from_le_bytes(key)
}

/// Normalizes the source text of a site for hashing.
///
/// Whitespace runs collapse to a single space and comments disappear; everything else is
/// preserved verbatim, including identifiers, literal values, literal suffixes and integer bases.
/// Preserving too little would let a `cargo fmt` run reshuffle the whole population; preserving
/// too little meaning would let a genuine edit keep its old identity and silently reattach a stale
/// verdict to code whose behavior changed. When in doubt, this preserves.
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
                pending_space = false;
            }

            out.push_str(text.get(offset..end).unwrap_or(""));
            offset = end;
            continue;
        }

        let character = text
            .get(offset..)
            .and_then(|rest| rest.chars().next())
            .expect("the loop keeps the UTF-8 boundary below the text length");
        offset += character.len_utf8();

        if character.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }

        if pending_space {
            out.push(' ');
            pending_space = false;
        }

        out.push(character);
    }

    out
}

#[cfg(test)]
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

    /// Swapping the two counts produces a different identity, which is the whole reason they are
    /// named rather than adjacent: the wrong one is not an error anywhere, only a different mutant.
    #[test]
    fn the_two_site_counts_are_not_interchangeable() {
        let file = Utf8Path::new("src/lib.rs");
        let one_way = mutant_id(file, "subject::f", "arith.add_to_sub", "1 + 1", SiteIndex::new(2, 3));
        let other_way = mutant_id(file, "subject::f", "arith.add_to_sub", "1 + 1", SiteIndex::new(3, 2));

        assert_ne!(one_way, other_way);

        let site = SiteIndex::new(2, 3);

        assert_eq!(site.occurrence(), 2);
        assert_eq!(site.replacement_index(), 3);
        assert_eq!(SiteIndex::default(), SiteIndex::new(0, 0));
    }

    /// An identity reads, compares and serializes as the text it wraps, so the newtype costs
    /// nothing at a call site and nothing on the wire.
    #[test]
    fn an_identity_behaves_as_the_text_it_wraps() {
        let id = MutantId::new("deadbeefcafe");

        assert!(<MutantId as PartialEq<str>>::eq(&id, "deadbeefcafe"));
        assert!(<MutantId as PartialEq<&str>>::eq(&id, &"deadbeefcafe"));
        assert!(<MutantId as PartialEq<String>>::eq(&id, &String::from("deadbeefcafe")));
        assert!(<str as PartialEq<MutantId>>::eq("deadbeefcafe", &id));
        assert!(<&str as PartialEq<MutantId>>::eq(&"deadbeefcafe", &id));
        assert!(<String as PartialEq<MutantId>>::eq(&String::from("deadbeefcafe"), &id));
        assert_eq!(id.as_str(), "deadbeefcafe");
        assert_eq!(id.to_string(), "deadbeefcafe");
        assert_eq!(id.len(), MUTANT_ID_HEX_LEN);
        assert!(!id.is_heap_allocated());
        assert_eq!(MutantId::from("deadbeefcafe"), id);
        assert_eq!(MutantId::from(String::from("deadbeefcafe")), id);
        assert_eq!(CompactString::from(id.clone()), CompactString::new("deadbeefcafe"));

        // Borrowed as `str`, so a map keyed on identities can still be probed with plain text.
        let borrowed: &str = &id;

        assert_eq!(borrowed, "deadbeefcafe");
    }

    /// Whitespace runs collapse to one space, and a run entirely at the start of the text
    /// disappears rather than producing a leading space.
    #[test]
    fn whitespace_runs_collapse_to_a_single_space() {
        assert_eq!(normalize_site_text("  let   x  =  1  ;  "), "let x = 1 ;");
        assert_eq!(normalize_site_text("\t\nfoo"), "foo");
    }

    /// A line comment disappears entirely, and the code around it is joined by exactly one space
    /// whether the comment sits between two tokens or trails the last one.
    #[test]
    fn line_comments_are_dropped_and_replaced_by_a_single_space() {
        assert_eq!(normalize_site_text("let x = 1; // set x\nlet y = 2;"), "let x = 1; let y = 2;");
        assert_eq!(normalize_site_text("// leading\nlet x = 1;"), "let x = 1;");
        assert_eq!(normalize_site_text("let x = 1; // trailing"), "let x = 1;");
    }

    /// A block comment disappears the same way a line comment does, including one that sits
    /// directly before a string literal so the literal branch is reached with a pending space.
    #[test]
    fn block_comments_are_dropped_and_replaced_by_a_single_space() {
        assert_eq!(normalize_site_text("let x = /* the answer */ 42;"), "let x = 42;");
        assert_eq!(normalize_site_text("let s = /* comment */ \"value\";"), "let s = \"value\";");
    }

    /// String, raw-string and char literals are copied verbatim, including whitespace and
    /// comment-shaped text inside them, rather than being normalized like ordinary code.
    #[test]
    fn literal_contents_are_preserved_verbatim() {
        assert_eq!(normalize_site_text("\"a  b // not a comment\""), "\"a  b // not a comment\"");
        assert_eq!(normalize_site_text("r\"raw /* text */\""), "r\"raw /* text */\"");
        assert_eq!(normalize_site_text("'x'"), "'x'");
    }

    /// Non-ASCII identifier characters outside any literal are copied one character at a time
    /// through the byte-length-aware fallback path, rather than being mistaken for a literal or a
    /// single-byte character.
    #[test]
    fn multibyte_characters_outside_literals_are_preserved() {
        assert_eq!(normalize_site_text("café + 1"), "café + 1");
    }

    /// A site key changes whenever any of its inputs changes, and is stable for identical inputs.
    #[test]
    fn site_key_is_stable_and_sensitive_to_its_inputs() {
        let baseline = site_key("subject::f", "arith.add_to_sub", "1 + 1");

        assert_eq!(baseline, site_key("subject::f", "arith.add_to_sub", "1 + 1"));
        assert_ne!(baseline, site_key("subject::g", "arith.add_to_sub", "1 + 1"));
        assert_ne!(baseline, site_key("subject::f", "arith.add_to_mul", "1 + 1"));
        assert_ne!(baseline, site_key("subject::f", "arith.add_to_sub", "2 + 2"));
    }
}
