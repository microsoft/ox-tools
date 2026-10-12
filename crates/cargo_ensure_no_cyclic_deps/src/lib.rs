// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

//! A cargo sub-command that detects cyclic dependencies between crates in a workspace. This is useful if you
//! want to prevent dev-dependencies from creating dependency cycles as that can cause issues,
//! e.g. for [`cargo-release`](https://github.com/crate-ci/cargo-release).
//!
//! # Usage
//!
//! Run this command in a cargo workspace:
//!
//! ```bash
//! cargo ensure-no-cyclic-deps
//! ```
//!
//! The command will:
//! * Analyze all workspace crates
//! * Check for cyclic dependencies (including dev-dependencies)
//! * Report any cycles found
//! * Exit with code 1 if cycles are detected, 0 otherwise
//!
//! # Installation
//!
//! ```bash
//! cargo install --path .
//! ```
//!
//! Or from within the workspace:
//!
//! ```bash
//! cargo install cargo-ensure-no-cyclic-deps
//! ```
//!
//! # Example Output
//!
//! When cycles are detected:
//!
//! ```text
//! Error: Cyclic dependencies detected!
//!
//! Cycle 1:
//!   crate_a -> crate_b -> crate_c -> crate_a
//!
//! Cycle 2:
//!   crate_x -> crate_y -> crate_x
//! ```
//!
//! When no cycles are found:
//!
//! ```text
//! No cyclic dependencies found.
//! ```
//!
//! The tool will exit with code 0 if no cycles are found, or code 1 if cycles are detected.
//!
//! # Cycle Reporting
//!
//! When multiple cycles exist within the same strongly connected component,
//! the tool reports one representative directed cycle for that component,
//! with self-loops reported separately. Resolving the reported cycle may
//! reveal remaining cycles in the same component on subsequent runs.

#![doc(
    html_logo_url = "https://media.githubusercontent.com/media/microsoft/ox-tools/refs/heads/main/crates/cargo_ensure_no_cyclic_deps/logo.png"
)]
#![doc(
    html_favicon_url = "https://media.githubusercontent.com/media/microsoft/ox-tools/refs/heads/main/crates/cargo_ensure_no_cyclic_deps/favicon.ico"
)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::process::ExitCode;

use cargo_metadata::{Metadata, MetadataCommand, PackageId};
use clap::Parser;
use ohno::{AppError, IntoAppError};
use petgraph::algo::tarjan_scc;
use petgraph::graph::{DiGraph, NodeIndex};

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

/// Main entry point for the library, called from the binary crate
///
/// Returns [`ExitCode::SUCCESS`] when no cycles are found and
/// [`ExitCode::FAILURE`] when cycles are detected. Returning an exit
/// code (rather than calling `std::process::exit`) lets `main` unwind
/// normally so the process terminates through the standard runtime
/// path -- important under coverage instrumentation, where an abrupt
/// `process::exit` skips the profile flush on some platforms (notably
/// Windows) and drops the coverage for the cycle-detection path.
///
/// # Errors
/// Returns an error if cargo metadata cannot be loaded or processed
pub fn run() -> Result<ExitCode, AppError> {
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

    let metadata = cmd.exec().into_app_err("Failed to load cargo metadata")?;

    let cycles = detect_cycles(&metadata);

    if cycles.is_empty() {
        println!("No cyclic dependencies found.");
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!("Error: Cyclic dependencies detected!\n");
        for (i, cycle) in cycles.iter().enumerate() {
            eprintln!("Cycle {}:", i + 1);
            eprintln!("  {}", format_cycle(cycle, &metadata));
            eprintln!();
        }
        Ok(ExitCode::FAILURE)
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
        .map(|scc| find_cycle_in_scc(&graph, &scc))
        .collect();

    // Detect self-loops (a node depending on itself)
    for package in metadata.workspace_packages() {
        // Every workspace package was inserted into `node_map` above, so
        // index directly (matching the edge-building loop) rather than a
        // fallible lookup whose `None` arm can never be taken.
        let node_idx = node_map[&package.id];
        // Check if there's an edge from this node to itself
        if graph.contains_edge(node_idx, node_idx) {
            cycles.push(vec![package.id.clone()]);
        }
    }

    cycles
}

/// Finds a directed cycle path within an SCC by traversing graph edges.
fn find_cycle_in_scc(graph: &DiGraph<PackageId, ()>, scc: &[NodeIndex]) -> Vec<PackageId> {
    let scc_set: HashSet<NodeIndex> = scc.iter().copied().collect();
    let start = scc[0];

    let mut queue = VecDeque::new();
    let mut visited = HashSet::new();
    visited.insert(start);
    queue.push_back((start, vec![start]));

    let mut cycle = None;
    while let Some((curr, path)) = queue.pop_front() {
        for neighbor in graph.neighbors(curr) {
            if !scc_set.contains(&neighbor) {
                continue;
            }
            if neighbor == start && path.len() > 1 {
                cycle = Some(path.into_iter().map(|idx| graph[idx].clone()).collect());
                break;
            }
            if visited.insert(neighbor) {
                let mut next_path = path.clone();
                next_path.push(neighbor);
                queue.push_back((neighbor, next_path));
            }
        }
        if cycle.is_some() {
            break;
        }
    }

    cycle.expect("guarded by Tarjan SCC invariant that every multi-node component contains a directed cycle back to start")
}

/// Format a cycle for display
fn format_cycle(cycle: &[PackageId], metadata: &Metadata) -> String {
    let mut names: Vec<String> = cycle
        .iter()
        .map(|id| {
            metadata
                .packages
                .iter()
                .find(|p| &p.id == id)
                .map_or_else(|| id.to_string(), |p| p.name.to_string())
        })
        .collect();

    let (min_idx, _) = names
        .iter()
        .enumerate()
        .min_by_key(|(_, name)| *name)
        .expect("guarded by caller invariant that cycle contains at least one package");
    names.rotate_left(min_idx);

    names
        .iter()
        .chain(core::iter::once(&names[0]))
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" -> ")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn scc_cycle_ignores_self_loop_on_start_node() {
        let mut graph = DiGraph::<PackageId, ()>::new();
        let a = graph.add_node(PackageId {
            repr: "crate_a 0.1.0".to_owned(),
        });
        let b = graph.add_node(PackageId {
            repr: "crate_b 0.1.0".to_owned(),
        });
        let c = graph.add_node(PackageId {
            repr: "crate_c 0.1.0".to_owned(),
        });
        let d = graph.add_node(PackageId {
            repr: "crate_d 0.1.0".to_owned(),
        });

        // a -> b -> a and a -> c -> a: no cycle contains every SCC node.
        graph.add_edge(a, b, ());
        graph.add_edge(b, a, ());
        graph.add_edge(a, c, ());
        graph.add_edge(c, a, ());
        // a -> d: external dependency outside the SCC to verify scc_set filtering.
        graph.add_edge(a, d, ());
        // a -> a: self-loop on start node, added last so petgraph visits it first
        // among outgoing edges, verifying the start visited guard skips it.
        graph.add_edge(a, a, ());

        let scc = vec![a, b, c];
        let cycle = find_cycle_in_scc(&graph, &scc);
        assert_eq!(cycle.len(), 2);
        assert_eq!(cycle[0].repr, "crate_a 0.1.0");
        assert_eq!(cycle[1].repr, "crate_c 0.1.0");
    }
}
