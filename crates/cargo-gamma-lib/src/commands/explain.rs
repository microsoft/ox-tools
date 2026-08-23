// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::Write;

use super::cli::ExplainArgs;
use super::dispatch::EXIT_OK;
use super::host::Host;
use crate::ops::registry;

/// Implements `explain`.
pub(super) fn explain<H: Host>(host: &mut H, args: &ExplainArgs) -> crate::Result<i32> {
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

            let _code = explain(&mut host, &ExplainArgs { subject: name.to_owned() }).expect("explain");

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
