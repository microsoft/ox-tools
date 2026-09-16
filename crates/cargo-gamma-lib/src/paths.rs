// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolving paths before operations whose destination has to be known.

use std::fs;

use camino::{Utf8Component, Utf8Path, Utf8PathBuf};

use crate::Result;
use crate::error::error;

const MAX_SYMLINKS: usize = 40;

/// Resolves every extant symlink in `path`, including a dangling final link's target spelling.
///
/// `canonicalize` cannot answer this for a dangling link: it fails at the link rather than
/// reporting where a later create or rename would land. Following components one at a time keeps
/// that destination visible while still leaving a genuinely absent tail in place.
pub(crate) fn physical(path: &Utf8Path) -> Result<Utf8PathBuf> {
    let mut pending = absolute(path)?;
    let mut followed_links = 0;

    loop {
        let components: Vec<Utf8Component<'_>> = pending.components().collect();
        let mut resolved = Utf8PathBuf::new();
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
                    let metadata = match fs::symlink_metadata(candidate.as_std_path()) {
                        Ok(metadata) => metadata,

                        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => {
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

                    if !metadata.file_type().is_symlink() {
                        resolved = candidate;

                        continue;
                    }

                    followed_links += 1;

                    if followed_links > MAX_SYMLINKS {
                        return Err(error!("could not resolve `{path}`: too many symlinks"));
                    }

                    let target = fs::read_link(candidate.as_std_path())
                        .map_err(|cause| error!("could not read the link `{candidate}` while resolving `{path}`").caused_by(cause))?;
                    let target = Utf8PathBuf::from_path_buf(target)
                        .map_err(|target| error!("the link `{candidate}` has a non-UTF-8 target `{}`", target.display()))?;
                    let mut next = if target.is_absolute() { target } else { resolved.join(target) };

                    append(&mut next, components[at + 1..].iter().copied());
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
    // #[gamma::skip(cond.always_false, reason = "joining an absolute path to the current directory returns that absolute path unchanged, so the fallback branch is observationally identical")]
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        let current = std::env::current_dir()
            .map_err(|cause| error!("could not resolve the current directory while resolving `{path}`").caused_by(cause))?;
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

    append(&mut normalized, absolute.components());

    Ok(normalized)
}

/// Appends components while preserving their filesystem meaning below an absolute root.
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

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;

    fn root() -> (tempfile::TempDir, Utf8PathBuf) {
        let directory = crate::testing::workdir("paths-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 path");
        (directory, root)
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
    fn windows_links_are_resolved_and_containment_uses_their_targets() {
        let (_directory, root) = root();
        let outside = root.join("outside");
        let tree = root.join("tree");
        let link = tree.join("link");
        fs::create_dir_all(&outside).expect("outside");
        fs::create_dir_all(&tree).expect("tree");
        std::os::windows::fs::symlink_dir(&outside, &link).expect("directory link");

        assert_eq!(
            physical(&link.join("future/file")).expect("link resolution"),
            physical(&outside).expect("outside resolution").join("future/file")
        );
        let failure = require_within(&link.join("future/file"), &tree, "a test destination").expect_err("the link target is outside");
        assert!(failure.to_string().contains("outside its permitted root"), "{failure}");
    }

    #[test]
    #[cfg(windows)]
    fn excessive_windows_link_indirection_is_refused() {
        let (_directory, root) = root();
        for index in 0..=40 {
            let next = (index + 1) % 41;
            std::os::windows::fs::symlink_file(format!("link-{next}"), root.join(format!("link-{index}"))).expect("file link");
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
