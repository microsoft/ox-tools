//! A cargo subcommand to detect cyclic dependencies in workspace crates.
//!
//! # Usage
//!
//! After installation, run in any cargo workspace:
//!
//! ```bash
//! cargo ensure-no-cyclic-deps
//! ```
//!
//! Or specify a manifest path:
//!
//! ```bash
//! cargo ensure-no-cyclic-deps --manifest-path path/to/Cargo.toml
//! ```
//!
//! The tool will exit with code 0 if no cycles are found, or code 1 if cycles are detected.

use anyhow::{Context, Result};
use cargo_metadata::{Metadata, MetadataCommand, PackageId};
use clap::Parser;
use petgraph::algo::tarjan_scc;
use petgraph::graph::DiGraph;
use std::collections::HashMap;

#[derive(Parser, Debug)]
#[command(
    name = "cargo-ensure-no-cyclic-deps",
    bin_name = "cargo",
    version,
    about = "Detects cyclic dependencies in workspace crates"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Command>,
}

#[derive(Parser, Debug)]
enum Command {
    #[command(name = "ensure-no-cyclic-deps")]
    EnsureNoCyclicDeps {
        /// Path to Cargo.toml
        #[arg(long, value_name = "PATH")]
        manifest_path: Option<std::path::PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let manifest_path = match cli.cmd {
        Some(Command::EnsureNoCyclicDeps { manifest_path }) => manifest_path,
        None => {
            // When called as `cargo-ensure-no-cyclic-deps` directly
            // (without the cargo wrapper), we still want it to work
            None
        }
    };

    let mut cmd = MetadataCommand::new();
    if let Some(path) = manifest_path {
        cmd.manifest_path(path);
    }
    // Use --no-deps to avoid Cargo resolving dependencies (which would fail on cycles)
    cmd.no_deps();

    let metadata = cmd.exec().context("Failed to load cargo metadata")?;

    let cycles = detect_cycles(&metadata);

    if cycles.is_empty() {
        println!("No cyclic dependencies found.");
        Ok(())
    } else {
        eprintln!("Error: Cyclic dependencies detected!\n");
        for (i, cycle) in cycles.iter().enumerate() {
            eprintln!("Cycle {}:", i + 1);
            eprintln!("  {}", format_cycle(cycle, &metadata));
            eprintln!();
        }
        std::process::exit(1);
    }
}

/// Detects cycles in workspace crate dependencies using Tarjan's strongly connected components algorithm
fn detect_cycles(metadata: &Metadata) -> Vec<Vec<PackageId>> {
    let mut graph = DiGraph::<PackageId, ()>::new();
    let mut node_map = HashMap::new();

    // Add nodes for each workspace package
    for package in metadata.workspace_packages() {
        let idx = graph.add_node(package.id.clone());
        node_map.insert(package.id.clone(), idx);
    }

    // Add edges for dependencies (only workspace dependencies)
    for package in metadata.workspace_packages() {
        let from_idx = node_map[&package.id];

        for dep in &package.dependencies {
            // Only consider workspace dependencies
            if let Some(dep_pkg) = metadata.packages.iter().find(|p| p.name == dep.name)
                && let Some(&to_idx) = node_map.get(&dep_pkg.id)
            {
                graph.add_edge(from_idx, to_idx, ());
            }
        }
    }

    // Find strongly connected components using Tarjan's algorithm
    let sccs = tarjan_scc(&graph);

    // Extract cycles (SCCs with more than one node indicate a cycle)
    // Also check for self-loops (nodes with edges to themselves)
    let mut cycles: Vec<Vec<PackageId>> = sccs
        .into_iter()
        .filter(|scc| scc.len() > 1)
        .map(|scc| scc.iter().map(|&idx| graph[idx].clone()).collect())
        .collect();

    // Detect self-loops (a node depending on itself)
    for package in metadata.workspace_packages() {
        if let Some(&node_idx) = node_map.get(&package.id) {
            // Check if there's an edge from this node to itself
            if graph.contains_edge(node_idx, node_idx) {
                cycles.push(vec![package.id.clone()]);
            }
        }
    }

    cycles
}

/// Format a cycle for display
fn format_cycle(cycle: &[PackageId], metadata: &Metadata) -> String {
    let names: Vec<String> = cycle
        .iter()
        .map(|id| {
            metadata
                .packages
                .iter()
                .find(|p| &p.id == id)
                .map_or_else(|| id.to_string(), |p| p.name.clone())
        })
        .collect();

    names
        .iter()
        .chain(core::iter::once(&names[0]))
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" -> ")
}
