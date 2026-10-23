// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::Write;

use camino::{Utf8Path, Utf8PathBuf};

use super::cli::ExplainArgs;
use super::dispatch::EXIT_OK;
use super::host::Host;
use crate::error::error;
use crate::model::MUTANT_ID_VERSION;
use crate::ops::registry;

/// Implements `explain`.
#[cfg_attr(coverage_nightly, coverage(off))]
pub(super) fn explain<H: Host>(host: &mut H, args: &ExplainArgs) -> crate::Result<i32> {
    if mutant_id_syntax(&args.subject) {
        return explain_mutant(host, args);
    }

    let names = registry::resolve(&args.subject)?;
    let mut stream = host.results();

    for name in names {
        let Some(mutator) = registry::find(name) else {
            continue;
        };

        writeln!(stream, "{}", mutator.name)?;
        writeln!(stream, "  {}", mutator.description)?;
        writeln!(stream, "  enabled by default: {}", if mutator.default_on { "yes" } else { "no" })?;
        if registry::optimistic_requires_explicit(mutator.name) {
            writeln!(stream, "  optimistic sites: require an explicit selector")?;
        }

        if !mutator.aliases.is_empty() {
            writeln!(stream, "  also known as: {}", mutator.aliases.join(", "))?;
        }

        writeln!(stream, "  suppress with: // #[gamma::skip({})]", mutator.name)?;
        writeln!(stream)?;
    }

    Ok(EXIT_OK)
}

// #[gamma::skip(fn_value.bool_true, tag = "outofmemory", reason = "the campaign exhausted memory when every subject was routed through mutant-id discovery")]
fn mutant_id_syntax(subject: &str) -> bool {
    super::CanonicalMutantId::parse(subject).is_ok()
}

fn explain_mutant<H: Host>(host: &mut H, args: &ExplainArgs) -> crate::Result<i32> {
    explain_mutant_with(host, args, |select, selection, cargo| {
        crate::discover::plan_for_build(select, selection, None, cargo, &mut |_| {})
    })
}

