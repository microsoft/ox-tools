// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::Write;

use super::cli::{CleanArgs, FeatureArgs};
use super::dispatch::EXIT_OK;
use super::host::Host;
use crate::discover::load_metadata;
use crate::exec::{clean_cache, gamma_base};
use crate::report::Styler;

/// Deletes the external cache belonging to the resolved workspace.
pub(super) fn clean<H: Host>(host: &mut H, args: &CleanArgs, styler: Styler) -> crate::Result<i32> {
    let metadata = load_metadata(&args.dir, &FeatureArgs::default())?;
    let root = camino::Utf8Path::new(metadata.workspace_root.as_str());
    let base = gamma_base(root, None);

    if clean_cache(root)? {
        writeln!(host.error(), "{} `{base}`", styler.verb("Cleaned"))?;
    } else {
        writeln!(host.error(), "{} no cached data under `{base}`", styler.verb("Finished"))?;
    }

    Ok(EXIT_OK)
}
