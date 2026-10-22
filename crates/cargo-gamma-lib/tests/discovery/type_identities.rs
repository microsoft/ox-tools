// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ffi::OsStr;

use camino::{Utf8Path, Utf8PathBuf};
use cargo_gamma_lib::internals::cfg::{CfgSet, Cfgs};
use cargo_gamma_lib::internals::discover::TargetFile;
use cargo_gamma_lib::internals::discover::acceptance::{Scan, physical, scan};
use cargo_gamma_lib::internals::ops::registry::Selection;
use rustc_hash::FxHashMap as HashMap;
use walkdir::WalkDir;

fn fixture_root() -> Utf8PathBuf {
    Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/correctness/package_identities")
}

fn package_scan(root: &Utf8Path, selected: &[&str], selectors: &str) -> Scan {
    let root = physical(root).expect("acceptance fixture roots must refer to readable checked-in source trees");
    let sources = WalkDir::new(&root)
        .into_iter()
        .map(|entry| entry.expect("acceptance fixtures must contain only readable directory entries"))
        .filter(|entry| entry.file_type().is_file() && entry.path().extension() == Some(OsStr::new("rs")))
        .map(|entry| {
            Utf8PathBuf::from_path_buf(entry.path().to_owned())
                .expect("checked-in fixture paths and their parent directories must be UTF-8")
        })
        .collect::<Vec<_>>();
    let files = selected
        .iter()
        .map(|name| TargetFile {
            absolute: root.join(name),
            path: (*name).into(),
            package: "fixture".to_owned(),
            source: None,
        })
        .collect::<Vec<_>>();
    let files = files.iter().collect::<Vec<_>>();
    let cfgs = Cfgs::new(&CfgSet::parse(""), &HashMap::default());
    let cfg = cfgs.for_package("fixture");
    let declarations = sources.iter().map(|path| (path.as_path(), cfg, "fixture")).collect::<Vec<_>>();
    scan(
        &files,
        &declarations,
        &[root.join("lib.rs")],
        &Selection::parse(selectors).expect("the acceptance test must name registered mutator selectors"),
        &cfgs,
        &[],
    )
    .expect("the checked-in acceptance source graph must parse successfully")
}

#[test]
fn checked_in_multifile_reexports_use_the_real_package_index_even_when_only_the_consumer_is_selected() {
    let scanned = package_scan(&fixture_root(), &["consumers.rs"], "fn_value,option.some_to_none");
    assert!(scanned.skipped.is_empty(), "{:?}", scanned.skipped);
    let has = |item: &str, selector: &str| {
        scanned
            .mutants
            .iter()
            .any(|mutant| mutant.item_path.as_ref() == item && mutant.mutator.as_ref() == selector)
    };
    for (item, selector) in [
        ("artifact", "fn_value.default"),
        ("aliased", "fn_value.default"),
        ("qualified", "fn_value.default"),
        ("relocated", "fn_value.default"),
        ("artifacts", "fn_value.one_element"),
        ("local_feature", "option.some_to_none"),
    ] {
        assert!(!has(item, selector), "{item}: {selector}: {:?}", scanned.mutants);
    }
    for (item, selector) in [
        ("artifacts", "fn_value.empty_collection"),
        ("alternate", "fn_value.default"),
        ("manual", "fn_value.default"),
        ("configured", "fn_value.default"),
        ("dependency_alias", "option.some_to_none"),
        ("custom_paired", "option.some_to_none"),
    ] {
        assert!(has(item, selector), "{item}: {selector}: {:?}", scanned.mutants);
    }
}

#[test]
fn artifact_reexports_retain_empty_vectors_but_reject_default_elements() {
    let scanned = package_scan(&fixture_root(), &["consumers.rs"], "fn_value.empty_collection,fn_value.one_element");
    assert!(scanned.skipped.is_empty(), "{:?}", scanned.skipped);
    for item in ["artifacts", "renamed_artifacts", "qualified_artifacts"] {
        assert!(
            scanned
                .mutants
                .iter()
                .any(|mutant| mutant.item_path.as_ref() == item && mutant.mutator.as_ref() == "fn_value.empty_collection"),
            "{item}: {:?}",
            scanned.mutants
        );
        assert!(
            !scanned
                .mutants
                .iter()
                .any(|mutant| mutant.item_path.as_ref() == item && mutant.mutator.as_ref() == "fn_value.one_element"),
            "{item}: {:?}",
            scanned.mutants
        );
    }
}

#[test]
fn actual_metadata_feature_option_is_a_local_enum_not_an_option_alias() {
    let root = fixture_root().parent().unwrap().join("metadata_features");
    let scanned = package_scan(&root, &["lib.rs"], "option.some_to_none");
    assert!(scanned.skipped.is_empty(), "{:?}", scanned.skipped);
    assert!(
        !scanned
            .mutants
            .iter()
            .any(|mutant| mutant.item_path.as_ref() == "metadata_feature_options" && mutant.mutator.as_ref() == "option.some_to_none")
    );
    assert!(
        scanned
            .mutants
            .iter()
            .any(|mutant| mutant.mutator.as_ref() == "option.some_to_none"),
        "standard Some sites in the same fixture remain mutable"
    );
}

#[test]
fn conditional_paths_and_multiply_mounted_files_remain_unknown() {
    for fixture in ["conditional_modules", "ambiguous_modules"] {
        let root = fixture_root().parent().unwrap().join(fixture);
        let scanned = package_scan(&root, &["lib.rs"], "fn_value.empty_collection,fn_value.one_element");
        for selector in ["fn_value.empty_collection", "fn_value.one_element"] {
            assert!(
                scanned
                    .mutants
                    .iter()
                    .any(|mutant| mutant.item_path.as_ref() == "values" && mutant.mutator.as_ref() == selector),
                "{fixture}: {selector}: {:?}",
                scanned.mutants
            );
        }
    }
}
