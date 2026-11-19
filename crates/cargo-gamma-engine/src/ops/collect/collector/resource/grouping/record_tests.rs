// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fmt::Write as _;

use syn::spanned::Spanned as _;

use crate::cfg::CfgSet;
use crate::ops::collect::{Defaults, ResourceDependency, collect_with};
use crate::ops::registry::Selection;
use crate::parse::SourceFile;

const HINTS: &str = include_str!("../fixtures/campaign/discover/hints.rs");

fn indexed(source: &str, dependency: bool) -> (SourceFile, Defaults) {
    let cfg = CfgSet::unconditional();
    let root = SourceFile::parse("grouping-proof\\lib.rs", "mod discover { pub mod hints; }".into()).unwrap();
    let file = SourceFile::parse("grouping-proof\\discover\\hints.rs", source.into()).unwrap();
    let mut defaults = Defaults::of_source_in(&root.ast, &cfg, root.path.as_ref());
    defaults.absorb(Defaults::of_source_in(&file.ast, &cfg, file.path.as_ref()));
    defaults.bind_sources(&[
        (root.path.to_string(), root.path.to_string(), Vec::new()),
        (
            file.path.to_string(),
            root.path.to_string(),
            vec!["discover".into(), "hints".into()],
        ),
    ]);
    if dependency {
        defaults.bind_resource_dependencies(&[ResourceDependency {
            crate_root: root.path.to_string(),
            import_name: "camino".into(),
            package_id: "registry+https://github.com/rust-lang/crates.io-index#camino@1.2.5".into(),
            package_name: "camino".into(),
            version: "1.2.5".into(),
            source: "registry+https://github.com/rust-lang/crates.io-index".into(),
            features: Vec::new(),
            registry_closure: true,
            library_root: None,
        }]);
    }
    (file, defaults)
}

fn progress_retained(source: &str, dependency: bool) -> bool {
    let (file, defaults) = indexed(source, dependency);
    let candidates = collect_with(
        &file,
        &Selection::parse("stmt.delete_assign").unwrap(),
        &CfgSet::unconditional(),
        &defaults,
    );
    let start = file.text.find("start = end;").unwrap();
    candidates.iter().any(|candidate| candidate.span.start == start)
}

fn record_loop(file: &syn::File) -> &syn::ExprWhile {
    file.items
        .iter()
        .find_map(|item| match item {
            syn::Item::Impl(implementation) => implementation.items.iter().find_map(|item| match item {
                syn::ImplItem::Fn(function) => function.block.stmts.iter().find_map(|statement| match statement {
                    syn::Stmt::Expr(syn::Expr::While(loop_), _) => Some(loop_),
                    _ => None,
                }),
                _ => None,
            }),
            _ => None,
        })
        .unwrap()
}

fn record_certificate(source: &str, dependency: bool) -> Option<String> {
    let (file, defaults) = indexed(source, dependency);
    certificate_with_evidence(&file, &defaults)
}

fn certificate_with_evidence(file: &SourceFile, defaults: &Defaults) -> Option<String> {
    super::records::prove(
        &file.ast,
        record_loop(&file.ast),
        defaults.resource_sources(),
        file.path.as_ref(),
        &[],
        &crate::HashSet::from_iter(["files".into()]),
    )
}

fn record_effects_admitted(source: &str) -> Option<bool> {
    let (file, defaults) = indexed(source, false);
    super::Grouping::new(
        &file.ast,
        &syn::Expr::While(record_loop(&file.ast).clone()),
        &crate::HashMap::from_iter([("hints.mutants".into(), super::Binding::Vec)]),
        defaults.resource_sources(),
        file.path.as_ref(),
        &[],
    )
    .map(|proof| proof.record_body.is_some())
}

#[test]
fn record_grouping_keeps_qualified_input_vectors_outside_bare_vector_effect_replay() {
    let check = |source: &str| {
        let (file, defaults) = indexed(source, true);
        let expression = syn::Expr::While(record_loop(&file.ast).clone());
        let bindings = crate::HashMap::from_iter([("hints.mutants".into(), super::Binding::Vec)]);
        super::Grouping::new(
            &file.ast,
            &expression,
            &bindings,
            defaults.resource_sources(),
            file.path.as_ref(),
            &[],
        )
        .map(|proof| proof.record_body.is_some())
    };
    assert_eq!(check(HINTS), Some(true));
    let qualified = HINTS.replace("mutants: Vec<Hint>,", "mutants: std::vec::Vec<Hint>,");
    assert!(progress_retained(&qualified, true));
    assert_eq!(check(&qualified), Some(false));
}

#[test]
fn record_grouping_reads_the_element_not_the_explicit_input_allocator() {
    let allocated = HINTS.replace("mutants: Vec<Hint>,", "mutants: Vec<Hint, std::alloc::Global>,");
    assert!(record_certificate(HINTS, true).is_some());
    assert!(!progress_retained(&allocated, true));
    assert!(record_certificate(&allocated, true).is_some());
}

#[test]
fn record_grouping_requires_independent_primitive_identity_evidence() {
    let source = record_budget_source("u32").replace("file: String,", "file: u32,");
    let (file, defaults) = indexed(&source, false);
    assert!(certificate_with_evidence(&file, &defaults).is_some());
    let (_, ambiguous) = indexed(&format!("{source} struct Vec;"), false);
    assert!(!ambiguous.resource_sources().primitive_scope_for(file.path.as_ref(), &[]));
    assert!(ambiguous.resource_sources().local_record_for(file.path.as_ref(), &[], "Hint"));
    assert!(super::super::fixed::numeric_names(
        &file.ast,
        &record_loop(&file.ast).span().byte_range()
    ));
    assert_eq!(certificate_with_evidence(&file, &ambiguous), None);
}

#[test]
fn record_grouping_requires_output_declarations_in_the_supplied_namespace_evidence() {
    let (file, defaults) = indexed(HINTS, true);
    assert!(certificate_with_evidence(&file, &defaults).is_some());
    let mut partial = file.ast.clone();
    partial
        .items
        .retain(|item| !matches!(item, syn::Item::Struct(record) if record.ident == "FileHints"));
    let (_, evidence) = indexed(&super::text(&partial), true);
    assert!(evidence.resource_sources().primitive_scope_for(file.path.as_ref(), &[]));
    assert!(!evidence.resource_sources().local_record_for(file.path.as_ref(), &[], "FileHints"));
    assert_eq!(certificate_with_evidence(&file, &evidence), None);
}

