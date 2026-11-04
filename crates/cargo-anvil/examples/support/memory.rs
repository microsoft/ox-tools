// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A caller-owned in-memory repository. No host operations are performed.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use cargo_anvil::{RepositoryEntry, UpdateIo};
use ohno::{AppError, bail};

#[derive(Debug, Default)]
pub struct MemoryRepository {
    pub files: BTreeMap<String, String>,
    pub mutations: Vec<String>,
}

impl MemoryRepository {
    pub fn insert(&mut self, path: &str, content: &str) {
        self.files.insert(Self::key(Path::new(path)), content.to_owned());
    }

    fn key(path: &Path) -> String {
        path.components()
            .filter(|component| *component != Component::CurDir)
            .collect::<PathBuf>()
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn file_key(&self, path: &Path) -> Result<String, AppError> {
        let key = Self::key(path);
        let prefix = format!("{key}/");
        if key.is_empty() || self.files.keys().any(|file| file.starts_with(&prefix)) {
            bail!("cannot access '{key}' as a file: it is a directory");
        }
        for parent in Path::new(&key).ancestors().skip(1) {
            let parent = Self::key(parent);
            if self.files.contains_key(&parent) {
                bail!("cannot access '{key}': parent '{parent}' is a file");
            }
        }
        Ok(key)
    }
}

impl UpdateIo for MemoryRepository {
    fn read_file(&self, path: &Path) -> Result<Option<String>, AppError> {
        Ok(self.files.get(&self.file_key(path)?).cloned())
    }

    fn read_directory(&self, path: &Path) -> Result<Option<Vec<RepositoryEntry>>, AppError> {
        let directory = Self::key(path);
        if self.files.contains_key(&directory) {
            return Ok(None);
        }
        let prefix = if directory.is_empty() {
            String::new()
        } else {
            format!("{directory}/")
        };
        let mut entries = BTreeMap::new();
        for path in self.files.keys().filter_map(|path| path.strip_prefix(&prefix)) {
            if let Some((name, _)) = path.split_once('/') {
                entries.insert(name.to_owned(), true);
            } else {
                entries.entry(path.to_owned()).or_insert(false);
            }
        }
        if entries.is_empty() && !directory.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            entries
                .into_iter()
                .map(|(name, directory)| RepositoryEntry::new(name, directory))
                .collect(),
        ))
    }

    fn write_file(&mut self, path: &Path, content: &str) -> Result<(), AppError> {
        let key = self.file_key(path)?;
        self.mutations.push(format!("write {key}"));
        self.files.insert(key, content.to_owned());
        Ok(())
    }

    fn remove_file(&mut self, path: &Path) -> std::io::Result<()> {
        let key = Self::key(path);
        self.mutations.push(format!("remove {key}"));
        self.files
            .remove(&key)
            .map(|_| ())
            .ok_or_else(|| std::io::ErrorKind::NotFound.into())
    }
}
