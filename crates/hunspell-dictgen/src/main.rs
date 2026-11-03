// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

use std::path::{Path, PathBuf};

use clap::Parser;
use ohno::{AppError, IntoAppError};

#[cfg_attr(coverage_nightly, coverage(off))]
// #[gamma::skip(fn_value.ok, reason = "The entry point only parses native process arguments and forwards to run_with; in-process tests cover parsing and the injected generator without launching a process.", tag = "native-boundary")]
fn main() -> Result<(), AppError> {
    run_with(&Cli::parse(), hunspell_dictgen::generate)
}

fn run_with<E: std::error::Error + Send + Sync + 'static>(
    cli: &Cli,
    generate: impl FnOnce(&Path, &Path) -> Result<(), E>,
) -> Result<(), AppError> {
    generate(&cli.input, &cli.output).into_app_err("failed to generate Hunspell dictionary")
}

/// Generate a deterministic Hunspell dictionary from a UTF-8 word list.
#[derive(Debug, Parser)]
struct Cli {
    /// Path to the human-maintained input word list.
    #[arg(long, value_name = "PATH")]
    input: PathBuf,

    /// Path where the generated Hunspell dictionary will be written.
    #[arg(long, value_name = "PATH")]
    output: PathBuf,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::cell::Cell;
    use std::io;

    use super::*;

    #[test]
    fn cli_parses_input_and_output_as_paths() {
        let cli = Cli::try_parse_from(["hunspell-dictgen", "--input", ".spelling", "--output", "target/spelling.dic"])
            .expect("valid explicit paths should parse");

        assert_eq!(
            (cli.input, cli.output),
            (PathBuf::from(".spelling"), PathBuf::from("target/spelling.dic"))
        );
    }

    #[test]
    fn dispatch_forwards_paths_once_and_preserves_success() {
        let calls = Cell::new(0);
        let cli = Cli {
            input: "words.list".into(),
            output: "words.dic".into(),
        };
        run_with(&cli, |input, output| {
            calls.set(calls.get() + 1);
            assert_eq!(input, Path::new("words.list"));
            assert_eq!(output, Path::new("words.dic"));
            Ok::<(), io::Error>(())
        })
        .unwrap();
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn dispatch_preserves_the_generation_failure_and_its_actionable_context() {
        let cli = Cli {
            input: "words.list".into(),
            output: "words.dic".into(),
        };
        let error = run_with(&cli, |input, output| {
            assert_eq!(input, Path::new("words.list"));
            assert_eq!(output, Path::new("words.dic"));
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "scripted dictionary refusal"))
        })
        .unwrap_err();
        assert_eq!(error.find_source::<io::Error>().unwrap().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.find_source::<io::Error>().unwrap().to_string(), "scripted dictionary refusal");
        let rendered = error.to_string();
        let contexts: Vec<_> = rendered.lines().map(|line| line.split(" (at ").next().unwrap()).collect();
        assert_eq!(
            contexts,
            ["scripted dictionary refusal", "> failed to generate Hunspell dictionary"]
        );
    }
}