#[test]
fn record_grouping_rejects_a_valid_pool_initializer_after_retained_output_growth() {
    assert!(record_certificate(HINTS, true).is_some());
    let source = HINTS.replace(
        "start = end;",
        "let later: Vec<Killer> = group.iter().filter_map(|hint| hint.killer.clone()).collect(); start = end;",
    );
    assert_eq!(record_certificate(&source, true), None);
}

#[test]
fn record_grouping_rejects_shadowed_pool_recreation_even_with_consistent_uses() {
    assert!(record_certificate(HINTS, true).is_some());
    let source = HINTS.replace(
        "killers.sort_by(",
        "let mut killers: Vec<Killer> = group.iter().filter_map(|hint| hint.killer.clone()).collect(); killers.sort_by(",
    );
    assert_eq!(record_certificate(&source, true), None);
}

#[test]
fn record_grouping_requires_a_borrowed_input_even_when_an_owned_conversion_is_well_formed() {
    assert!(record_certificate(HINTS, true).is_some());
    let owned = HINTS
        .replace("From<&Hints>", "From<Hints>")
        .replace("fn from(hints: &Hints)", "fn from(hints: Hints)");
    assert_ne!(owned, HINTS);
    assert_eq!(record_certificate(&owned, true), None);
}

#[test]
fn record_grouping_cannot_treat_a_different_nullary_pool_operation_as_deduplication() {
    assert!(record_certificate(HINTS, true).is_some());
    assert_eq!(
        record_certificate(&HINTS.replace("killers.dedup();", "killers.clear();"), true),
        None
    );
}

#[test]
fn record_grouping_keeps_vector_alias_annotations_outside_its_closed_vector_grammar() {
    assert!(record_certificate(HINTS, true).is_some());
    let rejected = |source: &str| {
        let (file, defaults) = indexed(source, true);
        let loop_ = record_loop(&file.ast);
        assert!(defaults.resource_sources().primitive_scope_for(file.path.as_ref(), &[]));
        assert!(super::super::fixed::numeric_names(&file.ast, &loop_.span().byte_range()));
        assert_eq!(
            super::records::prove(
                &file.ast,
                loop_,
                defaults.resource_sources(),
                file.path.as_ref(),
                &[],
                &crate::HashSet::from_iter(["files".into()]),
            ),
            None
        );
    };
    let local = format!(
        "{} type Rows = Vec<GroupedHint>;",
        HINTS.replace("let mutants = group", "let mutants: Rows = group")
    );
    rejected(&local);
    let retained = format!(
        "{} type Pool = Vec<Killer>;",
        HINTS.replace("killers: Vec<Killer>,", "killers: Pool,")
    );
    rejected(&retained);
}

fn record_budget_source(path_type: &str) -> String {
    // Alias traversal reaches the effect budget without exhausting the earlier syntax-work gate.
    let fields = "v0: S8, v1: S8, v2: S7,";
    let mapped = "v0: hint.v0, v1: hint.v1, v2: hint.v2,";
    let mut scalars = "type S0 = u32;".to_owned();
    for index in 1..9 {
        write!(scalars, "type S{index} = S{};", index - 1).unwrap();
    }
    let mut vectors = String::new();
    let mut locals = String::new();
    for index in 0..4 {
        write!(vectors, "rows{index}: Vec<GroupedHint>,").unwrap();
        write!(
            locals,
            "let rows{index} = group.iter().map(|hint| GroupedHint {{ {mapped} }}).collect();"
        )
        .unwrap();
    }
    format!(
        "{scalars} type P0 = String; type P1 = P0; type P2 = P1;
        struct Hint {{ file: String, {fields} }}
        struct Hints {{ mutants: Vec<Hint> }}
        struct GroupedHint {{ {fields} }}
        struct FileHints {{ path: {path_type}, {vectors} }}
        struct Files {{ files: Vec<FileHints> }}
        impl From<&Hints> for Files {{
            fn from(hints: &Hints) -> Self {{
                let mut files = Vec::new();
                let mut start = 0;
                while start < hints.mutants.len() {{
                    let path = hints.mutants[start].file.clone();
                    let end = hints.mutants[start..].iter().position(|hint| hint.file != path)
                        .map_or(hints.mutants.len(), |offset| start + offset);
                    let group = &hints.mutants[start..end];
                    {locals}
                    files.push(FileHints {{ path, rows0, rows1, rows2, rows3 }});
                    start = end;
                }}
                Self {{ files }}
            }}
        }}"
    )
}

#[test]
fn record_grouping_accepts_the_last_complete_effect_budget_and_rejects_one_more_alias_step() {
    let last_complete = record_budget_source("P1");
    let (file, _) = indexed(&last_complete, false);
    let mut expected = record_loop(&file.ast).body.clone();
    let _progress = expected.stmts.pop().unwrap();
    assert_eq!(record_certificate(&last_complete, false), Some(super::text(&expected)));
    assert_eq!(record_certificate(&record_budget_source("P2"), false), None);
}

#[test]
fn record_grouping_requires_fuel_for_the_last_retained_scalar_type() {
    let source = |ty: &str| {
        record_budget_source(ty).replace(
            "FileHints { path, rows0, rows1, rows2, rows3 }",
            "FileHints { rows0, rows1, rows2, rows3, path }",
        )
    };
    assert!(record_certificate(&source("P1"), false).is_some());
    assert_eq!(record_certificate(&source("P2"), false), None);
}

#[test]
fn record_grouping_requires_fuel_for_the_last_copied_input_projection() {
    let source = record_budget_source("String").replace(
        "FileHints { path, rows0, rows1, rows2, rows3 }",
        "FileHints { rows0, rows1, rows2, rows3, path }",
    );
    assert!(record_certificate(&source.replace("file: String,", "file: P0,"), false).is_some());
    assert_eq!(record_certificate(&source.replace("file: String,", "file: P1,"), false), None);
}

