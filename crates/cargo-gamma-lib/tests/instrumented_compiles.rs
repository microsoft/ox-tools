// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![cfg(not(miri))]

//! Every mutant this tool generates has to compile.
//!
//! Parsing is not the same question. The instrumented text is spliced together from source
//! fragments, and a splice can parse perfectly while binding a name that is out of scope, moving a
//! value twice, or leaving a `match` the compiler no longer considers exhaustive. Each of those
//! reaches the user as an unviable mutant: a full build round spent to learn nothing, on a tool
//! whose whole cost is builds.
//!
//! Legacy compiler-backed cases hand the instrumented text to `rustc`. New admission regressions
//! check collection, replacements, and schema rendering in-process; compiled correctness fixtures
//! separately exercise active and inactive guards without launching child processes.

use std::env;
use std::env::consts::{DLL_PREFIX, DLL_SUFFIX};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::OnceLock;

use cargo_gamma_lib::internals::cfg::CfgSet;
use cargo_gamma_lib::internals::ops::collect;
use cargo_gamma_lib::internals::ops::registry::Selection;
use cargo_gamma_lib::internals::parse::SourceFile;
use cargo_gamma_lib::internals::schema;

/// A directory private to this test process.
///
/// Nextest runs each test in a separate process, so a fixed system-temporary path lets several
/// independent `OnceLock`s compile the same stub files concurrently. Including the pid keeps the
/// shared stubs shared within one process and isolated across parallel test processes.
fn workdir() -> PathBuf {
    env::temp_dir().join(format!("cargo-gamma-instrumented-compiles-{}", process::id()))
}

/// Builds the stub the guards call, once for the whole test binary.
///
/// The guard path is `::gamma_rt::a`, which names an external crate rather than anything the
/// instrumented file could declare for itself, so there has to be a real crate to point at.
///
/// # Panics
///
/// Panics when the stub cannot be built, rather than reporting a skip. Every host that can run
/// these tests has just compiled this crate with the very `rustc` this reaches for, so a failure
/// here is a broken invocation rather than an unequipped machine — and a skip would be reported as
/// a count no assertion in this file can tell from a real one, which is how the whole suite could
/// pass without a single instrumented fixture reaching a compiler.
fn guard_crate() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();

    BUILT
        .get_or_init(|| {
            let directory = workdir();

            std::fs::create_dir_all(&directory).expect("the shared work directory must be creatable");

            let source = directory.join("gamma_rt.rs");
            let library = directory.join("libgamma_rt.rlib");

            // Always false, so the compiler sees both branches as live and type-checks the
            // replacement as well as the original. A `const true` would let it discard one.
            //
            // Use the real iterator adapter and type-preserving helpers so this stub cannot
            // introduce compilation failures absent from the vendored runtime.
            let (_, either) = gamma_rt::embedded::SOURCES
                .iter()
                .find(|(path, _)| *path == "either.rs")
                .expect("the embedded runtime implements its iterator adapter in either.rs");
            let mutation = include_str!("fixtures/correctness/runtime_mutation.rs");
            let stub = format!("#[inline] pub fn a(_ordinal: u32) -> bool {{ std::hint::black_box(false) }}\n{either}\n{mutation}");

            std::fs::write(&source, stub).expect("the guard stub source must be writable");

            let built = Command::new(rustc())
                .args(["--edition", "2024", "--crate-type", "lib", "--crate-name", "gamma_rt", "-o"])
                .arg(&library)
                .arg(&source)
                .output()
                .unwrap_or_else(|cause| {
                    panic!(
                        "`{}` could not be run to build the guard stub, so no instrumented fixture in this file was compiled: {cause}",
                        rustc()
                    )
                });

            assert!(
                built.status.success(),
                "the guard stub did not compile, so no instrumented fixture in this file was compiled\n--- rustc ---\n{}",
                String::from_utf8_lossy(&built.stderr)
            );

            library
        })
        .as_path()
}

fn rustc() -> String {
    env::var("RUSTC").unwrap_or_else(|_missing| "rustc".to_owned())
}

#[test]
fn configured_boolean_blocks_preserve_statement_grammar_and_selected_values() {
    let selection = Selection::parse("bool_expr.negate").unwrap();
    for attributes in ["#[cfg(all())]", "#[cfg_attr(all(), cfg(all()))] #[allow(unused_braces)]"] {
        for (expression, other) in [
            (
                "{ let left = left.trim(); left.eq_ignore_ascii_case(right) }",
                "match (left, right) { (a, b) => a == b }",
            ),
            ("match (left.trim(), right) { (a, b) => a.eq_ignore_ascii_case(b) }", "{ false }"),
        ] {
            for (layout, body) in [
                format!("{attributes} {expression} #[cfg(any())] {other}"),
                format!("#[cfg(any())] {other} {attributes} {expression}"),
            ]
            .into_iter()
            .enumerate()
            {
                let source = format!(
                    r#"
                extern crate self as gamma_rt;
                pub fn a(_ordinal: u32) -> bool {{ std::hint::black_box(false) }}
                fn same(left: &str, right: &str) -> bool {{ {body} }}
                fn main() {{ print!("{{}} {{}}", same(" a ", "A"), same("a", "b")); }}
                "#
                );
                let file = SourceFile::parse("subject.rs", source.clone()).unwrap();
                let defaults = collect::Defaults::of(file.ast());
                let candidates = collect::collect_with(&file, &selection, &CfgSet::parse(""), &defaults)
                    .into_iter()
                    .filter(|candidate| candidate.item_path.as_ref() == "same" && file.slice(&candidate.span).trim() == expression)
                    .collect();
                let mut mutants = collect::into_mutants(&file, "subject", candidates);
                assert_eq!(mutants.len(), 1, "{body}");
                mutants[0].ordinal = 1;
                let mutant = &mutants[0];
                assert_eq!(rust_tokens(&mutant.replacement), rust_tokens(&format!("!({expression})")));
                let instrumented = schema::instrument(&source, &[mutant]).unwrap();
                syn::parse_file(&instrumented).unwrap();
                let guard = format!("if ::gamma_rt::a(1u32) {{ !({expression}) }} else {{ {expression} }}");
                let guarded = if layout == 0 {
                    format!("{{ {guard} }}")
                } else {
                    format!("({guard})")
                };
                let expected = source.replacen(expression, &guarded, 1);
                assert_eq!(rust_tokens(&instrumented), rust_tokens(&expected), "{body}");
            }
        }
    }
}

