// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Immutable Git comparisons, worktree fingerprints, and guarded file copying.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::fs;
use std::io::Cursor;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::model::{Package, Workspace};
use crate::{Result, fail};

pub(crate) fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git").arg("--no-pager").args(args).current_dir(root).output()?;
    if !output.status.success() {
        return fail(format!("Git failed: {}", String::from_utf8_lossy(&output.stderr)));
    }
    Ok(output.stdout)
}

pub(crate) fn manifest_path(path: &Path) -> Result<PathBuf> {
    let path = anchored_path(path)?;
    let manifest = if path.is_dir() { path.join("Cargo.toml") } else { path };
    reject_links(&manifest)?;
    Ok(manifest
        .canonicalize()
        .map_err(|error| path_error("cannot resolve manifest", &manifest, &error))?)
}

pub(crate) fn git_text(root: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(git(root, args)?)?.trim().to_owned())
}

pub(crate) fn claim_output(path: &Path) -> Result<PathBuf> {
    let path = anchored_path(path)?;
    if path.exists() {
        reject_links(&path)?;
        if !path.is_dir()
            || fs::read_dir(&path)
                .map_err(|error| path_error("cannot inspect output directory", &path, &error))?
                .next()
                .is_some()
        {
            return fail("output directory is not empty; choose a fresh directory (nothing was deleted)");
        }
    } else {
        reject_links(existing_ancestor(&path)?)?;
        fs::create_dir_all(&path).map_err(|error| path_error("cannot create output directory", &path, &error))?;
    }
    reject_links(&path)?;
    let path = path
        .canonicalize()
        .map_err(|error| path_error("cannot resolve output directory", &path, &error))?;
    let marker = path.join(".release-guard-owned");
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .map_err(|error| path_error("cannot claim output directory marker", &marker, &error))?;
    Ok(path)
}

fn existing_ancestor(path: &Path) -> Result<&Path> {
    path.ancestors()
        .find(|parent| parent.exists())
        .ok_or_else(|| std::io::Error::other(format!("cannot inspect output directory root for '{}'", path.display())).into())
}

pub(crate) fn reject_links(path: &Path) -> Result<()> {
    let path = anchored_path(path)?;
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!(
                    "cannot inspect filesystem path '{}' while checking '{}' for shortcuts: {error}",
                    ancestor.display(),
                    path.display()
                ),
            )
        })?;
        if metadata.file_type().is_symlink() || is_reparse(&metadata) {
            return fail(format!("symlink or filesystem shortcut is not supported: {}", path.display()));
        }
    }
    Ok(())
}

fn anchored_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_owned());
    }
    let root = std::env::current_dir().map_err(|error| path_error("cannot determine the working directory for", path, &error))?;
    // Joining, rather than canonicalizing, keeps every component available for shortcut inspection.
    let anchored = root.join(path);
    if !anchored.is_absolute() {
        return fail(format!(
            "cannot resolve drive-relative path '{}'; use an absolute or working-directory-relative path",
            path.display()
        ));
    }
    Ok(anchored)
}

fn path_error(operation: &str, path: &Path, error: &std::io::Error) -> std::io::Error {
    std::io::Error::new(error.kind(), format!("{operation} '{}': {error}", path.display()))
}

#[cfg(windows)]
fn is_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse(_: &fs::Metadata) -> bool {
    false
}

pub(crate) fn read_workspace(manifest: &Path) -> Result<Workspace> {
    let manifest = manifest_path(manifest)?;
    let mut command = cargo_metadata::MetadataCommand::new();
    command
        .manifest_path(&manifest)
        .current_dir(
            manifest
                .parent()
                .expect("canonical path names a Cargo.toml file, not a filesystem root"),
        )
        .no_deps();
    let metadata = command
        .exec()
        .map_err(|error| std::io::Error::other(format!("cannot discover Cargo workspace from '{}': {error}", manifest.display())))?;
    workspace_from_metadata(&metadata)
}

fn workspace_from_metadata(metadata: &cargo_metadata::Metadata) -> Result<Workspace> {
    let root = metadata.workspace_root.as_std_path().canonicalize()?;
    let document = fs::read_to_string(root.join("Cargo.toml"))?.parse()?;
    let mut packages = BTreeMap::new();
    for package in metadata.workspace_packages() {
        let manifest = manifest_path(package.manifest_path.as_std_path())?;
        if !manifest.starts_with(&root) {
            return fail(format!("workspace member {} is outside the workspace root", package.name));
        }
        packages.insert(
            package.name.to_string(),
            Package {
                name: package.name.to_string(),
                version: package.version.clone(),
                document: fs::read_to_string(&manifest)?.parse()?,
                manifest,
                publish: package.publish.clone(),
            },
        );
    }
    Ok(Workspace { root, document, packages })
}

