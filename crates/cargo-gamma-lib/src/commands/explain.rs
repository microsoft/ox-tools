// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::Write;

use super::cli::ExplainArgs;
use super::dispatch::EXIT_OK;
use super::host::Host;
use crate::commands::exact;
use crate::config::Config;
use crate::discover::Survey;
use crate::elements::Report;
use crate::error::error;
use crate::model::Outcome;
use crate::ops::registry;
use crate::report::encode_controls;

/// Implements `explain`.
pub(super) fn explain<H: Host>(host: &mut H, args: &ExplainArgs) -> crate::Result<i32> {
    if args.report.is_some() {
        return historical(host, args);
    }
    if exact::is_id(&args.subject) {
        return current(host, args);
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

        if !mutator.aliases.is_empty() {
            writeln!(stream, "  also known as: {}", mutator.aliases.join(", "))?;
        }

        writeln!(stream, "  suppress with: // #[gamma::skip({})]", mutator.name)?;
        writeln!(stream)?;
    }

    Ok(EXIT_OK)
}

fn current<H: Host>(host: &mut H, args: &ExplainArgs) -> crate::Result<i32> {
    let mut select = args.select.clone();
    let config = Config::resolve(&select)?;
    let cargo = config.cargo_options(&select);
    config.apply_selection(&mut select)?;
    exact::validate_options(&select, true, false)?;
    let selection = exact::selection(&select, true)?;
    let request =
        exact::Request::load(std::slice::from_ref(&args.subject), None)?.expect("one explicit subject always creates an exact request");
    let mut survey = Survey::for_build(&select, None, &cargo)?;
    request.resolve(&mut survey, &selection, true)?;
    let scanned = survey.scan(None, &selection, &mut 0)?;
    let mutant = &scanned.mutants[0];
    let mut stream = host.results();
    writeln!(stream, "{} (current workspace discovery; not executed)", mutant.id)?;
    writeln!(stream, "  package: {}", encode_controls(&mutant.package))?;
    writeln!(stream, "  item: {}", encode_controls(&mutant.item_path))?;
    writeln!(
        stream,
        "  location: {}:{}:{}",
        encode_controls(mutant.file.as_str()),
        mutant.line,
        mutant.column
    )?;
    writeln!(stream, "  mutator: {}", encode_controls(&mutant.mutator))?;
    writeln!(stream, "  original: {}", encode_controls(&mutant.original))?;
    writeln!(stream, "  replacement: {}", encode_controls(&mutant.replacement))?;
    writeln!(
        stream,
        "  eligibility: {}",
        if mutant.outcome == Outcome::Ignored {
            "suppressed"
        } else {
            "selected"
        }
    )?;
    if let Some(suppression) = &mutant.suppression {
        writeln!(
            stream,
            "  suppression: {} ({})",
            suppression.channel.as_str(),
            encode_controls(suppression.reason.as_deref().unwrap_or("no reason recorded"))
        )?;
    }
    Ok(EXIT_OK)
}

