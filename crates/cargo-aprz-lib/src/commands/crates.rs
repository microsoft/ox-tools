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
    common
        .process_crates(&args.crates, true)
        .await
        .and_then(|crate_facts| common.report(crate_facts))
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use camino::Utf8PathBuf;

    use super::*;
    use crate::commands::host::TestHost;

    #[tokio::test]
    async fn common_initialization_errors_are_returned() {
        let mut args = CratesArgs::parse_from(["crates"]);
        args.common.manifest_path = Utf8PathBuf::from("missing-manifest-for-crates-test.toml");
        let error = process_crates(&mut TestHost::new(), &args)
            .await
            .expect_err("a missing manifest must be reported");
        assert!(error.to_string().contains("retrieving workspace metadata"));
    }
}