pub(crate) fn files(root: &Path, excluded: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let git_root = PathBuf::from(git_text(root, &["rev-parse", "--show-toplevel"])?).canonicalize()?;
    let output = git(&git_root, &["ls-files", "--cached", "--others", "--exclude-standard", "-z"])?;
    let mut paths = BTreeSet::new();
    for entry in output.split(|byte| *byte == 0).filter(|entry| !entry.is_empty()) {
        let relative = PathBuf::from(std::str::from_utf8(entry)?);
        let absolute = git_root.join(relative);
        if !absolute.starts_with(root) || excluded.iter().any(|directory| absolute.starts_with(directory)) {
            continue;
        }
        let relative = absolute.strip_prefix(root)?.to_owned();
        if relative.starts_with("target") {
            continue;
        }
        let metadata = fs::symlink_metadata(&absolute);
        if metadata.as_ref().is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound) {
            // A tracked deletion is represented by its absence from the hashed path list.
            continue;
        }
        let metadata = metadata?;
        if metadata.file_type().is_symlink()
            && !relative
                .file_name()
                .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("Cargo.toml"))
        {
            reject_links(absolute.parent().expect("Git source entries are joined below the repository root"))?;
        } else {
            reject_links(&absolute)?;
            if !metadata.is_file() {
                return fail(format!("unsupported source entry (possibly a submodule): {}", absolute.display()));
            }
        }
        paths.insert(relative);
    }
    Ok(paths.into_iter().collect())
}

pub(crate) fn digest(root: &Path, excluded: &[PathBuf]) -> Result<String> {
    let mut hash = Sha256::new();
    for relative in files(root, excluded)? {
        hash.update(relative.to_string_lossy().as_bytes());
        hash.update([0]);
        let absolute = root.join(&relative);
        let bytes = if fs::symlink_metadata(&absolute)?.file_type().is_symlink() {
            hash.update(b"symlink\0");
            fs::read_link(&absolute)?.as_os_str().as_encoded_bytes().to_vec()
        } else {
            hash.update(b"file\0");
            fs::read(&absolute)?
        };
        hash.update(bytes.len().to_le_bytes());
        hash.update(bytes);
    }
    let mut result = String::with_capacity(64);
    for byte in hash.finalize() {
        write!(result, "{byte:02x}")?;
    }
    Ok(result)
}

pub(crate) fn historical_workspace(workspace: &Workspace, base: &str, output: &Path, diagnostics: &mut Vec<String>) -> Result<Workspace> {
    let git_root = PathBuf::from(git_text(&workspace.root, &["rev-parse", "--show-toplevel"])?).canonicalize()?;
    let archive = git(&git_root, &["archive", "--format=tar", base])?;
    let destination = output.join("base");
    fs::create_dir(&destination)?;
    extract_archive(&archive, &destination)?;
    // Publication export attributes must not hide or substitute historical package identities.
    let tree = git(&git_root, &["ls-tree", "-r", "-z", base])?;
    let links = restore_historical_metadata(&git_root, base, &destination, &tree, diagnostics)?;
    let root = destination.join(workspace.root.strip_prefix(&git_root)?);
    if let Some(link) = links
        .iter()
        .find(|link| root.join("Cargo.toml").starts_with(destination.join(link)))
    {
        return fail(format!(
            "historical workspace root or manifest requires symbolic link '{}'; use regular metadata paths",
            link.display()
        ));
    }
    if !root.join("Cargo.toml").is_file() {
        return Ok(Workspace {
            root,
            document: toml_edit::DocumentMut::new(),
            packages: BTreeMap::new(),
        });
    }
    if links.is_empty() {
        return read_workspace(&root);
    }
    read_workspace(&root).map_err(|error| {
        std::io::Error::other(format!(
            "cannot inspect historical workspace metadata with symbolic link entries unavailable ({}): {error}",
            links.iter().map(|link| link.display().to_string()).collect::<Vec<_>>().join(", "),
        ))
        .into()
    })
}

