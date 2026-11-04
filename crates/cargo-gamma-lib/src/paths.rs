// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolving paths before operations whose destination has to be known.

use std::fs;
use std::path::PathBuf;

use camino::{Utf8Component, Utf8Path, Utf8PathBuf};

use crate::Result;
use crate::error::error;

const MAX_SYMLINKS: usize = 40;

/// Resolves every extant symlink in `path`, including a dangling final link's target spelling.
///
/// `canonicalize` cannot answer this for a dangling link: it fails at the link rather than
/// reporting where a later create or rename would land. Following components one at a time keeps
/// that destination visible while still leaving a genuinely absent tail in place.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn physical(path: &Utf8Path) -> Result<Utf8PathBuf> {
    physical_with_links(path, |candidate| fs::read_link(candidate))
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn physical_with_links(path: &Utf8Path, mut read_link: impl FnMut(&Utf8Path) -> std::io::Result<PathBuf>) -> Result<Utf8PathBuf> {
    physical_with_filesystem(path, &mut read_link, |candidate| {
        fs::symlink_metadata(candidate.as_std_path()).map(|metadata| metadata.file_type().is_symlink())
    })
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn physical_with_filesystem(
    path: &Utf8Path,
    mut read_link: impl FnMut(&Utf8Path) -> std::io::Result<PathBuf>,
    mut is_link: impl FnMut(&Utf8Path) -> std::io::Result<bool>,
) -> Result<Utf8PathBuf> {
    let mut pending = absolute(path)?;
    let mut followed_links = 0;

    loop {
        let components: Vec<Utf8Component<'_>> = pending.components().collect();
        let mut resolved = Utf8PathBuf::new();
        // #[gamma::skip(literal.bool_flip, tag = "timeout", reason = "claiming a restart without following a link repeatedly resolves the same path")]
        let mut restarted = false;

        for (at, component) in components.iter().enumerate() {
            match component {
                Utf8Component::CurDir => {}

                Utf8Component::ParentDir => {
                    let _popped = resolved.pop();
                }

                Utf8Component::Prefix(_) | Utf8Component::RootDir => resolved.push(component.as_str()),

                Utf8Component::Normal(name) => {
                    let candidate = resolved.join(name);
                    let symlink = match is_link(&candidate) {
                        Ok(symlink) => symlink,

                        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => {
                            // #[gamma::skip(call_result.default, reason = "This call returns (); default_after performs the original side effects and substitutes only the identical unit value.", tag = "equivalent")]
                            append(
                                &mut resolved,
                                core::iter::once(*component).chain(components[at + 1..].iter().copied()),
                            );

                            return Ok(resolved);
                        }

                        Err(cause) => {
                            return Err(error!("could not resolve `{path}`").caused_by(cause));
                        }
                    };

                    if !symlink {
                        resolved = candidate;

                        continue;
                    }

                    // #[gamma::skip(literal.int_decrement, tag = "timeout", reason = "the symlink counter must advance to bound cyclic resolution")]
                    followed_links += 1;

                    // #[gamma::skip(cond.always_false, tag = "timeout", reason = "cyclic symlink resolution must stop at the traversal limit")]
                    if followed_links > MAX_SYMLINKS {
                        return Err(error!("could not resolve `{path}`: too many symlinks"));
                    }

                    let target = read_link(&candidate)
                        .map_err(|cause| error!("could not read the link `{candidate}` while resolving `{path}`").caused_by(cause))?;
                    let target = Utf8PathBuf::from_path_buf(target)
                        .map_err(|target| error!("the link `{candidate}` has a non-UTF-8 target `{}`", target.display()))?;
                    let mut next = if
                    // #[gamma::skip(cond.always_false, reason = "For an absolute target, Path::join replaces the base with that absolute target. Thus target directly versus resolved.join(target) yields the same absolute path on Unix and Windows; relative targets already take join in both versions.", tag = "equivalent")]
                    target.is_absolute() { target } else { resolved.join(target) };

                    for remaining in &components[at + 1..] {
                        next.push(remaining.as_str());
                    }
                    pending = absolute(&next)?;
                    restarted = true;

                    break;
                }
            }
        }

        // #[gamma::skip(all, reason = "mutating the restart guard prevents the symlink-resolution loop from terminating; the mutation runner can observe that only as a timeout")]
        if !restarted {
            return Ok(resolved);
        }
    }
}

/// Resolves `path` and refuses a destination outside `root`.
///
/// This check is for a destination that will be written, rather than a lexical spelling someone
/// happened to pass. A copied symlink can make those two places different.
pub(crate) fn require_within(path: &Utf8Path, root: &Utf8Path, purpose: &str) -> Result<Utf8PathBuf> {
    let destination = physical(path)?;
    let boundary = physical(root)?;

    if destination.starts_with(&boundary) {
        return Ok(destination);
    }

    Err(error!(
        "refusing to use {purpose} `{path}`, which resolves to `{destination}` outside its permitted root `{boundary}`"
    ))
}

/// Refuses output names that would publish over the same physical destination.
#[cfg_attr(coverage_nightly, coverage(off))]
pub(crate) fn reject_collisions(outputs: &[(&str, &Utf8Path)]) -> Result<()> {
    let mut destinations: Vec<(&str, Utf8PathBuf)> = Vec::with_capacity(outputs.len());

    for &(name, path) in outputs {
        // A path with a regular file where a directory is needed cannot be physically resolved,
        // but that is a publication error rather than an output-alias error. Keep validation
        // from pre-empting the writer's more useful diagnostic while still catching identical
        // lexical spellings among such invalid paths.
        let destination = match physical(path) {
            Ok(destination) => destination,
            Err(_unresolved) => lexical(path)?,
        };

        if let Some((other, _previous)) = destinations.iter().find(|(_other, previous)| previous == &destination) {
            return Err(error!(
                "`{name}` at `{path}` and `{other}` resolve to the same output path `{destination}`; choose distinct output paths"
            )
            .usage());
        }

        destinations.push((name, destination));
    }

    Ok(())
}

/// Makes `path` absolute without resolving its components.
///
/// Parent components must survive until [`physical`] has followed preceding symlinks: `link/..`
/// names the parent of the link's target, not necessarily the parent of the link itself.
fn absolute(path: &Utf8Path) -> Result<Utf8PathBuf> {
    absolute_with_current_dir(path, current_directory)
}

#[cfg(test)]
type CurrentDirectoryProvider = Box<dyn FnMut() -> std::io::Result<PathBuf>>;
#[cfg(test)]
thread_local! {
    static CURRENT_DIRECTORY: std::cell::RefCell<Option<CurrentDirectoryProvider>> = const { std::cell::RefCell::new(None) };
}

fn current_directory() -> std::io::Result<PathBuf> {
    #[cfg(test)]
    if let Some(result) = CURRENT_DIRECTORY.with_borrow_mut(|provider| provider.as_mut().map(|provider| provider())) {
        return result;
    }
    std::env::current_dir()
}

fn absolute_with_current_dir(path: &Utf8Path, current_dir: impl FnOnce() -> std::io::Result<PathBuf>) -> Result<Utf8PathBuf> {
    // #[gamma::skip(cond.always_false, reason = "joining an absolute path to the current directory returns that absolute path unchanged, so the fallback branch is observationally identical")]
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        let current =
            current_dir().map_err(|cause| error!("could not resolve the current directory while resolving `{path}`").caused_by(cause))?;
        let current = Utf8PathBuf::from_path_buf(current)
            .map_err(|current| error!("the current directory `{}` is not a UTF-8 path", current.display()))?;

        Ok(current.join(path))
    }
}

