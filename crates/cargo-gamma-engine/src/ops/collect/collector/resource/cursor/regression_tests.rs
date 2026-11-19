// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fmt::Write as _;

use super::*;

const FRAME: &str = r"
    while let Some(frame) = frames.last_mut() {
        let node = frame.node;
        if frame.edge < edges[node].len() {
            let neighbor = edges[node][frame.edge];
            frame.edge += 0;
            if let Some(index) = indices[neighbor] {
                if marked[neighbor] {
                    minima[node] = minima[node].min(index);
                }
            }
        }
    }
";

fn frame_context() -> Cursor {
    Cursor {
        types: [
            ("frames", syn::parse_quote!(Vec<Frame>)),
            ("edges", syn::parse_quote!(Vec<Vec<usize>>)),
            ("indices", syn::parse_quote!(Vec<Option<u32>>)),
            ("marked", syn::parse_quote!(Vec<bool>)),
            ("minima", syn::parse_quote!(Vec<u32>)),
        ]
        .into_iter()
        .map(|(name, ty)| (name.into(), ty))
        .collect(),
        records: HashMap::from_iter([(
            "Frame".into(),
            HashMap::from_iter([("node".into(), syn::parse_quote!(usize)), ("edge".into(), syn::parse_quote!(usize))]),
        )]),
        closures: HashSet::default(),
        entry: None,
    }
}

#[test]
fn frame_certificate_requires_the_exact_stationary_branch_not_just_a_similar_loop() {
    let context = frame_context();
    let stationary: Expr = syn::parse_str(FRAME).unwrap();
    assert!(context.branch(&stationary).is_some());
    let advancing: Expr = syn::parse_str(&FRAME.replace("frame.edge += 0;", "frame.edge += 1;")).unwrap();
    assert!(context.proves(&advancing, &stationary));
    assert!(!context.proves(&stationary, &advancing));
    assert!(!context.proves(&stationary, &stationary));
    assert!(
        context
            .branch(&syn::parse_str(&FRAME.replace("frame.edge += 0;", "")).unwrap())
            .is_some()
    );

    for (from, to) in [
        ("while let Some(frame)", "'outer: while let Some(frame)"),
        ("while let Some(frame)", "#[cfg(feature = \"x\")] while let Some(frame)"),
        ("Some(frame)", "Other(frame)"),
        ("Some(frame)", "Some(ref frame)"),
        ("Some(frame)", "Some(frame @ _)"),
        ("Some(frame)", "Some(frame, other)"),
        ("Some(frame)", "Some(_)"),
        ("frames.last_mut()", "frames.last()"),
        ("frames.last_mut()", "frames.last_mut(1)"),
        ("frames.last_mut()", "frames.last_mut::<Frame>()"),
        ("let node = frame.node;", "let node = frame.edge;"),
        ("let node = frame.node;", "let node = other.node;"),
        ("let node = frame.node;", "let ref node = frame.node;"),
        ("let node = frame.node;", "let node @ _ = frame.node;"),
        ("let node = frame.node;", "let node = frame.node else { return; };"),
        ("let node = frame.node;", "#[cfg(feature = \"x\")] let node = frame.node;"),
        ("let node = frame.node;", "let frames = frame.node;"),
        ("let node = frame.node;", "let frame = frame.node;"),
        ("frame.edge < edges", "frame.edge <= edges"),
        ("frame.edge < edges", "frame.node < edges"),
        ("frame.edge < edges", "other.edge < edges"),
        ("edges[node].len()", "edges[node].capacity()"),
        ("edges[node].len()", "edges[node].len(1)"),
        ("edges[node].len()", "edges[node].len::<usize>()"),
        ("edges[node][frame.edge]", "edges[node][node]"),
        ("edges[node][frame.edge]", "other[node][frame.edge]"),
        ("let neighbor =", "let ref neighbor ="),
        ("let neighbor =", "let node ="),
        ("frame.edge < edges", "(frame.edge < edges)"),
        ("if frame.edge <", "#[allow(unused)] if frame.edge <"),
        ("frame.edge += 0;", "#[allow(unused)] frame.edge += 0;"),
        ("frame.edge += 0;", "frame.node += 0;"),
        ("frame.edge += 0;", "frame.edge -= 0;"),
        ("frame.edge += 0;", "frame.edge += 1;"),
        ("frame.edge += 0;", "frame.edge += 0; frame.edge += 0;"),
        ("frame.edge += 0;", "observe(frame);"),
        ("Some(index)", "Other(index)"),
        ("Some(index)", "Some(ref index)"),
        ("Some(index)", "Some(index @ _)"),
        ("Some(index)", "Some(index, other)"),
        ("Some(index)", "Some(_)"),
        ("Some(index)", "Some(node)"),
        ("indices[neighbor]", "unknown[neighbor]"),
        ("if let Some(index)", "#[allow(unused)] if let Some(index)"),
        ("if marked[neighbor]", "if neighbor"),
        ("if marked[neighbor]", "if unknown"),
        ("if marked[neighbor]", "#[allow(unused)] if marked[neighbor]"),
        ("minima[node].min(index)", "minima[node].max(index)"),
        ("minima[node].min(index)", "minima[node].min(node)"),
        ("minima[node].min(index)", "minima[node].min(index, index)"),
        ("minima[node].min(index)", "minima[node].min::<u32>(index)"),
        ("minima[node] = minima", "minima[neighbor] = minima"),
        ("minima[node] = minima", "#[allow(unused)] minima[node] = minima"),
        ("minima[node] = minima[node].min(index);", "observe(index);"),
        (
            "minima[node] = minima[node].min(index);",
            "minima[node] = minima[node].min(index); observe(index);",
        ),
    ] {
        assert!(FRAME.contains(from), "{from}");
        let changed = syn::parse_str::<Expr>(&FRAME.replacen(from, to, 1)).unwrap();
        assert!(context.branch(&changed).is_none(), "{from} -> {to}");
    }
}