fn restore_historical_metadata(
    git_root: &Path,
    base: &str,
    destination: &Path,
    tree: &[u8],
    diagnostics: &mut Vec<String>,
) -> Result<Vec<PathBuf>> {
    let mut links = Vec::new();
    for entry in tree.split(|byte| *byte == 0).filter(|entry| !entry.is_empty()) {
        let (identity, path) = std::str::from_utf8(entry)?
            .split_once('\t')
            .expect("git ls-tree -z separates each entry identity from its path with a tab");
        let relative = Path::new(path);
        validate_archive_path(relative)?;
        if identity.starts_with("120000 ") {
            links.push(relative.to_owned());
            diagnostics.push(format!(
                "historical symbolic link '{}': target not followed; entry unavailable to workspace metadata",
                relative.display()
            ));
            if !relative.components().any(|part| part.as_os_str() == ".cargo") {
                let target = destination.join(relative);
                fs::create_dir_all(&target)?;
                // An invalid manifest preserves directory-glob matches without following
                // links or allowing a linked workspace member to disappear silently.
                fs::write(
                    target.join("Cargo.toml"),
                    "historical symbolic link unavailable to workspace metadata\n",
                )?;
            }
            continue;
        }
        if relative.file_name().is_some_and(|name| name == "Cargo.toml") && !relative.components().any(|part| part.as_os_str() == ".cargo")
        {
            let target = destination.join(relative);
            fs::create_dir_all(
                target
                    .parent()
                    .expect("archive manifest paths include a Cargo.toml filename below the destination"),
            )?;
            fs::write(target, git(git_root, &["show", &format!("{base}:{path}")])?)?;
        }
    }
    Ok(links)
}

fn validate_archive_path(path: &Path) -> Result<()> {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
    {
        return fail(format!("historical archive contains an escaping path: '{}'", path.display()));
    }
    Ok(())
}

fn extract_archive(archive: &[u8], destination: &Path) -> Result<()> {
    for entry in tar::Archive::new(Cursor::new(archive)).entries()? {
        let mut entry = entry?;
        // Git archives include a global PAX header carrying the commit ID, not a source file.
        if entry.header().entry_type().is_pax_global_extensions() {
            continue;
        }
        let path = entry.path()?.into_owned();
        validate_archive_path(&path)?;
        let kind = entry.header().entry_type();
        if kind.is_symlink() {
            // The authoritative Git tree supplies inert placeholders, including
            // links omitted from this archive by export attributes.
            continue;
        }
        if !kind.is_file() && !kind.is_dir() {
            return fail(format!("historical archive contains an unsupported entry: '{}'", path.display()));
        }
        if path.components().any(|part| part.as_os_str() == ".cargo") {
            continue;
        }
        let target = destination.join(path);
        // Only relative file/directory entries reach extraction; links are never
        // created and parent/root components were rejected above.
        fs::create_dir_all(
            target
                .parent()
                .expect("a relative archive entry is joined below the extraction directory"),
        )?;
        entry.unpack(target)?;
    }
    Ok(())
}

pub(crate) fn copy_file(source: &Path, destination: &Path) -> Result<()> {
    reject_links(source)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
        reject_links(parent)?;
    }
    if destination.exists() {
        return fail(format!("refusing to overwrite existing artifact: {}", destination.display()));
    }
    fs::copy(source, destination)?;
    Ok(())
}

