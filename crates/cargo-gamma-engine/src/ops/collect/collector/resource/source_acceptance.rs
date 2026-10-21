// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::path::{Path, PathBuf};

use syn::spanned::Spanned as _;

use crate::cfg::CfgSet;
use crate::ops::collect::{Defaults, ResourceDependency, collect_with};
use crate::ops::registry::Selection;
use crate::parse::SourceFile;
use crate::{HashMap, HashSet};

struct Package {
    defaults: Defaults,
    files: HashMap<Vec<String>, SourceFile>,
}

fn children(
    items: &[syn::Item],
    directory: &Path,
    module: &[String],
    sources: &HashMap<PathBuf, &str>,
    queue: &mut Vec<(PathBuf, Vec<String>)>,
) {
    for item in items {
        let syn::Item::Mod(item) = item else { continue };
        if item.attrs.iter().any(|attribute| attribute.path().is_ident("path")) {
            continue;
        }
        let mut child_module = module.to_vec();
        child_module.push(item.ident.to_string());
        let child_directory = directory.join(item.ident.to_string());
        if let Some((_, items)) = &item.content {
            children(items, &child_directory, &child_module, sources, queue);
        } else {
            let direct = directory.join(format!("{}.rs", item.ident));
            let path = if sources.contains_key(&direct) {
                direct
            } else {
                child_directory.join("mod.rs")
            };
            if sources.contains_key(&path) {
                queue.push((path, child_module));
            }
        }
    }
}

fn dependency(root: &str, alias: &str, package: &str, version: &str, features: &[&str]) -> ResourceDependency {
    ResourceDependency {
        crate_root: root.to_owned(),
        import_name: alias.to_owned(),
        package_id: format!("registry+https://github.com/rust-lang/crates.io-index#{package}@{version}"),
        package_name: package.to_owned(),
        version: version.to_owned(),
        source: "registry+https://github.com/rust-lang/crates.io-index".into(),
        features: features.iter().map(|feature| (*feature).to_owned()).collect(),
        registry_closure: true,
        library_root: None,
    }
}