fn rust_tokens(source: &str) -> String {
    source
        .parse::<proc_macro2::TokenStream>()
        .expect("test fixtures must contain valid Rust tokens")
        .to_string()
}

fn schema_replacements(source: &str, selectors: &str) -> Vec<(String, String)> {
    let file = SourceFile::parse("subject.rs", source.to_owned()).expect("test fixtures must contain valid Rust source");
    let selection = Selection::parse(selectors).expect("test selectors must name registered mutators");
    let candidates = collect::collect_with(&file, &selection, &CfgSet::unconditional(), &collect::Defaults::of(file.ast()));
    let mut mutants = collect::into_mutants(&file, "subject", candidates);
    for (index, mutant) in mutants.iter_mut().enumerate() {
        mutant.ordinal = u32::try_from(index + 1).expect("the small fixture candidate count fits u32");
    }
    let instrumented =
        schema::instrument(source, &mutants.iter().collect::<Vec<_>>()).expect("collected mutations must form nonconflicting schema edits");
    syn::parse_file(&instrumented).expect("mutation schema rendering must preserve Rust syntax");
    assert_eq!(instrumented.matches("::gamma_rt::a(").count(), mutants.len());
    let mut replacements = mutants
        .iter()
        .map(|mutant| (mutant.mutator.to_string(), rust_tokens(&mutant.replacement)))
        .collect::<Vec<_>>();
    replacements.sort();
    replacements
}

#[test]
fn ownership_generation_nested_generic_retry_schema_preserves_required_restorations() {
    let source = r"
        fn retry<P>(mut prepared: P, mut spawn: impl FnMut(P) -> Result<(), P>,
                    mut backoff: impl FnMut(P) -> P) {
            let mut waited = std::time::Duration::from_secs(1);
            for _ in 0..3 {
                match spawn(prepared) {
                    Ok(()) => return,
                    Err(returned) => { prepared = backoff(returned); }
                }
                waited = waited.saturating_mul(2);
            }
            let _ = spawn(prepared);
        }";
    assert_eq!(
        schema_replacements(source, "stmt.delete_assign"),
        [("stmt.delete_assign".into(), String::new())]
    );
    let nested = r"
        fn consume(_: String) {}
        fn make() -> String { String::new() }
        fn use_value(_: String) {}
        fn f(mut value: String) { consume(value); { value = make(); use_value(value); } }";
    assert_eq!(schema_replacements(nested, "stmt.delete_assign"), []);
}

#[test]
fn ownership_generation_returned_owner_and_alternate_copy_exit_remain_distinct() {
    let source = r"
        fn acquire(mut make: impl FnMut() -> String, paused: bool) -> String {
            loop {
                let permit = make();
                if paused { drop(permit); continue; }
                return permit;
            }
        }";
    assert_eq!(schema_replacements(source, "loop.delete_continue"), []);
    assert_eq!(
        schema_replacements(&source.replace("String", "u32"), "loop.delete_continue"),
        [("loop.delete_continue".into(), String::new())]
    );
    let value_arm = r"
        fn required(flag: bool) -> u32 {
            loop {
                let _: u32 = match flag { true => { let _mark = 1; continue; }, false => 7 };
                return 0;
            }
        }";
    assert_eq!(schema_replacements(value_arm, "loop.delete_continue"), []);
}

#[test]
fn ownership_generation_moved_exit_keeps_only_viable_neighbors() {
    let source = r"
        use std::cell::Cell;
        fn notify(progress: &Cell<usize>) { progress.set(progress.get() + 1); }
        fn moved_exit(mut line: String, word: &str, out: &mut Vec<String>, progress: &Cell<usize>) {
            for flag in [false, true] {
                if flag {
                    out.push(line);
                    notify(progress);
                    break;
                }
                line.push_str(word);
            }
        }";
    let selectors = "loop.delete_break,loop.break_to_continue";
    assert_eq!(schema_replacements(source, selectors), []);
    assert_eq!(
        schema_replacements(&source.replace("out.push(line)", "out.push(line.clone())"), selectors),
        [
            ("loop.break_to_continue".into(), "continue".into()),
            ("loop.delete_break".into(), String::new())
        ]
    );
    assert_eq!(
        schema_replacements(
            &source.replace("line.push_str(word);", "line = String::new(); line.push_str(word);"),
            selectors
        ),
        [(
            "loop.delete_break".into(),
            rust_tokens(
                "for flag in [false, true] { if flag { out.push(line); notify(progress); } line = String::new(); line.push_str(word); }"
            ),
        )]
    );
}