#[test]
fn record_grouping_requires_fuel_for_a_mapped_output_member_after_validating_its_schema() {
    let source = record_budget_source("String");
    assert!(record_certificate(&source, false).is_some());
    let mut fields = String::new();
    for index in 0..14 {
        write!(fields, "warm{index}: S10,").unwrap();
    }
    fields.push_str("warm14: S4,");
    let source = format!(
        "type S9 = S8; type S10 = S9; {}",
        source.replace("struct Hint {", &format!("struct Hint {{ {fields}"))
    );
    assert_eq!(record_certificate(&source, false), None);
}

#[test]
fn record_grouping_stops_before_a_retention_statement_when_prior_mappers_spend_all_fuel() {
    assert!(record_certificate(&record_budget_source("P1"), false).is_some());
    let source = format!(
        "type S9 = S8; type P3 = P2; {}",
        record_budget_source("P1")
            .replace("v0: S8, v1: S8, v2: S7,", "v0: S8, v1: S9, v2: S8,")
            .replace("file: String,", "file: P3,")
            .replace("rows3: Vec<GroupedHint>,", "rows3: Vec<GroupedHint>, rows4: Vec<GroupedHint>,")
            .replace(
                "files.push(",
                "let rows4 = group.iter().map(|hint| GroupedHint { v0: hint.v0, v1: hint.v1, v2: hint.v2, }).collect(); files.push(",
            )
            .replace("path, rows0, rows1, rows2, rows3", "path, rows0, rows1, rows2, rows3, rows4")
    );
    let (file, defaults) = indexed(&source, false);
    assert!(defaults.resource_sources().primitive_scope_for(file.path.as_ref(), &[]));
    assert!(super::super::fixed::numeric_names(
        &file.ast,
        &record_loop(&file.ast).span().byte_range()
    ));
    assert_eq!(certificate_with_evidence(&file, &defaults), None);
}

fn pool_budget_source(warmed: bool) -> String {
    let mut aliases = "type S0 = u32;".to_owned();
    for index in 1..11 {
        write!(aliases, "type S{index} = S{};", index - 1).unwrap();
    }
    let mut fields = String::new();
    if warmed {
        for index in 0..21 {
            write!(fields, "warm{index}: S10,").unwrap();
        }
        fields.push_str("warm21: S3,");
    }
    format!(
        "{aliases}
        #[derive(Clone, PartialEq)]
        struct Killer {{ first: String, second: String, a: S8, b: S8, c: S8 }}
        struct Hint {{ file: String, killer: Option<Killer>, {fields} }}
        struct Hints {{ mutants: Vec<Hint> }}
        struct FileHints {{ path: String, killers: Vec<Killer> }}
        struct Files {{ files: Vec<FileHints> }}
        fn key(killer: &Killer) -> (&str, &str) {{ (killer.first.as_str(), killer.second.as_str()) }}
        impl From<&Hints> for Files {{
            fn from(hints: &Hints) -> Self {{
                let mut files = Vec::new();
                let mut start = 0;
                while start < hints.mutants.len() {{
                    let path = hints.mutants[start].file.clone();
                    let end = hints.mutants[start..].iter().position(|hint| hint.file != path)
                        .map_or(hints.mutants.len(), |offset| start + offset);
                    let group = &hints.mutants[start..end];
                    let mut killers: Vec<Killer> = group.iter().filter_map(|hint| hint.killer.clone()).collect();
                    killers.sort_by(|left, right| key(left).cmp(&key(right)));
                    killers.dedup();
                    files.push(FileHints {{ path, killers }});
                    start = end;
                }}
                Self {{ files }}
            }}
        }}"
    )
}

#[test]
fn record_grouping_withholds_deduplication_without_remaining_equality_derivation_fuel() {
    assert!(record_certificate(&pool_budget_source(false), false).is_some());
    let (file, defaults) = indexed(&pool_budget_source(true), false);
    assert!(defaults.resource_sources().primitive_scope_for(file.path.as_ref(), &[]));
    assert!(super::super::fixed::numeric_names(
        &file.ast,
        &record_loop(&file.ast).span().byte_range()
    ));
    assert_eq!(certificate_with_evidence(&file, &defaults), None);
}

#[test]
fn record_grouping_requires_sorting_even_when_no_indexed_mapper_observes_the_pool() {
    let source = pool_budget_source(false);
    assert!(record_certificate(&source, false).is_some());
    let unsorted = source.replace("killers.sort_by(|left, right| key(left).cmp(&key(right)));", "");
    assert_ne!(source, unsorted);
    assert_eq!(record_certificate(&unsorted, false), None);
    assert!(record_certificate(&source.replace("killers.dedup();", ""), false).is_some());
}

#[test]
fn record_grouping_checks_the_retained_record_expression_not_only_its_call_and_fields() {
    let source = pool_budget_source(false);
    assert!(record_certificate(&source, false).is_some());
    let attributed = source.replace(
        "files.push(FileHints { path, killers });",
        "files.push(#[allow(unused)] FileHints { path, killers });",
    );
    assert_ne!(source, attributed);
    assert_eq!(record_certificate(&attributed, false), None);
}

#[test]
fn record_grouping_checks_configuration_on_otherwise_indexed_container_declarations() {
    let source = pool_budget_source(false);
    assert!(record_certificate(&source, false).is_some());
    for record in ["Hints", "FileHints"] {
        let changed = source.replace(
            &format!("struct {record} {{"),
            &format!("#[cfg_attr(any(), allow(dead_code))] struct {record} {{"),
        );
        let (file, defaults) = indexed(&changed, false);
        assert!(defaults.resource_sources().local_record_for(file.path.as_ref(), &[], record));
        assert!(defaults.resource_sources().primitive_scope_for(file.path.as_ref(), &[]));
        assert_eq!(certificate_with_evidence(&file, &defaults), None, "{record}");
    }
}

#[test]
fn record_grouping_keeps_inactive_omitted_fields_in_its_closed_output_schema() {
    let source = pool_budget_source(false);
    assert_eq!(record_effects_admitted(&source), Some(true));
    let changed = source.replace("struct FileHints {", "struct FileHints { #[cfg(any())] omitted: String,");
    assert_ne!(source, changed);
    assert_eq!(record_effects_admitted(&changed), Some(false));
}