#[test]
fn frame_certificate_requires_primitive_record_fields_and_typed_collection_inputs() {
    let expression = syn::parse_str::<Expr>(FRAME).unwrap();
    for (name, ty) in [
        ("frames", syn::parse_quote!(Custom<Frame>)),
        ("frames", syn::parse_quote!(Vec<Unknown>)),
        ("frames", syn::parse_quote!(Vec<crate::Frame>)),
        ("frames", syn::parse_quote!(Vec<(usize, usize)>)),
        ("edges", syn::parse_quote!(Vec<Vec<u32>>)),
        ("edges", syn::parse_quote!(Vec<usize>)),
        ("indices", syn::parse_quote!(Vec<u32>)),
        ("indices", syn::parse_quote!(Vec<Option<String>>)),
        ("marked", syn::parse_quote!(Vec<u32>)),
        ("minima", syn::parse_quote!(Vec<u64>)),
    ] {
        let mut context = frame_context();
        let _previous = context.types.insert(name.into(), ty);
        assert!(context.branch(&expression).is_none(), "{name}");
    }
    for (field, ty) in [
        ("node", syn::parse_quote!(u32)),
        ("edge", syn::parse_quote!(u32)),
        ("extra", syn::parse_quote!(String)),
    ] {
        let mut context = frame_context();
        let _previous = context.records.get_mut("Frame").unwrap().insert(field.into(), ty);
        assert!(context.branch(&expression).is_none(), "{field}");
    }
    for shadow in ["node", "neighbor", "index"] {
        let mut context = frame_context();
        let _previous = context.types.insert(shadow.into(), syn::parse_quote!(usize));
        assert!(context.branch(&expression).is_none(), "{shadow}");
    }
    let mut context = frame_context();
    let _removed = context.records.get_mut("Frame").unwrap().remove("edge");
    assert_eq!(context.branch(&expression), None);
}

#[test]
fn cursor_type_evidence_tracks_index_types_fields_and_checked_conversions() {
    let mut context = frame_context();
    context.types.extend([
        ("frame".into(), syn::parse_quote!(Frame)),
        ("slot".into(), syn::parse_quote!(usize)),
        ("offset".into(), syn::parse_quote!(isize)),
        ("other_offset".into(), syn::parse_quote!(isize)),
        ("other_unsigned".into(), syn::parse_quote!(u32)),
        ("text".into(), syn::parse_quote!(&[&str])),
        ("mutable".into(), syn::parse_quote!(&mut [usize])),
    ]);
    for (expression, expected) in [
        ("frame.node", syn::parse_quote!(usize)),
        ("edges[slot]", syn::parse_quote!(Vec<usize>)),
        ("text[slot]", syn::parse_quote!(&str)),
        ("text.len()", syn::parse_quote!(usize)),
        ("edges.len()", syn::parse_quote!(usize)),
        ("-offset", syn::parse_quote!(isize)),
        ("offset + other_offset", syn::parse_quote!(isize)),
        ("slot - slot", syn::parse_quote!(usize)),
        ("isize::try_from(slot).unwrap_or(isize::MAX)", syn::parse_quote!(isize)),
        ("vec![0_isize; slot]", syn::parse_quote!(Vec<isize>)),
    ] {
        assert_eq!(
            context.ty(&syn::parse_str(expression).unwrap(), &context.types),
            Some(expected),
            "{expression}"
        );
    }
    for expression in [
        "missing",
        "frame.missing",
        "frame.0",
        "edges[offset]",
        "edges[missing]",
        "mutable[slot]",
        "edges.capacity()",
        "edges.len(1)",
        "edges.len::<usize>()",
        "-slot",
        "!offset",
        "offset + slot",
        "other_unsigned + other_unsigned",
        "offset * other_offset",
        "slot + missing",
        "isize::try_from(offset).unwrap_or(isize::MAX)",
        "usize::try_from(slot).unwrap_or(isize::MAX)",
        "isize::try_from(slot, slot).unwrap_or(isize::MAX)",
        "isize::try_from(slot).unwrap_or(0)",
        "vec![0; slot]",
        "vec![0_isize, 1_isize]",
        "vec![slot; slot]",
        "custom![0_isize; slot]",
    ] {
        assert!(
            context.ty(&syn::parse_str(expression).unwrap(), &context.types).is_none(),
            "{expression}"
        );
    }
}

const DIFF: &str = include_str!("../fixtures/campaign/diff.rs");

fn first_while(file: &syn::File) -> Expr {
    struct Find(Option<Expr>);
    impl<'ast> Visit<'ast> for Find {
        fn visit_expr_while(&mut self, node: &'ast syn::ExprWhile) {
            if self.0.is_none() {
                self.0 = Some(Expr::While(node.clone()));
            }
        }
    }
    let mut find = Find(None);
    find.visit_file(file);
    find.0.expect("the controlled source contains a while loop")
}

fn diagonal_context() -> Cursor {
    Cursor {
        types: [
            ("d", syn::parse_quote!(isize)),
            ("n", syn::parse_quote!(usize)),
            ("m", syn::parse_quote!(usize)),
            ("old", syn::parse_quote!(&[&str])),
            ("new", syn::parse_quote!(&[&str])),
            ("max", syn::parse_quote!(usize)),
            ("furthest", syn::parse_quote!(Vec<isize>)),
        ]
        .into_iter()
        .map(|(name, ty)| (name.into(), ty))
        .collect(),
        records: HashMap::default(),
        closures: HashSet::from_iter(["shift".into()]),
        entry: Some(syn::parse_quote!(let mut k = -d;)),
    }
}

#[test]
fn diagonal_certificate_rejects_an_effectful_tail_after_the_exact_scan_and_escape() {
    let context = diagonal_context();
    let source = DIFF.replace("k += 2;", "k += 0;");
    let expression = first_while(&syn::parse_file(&source).unwrap());
    assert_eq!(context.diagonal(&expression), Some(()));
    assert_eq!(
        context.diagonal(&first_while(&syn::parse_file(&source.replace("k += 0;", "observe();")).unwrap())),
        None
    );
    assert_eq!(
        context.diagonal(&first_while(&syn::parse_file(&source.replace("k += 0;", "")).unwrap())),
        Some(())
    );
}

