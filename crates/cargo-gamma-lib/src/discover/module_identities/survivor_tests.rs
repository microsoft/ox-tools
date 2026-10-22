// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use crate::error::error;

#[test]
fn skipped_items_and_completed_inline_modules_do_not_hide_later_siblings() {
    let ast = syn::parse_file("fn unrelated() {} #[cfg(any())] mod disabled; mod inline { mod nested; } mod tail;").unwrap();
    let mut inspected = Vec::new();
    let edges = declarations_with(
        Utf8Path::new("source/lib.rs"),
        &ast,
        &CfgSet::parse(""),
        |path| {
            inspected.push(path.to_owned());
            path == Utf8Path::new("source/inline/nested.rs") || path == Utf8Path::new("source/tail.rs")
        },
        |path| Ok(path.to_owned()),
    )
    .unwrap();
    assert_eq!(
        inspected,
        [
            Utf8PathBuf::from("source/inline/nested.rs"),
            Utf8PathBuf::from("source/inline/nested/mod.rs"),
            Utf8PathBuf::from("source/tail.rs"),
            Utf8PathBuf::from("source/tail/mod.rs")
        ]
    );
    assert_eq!(
        edges.into_iter().map(|edge| (edge.target, edge.module)).collect::<Vec<_>>(),
        [
            ("source/inline/nested.rs".into(), Some(vec!["inline".into(), "nested".into()])),
            ("source/tail.rs".into(), Some(vec!["tail".into()]))
        ]
    );
}

#[test]
fn nested_module_resolution_errors_propagate_without_becoming_an_empty_graph() {
    let ast = syn::parse_file("mod inline { mod nested; }").unwrap();
    let failure = declarations_with(
        Utf8Path::new("source/lib.rs"),
        &ast,
        &CfgSet::unconditional(),
        |path| path == Utf8Path::new("source/inline/nested.rs"),
        |path| {
            assert_eq!(path, Utf8Path::new("source/inline/nested.rs"));
            Err(error!("nested module resolution failed"))
        },
    )
    .unwrap_err();
    assert_eq!(failure.message(), "nested module resolution failed");
}

#[test]
fn unresolved_modules_keep_the_preferred_candidate_and_invalidate_the_complete_layout() {
    let ast = syn::parse_file("mod missing; mod tail;").unwrap();
    let root = Utf8PathBuf::from("source/lib.rs");
    let edges = declarations_with(
        &root,
        &ast,
        &CfgSet::unconditional(),
        |path| path == Utf8Path::new("source/tail.rs"),
        |path| {
            assert_eq!(path, Utf8Path::new("source/tail.rs"));
            Ok(path.to_owned())
        },
    )
    .unwrap();
    assert_eq!(
        edges
            .iter()
            .map(|edge| (edge.target.clone(), edge.module.as_deref()))
            .collect::<Vec<_>>(),
        [
            (Utf8PathBuf::from("source/missing.rs"), None),
            (Utf8PathBuf::from("source/tail.rs"), Some(["tail".into()].as_slice()))
        ]
    );
    assert!(
        layouts(
            core::slice::from_ref(&root),
            &[(root.clone(), edges), ("source/tail.rs".into(), vec![])]
        )
        .is_empty()
    );
}

#[test]
fn different_files_cannot_own_the_same_logical_module() {
    let root = Utf8PathBuf::from("root.rs");
    let declarations = [
        (
            root.clone(),
            vec![
                Edge {
                    target: "a.rs".into(),
                    module: Some(vec!["alias".into()]),
                },
                Edge {
                    target: "b.rs".into(),
                    module: Some(vec!["alias".into()]),
                },
            ],
        ),
        ("a.rs".into(), vec![]),
        ("b.rs".into(), vec![]),
    ];
    assert!(layouts(core::slice::from_ref(&root), &declarations).is_empty());
    let mut disjoint = declarations;
    disjoint[0].1[0].module = Some(vec!["first".into()]);
    disjoint[0].1[1].module = Some(vec!["second".into()]);
    assert_eq!(
        layouts(core::slice::from_ref(&root), &disjoint),
        [
            ("a.rs".into(), "root.rs".into(), vec!["first".into()]),
            ("b.rs".into(), "root.rs".into(), vec!["second".into()]),
            ("root.rs".into(), "root.rs".into(), vec![])
        ]
    );
}

#[test]
fn a_repeated_consistent_edge_does_not_discard_another_queued_module() {
    let root = Utf8PathBuf::from("root.rs");
    let repeated = Edge {
        target: "shared.rs".into(),
        module: Some(vec!["shared".into()]),
    };
    assert_eq!(
        layouts(
            core::slice::from_ref(&root),
            &[
                (
                    root.clone(),
                    vec![
                        Edge {
                            target: "later.rs".into(),
                            module: Some(vec!["later".into()])
                        },
                        repeated.clone(),
                        repeated
                    ]
                ),
                ("shared.rs".into(), vec![]),
                ("later.rs".into(), vec![])
            ]
        ),
        [
            ("later.rs".into(), "root.rs".into(), vec!["later".into()]),
            ("root.rs".into(), "root.rs".into(), vec![]),
            ("shared.rs".into(), "root.rs".into(), vec!["shared".into()])
        ]
    );
}