#[test]
fn ownership_generation_path_collection_reversals_preserve_move_order() {
    let source = r#"
        fn paths(directory: &std::path::Path, name: String) -> Vec<std::path::PathBuf> {
            vec![directory.join(format!("{name}.rs")), directory.join(name).join("mod.rs")]
        }"#;
    assert_eq!(schema_replacements(source, "collection.reverse_vec"), []);
    assert_eq!(
        schema_replacements(&source.replace("join(name)", "join(&name)"), "collection.reverse_vec"),
        [(
            "collection.reverse_vec".into(),
            rust_tokens(r#"vec![directory.join(&name).join("mod.rs"), directory.join(format!("{name}.rs"))]"#)
        )]
    );
    assert_eq!(
        schema_replacements(
            "fn owned(first: String, second: String) -> Vec<String> { vec![first, second] }",
            "collection.reverse_vec"
        ),
        [("collection.reverse_vec".into(), rust_tokens("vec![second, first]"))]
    );
    assert_eq!(
        schema_replacements(
            r#"fn owned(name: String) -> [String; 2] { [format!("{name}"), name] }"#,
            "collection.reverse_array"
        ),
        []
    );
    let fields = r"
        struct Pair { left: String, right: String }
        fn borrow(value: &String) -> usize { value.len() }
        fn take(value: String) -> usize { value.len() }
        fn fields(pair: Pair) -> [usize; 2] { [borrow(&pair.left), take(pair.right)] }";
    assert_eq!(
        schema_replacements(fields, "collection.reverse_array"),
        [(
            "collection.reverse_array".into(),
            rust_tokens("[take(pair.right), borrow(&pair.left)]")
        )]
    );
    assert_eq!(
        schema_replacements(&fields.replace("take(pair.right)", "take(pair.left)"), "collection.reverse_array"),
        []
    );
    assert_eq!(
        schema_replacements("fn copy(value: u32) -> [u32; 2] { [value, 0] }", "collection.reverse_array"),
        [("collection.reverse_array".into(), rust_tokens("[0, value]"))]
    );
}

/// Builds a stand-in for the `gamma` attribute crate, once for the whole test binary.
///
/// A fixture that states its own return values still carries `#[gamma::value(...)]` after
/// instrumentation — the tool reads the attribute, it does not remove it — so the compiler has to
/// have something to resolve it against. The real crate is a proc-macro crate the workspace
/// already builds, but reaching a cargo artifact from a rustc invocation is more fragile than
/// rebuilding the two lines that matter: the attribute is inert, so a stub that returns the item
/// untouched is the same thing to a type-check.
fn attrs_crate() -> Option<&'static Path> {
    static BUILT: OnceLock<Option<PathBuf>> = OnceLock::new();

    BUILT
        .get_or_init(|| {
            let directory = workdir();

            std::fs::create_dir_all(&directory).ok()?;

            let source = directory.join("gamma_attrs.rs");
            let library = directory.join(format!("{DLL_PREFIX}gamma{DLL_SUFFIX}"));

            let stub = concat!(
                "extern crate proc_macro;\n",
                "use proc_macro::TokenStream;\n",
                "#[proc_macro_attribute]\n",
                "pub fn value(_attr: TokenStream, item: TokenStream) -> TokenStream { item }\n",
            );

            std::fs::write(&source, stub).ok()?;

            let built = Command::new(rustc())
                .args(["--edition", "2024", "--crate-type", "proc-macro", "--crate-name", "gamma", "-o"])
                .arg(&library)
                .arg(&source)
                .output()
                .ok()?;

            built.status.success().then_some(library)
        })
        .as_deref()
}

/// Instruments `source` with every mutant `mutators` selects and type-checks the result.
///
/// Returns how many mutants were spliced in, so a test cannot pass by generating nothing at all —
/// which is the failure mode a compile check is least able to notice on its own.
#[track_caller]
fn compiles(name: &str, source: &str, mutators: &str) -> usize {
    let selection = Selection::parse(mutators).expect("the selector must resolve");

    compiles_selection(name, source, &selection)
}

#[track_caller]
fn compiles_selection(name: &str, source: &str, selection: &Selection) -> usize {
    compiles_expected(name, source, selection, None)
}