#[test]
fn diagonal_certificate_rejects_changed_selection_scan_store_and_escape_behavior() {
    let context = diagonal_context();
    let expression = |source: &str| first_while(&syn::parse_file(source).unwrap());
    let advancing = expression(DIFF);
    let stationary = expression(&DIFF.replace("k += 2;", "k += 0;"));
    assert!(context.diagonal(&stationary).is_some());
    assert!(context.proves(&advancing, &stationary));
    assert!(!context.proves(&stationary, &advancing));
    assert!(!context.proves(&stationary, &stationary));
    assert!(context.diagonal(&expression(&DIFF.replace("k += 2;", ""))).is_some());
    for (from, to) in [
        ("while k <= d", "'outer: while k <= d"),
        ("while k <= d", "while k < d"),
        ("while k <= d", "while unknown <= d"),
        ("k == -d ||", "k != -d ||"),
        ("k != d && furthest[shift(k - 1)] < furthest[shift(k + 1)]", "opaque()"),
        ("else { furthest[shift(k - 1)] + 1 }", "else { opaque() }"),
        ("furthest[shift(k + 1)] } else", "furthest[shift(k - 1)] } else"),
        ("let mut x = if down", "let mut x = if unknown"),
        ("let mut y = x - k;", "let mut y = x + k;"),
        ("let mut y = x - k;", "let mut x = x - k;"),
        ("usize::try_from(x), usize::try_from(y)", "usize::try_from(y), usize::try_from(x)"),
        ("Ok(xi), Ok(yi)", "Some(xi), Ok(yi)"),
        ("xi < n", "xi <= n"),
        ("yi < m", "yi <= m"),
        ("old[xi] == new[yi]", "old[yi] == new[xi]"),
        ("old[xi] == new[yi]", "old[xi] != new[yi]"),
        ("x += 1;", "x += 2;"),
        ("y += 1;", ""),
        ("y += 1;", "y += 1; opaque();"),
        ("furthest[shift(k)] = x;", "furthest[shift(k)] = y;"),
        ("furthest[shift(k)] = x;", "furthest[shift(k + 1)] = x;"),
        ("unwrap_or(0) >= n", "unwrap_or(0) > n"),
        ("unwrap_or(0) >= m", "unwrap_or(0) > m"),
        ("return walk_back(old, new, &rounds, d);", "break;"),
        ("return walk_back(old, new, &rounds, d);", ""),
        ("return walk_back(old, new, &rounds, d);", "return; opaque();"),
        (
            "return walk_back(old, new, &rounds, d);\n            }",
            "return; } else { opaque(); }",
        ),
        ("k += 2;", "k += 1;"),
        ("k += 2;", "k -= 0;"),
        ("k += 2;", "k += 0; k += 0;"),
    ] {
        assert!(DIFF.contains(from), "{from}");
        let source = DIFF.replace("k += 2;", "k += 0;");
        let changed = if from == "k += 2;" {
            DIFF.replacen(from, to, 1)
        } else {
            source.replacen(from, to, 1)
        };
        assert!(context.diagonal(&expression(&changed)).is_none(), "{from} -> {to}");
    }
    for (name, ty) in [
        ("d", syn::parse_quote!(usize)),
        ("n", syn::parse_quote!(u32)),
        ("m", syn::parse_quote!(isize)),
        ("old", syn::parse_quote!(&mut [&str])),
        ("old", syn::parse_quote!(&[&mut str])),
        ("old", syn::parse_quote!(&[&String])),
        ("old", syn::parse_quote!(Vec<&str>)),
        ("old", syn::parse_quote!(&str)),
        ("new", syn::parse_quote!(&[String])),
        ("furthest", syn::parse_quote!(Vec<usize>)),
    ] {
        let mut context = diagonal_context();
        let _previous = context.types.insert(name.into(), ty);
        assert!(context.diagonal(&stationary).is_none(), "{name}");
    }
    for shadow in ["down", "x", "y"] {
        let mut context = diagonal_context();
        let _previous = context.types.insert(shadow.into(), syn::parse_quote!(usize));
        assert!(context.diagonal(&stationary).is_none(), "{shadow}");
        let mut context = diagonal_context();
        let _new = context.closures.insert(shadow.into());
        assert!(context.diagonal(&stationary).is_none(), "closure {shadow}");
    }
}

#[test]
fn diagonal_certificate_requires_its_dominating_entry_and_every_captured_type() {
    let expression = first_while(&syn::parse_file(&DIFF.replace("k += 2;", "k += 0;")).unwrap());
    for entry in [
        None,
        Some(syn::parse_quote!(-d;)),
        Some(syn::parse_quote!(let mut other = -d;)),
        Some(syn::parse_quote!(let mut k = d;)),
        Some(syn::parse_quote!(let mut k = -other;)),
    ] {
        let mut context = diagonal_context();
        context.entry = entry;
        assert_eq!(context.diagonal(&expression), None);
    }
    for missing in ["d", "furthest", "n", "m", "old", "new"] {
        let mut context = diagonal_context();
        let _removed = context.types.remove(missing);
        assert_eq!(context.diagonal(&expression), None, "{missing}");
    }
    let mut context = diagonal_context();
    context.closures.clear();
    assert_eq!(context.diagonal(&expression), None);
}

#[test]
fn shift_certificate_requires_the_captured_unsigned_bound_and_exact_checked_formula() {
    let context = diagonal_context();
    let source = "|k: isize| usize::try_from(k + isize::try_from(max).unwrap_or(isize::MAX)).unwrap_or(0)";
    assert!(context.shift(&syn::parse_str(source).unwrap()));
    for invalid in [
        "|| 0",
        "|k| 0",
        "|(k, other): (isize, isize)| 0",
        "|k: isize| 0",
        "|k: isize| value.unwrap_or(0)",
        "|k: isize| usize::try_from().unwrap_or(0)",
        "|k: isize| usize::try_from(k).unwrap_or(0)",
        "|k: isize| usize::try_from(k + offset).unwrap_or(0)",
        "|k: isize| usize::try_from(k + value.unwrap_or(isize::MAX)).unwrap_or(0)",
    ] {
        assert!(!context.shift(&syn::parse_str(invalid).unwrap()), "{invalid}");
    }
    for (from, to) in [
        ("k: isize", "k: usize"),
        ("|k: isize|", "|k: isize, other: isize|"),
        ("|k: isize|", "move |k: isize|"),
        ("k + isize", "k - isize"),
        ("from(max)", "from(unknown)"),
        ("from(max)", "from(max + max)"),
        ("isize::MAX", "0"),
        ("unwrap_or(0)", "unwrap_or(1)"),
        ("usize::try_from", "isize::try_from"),
    ] {
        assert!(
            !context.shift(&syn::parse_str(&source.replacen(from, to, 1)).unwrap()),
            "{from} -> {to}"
        );
    }
}

