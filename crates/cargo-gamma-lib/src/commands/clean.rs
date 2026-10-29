// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::Write;

use super::cli::{CleanArgs, FeatureArgs};
use super::dispatch::EXIT_OK;
use super::host::Host;
use crate::discover::load_metadata;
use crate::exec::{campaign_base, clean_cache, gamma_base};
use crate::report::{Styler, encode_controls};

/// Deletes the cache belonging to the resolved workspace.
pub(super) fn clean<H: Host>(host: &mut H, args: &CleanArgs, styler: Styler) -> crate::Result<i32> {
    clean_with(
        host,
        args,
        styler,
        |dir| {
            let metadata = load_metadata(dir, &FeatureArgs::default())?;
            Ok((metadata.workspace_root, metadata.target_directory))
        },
        clean_cache,
    )
}

fn clean_with<H: Host>(
    host: &mut H,
    args: &CleanArgs,
    styler: Styler,
    metadata: impl FnOnce(&camino::Utf8Path) -> crate::Result<(camino::Utf8PathBuf, camino::Utf8PathBuf)>,
    remove: impl FnOnce(&camino::Utf8Path, &camino::Utf8Path) -> crate::Result<bool>,
) -> crate::Result<i32> {
    let (root, target) = metadata(&args.dir)?;
    let base = campaign_base(&root, &target, None);
    let scratch = gamma_base(&root, None);
    let cleaned = remove(&root, &target)?;

    if cleaned {
        writeln!(
            host.error(),
            "{} `{}` and `{}`",
            styler.verb("Cleaned"),
            encode_controls(base.as_str()),
            encode_controls(scratch.as_str())
        )?;
    } else {
        writeln!(
            host.error(),
            "{} no cached data under `{}` or `{}`",
            styler.verb("Finished"),
            encode_controls(base.as_str()),
            encode_controls(scratch.as_str())
        )?;
    }

    Ok(EXIT_OK)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::cell::Cell;
    use std::fs;

    use camino::Utf8PathBuf;

    use super::*;
    use crate::exec::claim_workspace;
    use crate::testing::Sink;

    #[test]
    fn recovered_native_clean_returns_metadata_execution_errors_before_any_cache_removal_or_success_message() {
        let args = CleanArgs {
            dir: "scripted-clean-metadata-root".into(),
        };
        let calls = std::rc::Rc::new(Cell::new(0));
        let mut host = Sink::default();
        let failure = crate::discover::with_metadata_reader(
            Box::new({
                let calls = std::rc::Rc::clone(&calls);
                move |metadata| {
                    let command = metadata.cargo_command();
                    assert_eq!(
                        command.get_current_dir(),
                        Some(std::path::Path::new("scripted-clean-metadata-root"))
                    );
                    assert!(command.get_args().any(|argument| argument == "--no-deps"));
                    calls.set(calls.get() + 1);
                    Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied).into())
                }
            }),
            || clean(&mut host, &args, Styler::new(false)),
        )
        .unwrap_err();
        assert_eq!(
            failure.message(),
            "could not read cargo metadata for `scripted-clean-metadata-root`"
        );
        assert_eq!(calls.get(), 1);
        assert!(host.out().is_empty());
        assert!(host.err().is_empty());
    }

    fn clean_fixture<H: Host>(host: &mut H, args: &CleanArgs, styler: Styler) -> crate::Result<i32> {
        clean_with(host, args, styler, |dir| Ok((dir.to_owned(), dir.join("target"))), clean_cache)
    }

    #[test]
    fn survivor_clean_retains_requested_color_for_both_empty_and_populated_cache_results() {
        let root = Utf8PathBuf::from("workspace");
        let args = CleanArgs { dir: root.clone() };
        let base = campaign_base(&root, &root.join("target"), None);
        let scratch = gamma_base(&root, None);
        for cleaned in [false, true] {
            let mut host = Sink::default();
            let removed = Cell::new(false);
            assert_eq!(
                clean_with(
                    &mut host,
                    &args,
                    Styler::new(true),
                    |dir| Ok((dir.to_owned(), dir.join("target"))),
                    |workspace, target| {
                        assert_eq!(workspace, root);
                        assert_eq!(target, root.join("target"));
                        removed.set(true);
                        Ok(cleaned)
                    },
                )
                .expect("a scripted successful removal must report successfully"),
                EXIT_OK
            );
            assert!(removed.get(), "reporting must invoke the cache remover");
            let expected = if cleaned {
                format!(
                    "{} `{}` and `{}`\n",
                    Styler::new(true).verb("Cleaned"),
                    encode_controls(base.as_str()),
                    encode_controls(scratch.as_str())
                )
            } else {
                format!(
                    "{} no cached data under `{}` or `{}`\n",
                    Styler::new(true).verb("Finished"),
                    encode_controls(base.as_str()),
                    encode_controls(scratch.as_str())
                )
            };
            assert_eq!(host.err(), expected);
        }
    }

    #[test]
    fn survivor_clean_propagates_scripted_metadata_and_competing_workspace_lock_errors() {
        let root = Utf8PathBuf::from("workspace");
        let mut host = Sink::default();
        let error = clean_with(
            &mut host,
            &CleanArgs { dir: root.clone() },
            Styler::new(false),
            |dir| {
                assert_eq!(dir, root);
                Err(crate::error::Error::new("scripted metadata failure"))
            },
            |_, _| panic!("metadata failure must stop before cache removal"),
        )
        .expect_err("metadata cannot load a workspace directory that does not exist");
        assert_eq!(error.to_string(), "scripted metadata failure");
        assert!(host.err().is_empty(), "metadata failure must not claim a successful clean");
        let error = clean_with(
            &mut host,
            &CleanArgs { dir: root.clone() },
            Styler::new(false),
            |dir| Ok((dir.to_owned(), dir.join("target"))),
            |workspace, target| {
                assert_eq!(workspace, root);
                assert_eq!(target, root.join("target"));
                Err(crate::error::Error::new("scripted competing workspace lock"))
            },
        )
        .expect_err("the nonblocking workspace lock must prevent cleaning an active campaign");
        assert_eq!(error.to_string(), "scripted competing workspace lock");
        assert!(host.err().is_empty(), "a lock failure must not claim a successful clean");
    }

    #[test]
    fn survivor_clean_propagates_each_empty_and_populated_diagnostic_writer_failure() {
        let root = Utf8PathBuf::from("workspace");
        let args = CleanArgs { dir: root.clone() };
        for cleaned in [false, true] {
            crate::testing::fails_at_every_line(1, |host| {
                let removed = Cell::new(false);
                let result = clean_with(
                    host,
                    &args,
                    Styler::new(false),
                    |dir| Ok((dir.to_owned(), dir.join("target"))),
                    |workspace, target| {
                        assert_eq!(workspace, root);
                        assert_eq!(target, root.join("target"));
                        removed.set(true);
                        Ok(cleaned)
                    },
                );
                assert!(removed.get(), "cache removal must precede the diagnostic writer failure");
                result.map(|_| ())
            });
        }
    }

    fn workspace() -> (tempfile::TempDir, Utf8PathBuf) {
        let directory = crate::testing::workdir("clean-cache-output-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("temporary path should be UTF-8");

        (directory, root)
    }

    #[test]
    fn clean_reports_whether_cached_data_existed() {
        let (_directory, root) = workspace();
        let args = CleanArgs { dir: root.clone() };
        let styler = Styler::new(false);
        let base = gamma_base(&root, None);
        let mut host = Sink::default();

        assert_eq!(clean_fixture(&mut host, &args, styler).expect("clean absent cache"), EXIT_OK);
        assert!(host.err().contains("Finished no cached data"), "{}", host.err());

        drop(claim_workspace(&root).expect("claim cache"));
        fs::write(base.join("cached"), "data").expect("cached data");
        let mut host = Sink::default();

        assert_eq!(clean_fixture(&mut host, &args, styler).expect("clean populated cache"), EXIT_OK);
        assert!(host.err().contains("Cleaned"), "{}", host.err());
        assert!(!base.join("cached").exists());

        fs::remove_dir_all(base).expect("remove test cache");
    }
}
