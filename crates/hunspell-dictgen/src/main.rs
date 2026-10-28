// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

use std::path::PathBuf;

use clap::Parser;
use ohno::{AppError, IntoAppError};

#[mutants::skip] // Entry point: one-line dispatch to the library API.
#[cfg_attr(coverage_nightly, coverage(off))]
fn main() -> Result<(), AppError> {
    let cli = Cli::parse();

    hunspell_dictgen::generate(&cli.input, &cli.output).into_app_err("failed to generate Hunspell dictionary")
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
}