#[test]
fn record_grouping_rejects_an_extra_sort_argument_even_with_compiler_valid_local_dispatch() {
    let source = pool_budget_source(false);
    assert_eq!(record_effects_admitted(&source), Some(true));
    let changed = source
        .replace(
            "killers.sort_by(|left, right| key(left).cmp(&key(right)));",
            "killers.sort_by(|left, right| key(left).cmp(&key(right)), true);",
        )
        .replace(
            "Self { files }",
            "struct Vec<T>(std::vec::Vec<T>);
             impl<T> std::iter::FromIterator<T> for Vec<T> {
                 fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
                     Self(iter.into_iter().collect())
                 }
             }
             impl<T> Vec<T> {
                 fn new() -> Self { Self(std::vec::Vec::new()) }
                 fn push(&mut self, value: T) { self.0.push(value); }
                 fn sort_by(&mut self, compare: fn(&T, &T) -> std::cmp::Ordering, _: bool) {
                     self.0.sort_by(compare);
                 }
                 fn dedup(&mut self) {}
             }
             struct FileHints { path: String, killers: Vec<Killer> }
             Self { files: std::vec::Vec::new() }",
        );
    assert_ne!(source, changed);
    assert_eq!(record_effects_admitted(&changed), Some(false));
}

#[test]
fn record_grouping_rejects_unmodeled_retention_expressions_after_valid_pool_operations() {
    let source = pool_budget_source(false);
    assert!(record_certificate(&source, false).is_some());
    let cloned = source.replace("FileHints { path, killers }", "FileHints { path: path.clone(), killers }");
    assert_eq!(record_certificate(&cloned, false), None);
    let projected = source
        .replace("let mut files = Vec::new();", "let mut files = Self { files: Vec::new() };")
        .replace("files.push(", "files.files.push(")
        .replace("Self { files }", "files");
    assert_eq!(record_certificate(&projected, false), None);
}

#[test]
fn record_grouping_requires_the_group_binding_even_for_a_path_only_output() {
    let source = pool_budget_source(false)
        .replace("path: String, killers: Vec<Killer>", "path: String")
        .replace("FileHints { path, killers }", "FileHints { path }")
        .replace(
            "let mut killers: Vec<Killer> = group.iter().filter_map(|hint| hint.killer.clone()).collect();",
            "",
        )
        .replace("killers.sort_by(|left, right| key(left).cmp(&key(right)));", "")
        .replace("killers.dedup();", "");
    assert!(record_certificate(&source, false).is_some());
    assert_eq!(
        record_certificate(&source.replace("let group = &hints.mutants[start..end];", ""), false),
        None
    );
}

fn multiple_output_source() -> String {
    pool_budget_source(false)
        .replace(
            "struct Files { files: Vec<FileHints> }",
            "#[derive(Clone)] struct PoolHints { killers: Vec<Killer> }
             struct Files { files: Vec<FileHints>, pools: Vec<PoolHints> }",
        )
        .replace(
            "let mut files = Vec::new();",
            "let mut files = Vec::new(); let mut pools = Vec::new();",
        )
        .replace(
            "let mut killers: Vec<Killer>",
            "let more = group.iter().filter_map(|hint| hint.killer.clone()).collect(); let mut killers: Vec<Killer>",
        )
        .replace(
            "files.push(FileHints { path, killers });",
            "pools.push(PoolHints { killers: more }); files.push(FileHints { path, killers });",
        )
        .replace("Self { files }", "Self { files, pools }")
}

#[test]
fn record_grouping_checks_every_retained_output_not_only_the_first_push() {
    let source = multiple_output_source();
    assert_eq!(record_effects_admitted(&source), Some(true));
    for changed in [
        source.replace("files.push(FileHints", "files.push(self::FileHints"),
        source.replace("struct FileHints { path: String,", "struct FileHints<T = String> { path: T,"),
        source.replace("pools.push(", "pools.fill("),
    ] {
        assert_ne!(source, changed);
        assert_eq!(record_effects_admitted(&changed), Some(false));
    }
}

#[test]
fn record_grouping_requires_later_output_members_in_the_indexed_schema_despite_a_local_type_shadow() {
    let source = multiple_output_source();
    assert_eq!(record_effects_admitted(&source), Some(true));
    let changed = source
        .replace("FileHints { path, killers });", "FileHints { path, alternate: killers });")
        .replace(
            "Self { files, pools }",
            "struct FileHints { path: String, alternate: Vec<Killer> }
             Self { files: Vec::new(), pools }",
        );
    assert_ne!(source, changed);
    assert_eq!(record_effects_admitted(&changed), Some(false));
}

fn mapped_vector_source() -> String {
    pool_budget_source(false)
        .replace(
            "struct FileHints { path: String, killers: Vec<Killer> }",
            "struct Row { path: String } struct FileHints { rows: Vec<Row> }",
        )
        .replace(
            "let mut files = Vec::new();",
            "let mut rows: Vec<Row> = Vec::new(); let mut files = Vec::new();",
        )
        .replace(
            "let mut killers: Vec<Killer> = group.iter().filter_map(|hint| hint.killer.clone()).collect();",
            "let mut rows: Vec<Row> = group.iter().map(|hint| Row { path: hint.file.clone() }).collect();",
        )
        .replace("killers.sort_by(|left, right| key(left).cmp(&key(right)));", "")
        .replace("killers.dedup();", "")
        .replace("files.push(FileHints { path, killers });", "files.push(FileHints { rows });")
}

#[test]
fn record_grouping_does_not_treat_a_shadowing_local_vector_as_an_outer_retained_output() {
    let source = mapped_vector_source();
    assert_eq!(record_effects_admitted(&source), Some(true));
    let changed = source.replace(
        "files.push(FileHints { rows });",
        "rows.push(Row { path }); files.push(FileHints { rows });",
    );
    assert_ne!(source, changed);
    assert_eq!(record_effects_admitted(&changed), Some(false));
}

