//! A cargo sub-command to detect cyclic dependencies in workspace crates.
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
use anyhow::Result;

fn main() -> Result<()> {
    cargo_ensure_no_cyclic_deps::run()
}