pub(crate) fn write_json(path: &Path, data: &impl serde::Serialize) -> Result<()> {
    fs::write(path, serde_json::to_vec_pretty(data)?).map_err(|error| path_error("cannot write JSON artifact", path, &error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    #[cfg(windows)]
    use super::{anchored_path, claim_output, reject_links};
    use super::{copy_file, existing_ancestor, extract_archive, files, restore_historical_metadata, workspace_from_metadata};
    use crate::test_support;

    #[test]
    fn archive_paths_and_link_entries_are_rejected_before_extraction() {
        let directory = test_support::directory();
        for (path, kind, diagnostic) in [
            ("../escape", tar::EntryType::Regular, "escaping path"),
            ("shortcut", tar::EntryType::Link, "unsupported entry"),
        ] {
            let mut header = tar::Header::new_gnu();
            header.as_mut_bytes()[..path.len()].copy_from_slice(path.as_bytes());
            header.set_entry_type(kind);
            header.set_mode(0o644);
            header.set_size(0);
            header.set_cksum();
            let mut archive = tar::Builder::new(Vec::new());
            archive.append(&header, &[][..]).unwrap();
            let archive = archive.into_inner().unwrap();
            let error = extract_archive(&archive, directory.path()).unwrap_err().to_string();
            assert!(error.contains(diagnostic));
            assert!(error.contains(path));
        }
        assert!(!directory.path().parent().unwrap().join("escape").exists());
    }

    #[test]
    fn historical_metadata_write_failures_preserve_conflicting_entries() {
        let directory = test_support::directory();
        let destination = directory.path().join("base");
        fs::create_dir_all(destination.join("alias").join("Cargo.toml")).unwrap();
        let note = destination.join("alias").join("Cargo.toml").join("preserved");
        fs::write(&note, "user data").unwrap();
        let mut diagnostics = Vec::new();
        let link = b"120000 blob 0000000000000000000000000000000000000000\talias\0";
        restore_historical_metadata(directory.path(), "HEAD", &destination, link, &mut diagnostics).unwrap_err();
        assert_eq!(fs::read_to_string(&note).unwrap(), "user data");
        assert!(diagnostics[0].contains("alias"));

        fs::write(destination.join("blocked"), "preserved file").unwrap();
        let manifest = b"100644 blob 0000000000000000000000000000000000000000\tblocked/Cargo.toml\0";
        restore_historical_metadata(directory.path(), "HEAD", &destination, manifest, &mut diagnostics).unwrap_err();
        assert_eq!(fs::read_to_string(destination.join("blocked")).unwrap(), "preserved file");
    }

    #[test]
    fn archive_extraction_does_not_replace_a_file_with_a_parent_directory() {
        let directory = test_support::directory();
        fs::write(directory.path().join("blocked"), "preserved file").unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_path("blocked/file.rs").unwrap();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_size(0);
        header.set_cksum();
        let mut archive = tar::Builder::new(Vec::new());
        archive.append(&header, &[][..]).unwrap();
        assert!(extract_archive(&archive.into_inner().unwrap(), directory.path()).is_err());
        assert_eq!(fs::read_to_string(directory.path().join("blocked")).unwrap(), "preserved file");
    }

    #[test]
    fn copying_never_overwrites_files_or_a_filesystem_root() {
        let directory = test_support::directory();
        assert!(
            existing_ancestor(Path::new(""))
                .unwrap_err()
                .to_string()
                .contains("cannot inspect output directory root")
        );
        assert_eq!(existing_ancestor(directory.path()).unwrap(), directory.path());
        let source = directory.path().join("source");
        let destination = directory.path().join("destination");
        fs::write(&source, "original").unwrap();
        fs::write(&destination, "preserved").unwrap();
        assert!(
            copy_file(&source, &destination)
                .unwrap_err()
                .to_string()
                .contains("refusing to overwrite")
        );
        assert_eq!(fs::read_to_string(&destination).unwrap(), "preserved");
        let root = directory.path().ancestors().last().unwrap();
        assert!(copy_file(&source, root).unwrap_err().to_string().contains("refusing to overwrite"));
    }

    #[test]
    fn metadata_member_paths_must_belong_to_the_workspace() {
        let directory = test_support::directory();
        let workspace = test_support::workspace(directory.path(), &[("consumer", "")]);
        let metadata = cargo_metadata::MetadataCommand::new()
            .manifest_path(workspace.root.join("Cargo.toml"))
            .exec()
            .unwrap();
        let mut metadata = serde_json::to_value(metadata).unwrap();
        let nested = workspace.root.join("narrow");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("Cargo.toml"), "[workspace]\n").unwrap();
        metadata["workspace_root"] = serde_json::json!(nested);
        assert!(
            workspace_from_metadata(&serde_json::from_value(metadata).unwrap())
                .unwrap_err()
                .to_string()
                .contains("outside the workspace root")
        );
    }

    #[cfg(windows)]
    #[test]
    fn drive_relative_paths_and_junctions_are_not_shortcut_escapes() {
        let directory = test_support::directory();
        assert!(
            anchored_path(Path::new("Z:relative"))
                .unwrap_err()
                .to_string()
                .contains("drive-relative")
        );
        let link = directory.path().join("shortcut");
        let target = directory.path().join("target");
        fs::create_dir(&target).unwrap();
        let output = Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "New-Item -ItemType Junction -Path $env:RG_LINK -Target $env:RG_TARGET | Out-Null",
            ])
            .env("RG_LINK", &link)
            .env("RG_TARGET", &target)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(reject_links(&link).unwrap_err().to_string().contains("filesystem shortcut"));
        assert!(
            claim_output(&link.join("child"))
                .unwrap_err()
                .to_string()
                .contains("filesystem shortcut")
        );
        fs::remove_dir(&link).unwrap();
    }

    #[test]
    fn git_submodule_entries_are_not_treated_as_regular_source_files() {
        let directory = test_support::directory();
        for args in [
            vec!["init", "--quiet"],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "fixture",
            ],
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(directory.path())
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
        let head = super::git_text(directory.path(), &["rev-parse", "HEAD"]).unwrap();
        fs::create_dir(directory.path().join("submodule")).unwrap();
        assert!(
            Command::new("git")
                .args(["update-index", "--add", "--cacheinfo", &format!("160000,{head},submodule")])
                .current_dir(directory.path())
                .output()
                .unwrap()
                .status
                .success()
        );
        assert!(
            files(&directory.path().canonicalize().unwrap(), &[])
                .unwrap_err()
                .to_string()
                .contains("unsupported source entry")
        );
    }
}