#[test]
fn record_grouping_does_not_infer_an_optional_projection_from_a_shadowed_parameter_type() {
    let source = format!(
        "struct OtherHint {{ file: String, killer: Option<u32> }}
         struct OtherHints {{ mutants: Vec<OtherHint> }} {}",
        mapped_vector_source()
            .replace(
                "struct Hint { file: String, killer: Option<Killer>",
                "struct Hint { file: String, killer: Option<u32>"
            )
            .replace("struct Row { path: String }", "struct Row { path: String, killer: Option<u32> }")
            .replace(
                "Row { path: hint.file.clone() }",
                "Row { path: hint.file.clone(), killer: hint.killer.clone() }"
            )
            .replace(
                "let mut files = Vec::new();",
                "let hints = &OtherHints { mutants: Vec::new() }; let mut files = Vec::new();"
            )
    );
    assert_eq!(record_effects_admitted(&source), Some(true));
    let changed = source
        .replace(
            "struct Hint { file: String, killer: Option<u32>",
            "struct Hint { file: String, killer: u32",
        )
        .replace("killer: hint.killer.clone()", "killer: hint.killer.as_ref().map(|value| *value)");
    assert_ne!(source, changed);
    assert_eq!(record_effects_admitted(&changed), Some(false));
}

#[test]
fn record_grouping_requires_all_created_pools_and_vectors_to_be_retained() {
    let without_pool_field = HINTS.replace("    killers: Vec<Killer>,", "");
    let without_vector_field = HINTS.replace("    mutants: Vec<GroupedHint>,", "");
    for source in [
        without_pool_field.replace("FileHints { path, killers, mutants }", "FileHints { path, mutants }"),
        without_vector_field.replace("FileHints { path, killers, mutants }", "FileHints { path, killers }"),
    ] {
        assert_eq!(record_certificate(&source, true), None);
        assert!(progress_retained(&source, true));
    }

    let mut pool_only = without_vector_field.replace("FileHints { path, killers, mutants }", "FileHints { path, killers }");
    let start = pool_only.find("let mutants = group").unwrap();
    let end = pool_only.find("files.push(").unwrap();
    pool_only.replace_range(start..end, "");
    assert!(record_certificate(&pool_only, true).is_some());
    assert!(!progress_retained(&pool_only, true));

    let mut vector_only = without_pool_field
        .replace("FileHints { path, killers, mutants }", "FileHints { path, mutants }")
        .replace("killer: Option<u32>", "killer: Option<Killer>");
    let start = vector_only.find("let mut killers:").unwrap();
    let end = vector_only.find("let mutants = group").unwrap();
    vector_only.replace_range(start..end, "");
    let start = vector_only.find("killer: hint.killer.as_ref().map(").unwrap();
    let end = vector_only[start..].find("unviable: hint.unviable,").unwrap() + start;
    vector_only.replace_range(start..end, "killer: hint.killer.clone(),");
    assert!(record_certificate(&vector_only, true).is_some());
    assert!(!progress_retained(&vector_only, true));
}

#[test]
fn record_grouping_replays_deep_inert_fields_and_aliased_output_types() {
    let strings = HINTS.replace("use camino::Utf8PathBuf;", "").replace("Utf8PathBuf", "String");
    let mut records = "#[derive(Clone)] struct R0 { leaf: u32 }".to_owned();
    for level in 1..11 {
        write!(records, "#[derive(Clone)] struct R{level} {{ value: R{} }}", level - 1).unwrap();
    }
    let deep = format!(
        "{} {records}",
        strings
            .replace("    file: String,", "    file: String, deep: R10,")
            .replace("struct GroupedHint {", "struct GroupedHint { deep: R10,")
            .replace("unviable: hint.unviable,", "unviable: hint.unviable, deep: hint.deep.clone(),")
    );
    let mut aliases = "type A0 = String;".to_owned();
    for level in 1..12 {
        write!(aliases, "type A{level} = A{};", level - 1).unwrap();
    }
    let output = format!("{} {aliases}", strings.replace("    path: String,", "    path: A11,"));
    for source in [deep, output] {
        let (file, _) = indexed(&source, false);
        let mut body = record_loop(&file.ast).body.clone();
        let _progress = body.stmts.pop().unwrap();
        assert_eq!(record_certificate(&source, false), Some(super::text(&body)));
    }
}

#[test]
fn record_grouping_rejects_effects_on_input_and_retained_container_declarations() {
    for source in [
        format!("{HINTS} impl Hints {{ fn inspect(&self) {{}} }}"),
        format!("{HINTS} impl Drop for Hints {{ fn drop(&mut self) {{}} }}"),
        format!("{HINTS} impl FileHints {{ fn inspect(&self) {{}} }}"),
        format!("{HINTS} impl Drop for FileHints {{ fn drop(&mut self) {{}} }}"),
        HINTS.replace("struct FileHints {", "pub struct FileHints {"),
        HINTS.replace("path: Utf8PathBuf,", "path: String,"),
        HINTS.replace(
            "FileHints { path, killers, mutants }",
            "FileHints { #[allow(unused)] path, killers, mutants }",
        ),
        HINTS.replace(
            "FileHints { path, killers, mutants }",
            "FileHints { path, killers, mutants, ..other }",
        ),
        HINTS.replace("files.push(FileHints", "#[allow(unused)] files.push(FileHints"),
        HINTS.replace("files.push(FileHints", "files.push::<FileHints>(FileHints"),
        HINTS.replace("killers.dedup();", "killers.dedup(1);"),
        HINTS.replace(
            "killers.sort_by(|left, right| killer_key(left).cmp(&killer_key(right)));",
            "killers.reverse(|left, right| killer_key(left).cmp(&killer_key(right)));",
        ),
        HINTS.replace("let mutants = group", "let group = group"),
        HINTS.replace("let mutants = group", "let path = group"),
    ] {
        assert_eq!(record_certificate(&source, true), None);
    }
}

#[test]
fn record_grouping_certificate_replays_the_exact_body_with_source_and_dependency_evidence() {
    let strings = HINTS.replace("use camino::Utf8PathBuf;", "").replace("Utf8PathBuf", "String");
    for (source, dependency) in [
        (HINTS.to_owned(), true),
        (strings, false),
        (
            HINTS
                .replace("u32::try_from(index)", "usize::try_from(index)")
                .replace("killer: Option<u32>", "killer: Option<usize>"),
            true,
        ),
        (HINTS.replace("let mutants = group", "let mutants: Vec<GroupedHint> = group"), true),
        (HINTS.replace(".collect();", ".collect::<Vec<_>>();"), true),
        (
            HINTS.replace("type MutantId = String;", "type MutantId = Identifier; type Identifier = String;"),
            true,
        ),
    ] {
        let (file, _) = indexed(&source, dependency);
        let mut expected = record_loop(&file.ast).body.clone();
        let _progress = expected.stmts.pop().unwrap();
        assert_eq!(record_certificate(&source, dependency), Some(super::text(&expected)));
    }
    assert_eq!(record_certificate(HINTS, false), None);
}