#[expect(
    clippy::too_many_lines,
    reason = "current discovery and retained-report context are one explanation transaction"
)]
fn explain_mutant_with<H: Host>(
    host: &mut H,
    args: &ExplainArgs,
    discover: impl FnOnce(&super::cli::SelectArgs, &crate::ops::Selection, &crate::exec::CargoOptions) -> crate::Result<crate::discover::Plan>,
) -> crate::Result<i32> {
    let explicit_report = args
        .report
        .as_ref()
        .map(|path| crate::merge::read_limited(path, u64::MAX).map(|read| read.report))
        .transpose()?;
    let explicit_report_contains_subject = explicit_report.as_ref().is_some_and(|report| {
        report
            .files
            .values()
            .any(|file| file.mutants.iter().any(|mutant| mutant.id.as_str() == args.subject))
    });

    let discover_current = || {
        let mut select = super::cli::SelectArgs {
            dir: args.dir.clone(),
            config: args.config.clone(),
            features: args.features.clone(),
            packages: args.packages.clone(),
            workspace: args.workspace,
            ..super::cli::SelectArgs::default()
        };
        let config = crate::config::Config::resolve(&select)?;
        let cargo = config.cargo_options();
        config.apply_selection(&mut select)?;
        select.mutators = Some("all".to_owned());

        let selection = select.selection()?;
        let plan = discover(&select, &selection, &cargo)?;
        Ok((plan, config.artifact_dir))
    };
    let (plan, configured_artifact_dir) = match discover_current() {
        Ok((plan, artifact_dir)) => (Some(plan), artifact_dir),
        Err(_) if explicit_report_contains_subject => (None, None),
        Err(failure) => return Err(failure),
    };
    let current = plan
        .as_ref()
        .and_then(|plan| plan.mutants.iter().find(|mutant| mutant.id.as_str() == args.subject));
    let report_path = args.report.clone().unwrap_or_else(|| {
        let root = &plan
            .as_ref()
            .expect("current discovery succeeded when no explicit report was supplied")
            .root;
        default_report_path(root, configured_artifact_dir.as_ref())
    });
    let report = if explicit_report.is_some() {
        explicit_report
    } else if report_path.as_std_path().is_file() {
        Some(crate::merge::read_limited(&report_path, u64::MAX)?.report)
    } else {
        None
    };
    let historical = report.as_ref().and_then(|report| {
        report.files.iter().find_map(|(path, file)| {
            file.mutants
                .iter()
                .find(|mutant| mutant.id.as_str() == args.subject)
                .map(|mutant| (path, file, mutant))
        })
    });

    if current.is_none() && historical.is_none() {
        let report_hint = if args.report.is_some() {
            format!("; `{report_path}` does not contain it either")
        } else {
            "; pass `--report <PATH>` to inspect a retained CI report".to_owned()
        };
        return Err(error!(
            "mutant ID `{}` is not produced by current discovery{report_hint}. The source site may \
             have changed, the current selection may exclude it, or the report may use another identity version.",
            args.subject
        )
        .usage());
    }

    let identity_version = if current.is_some() {
        MUTANT_ID_VERSION
    } else {
        report
            .as_ref()
            .expect("a historical mutant was found in the loaded report")
            .identity_version()
    };
    let mut stream = host.results();
    writeln!(stream, "mutant {}", args.subject)?;
    writeln!(stream, "  identity version: {identity_version}")?;

    if let Some(mutant) = current {
        writeln!(stream, "  current: yes")?;
        writeln!(stream, "  package: {}", mutant.package)?;
        writeln!(stream, "  location: {}:{}:{}", mutant.file, mutant.line, mutant.column)?;
        writeln!(stream, "  item: {}", mutant.item_path)?;
        writeln!(stream, "  mutator: {}", mutant.mutator)?;
        writeln!(stream, "  original: {}", mutant.original)?;
        writeln!(stream, "  replacement: {}", mutant.replacement)?;
        writeln!(stream, "  suppress with: // #[gamma::skip({})]", mutant.mutator)?;
    } else {
        writeln!(stream, "  current: no (the current discovery no longer produces this mutant)")?;
    }

    if let Some((path, file, mutant)) = historical {
        let line = mutant.location.start.line;
        let source = file.source.lines().nth(line.saturating_sub(1)).unwrap_or("").trim();
        writeln!(stream, "  report: {report_path}")?;
        writeln!(stream, "  reported location: {path}:{line}:{}", mutant.location.start.column)?;
        writeln!(stream, "  reported source: {source}")?;
        writeln!(
            stream,
            "  reported replacement: {}",
            mutant.replacement.as_deref().unwrap_or("<not recorded>")
        )?;
        writeln!(stream, "  verdict: {}", mutant.status)?;
        if let Some(reason) = mutant.status_reason.as_deref() {
            writeln!(stream, "  reason: {reason}")?;
        }
        if let Some(killers) = mutant.killed_by.as_deref() {
            writeln!(stream, "  killed by: {}", killers.join(", "))?;
        }
    }

    Ok(EXIT_OK)
}

