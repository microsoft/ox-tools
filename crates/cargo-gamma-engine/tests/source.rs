// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use cargo_gamma_engine::ops::collect::{collect, into_definitions};
use cargo_gamma_engine::ops::registry::Selection;
use cargo_gamma_engine::parse::nesting::NESTING_LIMIT;
use cargo_gamma_engine::parse::{SourceFile, exceeds_nesting_limit};
use cargo_gamma_engine::schema::{AssignedMutant, Ordinal, instrument};

#[test]
fn shebang_spans_and_instrumentation_preserve_the_interpreter_line() {
    for prefix in ["#!/x\n", "#!/usr/bin/env -S cargo +nightly -Zscript\n", "#!/é\r\n"] {
        let file = SourceFile::parse("script.rs", format!("{prefix}fn f(v: u8) -> bool {{ 1 < 2 }}")).unwrap();
        let found = into_definitions(
            &file,
            collect(&file, &Selection::parse("relational.lt_to_le,parameter.default_shadow").unwrap()),
        );
        let comparison = found
            .iter()
            .find(|mutant| mutant.mutator.as_ref() == "relational.lt_to_le")
            .unwrap();
        assert_eq!(file.slice(comparison.span()), "1 < 2");
        assert_eq!(comparison.site.original, "1 < 2");
        assert_eq!(file.location(comparison.span().start).0, 2);
        let assigned = [AssignedMutant::from_parts(
            Ordinal::new(1),
            comparison.span(),
            &comparison.replacement,
            comparison.shape,
        )];
        let rendered = instrument(file.text(), &assigned).unwrap();
        assert!(rendered.starts_with(prefix), "{rendered}");
        syn::parse_file(&rendered).unwrap();
    }
    let file = SourceFile::parse(
        "script.rs",
        "#!/usr/bin/env -S cargo +nightly -Zscript\nfn f(v: u8) { g(v) }".into(),
    )
    .unwrap();
    assert!(!collect(&file, &Selection::parse("parameter.default_shadow").unwrap()).is_empty());
    for prefix in ["#![allow(unused)]", "#! [allow(unused)]", "#! /* not a shebang */ [allow(unused)]"] {
        let file = SourceFile::parse("attribute.rs", format!("{prefix}\nfn f() -> bool {{ 1 < 2 }}")).unwrap();
        assert!(file.ast().shebang.is_none(), "{prefix}");
        assert_eq!(file.ast().attrs.len(), 1);
        let found = collect(&file, &Selection::parse("relational.lt_to_le").unwrap());
        assert_eq!(file.slice(&found[0].span), "1 < 2");
    }
}

#[test]
#[cfg_attr(
    miri,
    ignore = "native parser stress; small_operand_keyword_budget runs the same guard with a small test-util limit"
)]
fn operand_keywords_are_bounded_before_recursive_parsing() {
    for keyword in [
        "return ",
        "break ",
        "break 'out ",
        "break /* label */ 'out /* value */ ",
        "break 'r#out ",
        "yield ",
        "become ",
    ] {
        let deep = format!("fn f() {{ {}1 }}", keyword.repeat(1000));
        assert!(exceeds_nesting_limit(&deep), "{keyword}");
        let error = SourceFile::parse("deep.rs", deep).unwrap_err();
        assert!(error.is_skippable());
        assert!(!error.is_usage());
        assert_eq!(
            error.to_string(),
            format!(
                "deep.rs:1: nests deeper than {NESTING_LIMIT} levels of brackets, prefix operators, chained operators or postfix expressions"
            )
        );
        assert!(!exceeds_nesting_limit(&format!("fn f() {{ {}1 }}", keyword.repeat(8))));
    }

    assert!(!exceeds_nesting_limit(&"fn f() { return_value(); }".repeat(1000)));
}

#[test]
#[cfg_attr(
    miri,
    ignore = "native parser stress; small_bom_and_shebang_budget covers normalization under Miri"
)]
fn bom_and_shebang_are_normalized_before_both_nesting_checks() {
    let interpreter = format!("#!/{}{}", "(".repeat(1000), "return ".repeat(1000));
    for ending in ["\n", "\r\n", ""] {
        let body = if ending.is_empty() { "" } else { "fn f() {}" };
        let text = format!("\u{feff}{interpreter}{ending}{body}");
        assert!(!exceeds_nesting_limit(&text));
        let file = SourceFile::parse("script.rs", text).unwrap();
        assert_eq!(
            file.ast().shebang.as_deref(),
            Some(format!("{interpreter}{}", ending.trim_end_matches('\n')).as_str())
        );
    }

    let deep = format!("\u{feff}#!/x\nfn f() {{ {}1 }}", "return ".repeat(1000));
    assert!(exceeds_nesting_limit(&deep));
    let error = SourceFile::parse("deep.rs", deep).unwrap_err();
    assert!(error.is_skippable());
    assert!(!error.is_usage());
    assert_eq!(
        error.to_string(),
        format!(
            "deep.rs:2: nests deeper than {NESTING_LIMIT} levels of brackets, prefix operators, chained operators or postfix expressions"
        )
    );
}

#[cfg(feature = "test-util")]
#[test]
fn small_operand_keyword_budget() {
    use cargo_gamma_engine::parse::test_util;

    let keywords: &[&str] = if cfg!(miri) {
        &["break 'x "]
    } else {
        &[
            "return ",
            "break ",
            "break 'out ",
            "break /* label */ 'out /* value */ ",
            "break 'r#out ",
            "yield ",
            "become ",
        ]
    };
    for keyword in keywords {
        let boundary = format!("fn f() {{ {}1 }}", keyword.repeat(4));
        let overflow = format!("fn f() {{ {}1 }}", keyword.repeat(5));
        assert!(!test_util::exceeds_nesting_limit(&boundary, 2));
        assert!(test_util::exceeds_nesting_limit(&overflow, 2));
        let error = test_util::parse("deep.rs", overflow, 2).unwrap_err();
        assert!(error.is_skippable());
        assert!(!error.is_usage());
        assert_eq!(
            error.to_string(),
            "deep.rs:1: nests deeper than 2 levels of brackets, prefix operators, chained operators or postfix expressions"
        );
    }
    assert!(!test_util::exceeds_nesting_limit("return_value", 2));
}

#[cfg(feature = "test-util")]
#[test]
fn small_bom_and_shebang_budget() {
    use cargo_gamma_engine::parse::test_util;

    let endings: &[&str] = if cfg!(miri) { &["\r\n"] } else { &["\n", "\r\n", ""] };
    let interpreter = "#!/(((((((((!!!!!!!!!";
    for ending in endings {
        let text = format!("\u{feff}{interpreter}{ending}");
        assert!(!test_util::exceeds_nesting_limit(&text, 2));
        let file = test_util::parse("script.rs", text, 2).unwrap();
        assert!(file.has_bom());
        assert_eq!(file.text(), format!("{interpreter}{ending}"));
        assert_eq!(
            file.ast().shebang.as_deref(),
            Some(format!("{interpreter}{}", ending.trim_end_matches('\n')).as_str())
        );
        assert_eq!(file.ast().items.len(), 0);
    }
}
