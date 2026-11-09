// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::fs as native;
use std::io::{self, Cursor, ErrorKind, Read};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub(in crate::exec) struct FileData {
    pub(in crate::exec) bytes: Vec<u8>,
    pub(in crate::exec) length: Result<u64, ErrorKind>,
    pub(in crate::exec) read_error: Option<ErrorKind>,
}

impl FileData {
    pub(in crate::exec) fn bytes(bytes: impl AsRef<[u8]>) -> Self {
        let bytes = bytes.as_ref().to_vec();
        Self {
            length: Ok(bytes.len() as u64),
            bytes,
            read_error: None,
        }
    }
}

#[derive(Clone, Copy)]
pub(in crate::exec) enum Kind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Default)]
pub(in crate::exec) struct Script {
    pub(in crate::exec) files: BTreeMap<PathBuf, Result<FileData, ErrorKind>>,
    pub(in crate::exec) directories: BTreeMap<PathBuf, Result<Vec<DirectoryItem>, ErrorKind>>,
    pub(in crate::exec) directory_reads: BTreeMap<PathBuf, VecDeque<Result<(), ErrorKind>>>,
    pub(in crate::exec) symlinks: BTreeMap<PathBuf, Result<bool, ErrorKind>>,
    pub(in crate::exec) environment: BTreeMap<OsString, OsString>,
    pub(in crate::exec) directory_errors: BTreeMap<PathBuf, ErrorKind>,
    pub(in crate::exec) cwd: Option<PathBuf>,
}

type DirectoryItem = Result<(PathBuf, Result<Kind, ErrorKind>), ErrorKind>;

pub(in crate::exec) fn active() -> bool {
    SCRIPT.with_borrow(Option::is_some)
}

pub(in crate::exec) fn inventory(root: &camino::Utf8Path, excluded: &camino::Utf8Path) -> Option<[u8; 32]> {
    super::super::inventory(&super::super::NativeFiles, root, excluded)
}

pub(in crate::exec) mod environment {
    use std::env as native;
    use std::ffi::{OsStr, OsString};
    use std::path::PathBuf;

    use super::SCRIPT;

    pub(in crate::exec) fn vars_os() -> std::vec::IntoIter<(OsString, OsString)> {
        SCRIPT
            .with_borrow(|script| {
                script.as_ref().map(|script| {
                    script
                        .environment
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect::<Vec<_>>()
                })
            })
            .unwrap_or_else(|| native::vars_os().collect())
            .into_iter()
    }

    pub(in crate::exec) fn var_os(name: impl AsRef<OsStr>) -> Option<OsString> {
        match SCRIPT.with_borrow(|script| script.as_ref().map(|script| script.environment.get(name.as_ref()).cloned())) {
            Some(value) => value,
            None => native::var_os(name),
        }
    }

    pub(in crate::exec) fn var(name: impl AsRef<OsStr>) -> Result<String, native::VarError> {
        var_os(name)
            .ok_or(native::VarError::NotPresent)?
            .into_string()
            .map_err(native::VarError::NotUnicode)
    }

    pub(in crate::exec) fn current_dir() -> std::io::Result<PathBuf> {
        match SCRIPT.with_borrow(|script| script.as_ref().map(|script| script.cwd.clone())) {
            Some(Some(path)) => Ok(path),
            Some(None) => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            None => native::current_dir(),
        }
    }
}

pub(in crate::exec) fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let _read = File::open(path)?.read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub(in crate::exec) fn write(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> io::Result<()> {
    let path = path.as_ref();
    if SCRIPT.with_borrow_mut(|script| {
        script.as_mut().is_some_and(|script| {
            let _previous = script.files.insert(path.to_owned(), Ok(FileData::bytes(bytes.as_ref())));
            if let Some(parent) = path.parent()
                && let Ok(entries) = script.directories.entry(parent.to_owned()).or_insert_with(|| Ok(Vec::new()))
                && !entries
                    .iter()
                    .any(|entry| entry.as_ref().is_ok_and(|(existing, _)| existing == path))
            {
                entries.push(Ok((path.to_owned(), Ok(Kind::File))));
            }
            true
        })
    }) {
        Ok(())
    } else {
        native::write(path, bytes)
    }
}

pub(in crate::exec) fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    match SCRIPT.with_borrow_mut(|script| script.as_mut().map(|script| script.files.remove(path))) {
        Some(Some(_)) => Ok(()),
        Some(None) => Err(io::Error::from(ErrorKind::NotFound)),
        None => native::remove_file(path),
    }
}

pub(in crate::exec) fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    match SCRIPT.with_borrow_mut(|script| {
        script.as_mut().map(|script| {
            if let Some(error) = script.directory_errors.get(path) {
                return Err(io::Error::from(*error));
            }
            let _previous = script.directories.entry(path.to_owned()).or_insert_with(|| Ok(Vec::new()));
            Ok(())
        })
    }) {
        Some(result) => result,
        None => native::create_dir_all(path),
    }
}

impl Script {
    pub(in crate::exec) fn from_files(files: impl IntoIterator<Item = (PathBuf, Vec<u8>)>) -> Self {
        let mut script = Self::default();
        let mut directories = BTreeMap::<PathBuf, BTreeMap<PathBuf, Kind>>::new();
        for (path, bytes) in files {
            let _previous = script.files.insert(path.clone(), Ok(FileData::bytes(bytes)));
            let mut child = path;
            let mut kind = Kind::File;
            while let Some(parent) = child.parent() {
                let _previous = directories.entry(parent.to_owned()).or_default().insert(child.clone(), kind);
                child = parent.to_owned();
                kind = Kind::Directory;
            }
        }
        script.directories = directories
            .into_iter()
            .map(|(path, children)| (path, Ok(children.into_iter().map(|(path, kind)| Ok((path, Ok(kind)))).collect())))
            .collect();
        script
    }
}

