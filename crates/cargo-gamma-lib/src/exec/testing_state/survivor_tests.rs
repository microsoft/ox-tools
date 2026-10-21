// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use crate::fixtures;

#[test]
fn checkpoint_confidence_rejects_unknown_ids_before_changing_any_selected_mutant() {
    let mutant = fixtures::mutant();
    let id = mutant.id.clone();
    let mut plan = fixtures::plan(vec![mutant]);
    let ids = core::iter::once(id.clone()).collect();
    let confidences = [(id, "optimistic".into()), ("unknown".into(), "explicit".into())]
        .into_iter()
        .collect();
    let failure = restore_confidences(&mut plan, &confidences, &ids, Utf8Path::new("state")).unwrap_err();
    assert_eq!(
        failure.message(),
        "testing state at `state` contains confidence for an unknown mutant; start a new campaign"
    );
    assert_eq!(plan.mutants[0].confidence, Confidence::Proven);
}

#[test]
fn explicit_proven_confidence_overrides_a_preexisting_optimistic_classification() {
    let mut mutant = fixtures::mutant();
    mutant.confidence = Confidence::Optimistic;
    let id = mutant.id.clone();
    let mut plan = fixtures::plan(vec![mutant]);
    let ids = core::iter::once(id.clone()).collect();
    let confidences = core::iter::once((id, "proven".into())).collect();
    restore_confidences(&mut plan, &confidences, &ids, Utf8Path::new("state")).unwrap();
    assert_eq!(plan.mutants[0].confidence, Confidence::Proven);
}

#[test]
fn loaded_libraries_require_both_a_dynamic_extension_and_a_configured_parent() {
    let libraries = [Utf8PathBuf::from("first"), Utf8PathBuf::from("second")];
    for (path, expected) in [
        ("first/runtime.dll", true),
        ("second/runtime.so", true),
        ("second/runtime.dylib", true),
        ("first/runtime.txt", false),
        ("first/runtime", false),
        ("other/runtime.dll", false),
        ("runtime.so", false),
        ("first/nested/runtime.dll", false),
    ] {
        assert_eq!(is_loaded_library(Utf8Path::new(path), &libraries), expected, "{path}");
    }
    assert!(!is_loaded_library(Utf8Path::new("first/runtime.dll"), &[]));
}

#[test]
fn artifact_metadata_discards_executable_bits_for_nonexecutable_objects() {
    for kind in [ArtifactKind::Directory, ArtifactKind::Other] {
        let expected = ArtifactMetadata {
            kind: match kind {
                ArtifactKind::Directory => ArtifactKind::Directory,
                _ => ArtifactKind::Other,
            },
            executable: 0,
            link: None,
        };
        let actual = ArtifactMetadata::read_with(
            Utf8Path::new("prepared"),
            |follow| {
                assert!(!follow);
                Ok((
                    match kind {
                        ArtifactKind::Directory => ArtifactKind::Directory,
                        _ => ArtifactKind::Other,
                    },
                    0o111,
                ))
            },
            || panic!("a non-symlink object must not read a symlink target"),
        )
        .unwrap();
        assert_eq!(actual, expected);
    }
}
