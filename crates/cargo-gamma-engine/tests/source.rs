// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use cargo_gamma_engine::ops::collect::{collect, into_definitions};
use cargo_gamma_engine::ops::registry::Selection;
use cargo_gamma_engine::parse::nesting::{CHAIN_FACTOR, NESTING_LIMIT};
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
fn operand_keywords_are_bounded_before_recursive_parsing() {
    let repetitions = if cfg!(miri) { NESTING_LIMIT * CHAIN_FACTOR + 1 } else { 1000 };
    for keyword in [
        "return ",
        "break ",
        "break 'out ",
        "break /* label */ 'out /* value */ ",
        "break 'r#out ",
        "yield ",
        "become ",
    ] {
        let deep = format!("fn f() {{ {}1 }}", keyword.repeat(repetitions));
        assert!(exceeds_nesting_limit(&deep), "{keyword}");
        assert!(SourceFile::parse("deep.rs", deep).unwrap_err().is_skippable());
        assert!(!exceeds_nesting_limit(&format!("fn f() {{ {}1 }}", keyword.repeat(8))));
    }

    assert!(!exceeds_nesting_limit(&"fn f() { return_value(); }".repeat(repetitions)));
}

#[test]
fn bom_and_shebang_are_normalized_before_both_nesting_checks() {
    let repetitions = if cfg!(miri) { NESTING_LIMIT * CHAIN_FACTOR + 1 } else { 1000 };
    let interpreter = format!("#!/{}{}", "(".repeat(repetitions), "return ".repeat(repetitions));
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
    let deep = format!("\u{feff}#!/x\nfn f() {{ {}1 }}", "return ".repeat(repetitions));
    assert!(exceeds_nesting_limit(&deep));
    assert!(SourceFile::parse("deep.rs", deep).unwrap_err().is_skippable());
}
