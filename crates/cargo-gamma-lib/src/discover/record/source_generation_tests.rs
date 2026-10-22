// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use crate::discover::TargetFile;
use crate::fixtures;

fn plan(sources: &[(&str, Option<&str>)], discovered: &[(&str, &str)]) -> Plan {
    let mut plan = fixtures::plan(Vec::new());
    plan.files = sources
        .iter()
        .map(|(path, source)| TargetFile {
            path: (*path).into(),
            absolute: plan.root.join(path),
            package: "subject".into(),
            source: source.map(Into::into),
        })
        .collect();
    plan.digests = discovered
        .iter()
        .map(|(path, text)| ((*path).into(), digest(text.as_bytes())))
        .collect();
    plan
}

fn snapshot(sources: &[(&str, &str)]) -> WorkspaceSnapshot {
    let mut sources = sources.to_vec();
    sources.sort_unstable_by_key(|(path, _)| *path);
    let files = sources
        .into_iter()
        .map(|(path, text)| {
            serde_json::json!({
                "path": path,
                "digest": digest(text.as_bytes()),
                "size": text.len()
            })
        })
        .collect::<Vec<_>>();
    serde_json::from_value(serde_json::json!({ "complete": false, "files": files }))
        .expect("the controlled source entries use the workspace snapshot schema")
}

#[test]
fn empty_discovery_has_no_source_generation_requirements() {
    let plan = fixtures::plan(Vec::new());
    assert!(RunRecord::contains_captured_sources(&plan, &WorkspaceSnapshot::default()));
}

#[test]
fn every_retained_file_must_match_even_when_another_file_is_valid() {
    let files = [("src/a.rs", "first"), ("src/b.rs", "second")];
    let inputs = snapshot(&files);
    let matching = plan(&[("src/a.rs", Some("first")), ("src/b.rs", Some("second"))], &files);
    assert!(RunRecord::contains_captured_sources(&matching, &inputs));
    for changed in ["src/a.rs", "src/b.rs"] {
        let mut stale = matching.clone();
        stale.files.iter_mut().find(|file| file.path == changed).unwrap().source = Some("changed".into());
        assert!(!RunRecord::contains_captured_sources(&stale, &inputs), "{changed}");
    }
}

#[test]
fn declaration_only_generations_are_required_in_addition_to_retained_sources() {
    let plan = plan(
        &[("src/lib.rs", Some("body"))],
        &[("src/lib.rs", "body"), ("src/declarations.rs", "declarations")],
    );
    for (sources, expected) in [
        (vec![("src/lib.rs", "body"), ("src/declarations.rs", "declarations")], true),
        (vec![("src/lib.rs", "body")], false),
        (vec![("src/lib.rs", "body"), ("src/declarations.rs", "changed")], false),
    ] {
        assert_eq!(RunRecord::contains_captured_sources(&plan, &snapshot(&sources)), expected);
    }
}

#[test]
fn retained_bytes_do_not_excuse_a_missing_or_different_discovery_generation() {
    let inputs = snapshot(&[("src/lib.rs", "body")]);
    for discovered in [vec![], vec![("src/lib.rs", "different")]] {
        let plan = plan(&[("src/lib.rs", Some("body"))], &discovered);
        assert!(!RunRecord::contains_captured_sources(&plan, &inputs));
    }
}

#[test]
fn bom_normalization_applies_to_discovery_but_not_the_raw_captured_input() {
    let plan = plan(&[("src/lib.rs", Some("\u{feff}body"))], &[("src/lib.rs", "body")]);
    assert!(RunRecord::contains_captured_sources(
        &plan,
        &snapshot(&[("src/lib.rs", "\u{feff}body")])
    ));
    assert!(!RunRecord::contains_captured_sources(&plan, &snapshot(&[("src/lib.rs", "body")])));
}

#[test]
fn ordinary_prefix_characters_are_not_treated_as_a_bom() {
    for source in ["xbody", "\0body"] {
        let inputs = snapshot(&[("src/lib.rs", source)]);
        let matching = plan(&[("src/lib.rs", Some(source))], &[("src/lib.rs", source)]);
        assert!(RunRecord::contains_captured_sources(&matching, &inputs), "{source:?}");
        let shortened = plan(&[("src/lib.rs", Some(source))], &[("src/lib.rs", "body")]);
        assert!(!RunRecord::contains_captured_sources(&shortened, &inputs), "{source:?}");
    }
}

#[test]
fn declaration_targets_without_retained_bytes_still_require_their_discovered_generation() {
    let plan = plan(&[("src/lib.rs", None)], &[("src/lib.rs", "declaration")]);
    assert!(RunRecord::contains_captured_sources(
        &plan,
        &snapshot(&[("src/lib.rs", "declaration")])
    ));
    assert!(!RunRecord::contains_captured_sources(
        &plan,
        &snapshot(&[("src/lib.rs", "different")])
    ));
}