/// Absolutizes `path` and removes textual `.` and `..` components without consulting the filesystem.
///
/// Used only when a destination cannot be resolved at all. The eventual writer still reports that
/// failure; this spelling merely lets output validation compare otherwise-identical invalid paths.
fn lexical(path: &Utf8Path) -> Result<Utf8PathBuf> {
    let absolute = absolute(path)?;
    let mut normalized = Utf8PathBuf::new();

    // #[gamma::skip(call_result.default, reason = "This call returns (); default_after performs the original side effects and substitutes only the identical unit value.", tag = "equivalent")]
    append(&mut normalized, absolute.components());

    Ok(normalized)
}

/// Appends components while preserving their filesystem meaning below an absolute root.
#[cfg_attr(coverage_nightly, coverage(off))]
fn append<'path>(destination: &mut Utf8PathBuf, components: impl IntoIterator<Item = Utf8Component<'path>>) {
    for component in components {
        match component {
            Utf8Component::CurDir => {}

            Utf8Component::ParentDir => {
                let _popped = destination.pop();
            }

            Utf8Component::Prefix(_) | Utf8Component::RootDir | Utf8Component::Normal(_) => destination.push(component.as_str()),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod current_directory_tests {
    use super::*;

    #[test]
    fn current_directory_query_failures_are_returned_and_absolute_paths_do_not_query_it() {
        let relative = Utf8Path::new("output");
        let error = absolute_with_current_dir(relative, || {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected current directory failure",
            ))
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("could not resolve the current directory while resolving `output`"),
            "{error}"
        );
        assert!(error.to_string().contains("injected current directory failure"), "{error}");
        #[cfg(windows)]
        let root = Utf8PathBuf::from(r"C:\synthetic\output");
        #[cfg(not(windows))]
        let root = Utf8PathBuf::from("/synthetic/output");
        assert_eq!(
            absolute_with_current_dir(&root, || panic!("absolute paths must not query the current directory")).unwrap(),
            root
        );
    }
}

#[cfg(all(test, not(miri)))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #[cfg(windows)]
    #[test]
    fn recovered_drive_relative_symlink_target_propagates_the_restarted_current_directory_error() {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                CURRENT_DIRECTORY.with_borrow_mut(|provider| *provider = None);
            }
        }
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        CURRENT_DIRECTORY.with_borrow_mut(|provider| {
            assert!(provider.is_none());
            *provider = Some(Box::new({
                let calls = std::rc::Rc::clone(&calls);
                move || {
                    calls.set(calls.get() + 1);
                    Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
                }
            }));
        });
        let _reset = Reset;
        let path = Utf8Path::new(r"C:\synthetic\alias");
        let target = std::path::PathBuf::from(r"Q:relative");
        assert!(!path.parent().unwrap().as_std_path().join(&target).is_absolute());
        let mut links = 0;
        let failure = physical_with_filesystem(
            path,
            |candidate| {
                assert_eq!(candidate, path);
                links += 1;
                Ok(target.clone())
            },
            |candidate| Ok(candidate == path),
        )
        .unwrap_err();
        assert_eq!(
            failure.message(),
            r"could not resolve the current directory while resolving `Q:relative`"
        );
        assert_eq!((links, calls.get()), (1, 1));
    }

    use std::collections::VecDeque;
    #[cfg(any(unix, windows))]
    use std::ffi::OsString;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt as _;
    #[cfg(windows)]
    use std::os::windows::ffi::OsStringExt as _;

    use super::*;

    #[test]
    fn two_links_are_followed_before_a_parent_without_filesystem_access() {
        #[cfg(windows)]
        let root = Utf8PathBuf::from(r"C:\synthetic");
        #[cfg(not(windows))]
        let root = Utf8PathBuf::from("/synthetic");
        let first = root.join("link1");
        let second = root.join("a").join("link2");
        let mut links = VecDeque::from([
            (first.clone(), PathBuf::from("a")),
            (second.clone(), PathBuf::from("..").join("b").join("sub")),
        ]);
        let mut metadata = VecDeque::from([
            (root.clone(), false),
            (first, true),
            (root.clone(), false),
            (root.join("a"), false),
            (second, true),
            (root.clone(), false),
            (root.join("a"), false),
            (root.join("b"), false),
            (root.join("b").join("sub"), false),
            (root.join("b").join("victim"), false),
        ]);
        let destination = physical_with_filesystem(
            &root.join("link1").join("link2").join("..").join("victim"),
            |candidate| {
                let (expected, target) = links.pop_front().expect("every link read must be scripted");
                assert_eq!(candidate, expected);
                Ok(target)
            },
            |candidate| {
                let (expected, is_link) = metadata.pop_front().expect("every metadata query must be scripted");
                assert_eq!(candidate, expected);
                Ok(is_link)
            },
        )
        .unwrap();
        assert_eq!(destination, root.join("b").join("victim"));
        assert!(links.is_empty());
        assert!(metadata.is_empty());
    }

    fn root() -> (tempfile::TempDir, Utf8PathBuf) {
        let directory = crate::testing::workdir("paths-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 path");
        (directory, root)
    }

    #[cfg(windows)]
    fn directory_link(link: &Utf8Path, target: &Utf8Path) {
        let result = std::process::Command::new("cmd.exe")
            .args(["/c", "mklink", "/J"])
            .arg(link.as_str().replace('/', "\\"))
            .arg(target.as_str().replace('/', "\\"))
            .output()
            .expect("Windows test hosts provide cmd.exe for creating the owned junction fixture");
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    }

    #[cfg(unix)]
    fn directory_link(link: &Utf8Path, target: &Utf8Path) {
        std::os::unix::fs::symlink(target, link).expect("the owned test directory permits creating its symlink fixture");
    }

    #[test]
    fn filesystem_resolution_errors_are_not_treated_as_absent_tails_or_unwrapped_by_containment() {
        let (_directory, root) = root();
        let invalid = root.join("a".repeat(300));
        let error = physical(&invalid).expect_err("a component longer than the filesystem limit cannot be resolved");
        assert!(error.to_string().contains(&format!("could not resolve `{invalid}`")));
        for (destination, boundary) in [(&invalid, &root), (&root, &invalid)] {
            let error = require_within(destination, boundary, "a regression destination").unwrap_err();
            assert!(error.to_string().contains(&format!("could not resolve `{invalid}`")));
        }
    }
    #[test]
    #[cfg(unix)]
    fn exactly_forty_finite_symlinks_are_allowed_but_forty_one_are_not() {
        let (_directory, root) = root();
        for index in 0..41 {
            std::os::unix::fs::symlink(format!("link-{}", index + 1), root.join(format!("link-{index}"))).unwrap();
        }

        fs::write(root.join("link-41"), "destination").unwrap();
        assert_eq!(physical(&root.join("link-1")).unwrap(), physical(&root.join("link-41")).unwrap());
        assert!(
            physical(&root.join("link-0"))
                .unwrap_err()
                .to_string()
                .contains("too many symlinks")
        );
    }

    #[test]
    #[cfg(windows)]
    fn exactly_forty_finite_junctions_are_allowed_but_forty_one_are_not() {
        let (_directory, root) = root();
        fs::create_dir(root.join("link-41")).unwrap();
        for index in (0..41).rev() {
            directory_link(&root.join(format!("link-{index}")), &root.join(format!("link-{}", index + 1)));
        }
        assert_eq!(physical(&root.join("link-1")).unwrap(), physical(&root.join("link-41")).unwrap());
        assert!(
            physical(&root.join("link-0"))
                .unwrap_err()
                .to_string()
                .contains("too many symlinks")
        );
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn link_read_failures_after_metadata_are_returned_without_panicking() {
        let (_directory, root) = root();
        let target = root.join("destination");
        fs::create_dir(&target).unwrap();
        let link = root.join("link");
        directory_link(&link, &target);
        let error = physical_with_links(&link, |candidate| {
            assert!(candidate.ends_with("link"));
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected link read failure",
            ))
        })
        .unwrap_err();
        assert!(error.to_string().contains("could not read the link"), "{error}");
        assert!(error.to_string().contains("injected link read failure"), "{error}");
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn non_utf8_working_directory_failures_are_isolated_and_recoverable() {
        #[cfg(windows)]
        let name = OsString::from_wide(&[0xd800]);
        #[cfg(unix)]
        let name = OsString::from_vec(vec![0xff]);
        let directory = tempfile::tempdir().unwrap();
        let non_utf8 = directory.path().join(name);
        fs::create_dir(&non_utf8).unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "paths::tests::non_utf8_working_directory_helper", "--ignored"])
            .env("CARGO_GAMMA_NON_UTF8_CWD_HELPER", "1")
            .current_dir(&non_utf8)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "helper stdout: {}\nhelper stderr: {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }

    #[test]
    #[ignore = "spawned in an isolated non-UTF-8 working directory"]
    #[cfg(any(unix, windows))]
    fn non_utf8_working_directory_helper() {
        assert!(std::env::var_os("CARGO_GAMMA_NON_UTF8_CWD_HELPER").is_some());
        let path = Utf8Path::new("output");
        for error in [
            absolute(path).unwrap_err(),
            physical(path).unwrap_err(),
            lexical(path).unwrap_err(),
            reject_collisions(&[("report", path)]).unwrap_err(),
        ] {
            assert!(error.to_string().contains("is not a UTF-8 path"), "{error}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn non_utf8_symlink_targets_are_reported_without_panicking() {
        let (_directory, root) = root();
        let link = root.join("link");
        std::os::unix::fs::symlink(OsString::from_vec(vec![0xff]), &link).unwrap();
        let error = physical(&link).unwrap_err();
        let resolved_link = physical(&root).unwrap().join("link");
        assert!(
            error
                .to_string()
                .contains(&format!("the link `{resolved_link}` has a non-UTF-8 target"))
        );
    }

    #[test]
    #[cfg(windows)]
    fn non_utf8_junction_targets_are_reported_without_panicking() {
        let (_directory, root) = root();
        let target = PathBuf::from(root.as_str().replace('/', "\\")).join(OsString::from_wide(&[0xd800]));
        fs::create_dir(&target).unwrap();
        let link = root.join("link");
        let result = std::process::Command::new("cmd.exe")
            .args(["/c", "mklink", "/J"])
            .arg(link.as_str().replace('/', "\\"))
            .arg(&target)
            .output()
            .unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        let error = physical(&link).unwrap_err();
        let resolved_link = physical(&root).unwrap().join("link");
        assert!(
            error
                .to_string()
                .contains(&format!("the link `{resolved_link}` has a non-UTF-8 target")),
            "{error}"
        );
    }

    #[test]
    fn relative_paths_are_absolutized_and_absolute_paths_are_preserved() {
        let relative = absolute(Utf8Path::new("nested/file")).expect("current directory is available");
        assert!(relative.is_absolute(), "{relative}");
        assert!(relative.ends_with("nested/file"), "{relative}");

        let root = Utf8Path::new(if cfg!(windows) { r"C:\" } else { "/" });
        assert_eq!(absolute(root).expect("absolute path"), root);
    }

    #[test]
    fn absent_tails_are_preserved_after_the_existing_prefix_is_resolved() {
        let (_directory, root) = root();
        let expected = physical(&root).expect("root resolution").join("created/nested/file");

        assert_eq!(physical(&root.join("created/nested/file")).expect("resolution"), expected);
    }

    #[test]
    fn regular_existing_paths_resolve_to_themselves() {
        let (_directory, root) = root();
        let file = root.join("file");
        fs::write(&file, "contents").expect("file");

        assert_eq!(
            physical(&file).expect("resolution"),
            physical(&root).expect("root resolution").join("file")
        );
    }

    #[test]
    fn containment_accepts_descendants_and_refuses_siblings() {
        let (_directory, root) = root();
        let boundary = root.join("tree");
        fs::create_dir_all(&boundary).expect("tree");

        let inside = boundary.join("future/file");
        assert_eq!(
            require_within(&inside, &boundary, "a test destination").expect("inside destination"),
            physical(&boundary).expect("boundary resolution").join("future/file")
        );

        let outside = root.join("outside");
        let failure = require_within(&outside, &boundary, "a test destination").expect_err("sibling destination");
        assert!(failure.to_string().contains("outside its permitted root"), "{failure}");
    }

    #[test]
    fn collisions_are_detected_for_resolvable_and_unresolvable_paths() {
        let (_directory, root) = root();
        fs::create_dir_all(root.join("tree")).expect("tree");

        let same = root.join("tree/output");
        let failure = reject_collisions(&[("first", &same), ("second", &same)]).expect_err("same destination");
        assert!(failure.is_usage());
        assert!(failure.to_string().contains("same output path"), "{failure}");

        let blocker = root.join("blocker");
        fs::write(&blocker, "not a directory").expect("blocker");
        let first = blocker.join("child/../output");
        let second = blocker.join("output");
        let failure = reject_collisions(&[("first", &first), ("second", &second)]).expect_err("same lexical destination");
        assert!(failure.is_usage());
        assert!(failure.to_string().contains("same output path"), "{failure}");
    }

    #[test]
    fn lexical_normalization_removes_current_and_parent_components() {
        let normalized = lexical(Utf8Path::new("one/./two/../file")).expect("lexical path");
        assert!(normalized.is_absolute(), "{normalized}");
        assert!(normalized.ends_with("one/file"), "{normalized}");
    }

    #[test]
    #[cfg(windows)]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn windows_links_are_resolved_and_containment_uses_their_targets() {
        let (_directory, root) = root();
        let outside = root.join("outside");
        let tree = root.join("tree");
        let link = tree.join("link");
        fs::create_dir_all(&outside).expect("outside");
        fs::create_dir_all(&tree).expect("tree");
        if let Err(cause) = std::os::windows::fs::symlink_dir(&outside, &link) {
            if cause.kind() == std::io::ErrorKind::PermissionDenied
                && crate::testing::standing_down(
                    "Windows link resolution and containment",
                    "creating directory links requires Developer Mode or symbolic-link privilege",
                )
            {
                return;
            }

            panic!("create directory link: {cause}");
        }

        assert_eq!(
            physical(&link.join("future/file")).expect("link resolution"),
            physical(&outside).expect("outside resolution").join("future/file")
        );
        let failure = require_within(&link.join("future/file"), &tree, "a test destination").expect_err("the link target is outside");
        assert!(failure.to_string().contains("outside its permitted root"), "{failure}");
    }

    #[test]
    #[cfg(windows)]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn excessive_windows_link_indirection_is_refused() {
        let (_directory, root) = root();
        for index in 0..=40 {
            let next = (index + 1) % 41;
            if let Err(cause) = std::os::windows::fs::symlink_file(format!("link-{next}"), root.join(format!("link-{index}"))) {
                if cause.kind() == std::io::ErrorKind::PermissionDenied
                    && crate::testing::standing_down(
                        "Windows excessive-link detection",
                        "creating file links requires Developer Mode or symbolic-link privilege",
                    )
                {
                    return;
                }

                panic!("create file link: {cause}");
            }
        }

        let failure = physical(&root.join("link-0")).expect_err("the link chain is cyclic");
        assert!(failure.to_string().contains("too many symlinks"), "{failure}");
    }

    #[test]
    #[cfg(unix)]
    fn a_dangling_link_resolves_to_the_file_a_write_would_create() {
        let directory = crate::testing::workdir("physical-dangling-link-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 path");
        let link = root.join("link");
        let target = root.join("created").join("file");

        std::os::unix::fs::symlink("created/file", link.as_std_path()).expect("link");

        assert_eq!(physical(&link).expect("resolution"), physical(&target).expect("target resolution"));
    }

    #[test]
    #[cfg(unix)]
    fn an_external_link_is_not_inside_its_lexical_parent() {
        let directory = crate::testing::workdir("physical-containment-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 path");
        let outside = root.join("outside");
        let tree = root.join("tree");

        fs::create_dir_all(&outside).expect("outside");
        fs::create_dir_all(&tree).expect("tree");
        std::os::unix::fs::symlink(outside.join("file").as_std_path(), tree.join("link").as_std_path()).expect("link");

        let _error = require_within(&tree.join("link"), &tree, "a test destination")
            .expect_err("external link must not be within its lexical parent");
    }

    #[test]
    #[cfg(unix)]
    fn a_parent_after_a_link_is_resolved_from_the_link_target() {
        let directory = crate::testing::workdir("physical-link-parent-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 path");
        let tree = root.join("tree");
        let outside = root.join("outside");

        fs::create_dir_all(&tree).expect("tree");
        fs::create_dir_all(&outside).expect("outside");
        std::os::unix::fs::symlink(&outside, tree.join("link")).expect("external link");

        let destination = tree.join("link/../victim");

        assert_eq!(
            physical(&destination).expect("resolution"),
            physical(&root).expect("root resolution").join("victim")
        );
        let _error =
            require_within(&destination, &tree, "a test destination").expect_err("a parent symlink must not stay within its lexical root");
    }
}