fn default_report_path(root: &Utf8Path, configured_artifact_dir: Option<&Utf8PathBuf>) -> Utf8PathBuf {
    configured_artifact_dir
        .map_or_else(|| root.join("target/cargo-gamma"), Clone::clone)
        .join("gamma-report.json")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {

    use super::*;
    use crate::elements::FileResult;
    use crate::testing::{BrokenHost, Sink};

    #[derive(Debug)]
    struct LineLimitedHost {
        remaining: usize,
        output: Vec<u8>,
        diagnostics: Vec<u8>,
    }

    impl Host for LineLimitedHost {
        fn output(&mut self) -> impl Write {
            LineLimitedOutput {
                remaining: &mut self.remaining,
                output: &mut self.output,
            }
        }
        fn error(&mut self) -> impl Write {
            &mut self.diagnostics
        }
        fn is_terminal(&self) -> bool {
            false
        }
        fn terminal_width(&self) -> Option<u16> {
            None
        }
    }

    struct LineLimitedOutput<'a> {
        remaining: &'a mut usize,
        output: &'a mut Vec<u8>,
    }

    impl Write for LineLimitedOutput<'_> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if *self.remaining == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "explanation output denied",
                ));
            }
            self.output.extend_from_slice(buf);
            *self.remaining -= buf.iter().map(|&byte| usize::from(byte == b'\n')).sum::<usize>();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn survivor_mutator_explanations_return_non_pipe_errors_at_each_output_line() {
        let args = args("arith.add_to_sub", ".".into());
        let mut complete = Sink::default();
        assert_eq!(explain(&mut complete, &args).unwrap(), EXIT_OK);
        assert!(complete.out().starts_with("arith.add_to_sub\n"));
        assert!(complete.out().contains("  suppress with: // #[gamma::skip(arith.add_to_sub)]\n"));
        for remaining in 0..complete.out().lines().count() {
            let mut host = LineLimitedHost {
                remaining,
                output: Vec::new(),
                diagnostics: Vec::new(),
            };
            assert!(
                explain(&mut host, &args)
                    .unwrap_err()
                    .to_string()
                    .contains("explanation output denied")
            );
        }
    }

    fn args(subject: impl Into<String>, dir: Utf8PathBuf) -> ExplainArgs {
        ExplainArgs {
            subject: subject.into(),
            dir,
            report: None,
            features: super::super::cli::FeatureArgs::default(),
            packages: Vec::new(),
            workspace: false,
            config: super::super::cli::ConfigArgs::default(),
        }
    }

    fn source_plan(root: &Utf8Path) -> crate::discover::Plan {
        let parsed = crate::parse::SourceFile::parse("src/lib.rs", "pub fn answer() -> i32 { 42 }\n".to_owned()).unwrap();
        let candidates = crate::ops::collect_candidates(&parsed, &crate::ops::Selection::everything());
        crate::discover::Plan {
            root: root.to_owned(),
            files: Vec::new(),
            mutants: crate::ops::into_mutants(&parsed, "subject", candidates),
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            digests: crate::HashMap::default(),
            skipped: Vec::new(),
            reach: crate::HashMap::default(),
            specs: crate::HashMap::default(),
        }
    }

    #[cfg(all(feature = "system-tests", not(miri)))]
    mod native_acceptance {
        use super::*;

        #[test]
        fn survivor_current_explanation_consumes_effective_selection_and_writes_every_field() {
            let directory = crate::testing::workdir("explain-current-effective-");
            let root = Utf8Path::from_path(directory.path()).unwrap();
            let config_path = root.join("custom.toml");
            std::fs::write(&config_path, "features = ['configured']\nmutators = ['fn_value.unit']\n").unwrap();
            for workspace in [false, true] {
                let plan = source_plan(root);
                let mutant = plan
                    .mutants
                    .iter()
                    .find(|mutant| mutant.mutator.as_ref() == "literal.int_decrement")
                    .unwrap()
                    .clone();
                let mut selected = args(mutant.id.as_str(), root.to_owned());
                selected.config.path = Some(config_path.clone());
                selected.workspace = workspace;
                selected.packages = if workspace { Vec::new() } else { vec!["subject".into()] };
                selected.features.features = vec!["requested".into()];
                selected.features.all_features = true;
                selected.features.no_default_features = true;
                let mut host = Sink::default();
                explain_mutant_with(&mut host, &selected, |effective, selection, _| {
                    assert_eq!(effective.dir, root);
                    assert_eq!(effective.config.path.as_ref(), Some(&config_path));
                    assert_eq!(effective.workspace, workspace);
                    assert_eq!(effective.packages, selected.packages);
                    assert_eq!(effective.features.features, ["requested", "configured"]);
                    assert!(effective.features.all_features);
                    assert!(effective.features.no_default_features);
                    assert_eq!(effective.mutators.as_deref(), Some("all"));
                    assert!(selection.includes_optimistic("literal.int_decrement"));
                    Ok(plan.clone())
                })
                .unwrap();
                assert_eq!(
                    host.out(),
                    format!(
                        "mutant {}\n  identity version: {MUTANT_ID_VERSION}\n  current: yes\n  package: subject\n  location: {}:{}:{}\n  item: {}\n  mutator: {}\n  original: {}\n  replacement: {}\n  suppress with: // #[gamma::skip({})]\n",
                        mutant.id,
                        mutant.file,
                        mutant.line,
                        mutant.column,
                        mutant.item_path,
                        mutant.mutator,
                        mutant.original,
                        mutant.replacement,
                        mutant.mutator
                    )
                );
                for remaining in 0..host.out().lines().count() {
                    let mut denied = LineLimitedHost {
                        remaining,
                        output: Vec::new(),
                        diagnostics: Vec::new(),
                    };
                    assert!(
                        explain_mutant_with(&mut denied, &selected, |_, _, _| Ok(plan.clone()))
                            .unwrap_err()
                            .to_string()
                            .contains("explanation output denied")
                    );
                }
            }
        }

        #[test]
        fn survivor_explanation_missing_ids_and_discovery_errors_keep_their_specific_report_hints() {
            let directory = crate::testing::workdir("explain-missing-matrix-");
            let root = Utf8Path::from_path(directory.path()).unwrap();
            let report_path = root.join("empty.json");
            crate::elements::write_json(&crate::fixtures::report_with(None, 0, Vec::new()), &report_path).unwrap();
            for explicit in [false, true] {
                let mut selected = args("deadbeefcafe", root.to_owned());
                selected.report = explicit.then(|| report_path.clone());
                let mut plan = source_plan(root);
                plan.mutants.clear();
                let failure = explain_mutant_with(&mut Sink::default(), &selected, |_, _, _| Ok(plan)).unwrap_err();
                let hint = if explicit {
                    format!("; `{report_path}` does not contain it either")
                } else {
                    "; pass `--report <PATH>` to inspect a retained CI report".to_owned()
                };
                assert_eq!(
                    failure.message(),
                    format!(
                        "mutant ID `deadbeefcafe` is not produced by current discovery{hint}. The source site may have changed, the current selection may exclude it, or the report may use another identity version."
                    )
                );
                assert!(failure.is_usage());
                let failure = explain_mutant_with(&mut Sink::default(), &selected, |_, _, _| {
                    Err(error!("specific current discovery failure"))
                })
                .unwrap_err();
                assert_eq!(failure.message(), "specific current discovery failure");
            }
            assert_eq!(default_report_path(root, None), root.join("target/cargo-gamma/gamma-report.json"));
            let artifacts = root.join("custom-artifacts");
            assert_eq!(default_report_path(root, Some(&artifacts)), artifacts.join("gamma-report.json"));
        }

        #[test]
        fn survivor_explanation_configuration_and_default_report_failures_do_not_panic_or_discover() {
            let directory = crate::testing::workdir("explain-input-errors-");
            let root = Utf8Path::from_path(directory.path()).unwrap();
            let mut selected = args("deadbeefcafe", root.to_owned());
            let config = root.join("gamma.toml");
            std::fs::write(&config, "[").unwrap();
            assert!(
                explain_mutant_with(&mut Sink::default(), &selected, |_, _, _| panic!(
                    "malformed config must precede discovery"
                ))
                .unwrap_err()
                .to_string()
                .contains("gamma.toml")
            );
            std::fs::write(&config, "packages = ['configured-package']\n").unwrap();
            selected.workspace = true;
            assert!(
                explain_mutant_with(&mut Sink::default(), &selected, |_, _, _| panic!(
                    "conflicting effective selection must precede discovery"
                ))
                .unwrap_err()
                .is_usage()
            );
            std::fs::remove_file(config).unwrap();
            selected.workspace = false;
            let path = default_report_path(root, None);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "{").unwrap();
            assert!(
                explain_mutant_with(&mut Sink::default(), &selected, |_, _, _| Ok(source_plan(root)))
                    .unwrap_err()
                    .to_string()
                    .contains("not a mutation report")
            );
        }

        #[test]
        fn survivor_explicit_history_uses_the_read_generation_and_empty_source_fallback() {
            let directory = crate::testing::workdir("explain-retained-generation-");
            let root = Utf8Path::from_path(directory.path()).unwrap();
            let path = root.join("history.json");
            let mut report = crate::fixtures::report_with(None, 0, Vec::new());
            let mut historical = crate::fixtures::mutant_result_at("deadbeefcafe", 9, "Survived");
            historical.replacement = Some("replacement text".into());
            historical.status_reason = None;
            historical.killed_by = None;
            report.files.values_mut().next().unwrap().mutants.push(historical);
            crate::elements::write_json(&report, &path).unwrap();
            let mut selected = args("deadbeefcafe", root.to_owned());
            selected.report = Some(path.clone());
            let mut host = Sink::default();
            explain_mutant_with(&mut host, &selected, |_, _, _| {
                std::fs::write(&path, "{").unwrap();
                Err(error!("the current tree cannot be discovered"))
            })
            .unwrap();
            assert!(host.out().contains(&format!("  identity version: {MUTANT_ID_VERSION}\n")));
            assert!(host.out().contains("  reported source: \n"));
            assert!(host.out().contains("  reported replacement: replacement text\n"));
            assert!(!host.out().contains("  reason:"));
            assert!(!host.out().contains("  killed by:"));
        }

        #[test]
        fn review_unstamped_historical_explanations_keep_scheme_five() {
            let directory = crate::testing::workdir("explain-unstamped-");
            let root = Utf8Path::from_path(directory.path()).unwrap();
            let path = root.join("history.json");
            for with_metadata in [false, true] {
                let mut report =
                    crate::fixtures::report_with(None, 0, vec![crate::fixtures::mutant_result_at("deadbeefcafe", 1, "Survived")]);
                report.config.as_mut().unwrap().mutant_id_version = None;
                if !with_metadata {
                    report.config = None;
                }
                crate::elements::write_json(&report, &path).unwrap();
                let mut selected = args("deadbeefcafe", root.to_owned());
                selected.report = Some(path.clone());
                let mut host = Sink::default();
                explain_mutant_with(&mut host, &selected, |_, _, _| Err(error!("the current tree cannot be discovered"))).unwrap();
                assert!(host.out().contains("  identity version: 5\n"), "{}", host.out());
                assert!(host.out().contains("  current: no ("), "{}", host.out());
            }
        }

        #[test]
        fn survivor_current_identity_version_outranks_a_different_retained_report_generation() {
            let directory = crate::testing::workdir("explain-current-and-history-");
            let root = Utf8Path::from_path(directory.path()).unwrap();
            let plan = source_plan(root);
            let subject = plan.mutants[0].id.to_string();
            let mut report = crate::fixtures::report_with(None, 0, vec![crate::fixtures::mutant_result_at(&subject, 1, "Killed")]);
            report.config.as_mut().unwrap().mutant_id_version = Some(97);
            let path = root.join("history.json");
            crate::elements::write_json(&report, &path).unwrap();
            let mut selected = args(subject, root.to_owned());
            selected.report = Some(path);
            let mut host = Sink::default();
            explain_mutant_with(&mut host, &selected, |_, _, _| Ok(plan)).unwrap();
            assert!(host.out().contains(&format!("  identity version: {MUTANT_ID_VERSION}\n")));
            assert!(!host.out().contains("identity version: 97"));
            assert!(host.out().contains("  current: yes\n"));
            assert!(host.out().contains("  verdict: Killed\n"));
        }

        #[test]
        fn survivor_historical_explanation_uses_the_requested_id_line_and_retained_metadata() {
            let directory = crate::testing::workdir("explain-survivor-");
            let root = Utf8Path::from_path(directory.path()).unwrap();
            let path = root.join("history.json");
            let mut report = crate::fixtures::report_with(None, 0, Vec::new());
            report.files.clear();
            report.config.as_mut().unwrap().mutant_id_version = Some(97);
            let mut result = crate::fixtures::mutant_result_at("deadbeefcafe", 2, "Killed");
            result.location.start.column = 7;
            result.replacement = None;
            result.status_reason = Some("assertion rejected the change".to_owned());
            result.killed_by = Some(vec!["first".to_owned(), "second".to_owned()]);
            report.files.insert(
                "other.rs".to_owned(),
                FileResult {
                    source: "other".to_owned(),
                    language: "rust".to_owned(),
                    mutants: vec![crate::fixtures::mutant_result_at("0123456789ab", 1, "Survived")],
                },
            );
            report.files.insert(
                "source.rs".to_owned(),
                FileResult {
                    source: "wrong first line\n    expected second line    \nwrong third line".to_owned(),
                    language: "rust".to_owned(),
                    mutants: vec![crate::fixtures::mutant_result_at("abcdefabcdef", 1, "Survived"), result],
                },
            );
            crate::elements::write_json(&report, &path).unwrap();
            let mut args = args("deadbeefcafe", root.join("missing-workspace"));
            args.report = Some(path.clone());
            let mut host = Sink::default();
            assert_eq!(explain(&mut host, &args).unwrap(), EXIT_OK);
            assert_eq!(
                host.out(),
                format!(
                    "mutant deadbeefcafe\n  identity version: 97\n  current: no (the current discovery no longer produces this mutant)\n  report: {path}\n  reported location: source.rs:2:7\n  reported source: expected second line\n  reported replacement: <not recorded>\n  verdict: Killed\n  reason: assertion rejected the change\n  killed by: first, second\n"
                )
            );
            assert!(host.err.is_empty());
            assert_eq!(default_report_path(root, None), root.join("target/cargo-gamma/gamma-report.json"));
            for remaining in 0..host.out().lines().count() {
                let mut denied = LineLimitedHost {
                    remaining,
                    output: Vec::new(),
                    diagnostics: Vec::new(),
                };
                assert!(
                    explain(&mut denied, &args)
                        .unwrap_err()
                        .to_string()
                        .contains("explanation output denied")
                );
            }
        }

        #[test]
        fn survivor_malformed_explicit_report_is_returned_as_an_error_before_discovery() {
            let directory = crate::testing::workdir("explain-bad-report-");
            let root = Utf8Path::from_path(directory.path()).unwrap();
            let path = root.join("report.json");
            std::fs::write(&path, "{").unwrap();
            let mut args = args("deadbeefcafe", root.join("missing"));
            args.report = Some(path);
            let mut host = Sink::default();
            explain(&mut host, &args).expect_err("malformed explicit report must be rejected");
            assert!(host.out.is_empty());
        }
    }

    #[test]
    fn explanation_names_aliases_and_suppressions() {
        let mut host = Sink::default();

        let code = explain(
            &mut host,
            &ExplainArgs {
                subject: "fn_value.default".to_owned(),
                dir: Utf8PathBuf::from("."),
                report: None,
                features: super::super::cli::FeatureArgs::default(),
                packages: Vec::new(),
                workspace: false,
                config: super::super::cli::ConfigArgs::default(),
            },
        )
        .expect("explain");
        let text = String::from_utf8(host.out).expect("utf-8");

        assert_eq!(code, EXIT_OK);
        assert!(text.contains("also known as: RV"), "{text}");
        assert!(text.contains("suppress with: // #[gamma::skip(fn_value.default)]"), "{text}");
        assert!(!text.contains("optimistic sites"), "{text}");

        let mut host = Sink::default();
        let code = explain(&mut host, &args("literal.int_decrement", Utf8PathBuf::from("."))).expect("explain optimistic policy");

        assert_eq!(code, EXIT_OK);
        assert_eq!(
            host.out(),
            concat!(
                "literal.int_decrement\n",
                "  subtract one from an integer literal\n",
                "  enabled by default: yes\n",
                "  optimistic sites: require an explicit selector\n",
                "  also known as: CRP\n",
                "  suppress with: // #[gamma::skip(literal.int_decrement)]\n",
                "\n",
            )
        );
    }

    /// Piping into a consumer that exits early is successful consumption.
    #[test]
    fn a_closed_output_stream_ends_explanation_successfully() {
        let code = explain(
            &mut BrokenHost,
            &ExplainArgs {
                subject: "relational".to_owned(),
                dir: Utf8PathBuf::from("."),
                report: None,
                features: super::super::cli::FeatureArgs::default(),
                packages: Vec::new(),
                workspace: false,
                config: super::super::cli::ConfigArgs::default(),
            },
        )
        .expect("closed pipe");

        assert_eq!(code, EXIT_OK);
    }

    /// A mutator with no academic alias simply omits the line rather than printing an empty one.
    #[test]
    fn a_mutator_without_aliases_omits_the_alias_line() {
        let named: Vec<&str> = registry::REGISTRY
            .iter()
            .filter(|mutator| mutator.aliases.is_empty())
            .map(|mutator| mutator.name)
            .collect();

        assert!(!named.is_empty(), "the registry should have at least one unaliased mutator");

        for name in named {
            let mut host = Sink::default();

            let _code = explain(
                &mut host,
                &ExplainArgs {
                    subject: name.to_owned(),
                    dir: Utf8PathBuf::from("."),
                    report: None,
                    features: super::super::cli::FeatureArgs::default(),
                    packages: Vec::new(),
                    workspace: false,
                    config: super::super::cli::ConfigArgs::default(),
                },
            )
            .expect("explain");

            assert!(!host.out().contains("also known as"), "{}", host.out());
        }
    }

    /// A selector naming a whole family explains every mutator in it.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_family_selector_explains_each_of_its_mutators() {
        let mut host = Sink::default();

        let code = explain(
            &mut host,
            &ExplainArgs {
                subject: "relational".to_owned(),
                dir: Utf8PathBuf::from("."),
                report: None,
                features: super::super::cli::FeatureArgs::default(),
                packages: Vec::new(),
                workspace: false,
                config: super::super::cli::ConfigArgs::default(),
            },
        )
        .expect("explain");

        assert_eq!(code, EXIT_OK);
        assert!(
            host.out().lines().filter(|line| line.starts_with("relational.")).count() > 1,
            "{}",
            host.out()
        );
    }

    #[test]
    fn a_current_mutant_id_explains_its_source_identity() {
        let (_directory, root) = crate::fixtures::crate_dir("explain-current-mutant-", "pub fn less(a: i32, b: i32) -> bool { a < b }\n");
        let select = super::super::cli::SelectArgs {
            dir: root.clone(),
            ..super::super::cli::SelectArgs::default()
        };
        let plan =
            crate::discover::plan(&select, &registry::Selection::parse("all").expect("selection"), None, &mut |_| {}).expect("discovery");
        let mutant = plan.mutants.first().expect("a mutant");
        let mut host = Sink::default();

        let code = explain(&mut host, &args(mutant.id.to_string(), root)).expect("explain current mutant");

        assert_eq!(code, EXIT_OK);
        assert!(host.out().contains("current: yes"), "{}", host.out());
        assert!(host.out().contains("original:"), "{}", host.out());
        assert!(host.out().contains("replacement:"), "{}", host.out());
    }

    #[test]
    fn a_historical_mutant_remains_explainable_after_its_site_disappears() {
        let (_directory, root) = crate::fixtures::crate_dir("explain-historical-mutant-", "pub fn unchanged() {}\n");
        let report_path = root.join("history.json");
        let mut report = crate::fixtures::report();
        let mut result = crate::fixtures::mutant_result_at("deadbeefcafe", 1, "Survived");
        result.replacement = Some("false".into());
        result.status_reason = Some("no selected test failed".to_owned());
        let _ = report.files.insert(
            "src/lib.rs".to_owned(),
            FileResult {
                source: "pub fn old() -> bool { true }\n".to_owned(),
                language: "rust".to_owned(),
                mutants: vec![result],
            },
        );
        crate::elements::write_json(&report, &report_path).expect("report");
        let mut explain_args = args("deadbeefcafe", root);
        explain_args.report = Some(report_path);
        explain_args.dir = explain_args.dir.join("workspace-no-longer-present");
        let mut host = Sink::default();

        let code = explain(&mut host, &explain_args).expect("explain historical mutant");

        assert_eq!(code, EXIT_OK);
        assert!(host.out().contains("current: no"), "{}", host.out());
        assert!(host.out().contains("verdict: Survived"), "{}", host.out());
        assert!(host.out().contains("reported source:"), "{}", host.out());
    }

    #[test]
    fn configured_artifact_directory_supplies_the_default_report() {
        let (_directory, root) = crate::fixtures::crate_dir("explain-configured-report-", "pub fn value() -> bool { true }\n");
        let select = super::super::cli::SelectArgs {
            dir: root.clone(),
            ..super::super::cli::SelectArgs::default()
        };
        let plan =
            crate::discover::plan(&select, &registry::Selection::parse("all").expect("selection"), None, &mut |_| {}).expect("discovery");
        let mutant = plan.mutants.first().expect("a mutant");
        let artifact_dir = root.join("artifacts");
        std::fs::create_dir_all(&artifact_dir).expect("artifact directory");
        std::fs::write(root.join("gamma.toml"), format!("artifact-dir = '{}'\n", artifact_dir.as_str())).expect("config");
        let mut report = crate::fixtures::report();
        let _ = report.files.insert(
            "src/lib.rs".to_owned(),
            FileResult {
                source: "pub fn value() -> bool { true }\n".to_owned(),
                language: "rust".to_owned(),
                mutants: vec![crate::fixtures::mutant_result_at(mutant.id.as_str(), 1, "Survived")],
            },
        );
        crate::elements::write_json(&report, &artifact_dir.join("gamma-report.json")).expect("report");
        let mut host = Sink::default();

        let code = explain(&mut host, &args(mutant.id.to_string(), root)).expect("explain configured report");

        assert_eq!(code, EXIT_OK);
        assert!(host.out().contains("verdict: Survived"), "{}", host.out());
        assert!(host.out().contains("artifacts\\gamma-report.json") || host.out().contains("artifacts/gamma-report.json"));
    }

    #[test]
    fn relative_configured_artifact_directory_keeps_run_semantics() {
        assert_eq!(
            default_report_path(Utf8Path::new("/workspace"), Some(&Utf8PathBuf::from("artifacts"))),
            Utf8PathBuf::from("artifacts/gamma-report.json")
        );
    }
}
