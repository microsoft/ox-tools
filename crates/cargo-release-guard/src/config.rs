// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Preserve Cargo's configuration and credential-provider authority.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::{env, fs};

use sha2::{Digest, Sha256};
use toml_edit::{DocumentMut, Item};

use crate::{Result, fail, source};

#[derive(Debug)]
pub(crate) struct Config {
    pub paths: Vec<PathBuf>,
    pub default_registry: String,
    indexes: BTreeMap<String, String>,
    sources: BTreeMap<String, Source>,
    pub offline: bool,
    home: Option<PathBuf>,
}

#[derive(Debug, Default)]
struct Source {
    replacement: Option<String>,
    local: bool,
    registry: Option<String>,
}

impl Config {
    pub fn load(root: &Path, output: &Path) -> Result<Self> {
        for (name, _) in env::vars_os() {
            let name = name.to_string_lossy();
            if name == "CARGO_BUILD_BUILD_DIR" {
                return fail(
                    "unsupported Cargo build-output environment variable CARGO_BUILD_BUILD_DIR; unset it so build artifacts remain isolated",
                );
            }
            if name.starts_with("CARGO_SOURCE_") || name.starts_with("CARGO_PATCH_") || name == "CARGO_PATHS" {
                return fail(format!("unsupported source override environment variable: {name}"));
            }
        }
        let mut paths = Vec::new();
        let cargo_home = env::var_os("CARGO_HOME").map(PathBuf::from).or_else(|| {
            env::var_os("HOME")
                .or_else(|| env::var_os("USERPROFILE"))
                .map(|home| PathBuf::from(home).join(".cargo"))
        });
        if let Some(home) = cargo_home {
            find_config(&home, &mut paths)?;
        }
        let home = paths.first().cloned();
        let ancestors: Vec<_> = root.ancestors().collect();
        for directory in ancestors.into_iter().rev() {
            find_config(&directory.join(".cargo"), &mut paths)?;
        }
        // Output ancestry must not introduce a configuration absent from the source invocation.
        for ancestor in output.ancestors() {
            let mut additional = Vec::new();
            find_config(&ancestor.join(".cargo"), &mut additional)?;
            if additional.iter().any(|path| !paths.contains(path)) {
                return fail("output directory inherits an additional Cargo config; choose another output location");
            }
        }
        let mut config = Self {
            paths,
            default_registry: "crates-io".into(),
            indexes: BTreeMap::new(),
            sources: BTreeMap::new(),
            offline: false,
            home,
        };
        for path in &config.paths {
            source::reject_links(path)?;
            let document: DocumentMut = fs::read_to_string(path)?.parse().map_err(|_sensitive_parse_error| {
                std::io::Error::other(format!("cannot parse Cargo configuration at {}; contents omitted", path.display()))
            })?;
            validate(&document)?;
            if let Some(offline) = document.get("net").and_then(|item| item.get("offline")).and_then(Item::as_bool) {
                config.offline = offline;
            }
            if let Some(sources) = document.get("source").and_then(Item::as_table_like) {
                for (name, item) in sources.iter() {
                    let entry = config.sources.entry(name.into()).or_default();
                    if let Some(replacement) = item.get("replace-with").and_then(Item::as_str) {
                        entry.replacement = Some(replacement.into());
                    }
                    if let Some(registry) = item.get("registry").and_then(Item::as_str) {
                        entry.registry = Some(registry.into());
                    }
                    entry.local |= item.get("directory").is_some() || item.get("local-registry").is_some();
                }
            }
            if let Some(default) = document.get("registry").and_then(|item| item.get("default")).and_then(Item::as_str) {
                config.default_registry = default.into();
            }
            if let Some(registries) = document.get("registries").and_then(Item::as_table_like) {
                for (name, item) in registries.iter() {
                    if let Some(index) = item.get("index").and_then(Item::as_str) {
                        config.indexes.insert(name.into(), index.into());
                    }
                }
            }
        }
        if let Ok(default) = env::var("CARGO_REGISTRY_DEFAULT") {
            config.default_registry = default;
        }
        if let Ok(offline) = env::var("CARGO_NET_OFFLINE") {
            config.offline = offline == "true";
        }
        Ok(config)
    }

    pub fn automatic(&self, path: &Path, directory: &Path) -> bool {
        self.home.as_deref() == Some(path) || path.parent().and_then(Path::parent).is_some_and(|root| directory.starts_with(root))
    }

