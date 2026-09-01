// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::Write;

use super::cli::{CleanArgs, FeatureArgs};
use super::dispatch::EXIT_OK;
use super::host::Host;
use crate::discover::load_metadata;
use crate::exec::{clean_cache, gamma_base};
use crate::report::{Styler, encode_controls};

/// Deletes the external cache belonging to the resolved workspace.
pub(super) fn clean<H: Host>(host: &mut H, args: &CleanArgs, styler: Styler) -> crate::Result<i32> {
    let metadata = load_metadata(&args.dir, &FeatureArgs::default())?;
    let root = camino::Utf8Path::new(metadata.workspace_root.as_str());
    let base = gamma_base(root, None);
    let cleaned = clean_cache(root)?;

    if cleaned {
        writeln!(host.error(), "{} `{}`", styler.verb("Cleaned"), encode_controls(base.as_str()))?;
    } else {
        writeln!(
            host.error(),
            "{} no cached data under `{}`",
            styler.verb("Finished"),
            encode_controls(base.as_str())
        )?;
    }

    Ok(EXIT_OK)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use camino::Utf8PathBuf;

    use super::*;
    use crate::exec::claim_workspace;
    use crate::testing::Sink;

    fn workspace() -> (tempfile::TempDir, Utf8PathBuf) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("temporary path should be UTF-8");

        fs::write(root.join("Cargo.toml"), "[workspace]\nresolver = \"3\"\n").expect("workspace manifest");

        (directory, root)
    }

    #[test]
    fn clean_reports_whether_cached_data_existed() {
        let (_directory, root) = workspace();
        let args = CleanArgs { dir: root.clone() };
        let styler = Styler::new(false);
        let base = gamma_base(&root, None);
        let mut host = Sink::default();

        assert_eq!(clean(&mut host, &args, styler).expect("clean absent cache"), EXIT_OK);
        assert!(host.err().contains("Finished no cached data"), "{}", host.err());

        drop(claim_workspace(&root).expect("claim cache"));
        fs::write(base.join("cached"), "data").expect("cached data");
        let mut host = Sink::default();

        assert_eq!(clean(&mut host, &args, styler).expect("clean populated cache"), EXIT_OK);
        assert!(host.err().contains("Cleaned"), "{}", host.err());
        assert!(!base.join("cached").exists());

        fs::remove_dir_all(base).expect("remove test cache");
    }
}
