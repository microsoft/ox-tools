// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use camino::{Utf8Path, Utf8PathBuf};
use cargo_gamma_lib::internals::cfg::CfgSet;
use cargo_gamma_lib::internals::discover::acceptance::{Declaration, declarations, excluded_files, identity};

#[test]
fn parent_aliases_exclude_descendants_but_a_live_alias_preserves_them() {
    let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/correctness/package_identities");
    let source = root.join("lib.rs");
    let helper = root.join("catalog/mod.rs");
    let child = root.join("catalog/artifact.rs");
    for path in [&source, &helper, &child] {
        assert!(path.is_file(), "the checked-in identity fixture exists: {path}");
    }
    let graph = |path: &Utf8Path, text: &str| {
        let ast = syn::parse_file(text).unwrap();
        (path.to_owned(), declarations(path, &ast, &CfgSet::parse("")))
    };
    let mut graph = vec![
        graph(&source, "#[cfg(test)] #[path = \"catalog/../catalog/mod.rs\"] mod tests;"),
        graph(&helper, "mod artifact;"),
        graph(&child, ""),
    ];
    let excluded = excluded_files(core::slice::from_ref(&source), &graph);
    assert!(excluded.contains(&identity(&helper)));
    assert!(excluded.contains(&identity(&child)));
    graph[0].1.push(Declaration {
        target: root.join("catalog/../catalog/mod.rs"),
        excluded: false,
    });
    assert!(excluded_files(&[root.join("catalog/../lib.rs")], &graph).is_empty());
}