#[test]
fn cursor_discovery_rejects_unresolved_scope_and_oversized_record_fields() {
    use crate::cfg::CfgSet;
    use crate::ops::collect::Defaults;

    let base = include_str!("../fixtures/campaign/survey.rs");
    let discover = |source: &str| {
        let file = syn::parse_file(source).unwrap();
        let expression = first_while(&file);
        let defaults = Defaults::of_source_in(&file, &CfgSet::unconditional(), "cursor.rs");
        Cursor::new(&file, &expression, defaults.resource_sources(), "cursor.rs", &[]).is_some()
    };
    assert!(discover(base));
    for (from, to) in [
        ("struct Frame", "#[cfg(feature = \"x\")] struct Frame"),
        ("struct Frame", "struct Frame<T>"),
        ("node: usize,", "#[cfg(feature = \"x\")] node: usize,"),
        ("fn strongly_connected_components", "#[custom] fn strongly_connected_components"),
        ("fn strongly_connected_components(", "fn strongly_connected_components<T>("),
        ("while let Some(frame)", "declare_helpers!(); while let Some(frame)"),
        ("let node = frame.node;", "let node = frame.node; declare_helpers!();"),
        ("let node = frame.node;", "let node = frame.node; opaque();"),
    ] {
        assert!(!discover(&base.replacen(from, to, 1)), "{from} -> {to}");
    }
    let mut fields = String::new();
    for index in 0..15 {
        write!(fields, "extra_{index}: usize,").unwrap();
    }
    assert!(!discover(&base.replacen(
        "edge_at: usize,",
        &format!("edge_at: usize, {fields}"),
        1
    )));
    let file = syn::parse_file(base).unwrap();
    let expression = first_while(&file);
    let defaults = Defaults::of_source_in(&file, &CfgSet::unconditional(), "cursor.rs");
    assert!(Cursor::new(&file, &expression, defaults.resource_sources(), "cursor.rs", &["nested".into()]).is_none());
}

fn frame_source(prefix: &str) -> String {
    format!(
        "struct Frame {{ node: usize, edge: usize }}
        fn walk(mut frames: Vec<Frame>, edges: Vec<Vec<usize>>, indices: Vec<Option<u32>>,
                marked: Vec<bool>, mut minima: Vec<u32>) {{ {prefix} {FRAME} }}"
    )
}

fn discover_frame(source: &str) -> Option<Cursor> {
    use crate::cfg::CfgSet;
    use crate::ops::collect::Defaults;

    let file = syn::parse_file(source).unwrap();
    let expression = first_while(&file);
    let defaults = Defaults::of_source_in(&file, &CfgSet::unconditional(), "cursor.rs");
    Cursor::new(&file, &expression, defaults.resource_sources(), "cursor.rs", &[])
}

#[test]
fn frame_discovery_preserves_typed_bindings_but_removes_unknown_shadows() {
    for prefix in ["", "let frames: Vec<Frame> = frames;", "{ let edges = (); }"] {
        let context = discover_frame(&frame_source(prefix));
        assert!(context.is_some(), "{prefix}");
        let context = context.unwrap();
        assert!(context.branch(&syn::parse_str(FRAME).unwrap()).is_some(), "{prefix}");
    }
    let source = frame_source("").replace(FRAME, &format!("if true {{ {FRAME} }} else {{ (); }}"));
    assert!(discover_frame(&source).is_some());
    for prefix in [
        "let edges = ();",
        "let edges = unknown();",
        "let (edges, other) = (edges, ());",
        "let (edges, other) = ((), edges);",
        "let (other, edges) = (edges, ());",
        "let node: usize = 0;",
        "let neighbor: usize = 0;",
        "let index: u32 = 0;",
        "let ref edges = edges;",
        "let edges @ _ = edges;",
        "#[allow(unused)] let edges = edges;",
    ] {
        assert!(discover_frame(&frame_source(prefix)).is_none(), "{prefix}");
    }
    let base = frame_source("");
    for argument in [
        "#[allow(unused)] frames: Vec<Frame>",
        "ref frames: Vec<Frame>",
        "frames @ _: Vec<Frame>",
    ] {
        assert!(
            discover_frame(&base.replace("mut frames: Vec<Frame>", argument)).is_none(),
            "{argument}"
        );
    }
    let tuple = frame_source("let (rows, other) = (edges, ());").replace(FRAME, &FRAME.replace("edges", "rows"));
    let context = discover_frame(&tuple).unwrap();
    assert_eq!(context.branch(&syn::parse_str(&FRAME.replace("edges", "rows")).unwrap()), Some(()));
}

#[test]
fn frame_discovery_enforces_block_depth_statement_and_binding_envelopes() {
    for (depth, accepted) in [(14, true), (15, false), (16, false)] {
        let source = frame_source("").replace(FRAME, &format!("{} {FRAME} {}", "{".repeat(depth), "}".repeat(depth)));
        assert_eq!(discover_frame(&source).is_some(), accepted, "depth {depth}");
    }
    for (count, accepted) in [(63, true), (64, false)] {
        assert_eq!(
            discover_frame(&frame_source(&"();".repeat(count))).is_some(),
            accepted,
            "statements {count}"
        );
        let source = frame_source("").replace(FRAME, &format!("{FRAME}{}", "();".repeat(count)));
        assert_eq!(discover_frame(&source).is_some(), accepted, "following statements {count}");
    }
    for (count, accepted) in [(59, true), (60, false)] {
        let mut prefix = String::new();
        for index in 0..count {
            write!(prefix, "let extra_{index}: usize = 0;").unwrap();
        }
        assert_eq!(discover_frame(&frame_source(&prefix)).is_some(), accepted, "bindings {count}");
    }
}

#[test]
fn frame_discovery_enforces_file_and_syntax_size_envelopes() {
    for (count, accepted) in [(254, true), (255, false)] {
        let mut source = frame_source("");
        for index in 0..count {
            write!(source, "const EXTRA_{index}: usize = 0;").unwrap();
        }
        assert_eq!(discover_frame(&source).is_some(), accepted, "extra declarations {count}");
    }
    let base = frame_source("");
    let length = first_while(&syn::parse_file(&base).unwrap()).span().byte_range().len();
    for (limit, accepted) in [(4096, true), (4097, false)] {
        let padding = format!("/*{}*/", " ".repeat(limit - length - 4));
        let source = base.replace("let node = frame.node;", &format!("{padding}let node = frame.node;"));
        assert_eq!(first_while(&syn::parse_file(&source).unwrap()).span().byte_range().len(), limit);
        assert_eq!(discover_frame(&source).is_some(), accepted, "loop bytes {limit}");
    }
    for (limit, accepted) in [(256, true), (257, false)] {
        let ty = format!("Vec</*{}*/Vec<usize>>", " ".repeat(limit - "Vec<Vec<usize>>".len() - 4));
        let source = base.replace("edges: Vec<Vec<usize>>", &format!("edges: {ty}"));
        assert_eq!(discover_frame(&source).is_some(), accepted, "type bytes {limit}");
    }
    for (limit, accepted) in [(1024, true), (1025, false)] {
        let initializer = format!("(/*{}*/frames)", " ".repeat(limit - "(frames)".len() - 4));
        let source = frame_source(&format!("let frames: Vec<Frame> = {initializer};"));
        assert_eq!(discover_frame(&source).is_some(), accepted, "initializer bytes {limit}");
    }
    for (length, accepted) in [(256, true), (257, false)] {
        let ty = format!("Vec</*{}*/Vec<usize>>", " ".repeat(length - "Vec<Vec<usize>>".len() - 4));
        let source = frame_source(&format!("let edges: {ty} = edges;"));
        assert_eq!(discover_frame(&source).is_some(), accepted, "local type bytes {length}");
    }
}

