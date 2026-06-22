// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `.anvil.lock` — the sidecar manifest.
//!
//! Tracks, for every owned file and every managed region, the checksum of
//! what `cargo-anvil` most recently rendered there. This is the single
//! source of truth for "what did the tool last write" — drift detection
//! compares this against the current on-disk content and the current
//! template content.
//!
//! Schema is documented in [`updates.md §1`](../../docs/design/updates.md).
//! The schema version is `1`. Newer schemas cause the tool to refuse
//! running; older schemas are migrated automatically (no older schemas
//! exist today).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ohno::{AppError, IntoAppError as _, app_err, bail};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

/// File name of the manifest at the repo root.
pub const MANIFEST_FILE_NAME: &str = ".anvil.lock";

/// Current schema version we read/write.
pub const SCHEMA_VERSION: i64 = 1;

/// The full parsed manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    /// The tool that last wrote this lock: its cargo-subcommand token
    /// (`anvil`, or a downstream tool's subcommand). This is the identity
    /// the single-tool guard keys on. `None` for empty/never-written
    /// manifests and for legacy locks written before the field split (which
    /// is treated as "no recorded tool" so the guard never fires).
    pub tool: Option<String>,

    /// The version of the binary that last wrote the lock. Informational.
    pub tool_version: Option<String>,

    /// A `sha256` over the whole catalog the writing build carried.
    /// Provenance and diagnostics only — never a gate.
    pub catalog_checksum: Option<String>,

    /// Last-rendered checksum per owned file, keyed by repo-root-relative
    /// forward-slash path.
    pub files: BTreeMap<String, String>,

    /// Last-rendered checksum per managed region, keyed by `(host_path,
    /// region_id)`.
    pub regions: BTreeMap<RegionKey, String>,
}

/// Composite key identifying one managed region.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RegionKey {
    /// The host file's repo-root-relative forward-slash path.
    pub host: String,
    /// The region's stable `id` from the sentinel comments.
    pub id: String,
}

impl Manifest {
    /// Path the manifest should be saved at, given a workspace root.
    #[must_use]
    pub fn path_for(repo_root: &Path) -> PathBuf {
        repo_root.join(MANIFEST_FILE_NAME)
    }