#[test]
fn record_grouping_cached_nonempty_replay_does_not_certify_an_empty_group_without_progress() {
    let (file, defaults) = indexed(HINTS, true);
    let original = syn::Expr::While(record_loop(&file.ast).clone());
    let bindings = crate::HashMap::from_iter([("hints.mutants".into(), super::Binding::Vec)]);
    let proof = super::Grouping::new(
        &file.ast,
        &original,
        &bindings,
        defaults.resource_sources(),
        file.path.as_ref(),
        &[],
    )
    .unwrap();
    let mut stalled = record_loop(&file.ast).clone();
    let _progress = stalled.body.stmts.pop().unwrap();
    assert_eq!(proof.record_body, Some(super::text(&stalled.body)));
    assert_eq!(proof.recurrence(&stalled, false), Some(()));
    assert_eq!(proof.recurrence(&stalled, true), None);
}

#[test]
fn actual_record_grouping_omits_only_stalled_progress_in_a_resolved_source_graph() {
    assert!(!progress_retained(HINTS, true));
    assert!(
        progress_retained(HINTS, false),
        "missing dependency identity cannot establish standard path effects"
    );
    let (file, defaults) = indexed(HINTS, true);
    let original = file
        .ast
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Impl(implementation) => implementation.items.iter().find_map(|item| match item {
                syn::ImplItem::Fn(function) => function.block.stmts.iter().find_map(|statement| match statement {
                    syn::Stmt::Expr(expression @ syn::Expr::While(_), _) => Some(expression),
                    _ => None,
                }),
                _ => None,
            }),
            _ => None,
        })
        .unwrap();
    let bindings = crate::HashMap::from_iter([("hints.mutants".into(), super::Binding::Vec)]);
    let proof = super::Grouping::new(&file.ast, original, &bindings, defaults.resource_sources(), file.path.as_ref(), &[]).unwrap();
    assert!(
        proof.record_body.is_some(),
        "the original record-valued callbacks, not an empty-group shortcut, are certified"
    );
    let mut stalled = original.clone();
    let syn::Expr::While(loop_) = &mut stalled else { unreachable!() };
    let assignment = loop_.body.stmts.pop().unwrap();
    assert_eq!(assignment.span().byte_range().start, file.text.find("start = end;").unwrap());
    assert!(proof.proves(original, &stalled));
    assert!(!proof.proves(original, original), "advancing boundaries remain finite");
    let mut altered = stalled;
    let syn::Expr::While(loop_) = &mut altered else { unreachable!() };
    loop_.body.stmts.push(syn::parse_quote!(start += 1;));
    assert!(!proof.proves(original, &altered), "an alternate write is not a repeated boundary");
}

#[test]
fn record_grouping_effects_follow_declarations_under_arbitrary_renaming() {
    let mut source = HINTS.to_owned();
    for (from, to) in [
        ("GroupedHints", "CompiledRows"),
        ("GroupedHint", "CompiledRow"),
        ("FileHints", "Batch"),
        ("HintContext", "Metadata"),
        ("Hint", "Row"),
        ("Killer", "Witness"),
        ("killer_key", "ordering_projection"),
        ("killers", "witnesses"),
        ("killer", "witness"),
        ("mutants", "rows"),
        ("hints", "input"),
        ("group", "window"),
        ("left", "alpha"),
        ("right", "beta"),
        ("known", "candidate"),
        ("index", "ordinal"),
        ("offset", "distance"),
        ("start", "cursor"),
        ("end", "boundary"),
    ] {
        source = source.replace(from, to);
    }
    let (file, defaults) = indexed(&source, true);
    let candidates = collect_with(
        &file,
        &Selection::parse("stmt.delete_assign").unwrap(),
        &CfgSet::unconditional(),
        &defaults,
    );
    let start = file.text.find("cursor = boundary;").unwrap();
    assert!(!candidates.iter().any(|candidate| candidate.span.start == start));
    let mut expected = record_loop(&file.ast).body.clone();
    let _progress = expected.stmts.pop().unwrap();
    assert_eq!(record_certificate(&source, true), Some(super::text(&expected)));
}

#[test]
fn record_grouping_respects_harmless_lexical_shadowing_inside_the_finite_search() {
    let source = HINTS.replace("|known| known == killer", "|hint| hint == killer");
    assert!(
        !progress_retained(&source, true),
        "the inner hint is a pool element, not the outer input row"
    );
    assert!(record_certificate(&source, true).is_some());
}

