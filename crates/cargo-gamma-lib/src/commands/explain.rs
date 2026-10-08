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

fn mutant_id_syntax(subject: &str) -> bool {
    super::CanonicalMutantId::parse(subject).is_ok()
}

#[expect(
    clippy::too_many_lines,
    reason = "current discovery and retained-report context are one explanation transaction"
)]
fn explain_mutant<H: Host>(host: &mut H, args: &ExplainArgs) -> crate::Result<i32> {
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
        let plan = crate::discover::plan_for_build(&select, &selection, None, &cargo, &mut |_| {})?;
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
            .and_then(|report| report.config.as_ref())
            .and_then(|config| config.mutant_id_version)
            .unwrap_or(MUTANT_ID_VERSION)
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