#[track_caller]
fn compiles_expected(name: &str, source: &str, selection: &Selection, expected_count: Option<NonZeroUsize>) -> usize {
    let guard = guard_crate();

    let file = SourceFile::parse("subject.rs", source.to_owned()).expect("the subject must parse");
    let defaults = collect::Defaults::of(file.ast());
    let candidates = collect::collect_with(&file, selection, &CfgSet::unconditional(), &defaults);
    assert!(!candidates.is_empty(), "{name}: compile oracle produced no candidates");
    if let Some(expected_count) = expected_count {
        assert_eq!(candidates.len(), expected_count.get(), "{name}: fixture population changed");
    }
    let mutants = collect::into_mutants(&file, "subject", candidates);
    let refs: Vec<&_> = mutants.iter().collect();

    let instrumented = schema::instrument(file.text(), &refs).expect("the mutants must splice");
    let directory = workdir();
    let path = directory.join(format!("{name}.rs"));

    std::fs::write(&path, &instrumented).expect("the instrumented source must be writable");

    let mut rustc = Command::new(rustc());

    let _args = rustc
        .args(["--edition", "2024", "--crate-type", "lib", "--emit", "metadata"])
        .arg("--extern")
        .arg(format!("gamma_rt={}", guard.display()))
        .arg("-o")
        .arg(directory.join(format!("{name}.rmeta")))
        .arg(&path);

    // Only when it built: a host that cannot compile a proc-macro crate still answers every
    // question that does not involve one, and the fixtures that do involve one say so themselves
    // by failing to resolve the attribute.
    if let Some(attrs) = attrs_crate() {
        let _extern = rustc.arg("--extern").arg(format!("gamma={}", attrs.display()));
    }

    let checked = rustc.output().expect("rustc must run");

    assert!(
        checked.status.success(),
        "the instrumented source does not compile\n--- source ---\n{instrumented}\n--- rustc ---\n{}",
        String::from_utf8_lossy(&checked.stderr)
    );

    mutants.len()
}

#[test]
#[cfg(feature = "system-tests")]
fn previously_uncovered_mutator_families_compile_nonempty_schemas() {
    let cases = [
        (
            "assign.add_to_sub",
            "pub fn f(mut value: i32, other: i32) { value += other; std::hint::black_box(value); }",
        ),
        ("bitwise.and_to_or", "pub fn f(value: u8, other: u8) -> u8 { value & other }"),
        ("cond.negate", "pub fn f(flag: bool) -> u8 { if flag { 7 } else { 9 } }"),
        (
            "fallback.unwrap_or_to_default",
            "pub fn f(value: Option<u8>) -> u8 { value.unwrap_or(7) }",
        ),
        (
            "match_arm.never_matches",
            "pub fn f(value: u8) -> u8 { match value { 1 => 7, _ => 9 } }",
        ),
        (
            "match_guard.negate",
            "pub fn f(value: u8) -> u8 { match value { value if value > 1 => 7, _ => 9 } }",
        ),
        (
            "regex.remove_start_anchor",
            "struct Regex; impl Regex { fn new(_: &str) -> Self { Self } } pub fn f() { let _ = Regex::new(\"^abc\"); }",
        ),
        ("shift.shl_to_shr", "pub fn f(value: u8, count: u32) -> u8 { value << count }"),
        (
            "struct_field.omit",
            "#[derive(Default)] pub struct Record { value: u8 } pub fn f(value: u8) -> Record { Record { value, ..Default::default() } }",
        ),
    ];
    for (selector, source) in cases {
        let name = selector.replace('.', "_");
        let selection = Selection::parse(selector).unwrap();
        assert_eq!(compiles_expected(&name, source, &selection, Some(NonZeroUsize::MIN)), 1);
    }
}

/// Instruments `source` and returns what `rustc` said about it, insisting it was rejected.
///
/// The mirror of [`compiles`], for the one case where a mutant is *supposed* to be unviable: a
/// value the source states that its own signature does not accept. The tool never type-checks, so
/// the only honest account of what happens next is the compiler's, and it is worth having in a
/// test rather than in prose.
#[track_caller]
fn does_not_compile(name: &str, source: &str, mutators: &str) -> String {
    let guard = guard_crate();

    let file = SourceFile::parse("subject.rs", source.to_owned()).expect("the subject must parse");
    let selection = Selection::parse(mutators).expect("the selector must resolve");
    let candidates = collect::collect(&file, &selection);
    let mutants = collect::into_mutants(&file, "subject", candidates);

    assert!(
        !mutants.is_empty(),
        "the mutant was never generated, so nothing was left for the compiler to reject"
    );

    let refs: Vec<&_> = mutants.iter().collect();
    let instrumented = schema::instrument(file.text(), &refs).expect("the mutants must splice");
    let directory = workdir();
    let path = directory.join(format!("{name}.rs"));

    std::fs::write(&path, &instrumented).expect("the instrumented source must be writable");

    let mut rustc = Command::new(rustc());

    let _args = rustc
        .args(["--edition", "2024", "--crate-type", "lib", "--emit", "metadata"])
        .arg("--extern")
        .arg(format!("gamma_rt={}", guard.display()))
        .arg("-o")
        .arg(directory.join(format!("{name}.rmeta")))
        .arg(&path);

    // Only when it built: a host that cannot compile a proc-macro crate still answers every
    // question that does not involve one, and the fixtures that do involve one say so themselves
    // by failing to resolve the attribute.
    if let Some(attrs) = attrs_crate() {
        let _extern = rustc.arg("--extern").arg(format!("gamma={}", attrs.display()));
    }

    let checked = rustc.output().expect("rustc must run");

    assert!(
        !checked.status.success(),
        "the compiler accepted a value the signature cannot return\n--- source ---\n{instrumented}"
    );

    String::from_utf8_lossy(&checked.stderr).into_owned()
}

