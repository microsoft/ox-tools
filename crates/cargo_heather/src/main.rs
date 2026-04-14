// Licensed under the MIT License.

//! `cargo-heather`: Validate license headers in Rust source files.

use clap::Parser;
use ohno::AppError;
use tracing_subscriber::fmt::format::FmtSpan;

use cargo_heather::cli::CargoCli;

fn main() -> Result<(), AppError> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(false)
        .with_span_events(FmtSpan::NONE)
        .without_time()
        .init();

    let CargoCli::Heather(args) = CargoCli::parse();
    cargo_heather::run(&args)
}