    pub fn fingerprint(&self) -> Result<String> {
        let mut hash = Sha256::new();
        for path in &self.paths {
            hash.update(path.to_string_lossy().as_bytes());
            hash.update([0]);
            hash.update(fs::read(path)?);
            hash.update([0]);
        }
        let environment: BTreeMap<_, _> = env::vars_os()
            .filter(|(key, _)| {
                let key = key.to_string_lossy();
                key == "CARGO_REGISTRY_DEFAULT" || (key.starts_with("CARGO_REGISTRIES_") && key.ends_with("_INDEX"))
            })
            .collect();
        for (key, value) in environment {
            hash.update(key.to_string_lossy().as_bytes());
            hash.update([0]);
            hash.update(value.to_string_lossy().as_bytes());
            hash.update([0]);
        }
        let mut digest = String::with_capacity(64);
        for byte in hash.finalize() {
            write!(digest, "{byte:02x}")?;
        }
        Ok(digest)
    }

    pub fn authoritative_local(&self, registry: &str) -> bool {
        let index = self.patch_key(registry).ok();
        let mut name = self
            .sources
            .iter()
            .find(|(_, source)| source.registry.as_deref() == index.as_deref())
            .map_or(registry, |(name, _)| name.as_str());
        let mut visited = std::collections::BTreeSet::new();
        while visited.insert(name.to_owned()) {
            let Some(source) = self.sources.get(name) else { return false };
            if source.local {
                return true;
            }
            let Some(replacement) = &source.replacement else { return false };
            name = replacement;
        }
        false
    }

    pub fn patch_key(&self, registry: &str) -> Result<String> {
        if registry == "crates-io" {
            return Ok(registry.into());
        }
        let variable = format!("CARGO_REGISTRIES_{}_INDEX", registry.to_ascii_uppercase().replace('-', "_"));
        if let Ok(index) = env::var(variable) {
            return Ok(index);
        }
        self.indexes
            .get(registry)
            .cloned()
            .map_or_else(|| fail(format!("registry {registry} has no configured index")), Ok)
    }
}

fn find_config(directory: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
    let old = directory.join("config");
    let new = directory.join("config.toml");
    let path = if old.is_file() { old } else { new };
    if path.is_file() {
        let path = path.canonicalize()?;
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    Ok(())
}

fn validate(document: &DocumentMut) -> Result<()> {
    for unsupported in ["paths", "patch", "replace", "include"] {
        if document.contains_key(unsupported) {
            return fail(format!(
                "unsupported Cargo config `{unsupported}`: source overrides cannot enter W_b"
            ));
        }
    }
    if document.get("unstable").and_then(|item| item.get("config-include")).is_some() {
        return fail("unsupported Cargo config inclusion");
    }
    if let Some(environment) = document.get("env").and_then(Item::as_table_like) {
        for (_, value) in environment.iter() {
            if value.get("relative").and_then(Item::as_bool) == Some(true) {
                return fail("unsupported Cargo config env relative=true: source-relative paths cannot enter W_b");
            }
        }
    }
    if let Some(build) = document.get("build") {
        for key in ["build-dir", "target-dir"] {
            if build.get(key).is_some() {
                return fail(format!("unsupported Cargo config build.{key}; output must remain isolated"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{Config, Source, validate};

    #[test]
    fn source_replacement_is_preserved_but_path_overrides_are_rejected() {
        validate(
            &"[source.crates-io]\nreplace-with='approved'\n[source.approved]\ndirectory='vendor'\n"
                .parse()
                .unwrap(),
        )
        .unwrap();
        for text in [
            "paths=['../private']",
            "[patch.crates-io]\na={path='..'}",
            "include=['elsewhere.toml']",
            "[build]\ntarget-dir='../shared'",
        ] {
            assert!(validate(&text.parse().unwrap()).is_err(), "{text}");
        }
    }

    #[test]
    fn source_cycles_and_unknown_registries_are_not_authoritative() {
        let config = Config {
            paths: Vec::new(),
            default_registry: "crates-io".into(),
            indexes: BTreeMap::new(),
            offline: true,
            home: None,
            sources: BTreeMap::from([
                (
                    "crates-io".into(),
                    Source {
                        replacement: Some("second".into()),
                        ..Source::default()
                    },
                ),
                (
                    "second".into(),
                    Source {
                        replacement: Some("crates-io".into()),
                        ..Source::default()
                    },
                ),
            ]),
        };
        assert!(!config.authoritative_local("crates-io"));
        assert!(
            config
                .patch_key("unknown-fixture-registry")
                .unwrap_err()
                .to_string()
                .contains("no configured index")
        );
    }
}