#[test]
fn record_grouping_uses_successful_replay_not_the_text_of_a_checked_index_message() {
    let source = HINTS.replace(
        "a killer index cannot exceed the number of mutants retained in the bounded artifact",
        "checked index",
    );
    assert!(!progress_retained(&source, true));
    assert!(record_certificate(&source, true).is_some());
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the negative effect matrix shares one frozen source and collector oracle"
)]
fn record_grouping_retains_unknown_effects_custom_dispatch_and_escape_controls() {
    for (description, source) in [
        (
            "custom clone",
            format!(
                "{}\nimpl Clone for Killer {{ fn clone(&self) -> Self {{ panic!() }} }}",
                HINTS.replace("#[derive(Clone, PartialEq)]", "#[derive(PartialEq)]")
            ),
        ),
        (
            "custom equality",
            format!(
                "{}\nimpl PartialEq for Killer {{ fn eq(&self, _: &Self) -> bool {{ panic!() }} }}",
                HINTS.replace("#[derive(Clone, PartialEq)]", "#[derive(Clone)]")
            ),
        ),
        (
            "custom ordering",
            format!(
                "{}\nimpl Ord for Killer {{ fn cmp(&self, _: &Self) -> std::cmp::Ordering {{ panic!() }} }}",
                HINTS.replace("#[derive(Clone, PartialEq)]", "#[derive(Clone, PartialEq, Eq, PartialOrd)]")
            ),
        ),
        (
            "custom drop",
            format!("{HINTS}\nimpl Drop for Killer {{ fn drop(&mut self) {{ panic!() }} }}"),
        ),
        (
            "custom output drop",
            format!("{HINTS}\nimpl Drop for GroupedHint {{ fn drop(&mut self) {{ panic!() }} }}"),
        ),
        (
            "drop through alias",
            format!("{HINTS}\ntype Alias = Killer; impl Drop for Alias {{ fn drop(&mut self) {{ panic!() }} }}"),
        ),
        (
            "qualified drop",
            format!("{HINTS}\nimpl Drop for self::FileHints {{ fn drop(&mut self) {{ panic!() }} }}"),
        ),
        (
            "public record needs cross-file effects",
            HINTS.replace("struct Killer {", "pub struct Killer {"),
        ),
        ("descendant effects are unavailable", format!("{HINTS}\nmod callbacks;")),
        ("public alias exposes effects", format!("{HINTS}\npub type Export = Killer;")),
        ("unknown helper", HINTS.replace("killer_key(left)", "external_key(left)")),
        (
            "effectful helper",
            HINTS.replace(
                "(killer.package.as_str(), killer.target.as_str(), killer.test.as_str())",
                "{ panic!(); (killer.package.as_str(), killer.target.as_str(), killer.test.as_str()) }",
            ),
        ),
        ("unknown mapper", HINTS.replace("hint.id.clone()", "inspect(hint.id.clone())")),
        (
            "unknown collection effect",
            HINTS.replace("killers.dedup();", "killers.retain(|_| inspect());"),
        ),
        (
            "mutable input",
            HINTS.replace("fn from(hints: &Hints)", "fn from(hints: &mut Hints)"),
        ),
        (
            "unknown alias",
            HINTS.replace("type MutantId = String;", "type MutantId = External;"),
        ),
        (
            "conditional record",
            HINTS.replace("struct Killer {", "#[cfg(feature = \"witness\")] struct Killer {"),
        ),
        (
            "conditional field",
            HINTS.replace("package: String,", "#[cfg(feature = \"witness\")] package: String,"),
        ),
        (
            "conditional input field",
            HINTS.replace("mutants: Vec<Hint>,", "#[cfg(feature = \"rows\")] mutants: Vec<Hint>,"),
        ),
        (
            "conditional alias",
            HINTS.replace("type MutantId", "#[cfg(feature = \"ids\")] type MutantId"),
        ),
        (
            "conditional helper",
            HINTS.replace("fn killer_key", "#[cfg(feature = \"witness\")] fn killer_key"),
        ),
        (
            "pool alias",
            HINTS.replace("let mutants = group", "let alias = &killers; let mutants = group"),
        ),
        ("progress guard", HINTS.replace("start = end;", "start = end; assert!(start > 0);")),
        ("boundary guard", HINTS.replace("let group =", "assert!(end > start); let group =")),
        (
            "explicit escape",
            HINTS.replace("let mutants = group", "if killers.is_empty() { break; } let mutants = group"),
        ),
        (
            "checked index depends on output",
            HINTS.replace("u32::try_from(index)", "u32::try_from(index + files.len())"),
        ),
        (
            "fallible search is not membership",
            HINTS.replace("known == killer", "known != killer"),
        ),
        (
            "searched pool differs",
            HINTS.replace("let index = killers", "let index = other_pool"),
        ),
        ("narrowing escape", HINTS.replace("u32::try_from(index)", "u8::try_from(index)")),
        (
            "checked-index early return",
            HINTS.replace(
                "u32::try_from(index).expect(\"a killer index cannot exceed the number of mutants retained in the bounded artifact\")",
                "match u32::try_from(index) { Ok(index) => index, Err(_) => return None }",
            ),
        ),
        (
            "checked-index output assertion",
            HINTS.replace(
                "u32::try_from(index).expect",
                "{ assert!(files.len() < 2); u32::try_from(index) }.expect",
            ),
        ),
        ("primitive shadow", format!("struct u32;\n{HINTS}")),
        (
            "callback captures",
            HINTS.replace("hint.killer.clone()", "{ observe(); hint.killer.clone() }"),
        ),
        ("unknown pool type", HINTS.replace("Vec<Killer> = group", "Custom = group")),
        (
            "custom comparison trait",
            format!("{HINTS}\ntrait Comparison {{ fn cmp(self, other: Self) -> std::cmp::Ordering; }}"),
        ),
        (
            "custom optional reference trait",
            format!("{HINTS}\ntrait Borrow {{ fn as_ref(self) -> Self; }}"),
        ),
        (
            "custom conversion trait",
            format!("{HINTS}\ntrait Convert {{ fn try_from(index: usize) -> Result<u32, ()>; }}"),
        ),
        (
            "imported comparison trait",
            format!(
                "mod extension {{ pub trait Compare {{ fn cmp(self, other: Self) -> std::cmp::Ordering; }} }} use extension::Compare;\n{HINTS}"
            ),
        ),
        ("finite scan", HINTS.replace("start = end;", "start = end; break;")),
        ("normal zero offset", HINTS.replace("start + offset", "offset")),
        ("different initial cursor", HINTS.replace("let mut start = 0", "let mut start = 1")),
        (
            "input is not a parameter",
            HINTS.replace(
                "fn from(hints: &Hints) -> Self {",
                "fn from(original: &Hints) -> Self { let hints = &Hints { mutants: Vec::new(), ..original.clone() };",
            ),
        ),
    ] {
        assert!(progress_retained(&source, true), "{description}");
        if !matches!(description, "mutable input" | "normal zero offset" | "different initial cursor") {
            assert_eq!(
                record_certificate(&source, true),
                None,
                "{description}: record effects must not use a fallback proof"
            );
        }
    }
}