    /// Load the manifest from `repo_root`, returning an empty manifest if no
    /// file exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but can't be read, can't be
    /// parsed, or declares an unsupported schema version.
    pub fn load(repo_root: &Path) -> Result<Self, AppError> {
        let path = Self::path_for(repo_root);
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path).into_app_err_with(|| format!("failed to read {}", path.display()))?;
        Self::parse(&text).into_app_err_with(|| format!("failed to parse manifest at {}", path.display()))
    }

    /// Parse a manifest from a TOML string.
    ///
    /// # Errors
    ///
    /// Returns an error on malformed TOML or unsupported schema version.
    pub fn parse(text: &str) -> Result<Self, AppError> {
        let doc: DocumentMut = text.parse::<DocumentMut>().into_app_err("manifest is not valid TOML")?;

        let version = doc
            .get("version")
            .and_then(Item::as_integer)
            .ok_or_else(|| app_err!("manifest is missing a top-level `version` integer"))?;
        if version > SCHEMA_VERSION {
            bail!("manifest schema version {version} is newer than supported ({SCHEMA_VERSION}); upgrade cargo-anvil");
        }

        let tool = doc.get("tool").and_then(Item::as_str).map(str::to_owned);
        // `tool_version` falls back to the version token of a legacy
        // `rendered_by` string ("cargo-anvil 0.1.0"). The `tool` guard field
        // deliberately does NOT fall back to `rendered_by`: a pre-split lock
        // has no recorded tool, so the guard must not fire on it.
        let tool_version = doc.get("tool_version").and_then(Item::as_str).map(str::to_owned).or_else(|| {
            doc.get("rendered_by")
                .and_then(Item::as_str)
                .and_then(|rb| rb.split_whitespace().last())
                .map(str::to_owned)
        });
        let catalog_checksum = doc.get("catalog_checksum").and_then(Item::as_str).map(str::to_owned);

        let mut files = BTreeMap::new();
        if let Some(arr) = doc.get("file").and_then(Item::as_array_of_tables) {
            for table in arr {
                let path = table
                    .get("path")
                    .and_then(Item::as_str)
                    .ok_or_else(|| app_err!("[[file]] entry is missing `path`"))?
                    .to_owned();
                let checksum = table
                    .get("checksum")
                    .and_then(Item::as_str)
                    .ok_or_else(|| app_err!("[[file]] entry '{path}' is missing `checksum`"))?
                    .to_owned();
                if files.insert(path.clone(), checksum).is_some() {
                    bail!("duplicate [[file]] entry for '{path}'");
                }
            }
        }

        let mut regions = BTreeMap::new();
        if let Some(arr) = doc.get("region").and_then(Item::as_array_of_tables) {
            for table in arr {
                let host = table
                    .get("host")
                    .and_then(Item::as_str)
                    .ok_or_else(|| app_err!("[[region]] entry is missing `host`"))?
                    .to_owned();
                let id = table
                    .get("id")
                    .and_then(Item::as_str)
                    .ok_or_else(|| app_err!("[[region]] entry '{host}' is missing `id`"))?
                    .to_owned();
                let checksum = table
                    .get("checksum")
                    .and_then(Item::as_str)
                    .ok_or_else(|| app_err!("[[region]] entry '{host}'/'{id}' is missing `checksum`"))?
                    .to_owned();
                let key = RegionKey { host, id };
                if regions.insert(key.clone(), checksum).is_some() {
                    bail!("duplicate [[region]] entry for host '{}' id '{}'", key.host, key.id);
                }
            }
        }

        Ok(Self {
            tool,
            tool_version,
            catalog_checksum,
            files,
            regions,
        })
    }

    /// Render the manifest as a deterministic TOML string.
    ///
    /// Entries are sorted (files alphabetically by path; regions by
    /// `(host, id)`). The output ends with a trailing newline.
    #[must_use]
    pub fn to_toml(&self) -> String {
        let mut doc = DocumentMut::new();

        doc.insert("version", value(SCHEMA_VERSION));
        if let Some(tool) = &self.tool {
            doc.insert("tool", value(tool.as_str()));
        }
        if let Some(tool_version) = &self.tool_version {
            doc.insert("tool_version", value(tool_version.as_str()));
        }
        if let Some(catalog_checksum) = &self.catalog_checksum {
            doc.insert("catalog_checksum", value(catalog_checksum.as_str()));
        }

        if !self.files.is_empty() {
            let mut tables = ArrayOfTables::new();
            for (path, checksum) in &self.files {
                let mut t = Table::new();
                t.insert("path", value(path.as_str()));
                t.insert("checksum", value(checksum.as_str()));
                tables.push(t);
            }
            doc.insert("file", Item::ArrayOfTables(tables));
        }

        if !self.regions.is_empty() {
            let mut tables = ArrayOfTables::new();
            for (key, checksum) in &self.regions {
                let mut t = Table::new();
                t.insert("host", value(key.host.as_str()));
                t.insert("id", value(key.id.as_str()));
                t.insert("checksum", value(checksum.as_str()));
                tables.push(t);
            }
            doc.insert("region", Item::ArrayOfTables(tables));
        }

        let mut out = doc.to_string();
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out
    }

    /// Save the manifest to `<repo_root>/.anvil.lock` atomically (write
    /// to a temp file, then rename).
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    pub fn save(&self, repo_root: &Path) -> Result<(), AppError> {
        let path = Self::path_for(repo_root);
        let text = self.to_toml();
        let tmp = path.with_extension("lock.tmp");
        std::fs::write(&tmp, text.as_bytes()).into_app_err_with(|| format!("failed to write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).into_app_err_with(|| format!("failed to rename {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Insert or update one file entry.
    pub fn set_file(&mut self, path: impl Into<String>, checksum: impl Into<String>) {
        self.files.insert(path.into(), checksum.into());
    }

    /// Insert or update one region entry.
    pub fn set_region(&mut self, host: impl Into<String>, id: impl Into<String>, checksum: impl Into<String>) {
        self.regions.insert(
            RegionKey {
                host: host.into(),
                id: id.into(),
            },
            checksum.into(),
        );
    }
}

// Suppress an unused-import lint when no callers reference `Array`/`Value`
// yet (they will once writers gain inline-table support in later commits).

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn sample_manifest() -> Manifest {
        let mut m = Manifest {
            tool: Some("anvil".into()),
            tool_version: Some("0.1.0".into()),
            catalog_checksum: Some("sha256:abcd".into()),
            ..Manifest::default()
        };
        m.set_file("Justfile", "sha256:aaaa");
        m.set_file("justfiles/anvil/checks.just", "sha256:bbbb");
        m.set_region("Cargo.toml", "anvil-workspace-lints", "sha256:cccc");
        m.set_region("Justfile", "anvil-imports", "sha256:dddd");
        m
    }

    #[test]
    fn round_trip_preserves_content() {
        let m1 = sample_manifest();
        let text = m1.to_toml();
        let m2 = Manifest::parse(&text).unwrap();
        assert_eq!(m1, m2);
    }

    #[test]
    fn empty_manifest_round_trip() {
        let m1 = Manifest::default();
        let text = m1.to_toml();
        let m2 = Manifest::parse(&text).unwrap();
        assert_eq!(m1, m2);
    }

    #[test]
    fn to_toml_always_ends_with_exactly_one_newline() {
        // Catches mutation of the `if !out.ends_with('\n')` guard in to_toml.
        // Asserting `ends_with('\n')` alone is insufficient: when toml_edit's
        // serialization naturally ends with '\n' (the common case), the
        // mutated guard would double the newline -- still ending with '\n',
        // so a weaker assertion would pass.
        for m in [Manifest::default(), sample_manifest()] {
            let text = m.to_toml();
            let stripped = text.trim_end_matches('\n');
            assert_eq!(
                format!("{stripped}\n"),
                text,
                "to_toml output must end with exactly one newline, got: {text:?}"
            );
        }
    }

    #[test]
    fn toml_output_is_deterministic() {
        // Same content via two different insertion orders should serialize identically.
        let mut a = Manifest::default();
        a.set_file("z", "sha256:1");
        a.set_file("a", "sha256:2");
        let mut b = Manifest::default();
        b.set_file("a", "sha256:2");
        b.set_file("z", "sha256:1");
        assert_eq!(a.to_toml(), b.to_toml());
    }

    #[test]
    fn rejects_newer_schema() {
        let text = "version = 999\n";
        let err = Manifest::parse(text).unwrap_err();
        assert!(err.to_string().contains("newer than supported"));
    }

    #[test]
    fn provenance_fields_round_trip() {
        let m = sample_manifest();
        let parsed = Manifest::parse(&m.to_toml()).unwrap();
        assert_eq!(parsed.tool.as_deref(), Some("anvil"));
        assert_eq!(parsed.tool_version.as_deref(), Some("0.1.0"));
        assert_eq!(parsed.catalog_checksum.as_deref(), Some("sha256:abcd"));
    }

    #[test]
    fn legacy_rendered_by_parses_without_setting_tool() {
        // A pre-split lock has only `rendered_by`. `tool` must stay None so
        // the single-tool guard never fires on it; `tool_version` is
        // recovered from the version token for display.
        let text = "version = 1\nrendered_by = \"cargo-anvil 0.3.1\"\n";
        let m = Manifest::parse(text).unwrap();
        assert_eq!(m.tool, None, "legacy lock must have no recorded tool");
        assert_eq!(m.tool_version.as_deref(), Some("0.3.1"));
        assert_eq!(m.catalog_checksum, None);
    }

    #[test]
    fn provenance_keys_serialize_after_version() {
        let text = sample_manifest().to_toml();
        let version_pos = text.find("version =").unwrap();
        let tool_pos = text.find("tool =").unwrap();
        let file_pos = text.find("[[file]]").unwrap();
        assert!(
            version_pos < tool_pos && tool_pos < file_pos,
            "provenance keys must sit between version and [[file]]:\n{text}"
        );
    }

    #[test]
    fn rejects_missing_version() {
        let text = "rendered_by = \"x\"\n";
        let err = Manifest::parse(text).unwrap_err();
        assert!(err.to_string().contains("`version`"));
    }

    #[test]
    fn rejects_malformed_file_entry() {
        let text = "version = 1\n[[file]]\npath = \"foo\"\n";
        let err = Manifest::parse(text).unwrap_err();
        assert!(err.to_string().contains("`checksum`"));
    }

    #[test]
    fn rejects_duplicate_file_entry() {
        let text = "version = 1\n[[file]]\npath=\"x\"\nchecksum=\"sha256:1\"\n[[file]]\npath=\"x\"\nchecksum=\"sha256:2\"\n";
        let err = Manifest::parse(text).unwrap_err();
        assert!(err.to_string().contains("duplicate"));
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn load_missing_file_yields_empty_manifest() {
        let tmp = TempDir::new().unwrap();
        let m = Manifest::load(tmp.path()).unwrap();
        assert_eq!(m, Manifest::default());
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn save_then_load_round_trip() {
        let tmp = TempDir::new().unwrap();
        let m1 = sample_manifest();
        m1.save(tmp.path()).unwrap();
        assert!(Manifest::path_for(tmp.path()).is_file());

        let m2 = Manifest::load(tmp.path()).unwrap();
        assert_eq!(m1, m2);
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn save_overwrites_existing() {
        let tmp = TempDir::new().unwrap();
        let m1 = sample_manifest();
        m1.save(tmp.path()).unwrap();

        let mut m2 = Manifest::default();
        m2.set_file("only", "sha256:e");
        m2.save(tmp.path()).unwrap();

        let loaded = Manifest::load(tmp.path()).unwrap();
        assert_eq!(loaded, m2);
        assert_eq!(loaded.files.len(), 1);
        assert!(loaded.regions.is_empty());
    }

    #[test]
    fn toml_ends_with_newline() {
        let text = sample_manifest().to_toml();
        assert!(text.ends_with('\n'));
    }
}