thread_local! {
    static SCRIPT: RefCell<Option<Script>> = const { RefCell::new(None) };
}

pub(in crate::exec) fn with<T>(script: Script, operation: impl FnOnce() -> T) -> T {
    struct Restore(Option<Script>);
    impl Drop for Restore {
        fn drop(&mut self) {
            SCRIPT.with_borrow_mut(|slot| *slot = self.0.take());
        }
    }
    let _restore = Restore(SCRIPT.with_borrow_mut(|slot| slot.replace(script)));
    operation()
}

enum FileSource {
    Native(native::File),
    Scripted(Cursor<Vec<u8>>, Result<u64, ErrorKind>, Option<ErrorKind>),
}

pub(in crate::exec) struct File(FileSource);

impl File {
    pub(in crate::exec) fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        match SCRIPT.with_borrow(|script| {
            script
                .as_ref()
                .map(|script| script.files.get(path).cloned().unwrap_or(Err(ErrorKind::NotFound)))
        }) {
            Some(result) => result
                .map(|data| Self(FileSource::Scripted(Cursor::new(data.bytes), data.length, data.read_error)))
                .map_err(io::Error::from),
            None => native::File::open(path).map(|file| Self(FileSource::Native(file))),
        }
    }

    pub(in crate::exec) fn metadata(&self) -> io::Result<Metadata> {
        match &self.0 {
            FileSource::Native(file) => file
                .metadata()
                .map(|metadata| Metadata(metadata.len(), FileType::Native(metadata.file_type()))),
            FileSource::Scripted(_, length, _) => length
                .map(|length| Metadata(length, FileType::Scripted(Kind::File)))
                .map_err(io::Error::from),
        }
    }
}

impl Read for File {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.0 {
            FileSource::Native(file) => file.read(buf),
            FileSource::Scripted(_, _, Some(error)) => Err(io::Error::from(*error)),
            FileSource::Scripted(reader, _, None) => reader.read(buf),
        }
    }
}

pub(in crate::exec) struct Metadata(u64, FileType);

impl Metadata {
    pub(in crate::exec) const fn len(&self) -> u64 {
        self.0
    }

    pub(in crate::exec) const fn file_type(self) -> FileType {
        self.1
    }
}

pub(in crate::exec) enum FileType {
    Native(native::FileType),
    Scripted(Kind),
}

impl FileType {
    pub(in crate::exec) fn is_symlink(&self) -> bool {
        match self {
            Self::Native(kind) => kind.is_symlink(),
            Self::Scripted(kind) => matches!(kind, Kind::Symlink),
        }
    }

    pub(in crate::exec) fn is_dir(&self) -> bool {
        match self {
            Self::Native(kind) => kind.is_dir(),
            Self::Scripted(kind) => matches!(kind, Kind::Directory),
        }
    }

    pub(in crate::exec) fn is_file(&self) -> bool {
        match self {
            Self::Native(kind) => kind.is_file(),
            Self::Scripted(kind) => matches!(kind, Kind::File),
        }
    }
}

pub(in crate::exec) enum DirEntry {
    Native(Box<native::DirEntry>),
    Scripted(PathBuf, Result<Kind, ErrorKind>),
}

impl DirEntry {
    pub(in crate::exec) fn path(&self) -> PathBuf {
        match self {
            Self::Native(entry) => entry.path(),
            Self::Scripted(path, _) => path.clone(),
        }
    }

    pub(in crate::exec) fn file_type(&self) -> io::Result<FileType> {
        match self {
            Self::Native(entry) => entry.file_type().map(FileType::Native),
            Self::Scripted(_, kind) => kind.map(FileType::Scripted).map_err(io::Error::from),
        }
    }
}

pub(in crate::exec) fn read_dir(path: impl AsRef<Path>) -> io::Result<Box<dyn Iterator<Item = io::Result<DirEntry>>>> {
    let path = path.as_ref();
    match SCRIPT.with_borrow_mut(|script| {
        script.as_mut().map(|script| {
            if let Some(result) = script.directory_reads.get_mut(path).and_then(VecDeque::pop_front) {
                result?;
            }
            script.directories.get(path).cloned().unwrap_or(Err(ErrorKind::NotFound))
        })
    }) {
        Some(result) => result
            .map(|entries| {
                Box::new(
                    entries
                        .into_iter()
                        .map(|entry| entry.map(|(path, kind)| DirEntry::Scripted(path, kind)).map_err(io::Error::from)),
                ) as Box<dyn Iterator<Item = io::Result<DirEntry>>>
            })
            .map_err(io::Error::from),
        None => native::read_dir(path).map(|entries| {
            Box::new(entries.map(|entry| entry.map(|entry| DirEntry::Native(Box::new(entry)))))
                as Box<dyn Iterator<Item = io::Result<DirEntry>>>
        }),
    }
}

pub(in crate::exec) fn symlink_metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    let path = path.as_ref();
    match SCRIPT.with_borrow(|script| {
        script
            .as_ref()
            .map(|script| script.symlinks.get(path).copied().unwrap_or(Err(ErrorKind::NotFound)))
    }) {
        Some(result) => result
            .map(|symlink| Metadata(0, FileType::Scripted(if symlink { Kind::Symlink } else { Kind::File })))
            .map_err(io::Error::from),
        None => native::symlink_metadata(path).map(|metadata| Metadata(metadata.len(), FileType::Native(metadata.file_type()))),
    }
}