/// Compiles `source` as a program, runs it, and returns what it printed.
fn output_of(name: &str, source: &str, guard: &Path) -> String {
    let directory = workdir();
    let path = directory.join(format!("{name}.rs"));
    let binary = directory.join(name);

    std::fs::write(&path, source).expect("the source must be writable");

    let built = Command::new(rustc())
        .args(["--edition", "2024", "--crate-type", "bin"])
        .arg("--extern")
        .arg(format!("gamma_rt={}", guard.display()))
        .arg("-o")
        .arg(&binary)
        .arg(&path)
        .output()
        .expect("rustc must run");

    assert!(
        built.status.success(),
        "{name} does not compile\n--- source ---\n{source}\n--- rustc ---\n{}",
        String::from_utf8_lossy(&built.stderr)
    );

    let run = Command::new(&binary).output().expect("the program must run");

    assert!(
        run.status.success(),
        "{name} exited {}\n--- stderr ---\n{}",
        run.status,
        String::from_utf8_lossy(&run.stderr)
    );

    String::from_utf8(run.stdout).expect("the program prints text")
}

/// Instruments `source`, runs both versions, and insists they behave identically.
///
/// The promise every mutant rests on is that an instrumented tree with no mutant active *is* the
/// original program. `compiles` only asks whether the text type-checks, which cannot see a splice
/// that changed a value on the `else` side — and the `else` side is the side that runs on every
/// build round, every baseline, and every mutant except the one being tested. A tool that quietly
/// altered it would report verdicts about a program nobody wrote.
///
/// Both versions are run as real programs rather than type-checked, and their whole output is
/// compared, so any difference in any printed value fails. The guard stub returns `false` through
/// `black_box`, so the compiler cannot fold the mutated branch away and the comparison is of what
/// actually executes.
///
/// Returns how many mutants were spliced in, so a fixture that generates nothing cannot pass.
#[track_caller]
fn runs_identically(name: &str, source: &str, mutators: &str) -> usize {
    let guard = guard_crate();

    let file = SourceFile::parse("subject.rs", source.to_owned()).expect("the subject must parse");
    let selection = Selection::parse(mutators).expect("the selector must resolve");
    let candidates = collect::collect(&file, &selection);
    let mut mutants = collect::into_mutants(&file, "subject", candidates);

    // `into_mutants` leaves every ordinal at zero, which only the caller that has seen the whole
    // run can fill in. Left as they are, every guard in the instrumented text would ask about the
    // same mutant, which is not the text this tool ever produces.
    for (ordinal, mutant) in mutants.iter_mut().enumerate() {
        mutant.ordinal = u32::try_from(ordinal).expect("the fixture is not that large");
    }

    let refs: Vec<&_> = mutants.iter().collect();
    let instrumented = schema::instrument(file.text(), &refs).expect("the mutants must splice");

    assert_ne!(instrumented, source, "nothing was instrumented, so there is nothing to compare");

    let before = output_of(&format!("{name}_original"), source, guard);
    let after = output_of(&format!("{name}_instrumented"), &instrumented, guard);

    assert!(!before.trim().is_empty(), "the fixture printed nothing, so this proves nothing");
    assert_eq!(
        after, before,
        "instrumenting changed what the program does with no mutant active\n--- instrumented ---\n{instrumented}"
    );

    mutants.len()
}

/// A program touching every family, run with no mutant active, must do exactly what it did before.
///
/// The fixture prints rather than asserts, so a difference shows up as a difference in output
/// rather than as a panic whose message says only that something went wrong. Everything it prints
/// is a value some family could have altered: the sides of a comparison, the result of arithmetic,
/// the value a function returns, the fields a struct was built with, the bounds of a range, which
/// arm of a `match` was taken, how a loop left, and what an iterator yielded.
#[test]
fn an_instrumented_program_with_no_mutant_active_behaves_exactly_as_the_original() {
    let source = r#"
#[derive(Debug, Default, PartialEq)]
pub struct Limits { pub floor: usize, pub ceiling: usize, pub strict: bool }

pub fn limits(len: usize) -> Limits {
    Limits { floor: 1, ceiling: len, ..Default::default() }
}

pub fn bound(values: &[usize], mode: usize) -> usize {
    let limits = limits(values.len());
    let mut total = 0;

    for index in limits.floor..=limits.ceiling {
        if index >= values.len() {
            break;
        }

        match mode {
            0 if values[index] > total => total += values[index],
            1 => continue,
            _ => total += index,
        }
    }

    total
}

pub fn arithmetic(a: i64, b: i64) -> i64 { a * b + a - b / 2 }

#[derive(Debug)]
pub enum Signal { Ready(u32), Waiting { since: u32 }, Done }

pub fn describe(signal: &Signal, budget: u32) -> String {
    match signal {
        Signal::Ready(n) if *n > budget => format!("over by {}", n - budget),
        Signal::Ready(n) => format!("ready at {n}"),
        Signal::Waiting { since } if *since == 0 => String::from("just started"),
        Signal::Waiting { since } => format!("waiting {since}"),
        Signal::Done => String::from("done"),
    }
}

pub fn relations(a: i64, b: i64) -> String {
    format!("{} {} {} {}", a < b, a <= b, a == b, a != b)
}

pub fn logic(a: bool, b: bool) -> String {
    format!("{} {} {}", a && b, a || b, !a)
}

pub fn flag() -> bool { true }
pub fn count() -> usize { 7 }
pub fn text() -> String { String::from("kept") }
pub fn maybe() -> Option<u32> { Some(3) }
pub fn fallible() -> Result<u32, String> { Ok(9) }
pub fn numbers() -> impl Iterator<Item = u32> { (0..4).map(|n| n * n) }

pub fn wanders(mut n: u32) -> u32 {
    let mut steps = 0;

    while n <= 20 {
        steps += 1;
        if steps > 50 { break; }
        if n % 3 == 0 { n += 5; continue; }
        n += 1;
    }

    n * 100 + steps
}

fn main() {
    println!("{:?}", limits(4));
    for mode in 0..3 {
        println!("bound {mode} {}", bound(&[3, 1, 4, 1, 5], mode));
    }
    println!("arithmetic {}", arithmetic(7, 3));
    for signal in [Signal::Ready(9), Signal::Ready(1), Signal::Waiting { since: 0 }, Signal::Waiting { since: 4 }, Signal::Done] {
        println!("describe {}", describe(&signal, 4));
    }
    println!("relations {}", relations(2, 5));
    println!("relations {}", relations(5, 5));
    println!("logic {}", logic(true, false));
    println!("flag {} count {} text {}", flag(), count(), text());
    println!("maybe {:?} fallible {:?}", maybe(), fallible());
    println!("numbers {:?}", numbers().collect::<Vec<_>>());
    for start in 0..6 {
        println!("wanders {start} {}", wanders(start));
    }
}
"#;

    assert!(
        runs_identically(
            "identical",
            source,
            "relational,arith,logical,cond,expr,fn_value,iter,match_arm,match_guard,struct_field,range,loop,stmt,option,result,collection,string,assign,assign_value"
        ) > 20
    );
}

