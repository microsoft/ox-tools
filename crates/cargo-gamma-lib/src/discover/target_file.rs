// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A source file that will be analyzed.

use camino::Utf8PathBuf;

/// A source file that will be analyzed.
#[derive(Debug, Clone)]
pub struct TargetFile {
    /// Path relative to the workspace root, with forward slashes.
    pub path: Utf8PathBuf,

    /// Absolute path on disk.
    pub absolute: Utf8PathBuf,

    /// The package the file belongs to.
    pub package: String,

    /// The source generation discovery read, including a leading UTF-8 byte-order mark when present.
    ///
    /// Parsing and generation digests use normalized text with that mark removed. Reports use the
    /// synchronized campaign tree when one was built; this retained raw copy covers campaigns that
    /// had no buildable workspace and keeps report publication independent of later checkout edits.
    pub source: Option<String>,
}