#[test]
fn frame_certificate_accepts_each_supported_primitive_minimum() {
    let expression = syn::parse_str(FRAME).unwrap();
    for primitive in ["bool", "usize", "u8", "u16", "u32", "u64", "u128"] {
        let mut context = frame_context();
        let _previous = context
            .types
            .insert("indices".into(), syn::parse_str(&format!("Vec<Option<{primitive}>>")).unwrap());
        let _previous = context
            .types
            .insert("minima".into(), syn::parse_str(&format!("Vec<{primitive}>")).unwrap());
        assert_eq!(context.branch(&expression), Some(()), "{primitive}");
    }
}

#[test]
fn frame_minimum_must_not_write_the_vector_that_selects_the_visited_branch() {
    use crate::cfg::CfgSet;
    use crate::ops::collect::collect_in;
    use crate::ops::registry::Selection;
    use crate::parse::SourceFile;

    let mut context = frame_context();
    let _previous = context.types.insert("indices".into(), syn::parse_quote!(Vec<Option<bool>>));
    let _previous = context.types.insert("minima".into(), syn::parse_quote!(Vec<bool>));
    let stationary = syn::parse_str(FRAME).unwrap();
    assert_eq!(context.branch(&stationary), Some(()));
    let dependent = FRAME.replace("if marked[neighbor]", "if minima[neighbor]");

    for (body, retained) in [(FRAME, false), (dependent.as_str(), true)] {
        let source = frame_source("")
            .replace(FRAME, body)
            .replace("Option<u32>", "Option<bool>")
            .replace("minima: Vec<u32>", "minima: Vec<bool>")
            .replace("frame.edge += 0;", "frame.edge += 1;");
        let at = source.find("frame.edge += 1;").unwrap() + "frame.edge += ".len();
        let file = SourceFile::parse("cursor.rs", source).unwrap();
        let candidates = collect_in(&file, &Selection::parse("literal.int_decrement").unwrap(), &CfgSet::unconditional());
        assert_eq!(
            candidates
                .iter()
                .any(|candidate| candidate.span.start == at && candidate.replacement == "0"),
            retained
        );
    }
    assert_eq!(context.branch(&syn::parse_str(&dependent).unwrap()), None);
}

#[test]
fn cursor_discovery_counts_expressions_in_the_dominating_condition() {
    for (count, accepted) in [(508, true), (509, true), (510, false)] {
        // The enclosing if, field access and tuple consume three visits before the literals.
        let source = frame_source("").replace(FRAME, &format!("if ({}).0 {{ {FRAME} }}", "true,".repeat(count)));
        assert_eq!(discover_frame(&source).is_some(), accepted, "condition literals {count}");
    }
    let source = frame_source("").replace(FRAME, &format!("if #[custom] true {{ {FRAME} }}"));
    assert!(discover_frame(&source).is_none());
}

#[test]
fn cursor_discovery_retains_records_after_unrelated_or_ineligible_declarations() {
    let base = frame_source("");
    for prefix in ["fn unrelated() {}", "struct Generic<T>(T);", "struct Tuple(usize);"] {
        assert!(discover_frame(&format!("{prefix} {base}")).is_some(), "{prefix}");
    }
    for (count, accepted) in [(14, true), (15, false)] {
        let mut fields = String::new();
        for index in 0..count {
            write!(fields, "extra_{index}: usize,").unwrap();
        }
        let source = base.replace("edge: usize", &format!("edge: usize, {fields}"));
        assert_eq!(discover_frame(&source).is_some(), accepted, "extra fields {count}");
    }
}

#[test]
fn diagonal_discovery_bounds_the_complete_dominating_entry_statement() {
    use crate::cfg::CfgSet;
    use crate::ops::collect::Defaults;

    for (length, accepted) in [(1024, true), (1025, false)] {
        let entry = format!("let mut k = /*{}*/-d;", " ".repeat(length - "let mut k = -d;".len() - 4));
        let source = DIFF.replace("let mut k = -d;", &entry);
        let file = syn::parse_file(&source).unwrap();
        let expression = first_while(&file);
        let defaults = Defaults::of_source_in(&file, &CfgSet::unconditional(), "cursor.rs");
        let context = Cursor::new(&file, &expression, defaults.resource_sources(), "cursor.rs", &[]);
        assert_eq!(context.is_some(), accepted, "entry bytes {length}");
    }
}