#[test]
fn a_disabled_match_arm_still_compiles() {
    // The guard is spliced into the pattern position, where the arm's bindings are in scope but
    // not yet moved. Getting this wrong is not a parse error, it is a borrow error.
    let source = "
pub fn classify(value: Option<String>) -> String {
    match value {
        Some(text) if text.is_empty() => String::from(\"empty\"),
        Some(text) => text,
        None => String::from(\"none\"),
        _ => String::from(\"other\"),
    }
}
";

    assert!(compiles("match_arm", source, "match_arm,match_guard") > 0);
}

#[test]
fn an_omitted_struct_field_still_compiles() {
    let source = "
#[derive(Default)]
pub struct Config { pub timeout: u32, pub retries: u32, pub name: String }

pub fn build(timeout: u32) -> Config {
    Config { timeout, retries: 3, ..Default::default() }
}
";

    assert!(compiles("struct_field", source, "struct_field") > 0);
}

#[test]
fn a_moved_range_boundary_still_compiles() {
    let source = "
pub fn total(values: &[u32], n: usize) -> u32 {
    let mut sum = 0;

    for index in 0..n {
        sum += values[index];
    }

    for value in &values[..n] {
        sum += *value;
    }

    for step in 1..=n {
        sum += step as u32;
    }

    sum
}
";

    assert!(compiles("range", source, "range") > 0);
}

#[test]
fn swapped_loop_exits_still_compile() {
    let source = "
pub fn first_even(values: &[i32]) -> i32 {
    let mut found = 0;

    'outer: for value in values {
        for _inner in 0..2 {
            if *value % 2 != 0 {
                continue 'outer;
            }

            if *value > 100 {
                break;
            }

            found = *value;
        }
    }

    found
}
";

    assert!(compiles("loop_exits", source, "loop") > 0);
}

#[test]
fn campaign_operand_and_borrowed_predicate_schema_replacements_are_exact() {
    let source = r"
        use std::time::{Duration, Instant};
        struct Timing { elapsed: Duration }
        fn update(value: &mut Timing, tuple: &mut (u8, Duration), delta: Duration) {
            value.elapsed += delta;
            tuple.1 += delta;
        }
        fn deadline(budget: Duration) -> Instant {
            let clock = || Instant::now();
            clock() + budget
        }
        fn text(first: char, chars: std::str::Chars<'_>) -> String {
            first.to_uppercase().collect::<String>() + chars.as_str()
        }
        struct Position(usize);
        fn range(start: Position, ends: (Position, Position)) -> std::ops::Range<Position> {
            start..ends.1
        }
        fn find(values: &[(usize, bool)]) -> Option<(usize, &(usize, bool))> {
            values.iter().enumerate().skip(1).find(|(_, (_, opens))| !opens)
        }
        fn contains(markers: &[usize]) -> bool {
            markers.iter().any(|offset| (1usize..9usize).contains(offset))
        }
        fn owned(value: usize) -> usize { value }
        fn dereferenced(value: &usize) -> usize { *value }
    ";
    assert_eq!(
        schema_replacements(
            source,
            "assign.add_to_mul,arith.add_to_mul,arith.add_to_sub,range.exclusive_to_inclusive,expr.increment,expr.decrement,unary.remove_not"
        ),
        [
            ("arith.add_to_sub".into(), rust_tokens("(clock()) - (budget)")),
            ("expr.decrement".into(), rust_tokens("(*value) - 1")),
            ("expr.decrement".into(), rust_tokens("(value) - 1")),
            ("expr.increment".into(), rust_tokens("(*value) + 1")),
            ("expr.increment".into(), rust_tokens("(value) + 1")),
            ("range.exclusive_to_inclusive".into(), rust_tokens("(1usize)..((9usize) + 1)")),
            ("unary.remove_not".into(), rust_tokens("*(opens)")),
        ]
    );
}

#[test]
fn perturbed_numeric_expressions_still_compile() {
    let source = "
pub fn lookup(values: &[u32], index: usize, count: usize) -> u32 {
    let scaled = scale(count);

    if scaled > 0 {
        return values[index];
    }

    values[index] + scaled
}

fn scale(count: usize) -> u32 {
    count as u32
}
";

    assert!(compiles("perturbation", source, "expr") > 0);
}

#[test]
fn every_new_family_at_once_still_compiles() {
    // The families nest: an arm guard inside a match inside a loop whose bounds are themselves
    // moved. Nesting is where a splice that is individually correct stops being so.
    //
    // The selector names the new families rather than `all`, and that is not the test avoiding an
    // inconvenient answer. Some mutants genuinely cannot compile — negating an unsigned literal is
    // the standard example — and the build converges by blaming the diagnostics, withdrawing those
    // mutants and going round again. Being unviable is a supported outcome, so `all` is not a
    // question with a yes-or-no answer. What is not supported is a family that is unviable
    // *systematically*, because then every run pays a build round to withdraw work it should never
    // have generated, and that is exactly what these tests exist to catch.
    let source = "
#[derive(Default)]
pub struct Limits { pub floor: usize, pub ceiling: usize }

pub fn bound(values: &[usize], mode: usize) -> usize {
    let limits = Limits { floor: 1, ceiling: values.len(), ..Default::default() };
    let mut total = 0;

    for index in limits.floor..=limits.ceiling {
        if index >= values.len() {
            break;
        }

        match mode {
            0 if values[index] > total => total += values[index],
            1 => continue,
            _ => total += index,
        }
    }

    total
}
";

    assert!(compiles("everything", source, "match_arm,match_guard,struct_field,range,loop,expr") > 0);
}

#[test]
fn recursive_typed_return_values_still_compile() {
    // The nested cases are the point: a `Result<Option<bool>, E>` has to compose three levels of
    // replacement and still name a value the compiler accepts at each one.
    let source = "
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::borrow::Cow;
use std::num::NonZeroUsize;
use std::rc::Rc;

pub fn nested() -> Result<Option<bool>, String> { Ok(Some(true)) }
pub fn pair() -> (u32, bool) { (1, true) }
pub fn deque() -> VecDeque<u32> { VecDeque::new() }
pub fn set() -> BTreeSet<u32> { BTreeSet::new() }
pub fn map() -> BTreeMap<String, u32> { BTreeMap::new() }
pub fn boxed() -> Box<u32> { Box::new(1) }
pub fn counted() -> Rc<String> { Rc::new(String::new()) }
pub fn borrowed() -> Cow<'static, str> { Cow::Borrowed(\"x\") }
pub fn nonzero() -> NonZeroUsize { NonZeroUsize::new(4).unwrap() }
pub fn iterator() -> impl Iterator<Item = u32> { std::iter::once(1) }
";

    assert!(compiles("returns", source, "fn_value") > 0);
}

#[test]
fn singleton_tuple_return_values_still_compile() {
    let source = "
pub fn singleton() -> (u32,) { (1,) }
";

    assert!(compiles("singleton_tuple", source, "fn_value") > 0);
}

#[test]
fn iterator_returns_still_compile() {
    // Each of these broke a different way while the `Either` splice was being written, and none
    // of them is caught by collecting mutants alone — only by handing the instrumented text to
    // the compiler.
    //
    // `shared` is the reborrow: `Box::leak` yields `&mut String`, and as an iterator's item that
    // is what `Item` is *inferred* from, so `&String` was promised and `&mut String` delivered.
    // `sendable` is why the wrapper is an enum rather than a `Box<dyn Iterator>`, which would have
    // erased the `Send`. `borrowing` checks the lifetime survives, and `bare` has no `Item` to
    // synthesize from at all.
    let source = "
pub fn plain() -> impl Iterator<Item = u32> { 0..10 }
pub fn borrowing(words: &[String]) -> impl Iterator<Item = &String> + '_ { words.iter() }
pub fn shared(words: &[String]) -> impl Iterator<Item = &String> { words.iter() }
pub fn sendable() -> impl Iterator<Item = u32> + Send { std::iter::repeat(7).take(3) }
pub fn bare() -> impl Iterator { 0..10 }
pub fn exact() -> impl ExactSizeIterator<Item = u32> { 0..10 }
pub fn both_ends() -> impl DoubleEndedIterator<Item = u32> { 0..10 }
";

    assert!(compiles("iterators", source, "fn_value") > 0);
}

#[test]
fn stated_return_values_still_compile() {
    // The shapes `fn_value` cannot guess for, which is why the attribute exists: a bare type
    // parameter, an associated type, a trait object, an opaque return, an alias, an iterator that
    // needs the `Either` splice, and a method rather than a free function. Each states a value its
    // signature accepts, and every one of them has to survive instrumentation — a stated value
    // spliced with the wrong shape would be an unviable mutant blamed on the author's expression.
    let source = "
pub trait Reader { fn read(&self) -> u8; }

#[derive(Default)]
pub struct Empty;
impl Empty { fn make() -> Self { Self } }
impl Reader for Empty { fn read(&self) -> u8 { 3 } }

pub struct Full;
impl Reader for Full { fn read(&self) -> u8 { 4 } }

pub type Alias = Vec<String>;

pub trait Source { type Item; fn produce(&self) -> Self::Item; }
pub struct Counter;
impl Source for Counter {
    type Item = u32;

    #[gamma::value(7)]
    fn produce(&self) -> Self::Item { 1 }
}

#[gamma::value(t)]
pub fn identity<T: Clone>(t: T) -> T { t.clone() }

#[gamma::value(Box::new(Full))]
pub fn boxed() -> Box<dyn Reader> { Box::new(Empty) }

#[gamma::value(Empty)]
pub fn opaque() -> impl Reader { Empty::make() }

#[gamma::value(Vec::new())]
pub fn aliased() -> Alias { vec![String::new()] }

#[gamma::value(core::iter::empty())]
pub fn streamed() -> impl Iterator<Item = u32> { 0..10 }

pub struct Holder;
impl Holder {
    #[gamma::value(String::from(\"stated\"))]
    pub fn named(&self) -> String { String::from(\"real\") }
}
";

    // Named rather than counted from the total, so that a change in what the tool guesses for the
    // fixture's scaffolding cannot quietly leave this test compiling seven fewer mutants than it
    // believes it is.
    let file = SourceFile::parse("subject.rs", source.to_owned()).expect("the subject must parse");
    let stated = collect::collect(&file, &Selection::parse("fn_value.stated").expect("the selector must resolve"));

    assert_eq!(stated.len(), 7, "{stated:?}");
    assert!(compiles("stated", source, "fn_value") > stated.len());
}

/// A stated value the compiler rejects is generated all the same, and dies where every other
/// unviable mutant dies.
///
/// The alternative would be for the tool to decide which expressions it believes in, and it has no
/// type information to decide with — it would have to guess, and a guess that says no silently
/// deletes the mutant of an author who was right. Generating it means the compiler answers the
/// question instead, in the build round the run already pays for, and the mutant is withdrawn with
/// a reason the author can read.
#[test]
fn a_stated_value_of_the_wrong_type_becomes_an_unviable_mutant() {
    let source = "#[gamma::value(\"seven\")]\npub fn count() -> u32 { 7 }\n";
    let rejected = does_not_compile("stated_wrong_type", source, "fn_value");

    assert!(rejected.contains("mismatched types"), "{rejected}");
}

#[test]
fn standard_library_semantics_still_compile() {
    let source = "
pub fn shapes(words: &[String], text: &str, limit: usize) -> usize {
    let mut total = 0;

    if words.iter().any(|word| word.starts_with(text)) {
        total += 1;
    }

    if words.iter().all(|word| word.ends_with(text)) {
        total += 1;
    }

    let taken: Vec<_> = words.iter().take(limit).collect();
    let skipped: Vec<_> = words.iter().skip(limit).collect();
    let filtered: Vec<_> = words.iter().filter(|word| !word.is_empty()).rev().collect();

    total += taken.len() + skipped.len() + filtered.len();
    total += words.first().map_or(0, String::len);
    total += words.last().map_or(0, String::len);
    total += words.iter().map(String::len).min().unwrap_or(0);
    total += words.iter().map(String::len).max().unwrap_or(0);
    total += text.to_lowercase().len() + text.to_uppercase().len();
    total += text.trim_start().len() + text.trim_end().len();

    let mut owned: Vec<usize> = vec![3, 1, 2];

    owned.sort();
    owned.dedup();

    total += owned.len();
    total
}

pub fn optional(flag: bool) -> Option<u32> {
    if flag { Some(1) } else { None }
}

pub fn fallible(flag: bool) -> Result<u32, String> {
    if flag { Ok(1) } else { Err(String::new()) }
}

pub fn assigned(mut value: u32) -> u32 {
    value = value + 1;
    value
}
";

    assert!(compiles("semantics", source, "option,result,iter,string,collection,assign_value") > 0);
}

#[test]
fn representative_default_on_candidates_compile_together() {
    let source = r#"
pub struct Regex;
impl Regex {
    pub fn new(_: &str) -> Result<Self, ()> { Ok(Self) }
}

pub fn helper() -> usize { 7 }
pub fn early(flag: bool) -> usize { if flag { return 7; } 8 }
pub fn mutable(mut value: usize) -> usize { value += 1; value }

pub fn boolean(flag: bool, other: bool) -> bool {
    let _: bool = flag && other;
    let _: bool = flag || other;
    return flag;
}

pub fn semantics(option: Option<usize>, result: Result<usize, usize>) -> Result<usize, usize> {
    let _ = option.is_some();
    let _ = option.is_none();
    let _ = result.is_ok();
    let _ = result.is_err();
    let _ = result?;
    let _ = option.unwrap_or(7);
    let _ = option.unwrap_or_else(|| 7);
    let _ = option.map_or(7, |value| value);
    let _ = option.map_or_else(|| 7, |value| value);
    let _: usize = helper();
    return Ok(helper());
}

pub fn collections() {
    let _ = vec![1, 2, 3];
    let _ = [1, 2, 3];
    let _ = 2.5f64;
    let _ = 'a';
    let _ = b'a';
    let _: usize = loop { break 3; };
    for _ in [1, 2, 3].into_iter().rev().filter(|value| *value > 0).take(2).skip(1) {}
    let _ = Regex::new("^a+b*[cd]?$");
}
"#;

    // The floor represents the default-on candidate families exercised above; recompute it when
    // that catalog changes rather than lowering it to accommodate a missing family.
    assert!(compiles_selection("representative_default_on", source, &Selection::default_preset()) >= 38);
}
