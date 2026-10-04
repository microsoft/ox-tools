// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Cargo-compatible command-line dispatch and artifact lifecycle.

use std::fs;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::config::Config;
use crate::model::{Candidates, Report};
use crate::{Result, execute, fail, materialize, selection, source};

#[derive(Debug, Parser)]
#[command(
    name = "cargo release-guard",
    version,
    about = "Rehearse release candidates against registry dependencies without publishing"
)]
struct Cli {
    #[command(subcommand)]
    operation: Operation,
}

#[derive(Debug, Subcommand)]
enum Operation {
    /// Identify publication intent against the PR merge base.
    Candidates(Options),
    /// Construct and resolve an isolated publication workspace from a candidate report.
    Prepare(Options),
    /// Select, prepare, build production candidates, and test every retained member.
    Check(Options),
}

#[derive(Debug, Args)]
pub(crate) struct Options {
    /// PR base ref. Compared using git merge-base with HEAD.
    #[arg(long)]
    pub base: Option<String>,
    /// Workspace Cargo.toml or workspace directory.
    #[arg(long, default_value = ".")]
    pub manifest_path: PathBuf,
    /// Fresh or empty output directory; existing artifacts are never deleted.
    #[arg(long)]
    pub output_dir: PathBuf,
    /// JSON array of names replacing inferred candidates, including [] for no release.
    #[arg(long, conflicts_with = "candidate_report")]
    pub candidate_list: Option<PathBuf>,
    /// Reuse an unchanged `candidates.json` source snapshot.
    #[arg(long, conflicts_with_all = ["base", "candidate_list", "registry"])]
    pub candidate_report: Option<PathBuf>,
    /// Publication registry, permitted by each selected package.
    #[arg(long)]
    pub registry: Option<String>,
    /// Do not access the network; registry absence must still be established.
    #[arg(long)]
    pub offline: bool,
    /// Runner for workspace tests; both runners also execute Cargo doctests.
    #[arg(long, value_enum, default_value = "cargo")]
    pub test_runner: TestRunner,
    /// Supported feature configuration(s); repeat to run a matrix.
    #[arg(long, value_enum, default_value = "default")]
    pub feature_mode: Vec<FeatureMode>,
    /// Additional Cargo feature selections.
    #[arg(long)]
    pub features: Option<String>,
    /// Cargo compilation target.
    #[arg(long)]
    pub target: Option<String>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub(crate) enum TestRunner {
    Cargo,
    Nextest,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub(crate) enum FeatureMode {
    Default,
    NoDefault,
    All,
}

pub(crate) fn run() -> Result<()> {
    let mut arguments: Vec<_> = std::env::args_os().collect();
    if arguments.get(1).is_some_and(|argument| argument == "release-guard") {
        arguments.remove(1);
    }
    let cli = Cli::parse_from(arguments);
    let (operation, options) = match &cli.operation {
        Operation::Candidates(options) => ("candidates", options),
        Operation::Prepare(options) => ("prepare", options),
        Operation::Check(options) => ("check", options),
    };
    let output = source::claim_output(&options.output_dir)?;
    let mut report = Report::new(operation);
    let result = perform(operation, options, &output, &mut report);
    if let Err(error) = &result {
        report.status = "failed".into();
        report.diagnostics.push(execute::redact(&error.to_string()));
    }
    source::write_json(&output.join("report.json"), &report)?;
    if result.is_ok() {
        println!("release guard: {} ({})", report.status, output.join("report.json").display());
    }
    result
}

fn perform(operation: &str, options: &Options, output: &std::path::Path, report: &mut Report) -> Result<()> {
    if operation == "prepare" && options.candidate_report.is_none() {
        return fail("prepare requires --candidate-report <candidates.json>");
    }
    let manifest = source::manifest_path(&options.manifest_path)?;
    let manifest_root = manifest
        .parent()
        .expect("canonical path names a Cargo.toml file, not a filesystem root");
    let config = Config::load(manifest_root, output)?;
    let workspace = source::read_workspace(&manifest)?;
    if manifest_root != workspace.root {
        return fail("--manifest-path must identify the workspace root, not an individual member");
    }
    let mut candidates: Candidates = if let Some(path) = &options.candidate_report {
        let candidates = serde_json::from_slice(&fs::read(path)?)?;
        selection::validate(&workspace, &candidates, output, &config)?;
        candidates
    } else {
        let Some(base) = &options.base else {
            return fail("candidates/check requires --base <PR-base-ref> or --candidate-report");
        };
        selection::select(
            &workspace,
            base,
            options.candidate_list.as_deref(),
            options.registry.as_deref(),
            &config,
            output,
            &mut report.diagnostics,
        )?
    };
    if !candidates.artifact_roots.contains(&output.to_owned()) {
        candidates.artifact_roots.push(output.to_owned());
    }
    report.candidates.clone_from(&candidates.candidates);
    source::write_json(&output.join("candidates.json"), &candidates)?;
    if candidates.candidates.is_empty() {
        report.status = "no_release".into();
        return Ok(());
    }
    if operation == "candidates" {
        report.status = "selected".into();
        return Ok(());
    }
    execute::probe_candidates(&candidates, output, &config, options, report)?;
    let directory = materialize::prepare(&workspace, &candidates, output, &config, report)?;
    if operation == "check" {
        execute::check(&directory, &workspace, &candidates, &config, options, report)?;
    } else {
        for mode in &options.feature_mode {
            execute::provenance(&directory, &workspace, &candidates, &config, options, *mode, report)?;
        }
    }
    selection::validate(&workspace, &candidates, output, &config)?;
    report.status = if operation == "check" { "passed" } else { "prepared" }.into();
    Ok(())
}
