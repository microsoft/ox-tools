// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::num::NonZero;
use std::fs;

#[cfg(feature = "system-tests")]
use camino::Utf8Path;
use camino::Utf8PathBuf;
use cargo_gamma_lib::internals::cfg::{CfgSet, Cfgs};
#[cfg(feature = "system-tests")]
use cargo_gamma_lib::internals::commands::SelectArgs;
#[cfg(feature = "system-tests")]
use cargo_gamma_lib::internals::discover::Survey;
use cargo_gamma_lib::internals::discover::TargetFile;
use cargo_gamma_lib::internals::discover::acceptance::{DeclarationOutcome, parse_declarations_of, parse_declarations_with_parser, scan};
#[cfg(feature = "system-tests")]
use cargo_gamma_lib::internals::exec::CargoOptions;
use cargo_gamma_lib::internals::ops::registry::Selection;

fn directory() -> Utf8PathBuf {
    Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("cargo-gamma-engine")
        .join("src")
        .join("ops")
        .join("collect")
        .join("collector")
        .join("resource")
}

fn digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[test]
#[cfg(feature = "system-tests")]
fn cached_native_scans_keep_ids_and_ordinals_and_reject_midcampaign_source_edits() {
    let directory = tempfile::Builder::new().prefix("declaration-cache-").tempdir_in(".").unwrap();
    let root = Utf8PathBuf::from_path_buf(directory.path().canonicalize().unwrap()).unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='declaration-cache-probe'\nversion='0.1.0'\nedition='2024'\n[workspace]\n",
    )
    .unwrap();
    let original = "pub fn value() -> i32 { 7 }\n";
    fs::write(root.join("src/lib.rs"), original).unwrap();
    let args = SelectArgs {
        dir: root.clone(),
        ..SelectArgs::default()
    };
    let cargo = CargoOptions::default();
    let survey = Survey::for_build(&args, None, &cargo).unwrap();
    let selection = Selection::parse("literal.int_decrement").unwrap();
    let first = survey.scan(None, &selection, &mut 0).unwrap();
    let second = survey.scan(None, &selection, &mut 0).unwrap();
    assert!(!first.mutants.is_empty());
    assert_eq!(
        first.mutants.iter().map(|mutant| (&mutant.id, mutant.ordinal)).collect::<Vec<_>>(),
        second.mutants.iter().map(|mutant| (&mutant.id, mutant.ordinal)).collect::<Vec<_>>()
    );
    assert_eq!(first.digests[Utf8Path::new("src/lib.rs")], digest(original.as_bytes()));
    fs::write(root.join("src/lib.rs"), "pub fn value() -> i32 { 8 }\n").unwrap();
    let failure = survey.scan(None, &selection, &mut 0).unwrap_err();
    assert!(failure.message().contains("src/lib.rs"), "{failure}");
    let fresh = Survey::for_build(&args, None, &cargo)
        .unwrap()
        .scan(None, &selection, &mut 0)
        .unwrap();
    assert!(!fresh.mutants.is_empty());
    assert_ne!(first.mutants[0].id, fresh.mutants[0].id);
}

#[test]
fn declaration_body_generations_are_separate_from_mutant_and_instrumentation_files() {
    let directory = directory();
    let target = TargetFile {
        path: "guarded.rs".into(),
        absolute: directory.join("acceptance").join("guarded.rs"),
        package: "fixture".into(),
        source: None,
    };
    let declaration = directory.join("fixtures").join("helpers.rs");
    let cfg = CfgSet::unconditional();
    let roots = [target.absolute.clone(), declaration.clone()];
    let scanned = scan(
        &[&target],
        &[(declaration.as_path(), &cfg, "fixture")],
        &roots,
        &Selection::parse("stmt.delete_assign").unwrap(),
        &Cfgs::unconditional(),
        &[],
    )
    .unwrap();
    assert!(!scanned.mutants.is_empty());
    assert!(scanned.mutants.iter().all(|mutant| mutant.file.as_ref() == target.path.as_path()));
    assert_eq!(scanned.sources.len(), 1);
    assert!(!scanned.sources.contains_key(&declaration));
    let expected = digest(&fs::read(&declaration).unwrap());
    assert_eq!(scanned.evidence_digests.get(&declaration), Some(&expected));
    assert!(!scanned.digests.contains_key(&declaration));
}

#[test]
fn declaration_generation_is_retained_from_the_parse_that_supplied_its_body() {
    let directory = directory();
    let path = directory.join("fixtures").join("helpers.rs");
    let cfg = CfgSet::unconditional();
    let generation = directory.join("acceptance").join("guarded.rs");
    let expected = digest(&fs::read(&generation).unwrap());
    let on_disk = digest(&fs::read(&path).unwrap());
    assert_ne!(expected, on_disk);
    let parsed = parse_declarations_with_parser(
        &[(path.as_path(), &cfg, "fixture")],
        || Ok(NonZero::<usize>::MIN),
        |requested, cfg| {
            let DeclarationOutcome::Declared(_, declarations, defaults, named, digest) = parse_declarations_of(&generation, cfg)? else {
                panic!("the immutable source-generation fixture must parse");
            };
            Ok(DeclarationOutcome::Declared(
                requested.to_owned(),
                declarations,
                defaults,
                named,
                digest,
            ))
        },
    )
    .unwrap();
    assert_eq!(parsed.digests.len(), 1);
    assert_eq!(parsed.digests.get(&path), Some(&expected));
    assert_ne!(parsed.digests.get(&path), Some(&on_disk));
    assert!(!parsed.digests.contains_key(&generation));
}
