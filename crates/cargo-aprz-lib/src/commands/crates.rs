// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use clap::Parser;

use super::Host;
use super::common::{Common, CommonArgs};
use crate::Result;
use crate::facts::CrateRef;

#[derive(Parser, Debug)]
pub struct CratesArgs {
    /// Crates to appraise (format: `crate_name` or `crate_name@version`)
    #[arg(value_name = "CRATE")]
    pub crates: Vec<CrateRef>,

    #[command(flatten)]
    pub common: CommonArgs,
}

pub async fn process_crates<H: Host>(host: &mut H, args: &CratesArgs) -> Result<()> {
    let mut common = Common::new(host, &args.common).await?;
    let crate_facts = common.process_crates(&args.crates, suggestions_enabled()).await?;

    common.report(crate_facts)
}

const fn suggestions_enabled() -> bool {
    true
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use camino::Utf8Path;

    use super::*;

    #[test]
    fn crates_requests_spelling_suggestions() {
        assert!(suggestions_enabled());
    }

    #[tokio::test]
    async fn crates_propagates_initialization_failures() {
        let missing_manifest = Utf8Path::new(env!("CARGO_MANIFEST_DIR")).join("target/crates-command-tests/missing/Cargo.toml");
        let args = CratesArgs::parse_from(["crates", "--manifest-path", missing_manifest.as_str()]);
        let mut host = crate::commands::host::TestHost::new();

        let error = process_crates(&mut host, &args)
            .await
            .expect_err("a missing manifest must not be reported as success");

        assert!(error.to_string().contains("retrieving workspace metadata"), "{error}");
    }
}