#[test]
fn frame_certificate_rejects_metadata_on_each_binding_layer() {
    let context = frame_context();
    for visited in [false, true] {
        for layer in ["expression", "constructor", "identifier"] {
            let mut expression: Expr = syn::parse_str(FRAME).unwrap();
            assert_eq!(context.branch(&expression), Some(()));
            let Expr::While(outer) = &mut expression else { unreachable!() };
            let binding = if visited {
                let Stmt::Expr(Expr::If(edge), _) = &mut outer.body.stmts[1] else {
                    unreachable!()
                };
                let Stmt::Expr(Expr::If(known), _) = &mut edge.then_branch.stmts[2] else {
                    unreachable!()
                };
                let Expr::Let(binding) = &mut *known.cond else { unreachable!() };
                binding
            } else {
                let Expr::Let(binding) = &mut *outer.cond else { unreachable!() };
                binding
            };
            let Pat::TupleStruct(pattern) = &mut *binding.pat else {
                unreachable!()
            };
            let Pat::Ident(ident) = &mut pattern.elems[0] else { unreachable!() };
            let attributes = match layer {
                "expression" => &mut binding.attrs,
                "constructor" => &mut pattern.attrs,
                _ => &mut ident.attrs,
            };
            attributes.push(syn::parse_quote!(#[cfg(disabled)]));
            assert_eq!(context.branch(&expression), None, "visited {visited}, {layer}");
        }
    }
}

#[test]
fn frame_certificate_requires_metadata_free_typed_locals_and_minimum_calls() {
    let context = frame_context();
    let typed = FRAME.replace("let node =", "let node: usize =");
    let mut expression: Expr = syn::parse_str(&typed).unwrap();
    assert_eq!(context.branch(&expression), Some(()));
    let Expr::While(outer) = &mut expression else { unreachable!() };
    let Stmt::Local(local) = &mut outer.body.stmts[0] else {
        unreachable!()
    };
    let Pat::Type(typed) = &mut local.pat else { unreachable!() };
    let Pat::Ident(ident) = &mut *typed.pat else { unreachable!() };
    ident.attrs.push(syn::parse_quote!(#[cfg(disabled)]));
    assert_eq!(context.branch(&expression), None);

    let mut expression: Expr = syn::parse_str(FRAME).unwrap();
    let Expr::While(outer) = &mut expression else { unreachable!() };
    let Stmt::Expr(Expr::If(edge), _) = &mut outer.body.stmts[1] else {
        unreachable!()
    };
    let Stmt::Expr(Expr::If(known), _) = &mut edge.then_branch.stmts[2] else {
        unreachable!()
    };
    let Stmt::Expr(Expr::If(marked), _) = &mut known.then_branch.stmts[0] else {
        unreachable!()
    };
    let Stmt::Expr(Expr::Assign(store), _) = &mut marked.then_branch.stmts[0] else {
        unreachable!()
    };
    let Expr::MethodCall(minimum) = &mut *store.right else {
        unreachable!()
    };
    minimum.attrs.push(syn::parse_quote!(#[cfg(disabled)]));
    assert_eq!(context.branch(&expression), None);
}

#[test]
fn cursor_type_evidence_requires_metadata_free_operations() {
    let mut context = frame_context();
    context.types.extend([
        ("frame".into(), syn::parse_quote!(Frame)),
        ("unregistered".into(), syn::parse_quote!(Unknown)),
        ("slot".into(), syn::parse_quote!(usize)),
        ("offset".into(), syn::parse_quote!(isize)),
    ]);
    for source in [
        "frame.node",
        "edges[slot]",
        "-offset",
        "offset + offset",
        "edges.len()",
        "vec![0_isize; slot]",
    ] {
        let mut expression: Expr = syn::parse_str(source).unwrap();
        assert!(context.ty(&expression, &context.types).is_some(), "{source}");
        let attributes = match &mut expression {
            Expr::Field(node) => &mut node.attrs,
            Expr::Index(node) => &mut node.attrs,
            Expr::Unary(node) => &mut node.attrs,
            Expr::Binary(node) => &mut node.attrs,
            Expr::MethodCall(node) => &mut node.attrs,
            Expr::Macro(node) => &mut node.attrs,
            _ => unreachable!(),
        };
        attributes.push(syn::parse_quote!(#[cfg(disabled)]));
        assert_eq!(context.ty(&expression, &context.types), None, "{source}");
    }
    for invalid in [
        "qualified::slot",
        "unregistered.node",
        "edges[unknown]",
        "unknown.len()",
        "isize::try_from(slot).unwrap_or_default(isize::MAX)",
        "isize::try_from(slot).unwrap_or()",
    ] {
        assert_eq!(context.ty(&syn::parse_str(invalid).unwrap(), &context.types), None, "{invalid}");
    }
    let _previous = context.types.insert("frame".into(), syn::parse_quote!(crate::Frame));
    assert_eq!(context.ty(&syn::parse_quote!(frame.node), &context.types), None);
}

#[test]
fn cursor_discovery_withholds_type_evidence_from_attributed_local_identifiers() {
    use crate::cfg::CfgSet;
    use crate::ops::collect::Defaults;

    let source = frame_source("let edges = edges;");
    let mut file = syn::parse_file(&source).unwrap();
    let defaults = Defaults::of_source_in(&file, &CfgSet::unconditional(), "cursor.rs");
    let expression = first_while(&file);
    assert!(Cursor::new(&file, &expression, defaults.resource_sources(), "cursor.rs", &[]).is_some());
    let Item::Fn(function) = &mut file.items[1] else { unreachable!() };
    let Stmt::Local(local) = &mut function.block.stmts[0] else {
        unreachable!()
    };
    let Pat::Ident(ident) = &mut local.pat else { unreachable!() };
    ident.attrs.push(syn::parse_quote!(#[cfg(disabled)]));
    assert!(Cursor::new(&file, &expression, defaults.resource_sources(), "cursor.rs", &[]).is_none());
}

#[test]
fn diagonal_discovery_certifies_captures_before_later_shadows_but_invalidates_rebound_closures() {
    use crate::cfg::CfgSet;
    use crate::ops::collect::Defaults;

    for (prefix, accepted) in [
        ("let max = opaque();", true),
        ("let max: isize = 0;", true),
        ("let shift: usize = 0;", false),
        ("let (shift, ignored) = (0, 0);", false),
        ("let (ignored, shift) = (0, 0);", false),
    ] {
        let source = DIFF.replace("let mut k = -d;", &format!("{prefix} let mut k = -d;"));
        let file = syn::parse_file(&source).unwrap();
        let expression = first_while(&file);
        let defaults = Defaults::of_source_in(&file, &CfgSet::unconditional(), "cursor.rs");
        assert_eq!(
            Cursor::new(&file, &expression, defaults.resource_sources(), "cursor.rs", &[]).is_some(),
            accepted,
            "{prefix}"
        );
    }
}

#[test]
fn cursor_type_evidence_rejects_noncanonical_vector_paths_and_generic_arguments() {
    let mut context = frame_context();
    let _previous = context.types.insert("slot".into(), syn::parse_quote!(usize));
    let index: Expr = syn::parse_quote!(edges[slot]);
    let _previous = context.types.insert("edges".into(), syn::parse_quote!(Vec<usize>));
    assert_eq!(context.ty(&index, &context.types), Some(syn::parse_quote!(usize)));
    for ty in [
        syn::parse_quote!(Vec<usize, isize>),
        syn::parse_quote!(Other<usize>),
        syn::parse_quote!(Vec<'a>),
        syn::parse_quote!(Vec<usize>::Associated),
        syn::parse_quote!(<Frame>::Vec<usize>),
    ] {
        let _previous = context.types.insert("edges".into(), ty);
        assert_eq!(context.ty(&index, &context.types), None);
    }
    let mut ty: Type = syn::parse_quote!(Vec<usize>);
    let Type::Path(path) = &mut ty else { unreachable!() };
    path.attrs.push(syn::parse_quote!(#[cfg(disabled)]));
    let _previous = context.types.insert("edges".into(), ty);
    assert_eq!(context.ty(&index, &context.types), None);
}

#[test]
fn frame_certificate_rejects_other_typed_frames_not_only_unknown_names() {
    let mut context = frame_context();
    let _previous = context.types.insert("other".into(), syn::parse_quote!(Frame));
    assert_eq!(context.branch(&syn::parse_str(FRAME).unwrap()), Some(()));
    for (from, to) in [("let node = frame.node;", "let node = other.node;"), ("frame.edge", "other.edge")] {
        let expression = syn::parse_str(&FRAME.replace(from, to)).unwrap();
        assert_eq!(context.branch(&expression), None, "{from} -> {to}");
    }
}

#[test]
fn frame_certificate_rejects_incomplete_statement_and_binding_shapes() {
    let mut context = frame_context();
    let _previous = context.types.insert("slot".into(), syn::parse_quote!(usize));
    assert_eq!(context.branch(&syn::parse_str(FRAME).unwrap()), Some(()));
    assert_eq!(context.branch(&syn::parse_quote!(loop {})), None);
    for (from, to) in [
        ("frames.last_mut()", "stacks[slot].last_mut()"),
        ("while let Some(frame) = frames.last_mut()", "while true"),
        ("Some(frame)", "frame"),
        ("let node = frame.node;", ""),
        ("let node = frame.node;", "let node = slot;"),
        ("frame.edge < edges[node].len()", "true"),
        ("let neighbor = edges[node][frame.edge];", "let neighbor = slot;"),
        (
            "if let Some(index) = indices[neighbor] {",
            "while let Some(index) = indices[neighbor] {",
        ),
        ("if let Some(index) = indices[neighbor]", "if true"),
        ("Some(index)", "index"),
        (
            "if marked[neighbor] {\n                    minima[node] = minima[node].min(index);\n                }",
            "minima[node] = minima[node].min(index);",
        ),
        (
            "minima[node] = minima[node].min(index);",
            "minima[node] = minima[node].min(index); } else { opaque();",
        ),
    ] {
        assert!(FRAME.contains(from), "{from}");
        assert_eq!(
            context.branch(&syn::parse_str(&FRAME.replacen(from, to, 1)).unwrap()),
            None,
            "{from}"
        );
    }
    let _previous = context.types.insert("minima".into(), syn::parse_quote!(u32));
    let source = FRAME.replace("minima[node]", "minima");
    assert_eq!(context.branch(&syn::parse_str(&source).unwrap()), None);
    let _previous = context.types.remove("minima");
    assert_eq!(context.branch(&syn::parse_str(FRAME).unwrap()), None);
    let _previous = context.types.insert("minima".into(), syn::parse_quote!(Vec<Vec<u32>>));
    let source = FRAME.replace("minima[node]", "minima[node][neighbor]");
    assert_eq!(context.branch(&syn::parse_str(&source).unwrap()), None);
    let context = frame_context();
    for retained in [0, 1] {
        let mut expression: Expr = syn::parse_str(FRAME).unwrap();
        let Expr::While(outer) = &mut expression else { unreachable!() };
        let Stmt::Expr(Expr::If(edge), _) = &mut outer.body.stmts[1] else {
            unreachable!()
        };
        edge.then_branch.stmts.truncate(retained);
        assert_eq!(context.branch(&expression), None, "edge statements {retained}");
    }
}

#[test]
fn frame_bound_requires_a_cursor_field_even_when_the_comparison_is_well_typed() {
    let context = frame_context();
    assert_eq!(context.branch(&syn::parse_str(FRAME).unwrap()), Some(()));
    let expression = syn::parse_str(&FRAME.replace("frame.edge < edges[node].len()", "0 < edges[node].len()")).unwrap();
    assert_eq!(context.branch(&expression), None);
}

#[test]
fn frame_update_metadata_cannot_certify_an_otherwise_exact_stationary_update() {
    let context = frame_context();
    let mut expression: Expr = syn::parse_str(FRAME).unwrap();
    assert_eq!(context.branch(&expression), Some(()));
    let Expr::While(outer) = &mut expression else { unreachable!() };
    let Stmt::Expr(Expr::If(edge), _) = &mut outer.body.stmts[1] else {
        unreachable!()
    };
    let Expr::Binary(bound) = &*edge.cond else { unreachable!() };
    let Stmt::Expr(Expr::Binary(update), _) = &mut edge.then_branch.stmts[1] else {
        unreachable!()
    };
    assert_eq!(update.left, bound.left);
    assert_eq!(*update.right, syn::parse_quote!(0));
    update.attrs.push(syn::parse_quote!(#[cfg(any())]));
    assert_eq!(context.branch(&expression), None);
}

#[test]
fn cursor_discovery_requires_the_loop_to_start_its_statement_and_known_prefix_metadata() {
    assert!(discover_frame(&frame_source("")).is_some());
    let initializer = frame_source("").replace(FRAME, &format!("let result = {FRAME};"));
    assert!(discover_frame(&initializer).is_none());
    assert!(discover_frame(&frame_source("#[custom] ();")).is_none());
    assert!(discover_frame(&frame_source("let edges;")).is_none());
    assert!(discover_frame(&frame_source("let edges = edges else { return; };")).is_none());
}

#[test]
fn frame_certificate_requires_a_primitive_minimum_even_when_both_vector_types_agree() {
    let expression = syn::parse_str(FRAME).unwrap();
    for primitive in ["String", "isize", "Custom"] {
        let mut context = frame_context();
        let _previous = context
            .types
            .insert("indices".into(), syn::parse_str(&format!("Vec<Option<{primitive}>>")).unwrap());
        let _previous = context
            .types
            .insert("minima".into(), syn::parse_str(&format!("Vec<{primitive}>")).unwrap());
        assert_eq!(context.branch(&expression), None, "{primitive}");
    }
}

#[test]
fn diagonal_certificate_rejects_metadata_on_the_bound_escape_and_optional_update() {
    let context = diagonal_context();
    let source = DIFF.replace("k += 2;", "k += 0;");
    for layer in ["loop", "bound", "cursor", "escape", "update"] {
        let mut expression = first_while(&syn::parse_file(&source).unwrap());
        assert_eq!(context.diagonal(&expression), Some(()));
        let Expr::While(outer) = &mut expression else { unreachable!() };
        let Expr::Binary(bound) = &mut *outer.cond else { unreachable!() };
        let Expr::Path(cursor) = &mut *bound.left else { unreachable!() };
        let attributes = match layer {
            "loop" => &mut outer.attrs,
            "bound" => &mut bound.attrs,
            "cursor" => &mut cursor.attrs,
            "escape" => {
                let Stmt::Expr(Expr::If(escape), _) = &mut outer.body.stmts[5] else {
                    unreachable!()
                };
                &mut escape.attrs
            }
            _ => {
                let Stmt::Expr(Expr::Binary(update), _) = &mut outer.body.stmts[6] else {
                    unreachable!()
                };
                &mut update.attrs
            }
        };
        attributes.push(syn::parse_quote!(#[cfg(disabled)]));
        assert_eq!(context.diagonal(&expression), None, "{layer}");
    }
    let source = source.replace("while k <= d", "while <Frame>::k <= d");
    assert_eq!(context.diagonal(&first_while(&syn::parse_file(&source).unwrap())), None);
}

#[test]
fn diagonal_certificate_rejects_incomplete_scan_and_initializer_shapes() {
    let context = diagonal_context();
    let source = DIFF.replace("k += 2;", "k += 0;");
    assert_eq!(context.diagonal(&first_while(&syn::parse_file(&source).unwrap())), Some(()));
    assert_eq!(context.diagonal(&syn::parse_quote!(loop {})), None);
    for statement in 0..3 {
        let mut expression = first_while(&syn::parse_file(&source).unwrap());
        let Expr::While(outer) = &mut expression else { unreachable!() };
        outer.body.stmts[statement] = syn::parse_quote!(opaque(););
        assert_eq!(context.diagonal(&expression), None, "initializer statement {statement}");
    }
    for (from, to) in [
        ("while k <= d", "while k <= (d)"),
        ("let mut y = x - k;", ""),
        (
            "let down = k == -d || (k != d && furthest[shift(k - 1)] < furthest[shift(k + 1)]);",
            "let down = true;",
        ),
        (
            "if down { furthest[shift(k + 1)] } else { furthest[shift(k - 1)] + 1 }",
            "furthest[shift(k + 1)]",
        ),
        ("if down { furthest[shift(k + 1)] }", "if down { opaque(); furthest[shift(k + 1)] }"),
        ("if down { furthest[shift(k + 1)] }", "if down { furthest[slot] }"),
        ("&& yi < m", ""),
        ("let (Ok(xi), Ok(yi))", "let Ok(xi)"),
        ("let (Ok(xi), Ok(yi))", "let (Ok(xi),)"),
        ("let (Ok(xi), Ok(yi))", "let (Ok(xi), Ok(yi), ignored)"),
        ("let (Ok(xi), Ok(yi))", "let (xi, Ok(yi))"),
        ("let (Ok(xi), Ok(yi))", "let (Ok(_), Ok(yi))"),
        ("old[xi] == new[yi]", "xi == yi"),
        ("xi < n", "xi < n + 1"),
        ("yi < m", "yi < m + 1"),
        ("old[xi]", "(old)[xi]"),
        ("new[yi]", "(new)[yi]"),
        ("if down { furthest[shift(k + 1)] }", "if down { (furthest)[shift(k + 1)] }"),
        ("if down { furthest[shift(k + 1)] }", "if down { furthest[(&shift)(k + 1)] }"),
        ("old[xi] == new[yi]", "old[xi] == new[yi] && true"),
        ("old[xi] == new[yi]", "old[xi] == new[yi] && true && true"),
        ("k += 0;", "other += 0;"),
    ] {
        assert!(source.contains(from), "{from}");
        let changed = source.replacen(from, to, 1);
        let expression = first_while(&syn::parse_file(&changed).unwrap());
        assert_eq!(context.diagonal(&expression), None, "{from} -> {to}");
    }
}

#[test]
fn frame_certificate_rejects_metadata_on_the_bound_lookup_and_store() {
    let context = frame_context();
    for layer in ["bound", "lookup", "store"] {
        let mut expression: Expr = syn::parse_str(FRAME).unwrap();
        assert_eq!(context.branch(&expression), Some(()));
        let Expr::While(outer) = &mut expression else { unreachable!() };
        let Stmt::Expr(Expr::If(edge), _) = &mut outer.body.stmts[1] else {
            unreachable!()
        };
        let attributes = match layer {
            "bound" => {
                let Expr::Binary(bound) = &mut *edge.cond else { unreachable!() };
                &mut bound.attrs
            }
            "lookup" => {
                let Stmt::Local(neighbor) = &mut edge.then_branch.stmts[0] else {
                    unreachable!()
                };
                let Expr::Index(index) = &mut *neighbor.init.as_mut().unwrap().expr else {
                    unreachable!()
                };
                &mut index.attrs
            }
            _ => {
                let Stmt::Expr(Expr::If(known), _) = &mut edge.then_branch.stmts[2] else {
                    unreachable!()
                };
                let Stmt::Expr(Expr::If(marked), _) = &mut known.then_branch.stmts[0] else {
                    unreachable!()
                };
                let Stmt::Expr(Expr::Assign(store), _) = &mut marked.then_branch.stmts[0] else {
                    unreachable!()
                };
                &mut store.attrs
            }
        };
        attributes.push(syn::parse_quote!(#[cfg(disabled)]));
        assert_eq!(context.branch(&expression), None, "{layer}");
    }
}

#[test]
fn diagonal_certificate_requires_distinct_names_even_when_the_rest_of_the_scan_is_canonical() {
    let context = diagonal_context();
    let source = DIFF.replace("k += 2;", "k += 0;");
    assert_eq!(context.diagonal(&first_while(&syn::parse_file(&source).unwrap())), Some(()));
    let source = source.replace("down", "x");
    assert_eq!(context.diagonal(&first_while(&syn::parse_file(&source).unwrap())), None);
}

#[test]
fn cursor_discovery_requires_known_file_and_function_declaration_attributes() {
    use crate::cfg::CfgSet;
    use crate::ops::collect::Defaults;

    let source = frame_source("");
    let file = syn::parse_file(&source).unwrap();
    let expression = first_while(&file);
    let defaults = Defaults::of_source_in(&file, &CfgSet::unconditional(), "cursor.rs");
    assert!(Cursor::new(&file, &expression, defaults.resource_sources(), "cursor.rs", &[]).is_some());
    for function in [false, true] {
        let mut file = file.clone();
        let attributes = if function {
            let Item::Fn(function) = &mut file.items[1] else { unreachable!() };
            &mut function.attrs
        } else {
            &mut file.attrs
        };
        attributes.push(syn::parse_quote!(#[custom]));
        assert!(Cursor::new(&file, &expression, defaults.resource_sources(), "cursor.rs", &[]).is_none());
    }
}
