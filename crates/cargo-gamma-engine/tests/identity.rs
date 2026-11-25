// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use core::borrow::Borrow;

use camino::Utf8Path;
use cargo_gamma_engine::model::{MUTANT_ID_HEX_LEN, MutantId, SiteIndex, mutant_id, normalize_site_text, site_key};
use compact_str::CompactString;

#[test]
fn identity_normalization_preserves_separator_and_trailing_comma_changes() {
    let id = |source| {
        mutant_id(
            Utf8Path::new("src/lib.rs"),
            "foo",
            "arith.add_to_sub",
            &normalize_site_text(source),
            SiteIndex::default(),
        )
    };
    assert_eq!(id("a + b"), id("a  +\n b"));
    assert_eq!(id("a + b"), id("a /* comment */ + b"));
    assert_ne!(id("a+b"), id("a + b"));
    assert_ne!(id("f(x, y)"), id("f(\n    x,\n    y,\n)"));
}

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

#[test]
fn a_site_key_keeps_the_first_digest_bytes_in_little_endian_order() {
    // This encoding is persisted in SiteIdentity digests and reach-cluster records.
    assert_eq!(
        site_key("subject::f", "arith.add_to_sub", "1 + 1"),
        0x13ab_1481_a7bb_abfa_59fb_46dd_e659_e8a3
    );
}

#[test]
fn an_identity_behaves_as_the_text_it_wraps() {
    let id = MutantId::new("deadbeefcafe");
    let other = "feedfacebabe";
    assert!(<MutantId as PartialEq<str>>::eq(&id, "deadbeefcafe"));
    assert!(<MutantId as PartialEq<&str>>::eq(&id, &"deadbeefcafe"));
    assert!(<MutantId as PartialEq<String>>::eq(&id, &String::from("deadbeefcafe")));
    assert!(<str as PartialEq<MutantId>>::eq("deadbeefcafe", &id));
    assert!(<&str as PartialEq<MutantId>>::eq(&"deadbeefcafe", &id));
    assert!(<String as PartialEq<MutantId>>::eq(&String::from("deadbeefcafe"), &id));
    assert!(!<MutantId as PartialEq<str>>::eq(&id, other));
    assert!(!<MutantId as PartialEq<&str>>::eq(&id, &other));
    assert!(!<MutantId as PartialEq<String>>::eq(&id, &String::from(other)));
    assert!(!<str as PartialEq<MutantId>>::eq(other, &id));
    assert!(!<&str as PartialEq<MutantId>>::eq(&other, &id));
    assert!(!<String as PartialEq<MutantId>>::eq(&String::from(other), &id));
    assert_eq!(id.as_str(), "deadbeefcafe");
    assert_eq!(<MutantId as Borrow<str>>::borrow(&id), "deadbeefcafe");
    assert_eq!(<MutantId as AsRef<str>>::as_ref(&id), "deadbeefcafe");
    assert_eq!(id.to_string(), "deadbeefcafe");
    assert_eq!(id.len(), MUTANT_ID_HEX_LEN);
    assert!(!id.is_heap_allocated());
    assert_eq!(MutantId::from("deadbeefcafe"), id);
    assert_eq!(MutantId::from(String::from("deadbeefcafe")), id);
    assert_eq!(MutantId::from(CompactString::new("deadbeefcafe")), id);
    assert_eq!(CompactString::from(id.clone()), CompactString::new("deadbeefcafe"));
    let id: &str = &id;
    assert_eq!(id, "deadbeefcafe");
    assert!(MutantId::new("x".repeat(128)).is_heap_allocated());
}

#[test]
fn whitespace_runs_collapse_to_a_single_space() {
    assert_eq!(normalize_site_text("  let   x  =  1  ;  "), "let x = 1 ;");
    assert_eq!(normalize_site_text("\t\nfoo"), "foo");
}

#[test]
fn line_comments_are_dropped_and_replaced_by_a_single_space() {
    assert_eq!(normalize_site_text("let x = 1; // set x\nlet y = 2;"), "let x = 1; let y = 2;");
    assert_eq!(normalize_site_text("// leading\nlet x = 1;"), "let x = 1;");
    assert_eq!(normalize_site_text("let x = 1; // trailing"), "let x = 1;");
}

#[test]
fn block_comments_are_dropped_and_replaced_by_a_single_space() {
    assert_eq!(normalize_site_text("let x = /* the answer */ 42;"), "let x = 42;");
    assert_eq!(normalize_site_text("let s = /* comment */ \"value\";"), "let s = \"value\";");
}

#[test]
fn adjacent_block_comments_preserve_exact_token_spacing() {
    assert_eq!(normalize_site_text("left/* interior */right"), "left right");
    assert_eq!(normalize_site_text("left/* interior */\"right\""), "left \"right\"");
    assert_eq!(normalize_site_text("/* leading */right"), "right");
}

#[test]
fn literal_contents_are_preserved_verbatim() {
    assert_eq!(normalize_site_text("\"a  b // not a comment\""), "\"a  b // not a comment\"");
    assert_eq!(normalize_site_text("r\"raw /* text */\""), "r\"raw /* text */\"");
    assert_eq!(normalize_site_text("'x'"), "'x'");
}

#[test]
fn multibyte_characters_outside_literals_are_preserved() {
    assert_eq!(normalize_site_text("café + 1"), "café + 1");
}

#[test]
fn site_key_is_stable_and_sensitive_to_its_inputs() {
    let baseline = site_key("subject::f", "arith.add_to_sub", "1 + 1");
    assert_eq!(baseline, site_key("subject::f", "arith.add_to_sub", "1 + 1"));
    assert_ne!(baseline, site_key("subject::g", "arith.add_to_sub", "1 + 1"));
    assert_ne!(baseline, site_key("subject::f", "arith.add_to_mul", "1 + 1"));
    assert_ne!(baseline, site_key("subject::f", "arith.add_to_sub", "2 + 2"));
}