fn historical<H: Host>(host: &mut H, args: &ExplainArgs) -> crate::Result<i32> {
    let path = args.report.as_ref().expect("historical explanation requires a report argument");
    let input = crate::merge::read_limited(path, u64::MAX).map_err(|cause| {
        error!(
            "cannot explain report `{}`: {}",
            encode_controls(path.as_str()),
            encode_controls(&cause.to_string())
        )
        .usage()
    })?;
    exact::validate_report(&input.report)?;
    let report = &input.report;
    let (path, file, mutant) = report
        .files
        .iter()
        .find_map(|(path, file)| {
            file.mutants
                .iter()
                .find(|mutant| mutant.id == args.subject)
                .map(|mutant| (path, file, mutant))
        })
        .ok_or_else(|| {
            error!(
                "unknown historical mutant ID `{}`; the report contains no such ID",
                encode_controls(&args.subject)
            )
            .usage()
        })?;
    let span = exact::source_span(&file.source, mutant.location)?;
    let mut stream = host.results();
    writeln!(
        stream,
        "{} (historical evidence; current source correspondence not checked)",
        encode_controls(&mutant.id)
    )?;
    writeln!(stream, "  report identity: {}", input.identity)?;
    writeln!(
        stream,
        "  report producer: {} {}",
        encode_controls(&report.framework.name),
        encode_controls(&report.framework.version)
    )?;
    writeln!(stream, "  report schema: {}", encode_controls(&report.schema_version))?;
    write_provenance(&mut stream, report, &mutant.id, path)?;
    writeln!(
        stream,
        "  location: {}:{}:{}",
        encode_controls(path),
        mutant.location.start.line,
        mutant.location.start.column
    )?;
    writeln!(stream, "  mutator: {}", encode_controls(&mutant.mutator_name))?;
    writeln!(stream, "  original (embedded source): {}", encode_controls(&file.source[span]))?;
    writeln!(
        stream,
        "  replacement: {}",
        encode_controls(mutant.replacement.as_deref().unwrap_or("unavailable"))
    )?;
    writeln!(
        stream,
        "  recorded outcome: {}",
        report
            .gamma_outcome(mutant)
            .map_or_else(|| mutant.status.to_string(), |outcome| format!("{outcome:?}"))
    )?;
    writeln!(
        stream,
        "  recorded evidence: {}",
        encode_controls(mutant.status_reason.as_deref().unwrap_or("unavailable"))
    )?;
    writeln!(
        stream,
        "  recorded tests: {}",
        encode_controls(
            &mutant
                .killed_by
                .as_ref()
                .map_or_else(|| "unavailable".to_owned(), |tests| tests.join(", "))
        )
    )?;
    Ok(EXIT_OK)
}

fn write_provenance(stream: &mut impl Write, report: &Report, id: &str, path: &str) -> crate::Result<()> {
    if let Some(config) = &report.config {
        writeln!(stream, "  report started at (Unix seconds): {}", config.started_at)?;
        writeln!(
            stream,
            "  mutant-ID scheme: {}",
            config
                .mutant_id_version
                .map_or_else(|| "unavailable".to_owned(), |version| version.to_string())
        )?;
        if let Some(verdict) = config.merge_provenance.as_ref().and_then(|provenance| provenance.verdicts.get(id)) {
            writeln!(
                stream,
                "  verdict origin: {} (Unix seconds: {})",
                encode_controls(&verdict.origin),
                verdict.started_at
            )?;
            writeln!(stream, "  verdict lineage: {}", encode_controls(&verdict.lineage))?;
        }
        if let Some(source) = config.merge_provenance.as_ref().and_then(|provenance| provenance.sources.get(path)) {
            writeln!(
                stream,
                "  source origin: {} (Unix seconds: {})",
                encode_controls(&source.origin),
                source.started_at
            )?;
        }
    } else {
        writeln!(stream, "  report time and mutant-ID scheme: unavailable")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{BrokenHost, Sink};

    #[test]
    fn explanation_names_aliases_and_suppressions() {
        let mut host = Sink::default();

        let code = explain(
            &mut host,
            &ExplainArgs {
                subject: "fn_value.default".to_owned(),
                ..ExplainArgs::default()
            },
        )
        .expect("explain");
        let text = String::from_utf8(host.out).expect("utf-8");

        assert_eq!(code, EXIT_OK);
        assert!(text.contains("also known as: RV"), "{text}");
        assert!(text.contains("suppress with: // #[gamma::skip(fn_value.default)]"), "{text}");
    }

    /// Piping into a consumer that exits early is successful consumption.
    #[test]
    fn a_closed_output_stream_ends_explanation_successfully() {
        let code = explain(
            &mut BrokenHost,
            &ExplainArgs {
                subject: "relational".to_owned(),
                ..ExplainArgs::default()
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
                    ..ExplainArgs::default()
                },
            )
            .expect("explain");

            assert!(!host.out().contains("also known as"), "{}", host.out());
        }
    }

    /// A selector naming a whole family explains every mutator in it.
    #[test]
    fn a_family_selector_explains_each_of_its_mutators() {
        let mut host = Sink::default();

        let code = explain(
            &mut host,
            &ExplainArgs {
                subject: "relational".to_owned(),
                ..ExplainArgs::default()
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
}