fn package() -> Package {
    let directory = Path::new("resource-campaign");
    let sources = [
        ("lib.rs", include_str!("fixtures/campaign/lib.rs")),
        ("engine.rs", include_str!("fixtures/campaign/engine.rs")),
        ("baseline.rs", include_str!("fixtures/campaign/baseline.rs")),
        ("survey.rs", include_str!("fixtures/campaign/survey.rs")),
        ("shard.rs", include_str!("fixtures/campaign/shard.rs")),
        ("isolation.rs", include_str!("fixtures/campaign/isolation.rs")),
        ("glob.rs", include_str!("fixtures/campaign/glob.rs")),
        ("diff.rs", include_str!("fixtures/campaign/diff.rs")),
        ("commands/dashboard.rs", include_str!("fixtures/campaign/commands/dashboard.rs")),
        ("discover/hints.rs", include_str!("fixtures/campaign/discover/hints.rs")),
        ("report/text.rs", include_str!("fixtures/campaign/report/text.rs")),
    ]
    .into_iter()
    .map(|(path, source)| (directory.join(path), source))
    .collect::<HashMap<_, _>>();
    let root = directory.join("lib.rs").to_string_lossy().into_owned();
    let cfg = CfgSet::unconditional();
    let engine_root = directory.join("engine.rs").to_string_lossy().into_owned();
    let mut queue = vec![
        (directory.join("lib.rs"), Vec::new(), root.clone()),
        (directory.join("engine.rs"), Vec::new(), engine_root.clone()),
    ];
    let mut seen = HashSet::default();
    let mut files = HashMap::default();
    let mut defaults = Defaults::default();
    let mut layouts: Vec<(String, String, Vec<String>)> = Vec::new();
    while let Some((path, module, crate_root)) = queue.pop() {
        assert!(seen.len() < 1024, "the checked-in package has a bounded number of source files");
        if !seen.insert(path.clone()) {
            layouts.retain(|(source, _, _)| source != &*path.to_string_lossy());
            continue;
        }
        let source = sources
            .get(&path)
            .expect("every queued module is present in the embedded source graph");
        let label = path.to_string_lossy().into_owned();
        let file = SourceFile::parse(&label, (*source).to_owned()).unwrap();
        defaults.absorb(Defaults::of_source_in(&file.ast, &cfg, &label));
        layouts.push((label, crate_root.clone(), module.clone()));
        let directory = if matches!(path.file_name().and_then(|name| name.to_str()), Some("lib.rs" | "mod.rs")) {
            path.parent().unwrap().to_owned()
        } else {
            path.with_extension("")
        };
        let mut nested = Vec::new();
        children(&file.ast.items, &directory, &module, &sources, &mut nested);
        queue.extend(nested.into_iter().map(|(path, module)| (path, module, crate_root.clone())));
        if crate_root == root {
            let _old = files.insert(module, file);
        }
    }
    defaults.bind_sources(&layouts);
    let mut engine_dependency = dependency(&root, "cargo_gamma_engine", "cargo-gamma-engine", "0.3.0", &[]);
    engine_dependency.library_root = Some(engine_root.clone());
    engine_dependency.source.clear();
    engine_dependency.registry_closure = false;
    defaults.bind_resource_dependencies(&[
        engine_dependency,
        dependency(&root, "unicode_width", "unicode-width", "0.2.2", &[]),
        dependency(&root, "owo_colors", "owo-colors", "4.4.0", &[]),
        dependency(&root, "camino", "camino", "1.2.5", &[]),
        dependency(&root, "saphyr_parser", "saphyr-parser", "0.0.12", &[]),
        dependency(&root, "tick", "tick", "0.7.0", &[]),
        dependency(&root, "serde", "serde", "1.0.228", &["derive", "serde_derive"]),
        dependency(&root, "serde::serde_derive", "serde_derive", "1.0.228", &[]),
        dependency(&engine_root, "serde", "serde", "1.0.228", &["derive", "serde_derive"]),
        dependency(&engine_root, "serde::serde_derive", "serde_derive", "1.0.228", &[]),
    ]);
    Package { defaults, files }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one shared actual-package index validates the related resource acceptance sites"
)]
fn actual_padding_helper_is_resolved_through_the_labeled_source_graph() {
    let package = package();
    let file = &package.files[&vec!["commands".into(), "dashboard".into()]];
    let sources = package.defaults.resource_sources();
    let function = sources.function(file.path.as_ref(), &[], &["visible_width".into()]).unwrap();
    assert!(
        super::certify(sources, function).is_some(),
        "the complete immutable-string wrapper must be certified"
    );
    assert!(
        sources.closed_methods_for(file.path.as_ref(), &[]),
        "all visible extension traits must be accounted for"
    );
    let candidates = collect_with(
        file,
        &Selection::parse("stmt.delete_call,cond.negate").unwrap(),
        &CfgSet::unconditional(),
        &package.defaults,
    );
    let deletion = candidates
        .iter()
        .filter(|candidate| {
            &*candidate.item_path == "equalize_panels"
                && candidate.mutator == "stmt.delete_call"
                && file.slice(&candidate.span).starts_with("left.insert")
        })
        .count();
    assert_eq!(
        deletion, 0,
        "deleting standard vector padding progress leaves only a stable immutable helper call"
    );
    assert!(!candidates.iter().any(|candidate| &*candidate.item_path == "equalize_panels"
        && candidate.mutator == "stmt.delete_call"
        && file.slice(&candidate.span).starts_with("right.insert")));
    assert!(candidates.iter().any(|candidate| &*candidate.item_path == "equalize_panels"
        && candidate.mutator == "cond.negate"
        && file.slice(&candidate.span).contains("right.len() < target")));

    let file = &package.files[&vec!["report".into(), "text".into()]];
    let candidates = collect_with(
        file,
        &Selection::parse("stmt.delete_assign,loop.delete_break,loop.break_to_continue").unwrap(),
        &CfgSet::unconditional(),
        &package.defaults,
    );
    let zero_width = file.text.find("if next.width().unwrap_or(0) == 0 {").unwrap();
    let zero_progress = zero_width + file.text[zero_width..].find("at += next.len_utf8();").unwrap();
    let function = file
        .ast
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Fn(function) if function.sig.ident == "fit" => Some(function),
            _ => None,
        })
        .unwrap();
    let syn::Expr::While(outer) = function
        .block
        .stmts
        .iter()
        .find_map(|statement| match statement {
            syn::Stmt::Expr(expression @ syn::Expr::While(_), _) => Some(expression),
            _ => None,
        })
        .unwrap()
    else {
        unreachable!()
    };
    let inner = outer
        .body
        .stmts
        .iter()
        .find_map(|statement| match statement {
            syn::Stmt::Expr(expression @ syn::Expr::While(_), _) => Some(expression),
            _ => None,
        })
        .unwrap();
    let proof = super::LoopProof::new(
        inner,
        [
            ("text".into(), super::Binding::StrReference),
            ("at".into(), super::Binding::Integer),
        ]
        .into_iter()
        .collect(),
        true,
    )
    .unwrap()
    .stable_calls(
        std::iter::once((
            super::CHAR_WIDTH.into(),
            super::StableCall {
                parameters: vec![super::Binding::Char],
                output: super::Binding::Integer,
                char_option: false,
            },
        ))
        .collect(),
    );
    assert!(
        proof.omits(&file.text, &(zero_progress..zero_progress + "at += next.len_utf8();".len()), ""),
        "direct stationary Unicode path"
    );
    assert!(
        !candidates
            .iter()
            .any(|candidate| candidate.mutator == "stmt.delete_assign" && candidate.span.start == zero_progress),
        "the stationary zero-width suffix character must be proven through its exact sealed width API"
    );
    let joined_progress = file.text.find("at += next.len_utf8();").unwrap();
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.mutator == "stmt.delete_assign" && candidate.span.start == joined_progress),
        "deleting the joiner advance changes a finite cluster boundary"
    );
    let span = inner.span().byte_range();
    let visible_break = span.start + file.text[span].rfind("break;").unwrap();
    assert!(proof.omits(&file.text, &(visible_break..visible_break + "break;".len()), ""));
    assert!(proof.omits(&file.text, &(visible_break..visible_break + "break;".len()), "continue;"));
    assert!(
        !candidates.iter().any(|candidate| candidate.span.start == visible_break),
        "neither removal nor restart may repeatedly read the same visible suffix character"
    );

    let file = &package.files[&vec!["discover".into(), "hints".into()]];
    assert!(
        sources.closed_methods_for(file.path.as_ref(), &[]),
        "the grouping scope requires closed extension methods"
    );
    let candidates = collect_with(
        file,
        &Selection::parse("arith.add_to_mul").unwrap(),
        &CfgSet::unconditional(),
        &package.defaults,
    );
    let boundary = file.text.find("start + offset").unwrap();
    assert!(
        !candidates
            .iter()
            .any(|candidate| candidate.span.start == boundary && candidate.replacement.contains('*')),
        "a zero-times-offset boundary keeps all user callbacks unexecuted and repeatedly retains the empty group"
    );
}
