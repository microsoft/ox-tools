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
}