#[test]
fn record_grouping_does_not_confuse_shadowed_callback_or_helper_bindings() {
    for source in [
        HINTS
            .replace(".map(|hint| GroupedHint", ".map(|killers| GroupedHint")
            .replace("id: hint.id", "id: killers.id")
            .replace("killer: hint.killer", "killer: killers.killer")
            .replace("unviable: hint.unviable", "unviable: killers.unviable"),
        HINTS
            .replace(".map(|killer| {", ".map(|killers| {")
            .replace("known == killer", "known == killers"),
        HINTS.replace("|left, right| killer_key(left)", "|killer_key, right| killer_key(killer_key)"),
        HINTS
            .replace("let index = killers", "let killer = killers")
            .replace("u32::try_from(index)", "u32::try_from(killer)"),
        HINTS.replace(
            "let mut files = Vec::new();",
            "let killer_key = |_: &Killer| panic!(); let mut files = Vec::new();",
        ),
    ] {
        assert!(progress_retained(&source, true));
        assert_eq!(record_certificate(&source, true), None);
    }
}

#[test]
#[expect(clippy::too_many_lines, reason = "one frozen declaration graph isolates record replay failures")]
fn record_grouping_requires_pool_identity_complete_output_and_no_post_growth_effects() {
    for (description, source) in [
        (
            "wrong slice input",
            HINTS.replace("&hints.mutants[start..end]", "&other.mutants[start..end]"),
        ),
        (
            "inclusive slice",
            HINTS.replace("&hints.mutants[start..end]", "&hints.mutants[start..=end]"),
        ),
        (
            "wrong lower bound",
            HINTS.replace("&hints.mutants[start..end]", "&hints.mutants[other..end]"),
        ),
        (
            "wrong upper bound",
            HINTS.replace("&hints.mutants[start..end]", "&hints.mutants[start..other]"),
        ),
        (
            "mutable slice",
            HINTS.replace("&hints.mutants[start..end]", "&mut hints.mutants[start..end]"),
        ),
        (
            "unsorted search",
            HINTS.replace("killers.sort_by(|left, right| killer_key(left).cmp(&killer_key(right)));", ""),
        ),
        ("undeduplicated search", HINTS.replace("killers.dedup();", "")),
        (
            "repeated dedup",
            HINTS.replace("killers.dedup();", "killers.dedup(); killers.dedup();"),
        ),
        (
            "dedup before sort",
            HINTS.replace("killers.sort_by(", "killers.dedup(); killers.sort_by("),
        ),
        (
            "repeated sort",
            HINTS.replace(
                "killers.dedup();",
                "killers.sort_by(|left, right| killer_key(left).cmp(&killer_key(right))); killers.dedup();",
            ),
        ),
        (
            "wrong pool projection",
            HINTS.replace("filter_map(|hint| hint.killer.clone())", "filter_map(|hint| hint.id.clone())"),
        ),
        (
            "wrong slice iteration",
            HINTS.replace("group.iter().filter_map", "other.iter().filter_map"),
        ),
        ("wrong output slice", HINTS.replace("let mutants = group", "let mutants = other")),
        (
            "different key projections",
            format!(
                "{}\nfn reverse_key(killer: &Killer) -> (&str, &str, &str) {{ (killer.test.as_str(), killer.target.as_str(), killer.package.as_str()) }}",
                HINTS.replace("killer_key(right)", "reverse_key(right)")
            ),
        ),
        ("wrong comparator argument", HINTS.replace("killer_key(right)", "killer_key(left)")),
        (
            "unretained pool",
            HINTS.replace("FileHints { path, killers, mutants }", "FileHints { path, mutants }"),
        ),
        (
            "duplicate output member",
            HINTS.replace(
                "FileHints { path, killers, mutants }",
                "FileHints { path, killers, mutants: mutants, path: path }",
            ),
        ),
        (
            "duplicate output member with correct field count",
            HINTS.replace("FileHints { path, killers, mutants }", "FileHints { path, killers, killers }"),
        ),
        (
            "duplicate mapped member with correct field count",
            HINTS.replace("unviable: hint.unviable,", "id: hint.id.clone(),"),
        ),
        (
            "wrong copied field",
            HINTS.replace(
                "FileHints { path, killers, mutants }",
                "FileHints { path: other, killers, mutants }",
            ),
        ),
        (
            "wrong output vector kind",
            HINTS.replace("mutants: Vec<GroupedHint>", "mutants: Vec<Killer>"),
        ),
        (
            "wrong mapped scalar kind",
            HINTS.replace(
                "skip_serializing_if = \"is_not_set\")]\n    unviable: bool,",
                "skip_serializing_if = \"is_not_set\")]\n    unviable: u32,",
            ),
        ),
        (
            "wrong mapped cloned kind",
            HINTS.replace("struct GroupedHint {\n    id: MutantId,", "struct GroupedHint {\n    id: bool,"),
        ),
        (
            "wrong pool vector kind",
            HINTS.replace("let mut killers: Vec<Killer>", "let mut killers: Vec<GroupedHint>"),
        ),
        (
            "tuple output vector",
            HINTS.replace("let mutants = group", "let (mutants,) = group"),
        ),
        ("duplicate local", HINTS.replace("let mutants = group", "let killers = group")),
        ("shadowed output", HINTS.replace("let mutants = group", "let files = group")),
        ("untracked output", HINTS.replace("files.push(", "other.push(")),
        (
            "extra push argument",
            HINTS.replace(
                "files.push(FileHints { path, killers, mutants });",
                "files.push(FileHints { path, killers, mutants }, other);",
            ),
        ),
        ("post-growth effect", HINTS.replace("start = end;", "observe(); start = end;")),
        (
            "second growth",
            HINTS.replace("start = end;", "files.push(FileHints { path, killers, mutants }); start = end;"),
        ),
        (
            "no output growth",
            HINTS.replace("files.push(FileHints { path, killers, mutants });", ""),
        ),
        (
            "custom output field",
            HINTS.replace("path: Utf8PathBuf,", "#[custom] path: Utf8PathBuf,"),
        ),
        (
            "conditional output record",
            HINTS.replace("struct FileHints", "#[cfg_attr(feature = \"x\", derive(Clone))] struct FileHints"),
        ),
        (
            "mapped record update",
            HINTS.replace("unviable: hint.unviable,", "unviable: hint.unviable, ..other"),
        ),
    ] {
        assert_eq!(record_certificate(&source, true), None, "{description}");
    }
}
