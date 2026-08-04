// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The repository-owned `.anvil/config.toml` container contract.
//!
//! This module parses and validates the declarative configuration described in
//! [`container-config.md`](../../docs/design/container-config.md): consumer image
//! extensions, managed cache volumes, explicit host mounts, and registered
//! repository commands.
//!
//! The drivers never read this file. `cargo-anvil` compiles it into generated
//! artifacts — a hashed `Containerfile` and an unhashed `runtime.conf` — so
//! validation happens once, in Rust, before any image is built and before any
//! container starts. The drivers are Bash and `PowerShell`; interpreting TOML
//! there would mean two parsers, two validators, and validation deferred to
//! container-start time.
//!
//! Every rejection here is a generation failure naming the file, the table, the
//! offending value, and the remedy.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;

use ohno::{AppError, IntoAppError as _, app_err, bail};
use toml_edit::{DocumentMut, Item, Table, Value};

/// The repository-owned configuration file, relative to the repository root.
pub const CONFIG_FILE_NAME: &str = ".anvil/config.toml";

/// The generated runtime file the drivers read, relative to the repository root.
pub const RUNTIME_FILE_NAME: &str = ".anvil/container/runtime.conf";

/// The marker declaring how packages are installed.
///
/// Its command, not Rust, decides the package ecosystem, so a downstream
/// catalog specializes it by replacing the `Containerfile` it already owns.
pub const PACKAGES_MARKER: &str = "# anvil-container-packages:";

/// The marker naming where consumer extensions are rendered.
pub const EXTENSIONS_MARKER: &str = "# anvil-container-extensions";

/// The placeholder substituted with shell-quoted package names.
pub const PACKAGES_PLACEHOLDER: &str = "{{packages}}";

/// Container paths `cargo-anvil` owns.
///
/// A declared target may neither nest inside one of these nor contain one:
/// a descendant such as `/workspace/target` would be shadowed by an
/// Anvil-owned mount, and an ancestor such as `/usr` would shadow the Cargo
/// mounts instead. Both directions are rejected.
pub const RESERVED_TARGETS: &[&str] = &[
    "/workspace",
    "/workspace/target",
    "/usr/local/cargo/registry",
    "/usr/local/cargo/git",
    "/anvil-git",
    "/run/secrets",
    "/tmp/anvil-lfs",
];

/// Environment variables the drivers or the image set themselves.
///
/// An image-level declaration of one of these would be silently overridden at
/// runtime, so it is rejected instead.
pub const RESERVED_ENV_KEYS: &[&str] = &["HOME", "PATH", "CARGO_HOME", "RUSTUP_HOME", "ANVIL_IN_CONTAINER"];

/// How widely a managed cache volume is shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheScope {
    /// One worktree, across every image identity.
    ///
    /// Named for the identifier it actually uses: the volume name derives from
    /// a hash of the worktree path, so two linked worktrees of one repository
    /// do not share it.
    Worktree,
    /// One worktree at one image identity.
    Image,
    /// Every repository and worktree on the host.
    Global,
}

impl CacheScope {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "worktree" => Some(Self::Worktree),
            "image" => Some(Self::Image),
            "global" => Some(Self::Global),
            _ => None,
        }
    }

    /// The wire form written to `runtime.conf`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Worktree => "worktree",
            Self::Image => "image",
            Self::Global => "global",
        }
    }
}

/// Whether a mount is writable from inside the container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountMode {
    /// The default: containerized code cannot modify the host path.
    ReadOnly,
    /// Explicitly requested write access to a host path.
    ReadWrite,
}

impl MountMode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "read-only" => Some(Self::ReadOnly),
            "read-write" => Some(Self::ReadWrite),
            _ => None,
        }
    }

    /// The wire form written to `runtime.conf`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::ReadWrite => "read-write",
        }
    }
}

/// Where a mount's host path comes from.
///
/// Tagged rather than inferred from the string, so that a path which escapes
/// the worktree has to say so. A bare `../sibling` reads as worktree-relative
/// while behaving as an escape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountSource {
    /// A path inside the worktree.
    Repository(String),
    /// A single directory beside the worktree root.
    Sibling(String),
    /// An absolute host path. Machine-specific.
    Host(String),
}

impl MountSource {
    /// The `kind` and `value` fields written to `runtime.conf`.
    #[must_use]
    pub fn wire(&self) -> (&'static str, &str) {
        match self {
            Self::Repository(value) => ("repository", value),
            Self::Sibling(value) => ("sibling", value),
            Self::Host(value) => ("host", value),
        }
    }
}

/// The value vocabulary a registered command's argument accepts.
///
/// A closed set rather than author-supplied regular expressions: Rust `regex`,
/// Bash ERE, and .NET differ in escaping and anchoring, so one pattern could
/// validate on one host and reject on the other — a defect visible only to
/// whoever does not use the author's platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgKind {
    /// Tags, names, and versions.
    Token,
    /// A signed decimal integer.
    Integer,
    /// A worktree-relative path that normalizes inside the worktree.
    Path,
    /// One of an explicit list of values.
    Enum(Vec<String>),
}

impl ArgKind {
    /// The wire form written to `runtime.conf`.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Token => "token",
            Self::Integer => "integer",
            Self::Path => "path",
            Self::Enum(_) => "enum",
        }
    }
}

/// A file copied from the worktree into the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSpec {
    /// Worktree-relative source path.
    pub source: String,
    /// Absolute path inside the image.
    pub target: String,
}

/// A build step rendered as its own `RUN` instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepSpec {
    /// Identifier used in diagnostics and in the rendered comment.
    pub name: String,
    /// The shell script body.
    pub run: String,
}

/// Static additions to the image.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageConfig {
    /// Installed with the package manager the effective `Containerfile` declares.
    pub packages: Vec<String>,
    /// `ENV` declarations, in sorted key order for deterministic rendering.
    pub env: Vec<(String, String)>,
    /// Files copied into the image.
    pub files: Vec<FileSpec>,
    /// Build steps, applied in declaration order.
    pub steps: Vec<StepSpec>,
}

impl ImageConfig {
    /// Whether the repository declared any image extension at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.packages.is_empty() && self.env.is_empty() && self.files.is_empty() && self.steps.is_empty()
    }
}

/// A persistent named volume Anvil creates, owns, and mounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheSpec {
    /// Unique within the file; part of the derived volume name.
    pub name: String,
    /// Absolute container path.
    pub target: String,
    /// How widely the volume is shared.
    pub scope: CacheScope,
}

/// An explicit host mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountSpec {
    /// Unique within the file; used in diagnostics.
    pub name: String,
    /// Where the host path comes from.
    pub source: MountSource,
    /// Absolute container path.
    pub target: String,
    /// Read-only unless explicitly widened.
    pub mode: MountMode,
}

/// One positional parameter of a registered command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgSpec {
    /// Unique within the command; used in diagnostics.
    pub name: String,
    /// The accepted value vocabulary.
    pub kind: ArgKind,
    /// Required arguments must precede optional ones.
    pub required: bool,
}

/// A repository-owned command runnable inside the container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// The name used on the `anvil-container` command line.
    pub name: String,
    /// The repository-owned `just` recipe it resolves to.
    pub recipe: String,
    /// Worktree-relative working directory, if not the worktree root.
    pub workdir: Option<String>,
    /// Ordered positional parameters.
    pub args: Vec<ArgSpec>,
}

/// A parsed and validated `.anvil/config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerConfig {
    /// Static additions to the image.
    pub image: ImageConfig,
    /// Managed cache volumes.
    pub caches: Vec<CacheSpec>,
    /// Explicit host mounts.
    pub mounts: Vec<MountSpec>,
    /// Registered repository commands.
    pub commands: Vec<CommandSpec>,
}

impl ContainerConfig {
    /// Read and validate the configuration file, if the repository has one.
    ///
    /// Returns `Ok(None)` when the file is absent: declaring nothing is the
    /// ordinary case, not an error path.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read, is not valid TOML, or
    /// fails any rule in
    /// [`container-config.md §6`](../../docs/design/container-config.md).
    pub fn load(repo_root: &Path) -> Result<Option<Self>, AppError> {
        let path = repo_root.join(CONFIG_FILE_NAME);
        if !path.is_file() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path).into_app_err_with(|| format!("failed to read {CONFIG_FILE_NAME}"))?;
        Self::parse(&text).map(Some)
    }

    /// Parse and validate configuration text.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed TOML, unknown tables or keys, wrong
    /// types, or any validation failure.
    pub fn parse(text: &str) -> Result<Self, AppError> {
        let doc: DocumentMut = text
            .parse::<DocumentMut>()
            .into_app_err_with(|| format!("{CONFIG_FILE_NAME} is not valid TOML"))?;

        let root = doc.as_table();
        reject_unknown_keys(root, &["container"], "the document root")?;

        let config = match root.get("container") {
            None => Self::default(),
            Some(item) => {
                let container = as_table(item, "[container]")?;
                reject_unknown_keys(container, &["image", "cache", "mount", "command"], "[container]")?;
                Self {
                    image: parse_image(container.get("image"))?,
                    caches: parse_caches(container.get("cache"))?,
                    mounts: parse_mounts(container.get("mount"))?,
                    commands: parse_commands(container.get("command"))?,
                }
            }
        };
        config.validate()?;
        Ok(config)
    }

    /// Whether the repository declared nothing at all.
    ///
    /// Used by tests and by downstream catalogs inspecting a loaded
    /// configuration; the generator keys off the file's presence instead, so an
    /// empty-but-present file still produces a runtime file and is still
    /// covered by the coherence record.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.image.is_empty() && self.caches.is_empty() && self.mounts.is_empty() && self.commands.is_empty()
    }

    /// Enforce every rule in
    /// [`container-config.md §6`](../../docs/design/container-config.md).
    fn validate(&self) -> Result<(), AppError> {
        self.validate_image()?;
        self.validate_caches()?;
        self.validate_mounts()?;
        self.validate_commands()?;
        self.validate_target_overlap()
    }

    fn validate_image(&self) -> Result<(), AppError> {
        for package in &self.image.packages {
            if package.is_empty() || !package.bytes().all(|b| b.is_ascii_graphic()) || package.starts_with('-') {
                bail!(
                    "{CONFIG_FILE_NAME}: [container.image] package '{package}' must be a non-empty printable name that does not start with '-'."
                );
            }
        }
        for (key, _) in &self.image.env {
            if RESERVED_ENV_KEYS.contains(&key.as_str()) || key.starts_with("ANVIL_CONTAINER_") {
                bail!(
                    "{CONFIG_FILE_NAME}: [container.image.env] must not set '{key}'; anvil sets it at runtime, so an image-level value would be silently overridden."
                );
            }
            if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                bail!("{CONFIG_FILE_NAME}: [container.image.env] key '{key}' must match [A-Za-z0-9_]+.");
            }
        }
        let mut seen = BTreeSet::new();
        for file in &self.image.files {
            validate_repository_path(&file.source, "[[container.image.file]] source")?;
            validate_container_path(&file.target, "[[container.image.file]] target")?;
            if !seen.insert(file.target.as_str()) {
                bail!(
                    "{CONFIG_FILE_NAME}: [[container.image.file]] target '{}' is declared twice.",
                    file.target
                );
            }
        }
        let mut seen = BTreeSet::new();
        for step in &self.image.steps {
            validate_name(&step.name, 63, "[[container.image.step]] name")?;
            if !seen.insert(step.name.as_str()) {
                bail!(
                    "{CONFIG_FILE_NAME}: [[container.image.step]] name '{}' is declared twice.",
                    step.name
                );
            }
            if step.run.trim().is_empty() {
                bail!(
                    "{CONFIG_FILE_NAME}: [[container.image.step]] '{}' has an empty `run` script.",
                    step.name
                );
            }
            // The script is rendered inside a heredoc; a line equal to the
            // delimiter would end it early and turn the remainder into
            // Containerfile instructions.
            if step.run.lines().any(|line| line.trim() == STEP_HEREDOC) {
                bail!(
                    "{CONFIG_FILE_NAME}: [[container.image.step]] '{}' contains a line equal to the reserved heredoc delimiter '{STEP_HEREDOC}'.",
                    step.name
                );
            }
        }
        Ok(())
    }

    fn validate_caches(&self) -> Result<(), AppError> {
        let mut seen = BTreeSet::new();
        for cache in &self.caches {
            validate_name(&cache.name, 31, "[[container.cache]] name")?;
            if !seen.insert(cache.name.as_str()) {
                bail!("{CONFIG_FILE_NAME}: [[container.cache]] name '{}' is declared twice.", cache.name);
            }
            validate_container_path(&cache.target, "[[container.cache]] target")?;
        }
        Ok(())
    }

    fn validate_mounts(&self) -> Result<(), AppError> {
        let mut seen = BTreeSet::new();
        for mount in &self.mounts {
            validate_name(&mount.name, 63, "[[container.mount]] name")?;
            if !seen.insert(mount.name.as_str()) {
                bail!("{CONFIG_FILE_NAME}: [[container.mount]] name '{}' is declared twice.", mount.name);
            }
            validate_container_path(&mount.target, "[[container.mount]] target")?;
            match &mount.source {
                MountSource::Repository(value) => validate_repository_path(value, "[[container.mount]] source.repository")?,
                MountSource::Sibling(value) => {
                    validate_field_charset(value, "[[container.mount]] source.sibling")?;
                    if value.is_empty() || value.contains('/') || value.contains('\\') || value == ".." || value == "." {
                        bail!(
                            "{CONFIG_FILE_NAME}: [[container.mount]] source.sibling '{value}' must be exactly one directory name beside the worktree root, with no path separators."
                        );
                    }
                }
                MountSource::Host(value) => {
                    validate_field_charset(value, "[[container.mount]] source.host")?;
                    if !value.starts_with('/') && !is_windows_absolute(value) {
                        bail!("{CONFIG_FILE_NAME}: [[container.mount]] source.host '{value}' must be an absolute path.");
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_commands(&self) -> Result<(), AppError> {
        let mut seen = BTreeSet::new();
        for command in &self.commands {
            validate_name(&command.name, 63, "[[container.command]] name")?;
            if command.name.starts_with("anvil-") || command.name.starts_with("_anvil-") {
                bail!(
                    "{CONFIG_FILE_NAME}: [[container.command]] name '{}' must not start with 'anvil-' or '_anvil-'; that prefix is what lets the first argument select between anvil recipes and registered commands.",
                    command.name
                );
            }
            if !seen.insert(command.name.as_str()) {
                bail!(
                    "{CONFIG_FILE_NAME}: [[container.command]] name '{}' is declared twice.",
                    command.name
                );
            }
            validate_recipe_name(&command.recipe, &command.name)?;
            if let Some(workdir) = &command.workdir {
                validate_repository_path(workdir, "[[container.command]] workdir")?;
            }

            let mut arg_names = BTreeSet::new();
            let mut seen_optional = false;
            for arg in &command.args {
                validate_name(&arg.name, 63, "[[container.command.arg]] name")?;
                if !arg_names.insert(arg.name.as_str()) {
                    bail!(
                        "{CONFIG_FILE_NAME}: [[container.command.arg]] name '{}' is declared twice in command '{}'.",
                        arg.name,
                        command.name
                    );
                }
                if arg.required && seen_optional {
                    bail!(
                        "{CONFIG_FILE_NAME}: [[container.command.arg]] '{}' in command '{}' is required but follows an optional argument; positional binding would be ambiguous.",
                        arg.name,
                        command.name
                    );
                }
                if !arg.required {
                    seen_optional = true;
                }
                if let ArgKind::Enum(values) = &arg.kind {
                    if values.is_empty() {
                        bail!(
                            "{CONFIG_FILE_NAME}: [[container.command.arg]] '{}' in command '{}' has type 'enum' but no `values`.",
                            arg.name,
                            command.name
                        );
                    }
                    for value in values {
                        validate_field_charset(value, "[[container.command.arg]] values entry")?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Two declared targets may not collide or nest in either direction: the
    /// result would otherwise depend on Docker's mount ordering.
    fn validate_target_overlap(&self) -> Result<(), AppError> {
        let mut declared: Vec<(&str, &str)> = Vec::new();
        for cache in &self.caches {
            declared.push((cache.target.as_str(), "[[container.cache]]"));
        }
        for mount in &self.mounts {
            declared.push((mount.target.as_str(), "[[container.mount]]"));
        }
        for (index, (target, kind)) in declared.iter().enumerate() {
            for (other, other_kind) in declared.iter().skip(index + 1) {
                if paths_overlap(target, other) {
                    bail!(
                        "{CONFIG_FILE_NAME}: {kind} target '{target}' overlaps {other_kind} target '{other}'; the effective mount would depend on ordering."
                    );
                }
            }
        }
        Ok(())
    }
}

/// Whether either path is the other, or contains the other.
fn paths_overlap(left: &str, right: &str) -> bool {
    left == right || is_descendant(left, right) || is_descendant(right, left)
}

/// Whether `candidate` lies strictly beneath `ancestor`.
fn is_descendant(candidate: &str, ancestor: &str) -> bool {
    let prefix = if ancestor.ends_with('/') {
        ancestor.to_owned()
    } else {
        format!("{ancestor}/")
    };
    candidate.starts_with(&prefix)
}

fn is_windows_absolute(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && (bytes[2] == b'/' || bytes[2] == b'\\')
}

/// Reject characters that would break the tab-separated runtime file, Docker's
/// comma-and-equals `--mount` syntax, or a shell that ever saw the value.
///
/// This is defense in depth: the drivers must also never interpolate a declared
/// value into a shell string.
fn validate_field_charset(value: &str, field: &str) -> Result<(), AppError> {
    if value.is_empty() {
        bail!("{CONFIG_FILE_NAME}: {field} must not be empty.");
    }
    for ch in value.chars() {
        if ch.is_whitespace() {
            bail!("{CONFIG_FILE_NAME}: {field} '{value}' must not contain whitespace; the generated runtime file is tab-separated.");
        }
        if "\"'`$;&|<>(){}[]*?!#~^,=\\".contains(ch) || ch.is_control() {
            bail!(
                "{CONFIG_FILE_NAME}: {field} '{value}' must not contain '{ch}'; declared values are restricted to a conservative character set."
            );
        }
    }
    Ok(())
}

fn validate_name(value: &str, max_len: usize, field: &str) -> Result<(), AppError> {
    let valid = !value.is_empty()
        && value.len() <= max_len + 1
        && value.bytes().next().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && value.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !valid {
        bail!(
            "{CONFIG_FILE_NAME}: {field} '{value}' must start with a lowercase letter or digit and contain only lowercase letters, digits, and hyphens (at most {} characters).",
            max_len + 1
        );
    }
    Ok(())
}

fn validate_recipe_name(value: &str, command: &str) -> Result<(), AppError> {
    let valid = !value.is_empty()
        && value.len() <= 64
        && value.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !valid {
        bail!(
            "{CONFIG_FILE_NAME}: [[container.command]] '{command}' recipe '{value}' must be a plain just recipe name ([A-Za-z0-9][A-Za-z0-9_-]*, at most 64 characters)."
        );
    }
    Ok(())
}

/// An absolute, normalized container path that does not collide with a path
/// Anvil owns, in either direction.
fn validate_container_path(value: &str, field: &str) -> Result<(), AppError> {
    validate_field_charset(value, field)?;
    if !value.starts_with('/') {
        bail!("{CONFIG_FILE_NAME}: {field} '{value}' must be an absolute container path.");
    }
    if value.len() > 1 && value.ends_with('/') {
        bail!("{CONFIG_FILE_NAME}: {field} '{value}' must not end with '/'.");
    }
    for segment in value.split('/').skip(1) {
        if segment.is_empty() || segment == "." || segment == ".." {
            bail!("{CONFIG_FILE_NAME}: {field} '{value}' must be normalized, with no empty, '.', or '..' segments.");
        }
    }
    for reserved in RESERVED_TARGETS {
        if value == *reserved {
            bail!("{CONFIG_FILE_NAME}: {field} '{value}' is a path anvil owns.");
        }
        if is_descendant(value, reserved) {
            bail!("{CONFIG_FILE_NAME}: {field} '{value}' is inside '{reserved}', which anvil owns, so it would be shadowed.");
        }
        if is_descendant(reserved, value) {
            bail!("{CONFIG_FILE_NAME}: {field} '{value}' contains '{reserved}', which anvil owns, so it would shadow it.");
        }
    }
    Ok(())
}

/// A relative path that normalizes to somewhere inside the worktree.
///
/// Normalization is lexical: the generator is not running when a container
/// starts, so the drivers resolve symlinks at runtime as well.
fn validate_repository_path(value: &str, field: &str) -> Result<(), AppError> {
    validate_field_charset(value, field)?;
    if value.starts_with('/') || is_windows_absolute(value) {
        bail!("{CONFIG_FILE_NAME}: {field} '{value}' must be relative to the worktree root.");
    }
    let mut depth = 0i32;
    for segment in value.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    bail!(
                        "{CONFIG_FILE_NAME}: {field} '{value}' escapes the worktree root. Use [[container.mount]] with source.sibling or source.host to reach outside it."
                    );
                }
            }
            _ => depth += 1,
        }
    }
    if depth == 0 && value != "." {
        bail!("{CONFIG_FILE_NAME}: {field} '{value}' must name a path inside the worktree.");
    }
    Ok(())
}

fn as_table<'a>(item: &'a Item, context: &str) -> Result<&'a Table, AppError> {
    item.as_table()
        .ok_or_else(|| app_err!("{CONFIG_FILE_NAME}: {context} must be a table."))
}

fn reject_unknown_keys(table: &Table, allowed: &[&str], context: &str) -> Result<(), AppError> {
    for (key, _) in table {
        if !allowed.contains(&key) {
            bail!(
                "{CONFIG_FILE_NAME}: unknown key '{key}' in {context}. Supported keys: {}.",
                allowed.join(", ")
            );
        }
    }
    Ok(())
}

fn string_field(table: &Table, key: &str, context: &str) -> Result<Option<String>, AppError> {
    match table.get(key) {
        None => Ok(None),
        Some(item) => item
            .as_str()
            .map(|value| Some(value.to_owned()))
            .ok_or_else(|| app_err!("{CONFIG_FILE_NAME}: {context} `{key}` must be a string.")),
    }
}

fn required_string(table: &Table, key: &str, context: &str) -> Result<String, AppError> {
    string_field(table, key, context)?.ok_or_else(|| app_err!("{CONFIG_FILE_NAME}: {context} is missing `{key}`."))
}

fn string_array(item: &Item, context: &str) -> Result<Vec<String>, AppError> {
    let array = item
        .as_array()
        .ok_or_else(|| app_err!("{CONFIG_FILE_NAME}: {context} must be an array of strings."))?;
    array
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| app_err!("{CONFIG_FILE_NAME}: {context} must contain only strings."))
        })
        .collect()
}

/// Iterate an array-of-tables that TOML may represent either as
/// `[[table]]` entries or as an inline array of inline tables.
fn table_entries<'a>(item: &'a Item, context: &str) -> Result<Vec<&'a Table>, AppError> {
    if let Some(array) = item.as_array_of_tables() {
        return Ok(array.iter().collect());
    }
    if item.as_array().is_some() {
        bail!("{CONFIG_FILE_NAME}: {context} must use [[...]] table syntax.");
    }
    bail!("{CONFIG_FILE_NAME}: {context} must be an array of tables.")
}

fn parse_image(item: Option<&Item>) -> Result<ImageConfig, AppError> {
    let Some(item) = item else {
        return Ok(ImageConfig::default());
    };
    let table = as_table(item, "[container.image]")?;
    reject_unknown_keys(table, &["packages", "env", "file", "step"], "[container.image]")?;

    let packages = match table.get("packages") {
        None => Vec::new(),
        Some(item) => string_array(item, "[container.image] packages")?,
    };

    let mut env = Vec::new();
    if let Some(item) = table.get("env") {
        let env_table = as_table(item, "[container.image.env]")?;
        for (key, value) in env_table {
            let value = value
                .as_str()
                .ok_or_else(|| app_err!("{CONFIG_FILE_NAME}: [container.image.env] '{key}' must be a string."))?;
            env.push((key.to_owned(), value.to_owned()));
        }
        env.sort();
    }

    let mut files = Vec::new();
    if let Some(item) = table.get("file") {
        for entry in table_entries(item, "[[container.image.file]]")? {
            reject_unknown_keys(entry, &["source", "target"], "[[container.image.file]]")?;
            files.push(FileSpec {
                source: required_string(entry, "source", "[[container.image.file]]")?,
                target: required_string(entry, "target", "[[container.image.file]]")?,
            });
        }
    }

    let mut steps = Vec::new();
    if let Some(item) = table.get("step") {
        for entry in table_entries(item, "[[container.image.step]]")? {
            reject_unknown_keys(entry, &["name", "run"], "[[container.image.step]]")?;
            steps.push(StepSpec {
                name: required_string(entry, "name", "[[container.image.step]]")?,
                run: required_string(entry, "run", "[[container.image.step]]")?,
            });
        }
    }

    Ok(ImageConfig {
        packages,
        env,
        files,
        steps,
    })
}

fn parse_caches(item: Option<&Item>) -> Result<Vec<CacheSpec>, AppError> {
    let Some(item) = item else { return Ok(Vec::new()) };
    let mut caches = Vec::new();
    for entry in table_entries(item, "[[container.cache]]")? {
        reject_unknown_keys(entry, &["name", "target", "scope"], "[[container.cache]]")?;
        let name = required_string(entry, "name", "[[container.cache]]")?;
        let scope = match string_field(entry, "scope", "[[container.cache]]")? {
            None => CacheScope::Worktree,
            Some(value) => CacheScope::parse(&value).ok_or_else(|| {
                app_err!("{CONFIG_FILE_NAME}: [[container.cache]] '{name}' scope '{value}' must be 'worktree', 'image', or 'global'.")
            })?,
        };
        caches.push(CacheSpec {
            target: required_string(entry, "target", "[[container.cache]]")?,
            name,
            scope,
        });
    }
    Ok(caches)
}

fn parse_mounts(item: Option<&Item>) -> Result<Vec<MountSpec>, AppError> {
    let Some(item) = item else { return Ok(Vec::new()) };
    let mut mounts = Vec::new();
    for entry in table_entries(item, "[[container.mount]]")? {
        reject_unknown_keys(entry, &["name", "source", "target", "mode"], "[[container.mount]]")?;
        let name = required_string(entry, "name", "[[container.mount]]")?;
        let mode = match string_field(entry, "mode", "[[container.mount]]")? {
            None => MountMode::ReadOnly,
            Some(value) => MountMode::parse(&value).ok_or_else(|| {
                app_err!("{CONFIG_FILE_NAME}: [[container.mount]] '{name}' mode '{value}' must be 'read-only' or 'read-write'.")
            })?,
        };
        mounts.push(MountSpec {
            source: parse_mount_source(entry, &name)?,
            target: required_string(entry, "target", "[[container.mount]]")?,
            name,
            mode,
        });
    }
    Ok(mounts)
}

fn parse_mount_source(entry: &Table, name: &str) -> Result<MountSource, AppError> {
    let item = entry
        .get("source")
        .ok_or_else(|| app_err!("{CONFIG_FILE_NAME}: [[container.mount]] '{name}' is missing `source`."))?;
    let source = item.as_inline_table().map_or_else(
        || {
            item.as_table().map_or_else(
                || {
                    Err(app_err!(
                        "{CONFIG_FILE_NAME}: [[container.mount]] '{name}' source must be a table naming exactly one of repository, sibling, or host."
                    ))
                },
                |table| {
                    Ok(table
                        .iter()
                        .filter_map(|(key, value)| value.as_str().map(|value| (key.to_owned(), value.to_owned())))
                        .collect::<Vec<_>>())
                },
            )
        },
        |inline| {
            Ok(inline
                .iter()
                .filter_map(|(key, value)| match value {
                    Value::String(text) => Some((key.to_owned(), text.value().clone())),
                    _ => None,
                })
                .collect::<Vec<_>>())
        },
    )?;

    let kinds: Vec<&(String, String)> = source
        .iter()
        .filter(|(key, _)| matches!(key.as_str(), "repository" | "sibling" | "host"))
        .collect();
    if source.len() != kinds.len() {
        bail!("{CONFIG_FILE_NAME}: [[container.mount]] '{name}' source may contain only one of repository, sibling, or host.");
    }
    match kinds.as_slice() {
        [(kind, value)] => match kind.as_str() {
            "repository" => Ok(MountSource::Repository(value.clone())),
            "sibling" => Ok(MountSource::Sibling(value.clone())),
            _ => Ok(MountSource::Host(value.clone())),
        },
        [] => bail!("{CONFIG_FILE_NAME}: [[container.mount]] '{name}' source must name one of repository, sibling, or host."),
        _ => bail!(
            "{CONFIG_FILE_NAME}: [[container.mount]] '{name}' source names {} kinds; exactly one is allowed.",
            kinds.len()
        ),
    }
}

fn parse_commands(item: Option<&Item>) -> Result<Vec<CommandSpec>, AppError> {
    let Some(item) = item else { return Ok(Vec::new()) };
    let mut commands = Vec::new();
    for entry in table_entries(item, "[[container.command]]")? {
        reject_unknown_keys(entry, &["name", "recipe", "workdir", "arg"], "[[container.command]]")?;
        let name = required_string(entry, "name", "[[container.command]]")?;
        let mut args = Vec::new();
        if let Some(arg_item) = entry.get("arg") {
            for arg in table_entries(arg_item, "[[container.command.arg]]")? {
                reject_unknown_keys(arg, &["name", "type", "required", "values"], "[[container.command.arg]]")?;
                let arg_name = required_string(arg, "name", "[[container.command.arg]]")?;
                let type_name = required_string(arg, "type", "[[container.command.arg]]")?;
                let kind = match type_name.as_str() {
                    "token" => ArgKind::Token,
                    "integer" => ArgKind::Integer,
                    "path" => ArgKind::Path,
                    "enum" => ArgKind::Enum(match arg.get("values") {
                        None => Vec::new(),
                        Some(values) => string_array(values, "[[container.command.arg]] values")?,
                    }),
                    other => bail!(
                        "{CONFIG_FILE_NAME}: [[container.command.arg]] '{arg_name}' type '{other}' must be 'token', 'integer', 'path', or 'enum'."
                    ),
                };
                let required = match arg.get("required") {
                    None => true,
                    Some(value) => value.as_bool().ok_or_else(|| {
                        app_err!("{CONFIG_FILE_NAME}: [[container.command.arg]] '{arg_name}' `required` must be a boolean.")
                    })?,
                };
                args.push(ArgSpec {
                    name: arg_name,
                    kind,
                    required,
                });
            }
        }
        commands.push(CommandSpec {
            recipe: required_string(entry, "recipe", "[[container.command]]")?,
            workdir: string_field(entry, "workdir", "[[container.command]]")?,
            name,
            args,
        });
    }
    Ok(commands)
}

/// The checksum recorded when the repository has no configuration file.
///
/// A distinct sentinel rather than the checksum of empty content, so adding an
/// empty configuration file is still detected as a change.
pub const ABSENT_CONFIG_CHECKSUM: &str = "absent";

/// The checksum of the configuration input, or [`ABSENT_CONFIG_CHECKSUM`].
#[must_use]
pub fn config_checksum(text: Option<&str>) -> String {
    text.map_or_else(|| ABSENT_CONFIG_CHECKSUM.to_owned(), crate::checksum::checksum_str)
}

/// Render the effective `Containerfile` for a repository's declarations.
///
/// The marker lines carry both the render position and the package-install
/// command, so a downstream catalog specializes the mechanism by editing the
/// `Containerfile` it already owns rather than through a second API.
///
/// # Errors
///
/// Returns an error when the repository declares image extensions but the
/// effective `Containerfile` has no extension marker, when a marker appears
/// more than once, or when packages are declared with no package marker.
pub fn render_containerfile(config: &ContainerConfig, containerfile: &str, tool: &str) -> Result<String, AppError> {
    let extension_lines: Vec<usize> = containerfile
        .lines()
        .enumerate()
        .filter(|(_, line)| line.trim() == EXTENSIONS_MARKER)
        .map(|(index, _)| index + 1)
        .collect();
    let package_lines: Vec<usize> = containerfile
        .lines()
        .enumerate()
        .filter(|(_, line)| line.trim_start().starts_with(PACKAGES_MARKER))
        .map(|(index, _)| index + 1)
        .collect();

    if extension_lines.len() > 1 {
        bail!(
            "{}: the extension marker appears on lines {}; exactly one is allowed.",
            CONTAINERFILE_NAME,
            join_lines(&extension_lines)
        );
    }
    if package_lines.len() > 1 {
        bail!(
            "{}: the package marker appears on lines {}; exactly one is allowed.",
            CONTAINERFILE_NAME,
            join_lines(&package_lines)
        );
    }

    if config.image.is_empty() {
        // Nothing declared: strip both markers so output is exactly today's.
        return Ok(strip_markers(containerfile));
    }
    if extension_lines.is_empty() {
        bail!(
            "{CONFIG_FILE_NAME} declares [container.image] extensions, but the Containerfile emitted by '{tool}' does not support consumer image extensions (no '{EXTENSIONS_MARKER}' marker). Remove the extensions, or use a catalog whose Containerfile hosts them."
        );
    }
    if !config.image.packages.is_empty() && package_lines.is_empty() {
        bail!(
            "{CONFIG_FILE_NAME} declares [container.image] packages, but the Containerfile emitted by '{tool}' declares no package-install command (no '{PACKAGES_MARKER}' marker). Install them with [[container.image.step]] instead."
        );
    }

    let package_command = containerfile
        .lines()
        .find(|line| line.trim_start().starts_with(PACKAGES_MARKER))
        .map(|line| line.trim_start().trim_start_matches(PACKAGES_MARKER).trim().to_owned());

    let block = render_extension_block(config, package_command.as_deref());
    let mut out = String::with_capacity(containerfile.len() + block.len());
    for line in containerfile.lines() {
        if line.trim_start().starts_with(PACKAGES_MARKER) {
            continue;
        }
        if line.trim() == EXTENSIONS_MARKER {
            out.push_str(&block);
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    Ok(out)
}

/// The `Containerfile` path, used in marker diagnostics.
const CONTAINERFILE_NAME: &str = ".anvil/container/Containerfile";

fn join_lines(lines: &[usize]) -> String {
    lines.iter().map(usize::to_string).collect::<Vec<_>>().join(", ")
}

fn strip_markers(containerfile: &str) -> String {
    let mut out = String::with_capacity(containerfile.len());
    for line in containerfile.lines() {
        if line.trim() == EXTENSIONS_MARKER || line.trim_start().starts_with(PACKAGES_MARKER) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Render packages, file copies, steps, and environment, in that order.
///
/// Ordering is documented in
/// [`container-config.md §11`](../../docs/design/container-config.md): consumer
/// extensions precede `anvil-setup`, which compiles Cargo tools that may need
/// consumer-provided libraries, headers, or environment.
fn render_extension_block(config: &ContainerConfig, package_command: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str("# >>> anvil: consumer image extensions from .anvil/config.toml\n");

    if !config.image.packages.is_empty()
        && let Some(command) = package_command
    {
        let quoted = config
            .image
            .packages
            .iter()
            .map(|package| shell_quote(package))
            .collect::<Vec<_>>()
            .join(" ");
        out.push_str("RUN ");
        out.push_str(&command.replace(PACKAGES_PLACEHOLDER, &quoted));
        out.push('\n');
    }

    for file in &config.image.files {
        // JSON form so a path can never be re-split on whitespace.
        let _ = writeln!(out, "COPY [{}, {}]", json_string(&file.source), json_string(&file.target));
    }

    for step in &config.image.steps {
        // A heredoc keeps the script's own structure and control flow. Joining
        // lines with `&&` or a line continuation would silently reinterpret a
        // two-command script as one command with extra arguments.
        //
        // `set -eu` is required, not decorative: without it only the final
        // command's status reaches Docker, so a failure mid-script would be
        // baked into the image as a success.
        let _ = writeln!(
            out,
            "# anvil: step '{}'\nRUN <<'{STEP_HEREDOC}'\nset -eu\n{}\n{STEP_HEREDOC}",
            step.name,
            step.run.trim_end()
        );
    }

    for (key, value) in &config.image.env {
        let _ = writeln!(out, "ENV {key}={}", json_string(value));
    }

    out.push_str("# <<< anvil: consumer image extensions\n");
    out
}

/// Render the build-context ignore file for a repository's declarations.
///
/// Identity and build context must be extended together: a declared file that
/// reached the context without being hashed would change image content under
/// an unchanged tag, and one that was hashed without reaching the context
/// would fail the build.
#[must_use]
pub fn render_ignore_file(config: &ContainerConfig, ignore: &str) -> String {
    if config.image.files.is_empty() {
        return ignore.to_owned();
    }
    let mut out = ignore.to_owned();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("# Declared by [[container.image.file]] in .anvil/config.toml.\n");
    for file in &config.image.files {
        let _ = writeln!(out, "!{}", file.source);
    }
    out
}

/// The heredoc delimiter for a rendered `[[container.image.step]]` script.
///
/// Quoted at the use site so the shell performs no expansion on the script
/// body. A step whose script contains this delimiter is rejected at validation
/// time, so it can never terminate its own heredoc.
const STEP_HEREDOC: &str = "ANVIL-STEP-EOF";

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// Single-quote a value for the package-install shell command.
///
/// Package names are validated to printable non-option ASCII, so this is a
/// second layer rather than the only one.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Render the generated runtime file the drivers read.
///
/// Line-oriented and tab-separated so both drivers can read it without a
/// parser. The coherence record is written last, after every artifact it
/// vouches for, so an interrupted generation fails closed.
#[must_use]
pub fn render_runtime_file(config: &ContainerConfig, config_checksum: &str, artifacts: &[(&str, &str)]) -> String {
    let mut out = String::new();
    out.push_str("# Generated by cargo-anvil from .anvil/config.toml. Do not edit.\n");
    out.push_str("# Tab-separated; the drivers read this file, never the TOML.\n");
    out.push_str("version\t1\n");

    for cache in &config.caches {
        let _ = writeln!(out, "cache\t{}\t{}\t{}", cache.name, cache.target, cache.scope.as_str());
    }
    for mount in &config.mounts {
        let (kind, value) = mount.source.wire();
        let _ = writeln!(
            out,
            "mount\t{}\t{kind}\t{value}\t{}\t{}",
            mount.name,
            mount.target,
            mount.mode.as_str()
        );
    }
    for command in &config.commands {
        let _ = writeln!(
            out,
            "command\t{}\t{}\t{}",
            command.name,
            command.recipe,
            command.workdir.as_deref().unwrap_or_default()
        );
        for arg in &command.args {
            let values = match &arg.kind {
                ArgKind::Enum(values) => values.join(","),
                _ => String::new(),
            };
            let _ = writeln!(
                out,
                "arg\t{}\t{}\t{}\t{}\t{values}",
                command.name,
                arg.name,
                arg.kind.as_str(),
                if arg.required { "required" } else { "optional" }
            );
        }
    }

    // Written last: everything above is vouched for by what follows.
    for (name, checksum) in artifacts {
        let _ = writeln!(out, "artifact\t{name}\t{checksum}");
    }
    let _ = writeln!(out, "config\t{config_checksum}");
    out
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn parse_err(text: &str) -> String {
        ContainerConfig::parse(text)
            .expect_err("configuration must be rejected")
            .to_string()
    }

    fn render_err(config: &ContainerConfig, containerfile: &str) -> String {
        render_containerfile(config, containerfile, "cargo-anvil")
            .expect_err("rendering must fail")
            .to_string()
    }

    const MARKED: &str = "FROM base\n# anvil-container-packages: apt-get install -y {{packages}}\n# anvil-container-extensions\nCOPY . .\n";

    #[test]
    fn absent_configuration_is_not_an_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(ContainerConfig::load(tmp.path()).unwrap(), None);
    }

    #[test]
    fn empty_document_parses_to_an_empty_configuration() {
        let config = ContainerConfig::parse("").unwrap();
        assert!(config.is_empty());
    }

    #[test]
    fn every_documented_key_parses() {
        let config = ContainerConfig::parse(
            r#"
[container.image]
packages = ["protobuf-compiler", "libpq-dev"]

[container.image.env]
PROTOC = "/usr/bin/protoc"

[[container.image.file]]
source = "build/pip.conf"
target = "/etc/pip.conf"

[[container.image.step]]
name = "install-kubectl"
run = "install -m 0755 /tmp/kubectl /usr/local/bin/kubectl"

[[container.cache]]
name = "pip"
target = "/tmp/anvil-user/.cache/pip"
scope = "worktree"

[[container.mount]]
name = "shared-protos"
source = { sibling = "shared-protos" }
target = "/shared-protos"
mode = "read-only"

[[container.command]]
name = "build-image"
recipe = "build-service-image"
workdir = "services/gateway"

[[container.command.arg]]
name = "tag"
type = "token"
required = true
"#,
        )
        .unwrap();

        assert_eq!(config.image.packages, ["protobuf-compiler", "libpq-dev"]);
        assert_eq!(config.image.env, [("PROTOC".to_owned(), "/usr/bin/protoc".to_owned())]);
        assert_eq!(config.image.files.len(), 1);
        assert_eq!(config.image.steps[0].name, "install-kubectl");
        assert_eq!(config.caches[0].scope, CacheScope::Worktree);
        assert_eq!(config.mounts[0].source, MountSource::Sibling("shared-protos".to_owned()));
        assert_eq!(config.mounts[0].mode, MountMode::ReadOnly);
        assert_eq!(config.commands[0].recipe, "build-service-image");
        assert_eq!(config.commands[0].args[0].kind, ArgKind::Token);
        assert!(!config.is_empty());
    }

    #[test]
    fn scope_and_mode_default_to_the_narrowest_option() {
        let config = ContainerConfig::parse(
            r#"
[[container.cache]]
name = "pip"
target = "/tmp/pip"

[[container.mount]]
name = "protos"
source = { sibling = "protos" }
target = "/protos"
"#,
        )
        .unwrap();
        assert_eq!(config.caches[0].scope, CacheScope::Worktree);
        assert_eq!(config.mounts[0].mode, MountMode::ReadOnly, "host mounts default to read-only");
    }

    #[test]
    fn unknown_tables_and_keys_are_rejected_rather_than_ignored() {
        assert!(parse_err("[container.imagine]\n").contains("unknown key 'imagine'"));
        assert!(parse_err("[container.image]\npackagez = []\n").contains("unknown key 'packagez'"));
        assert!(parse_err("[unrelated]\n").contains("unknown key 'unrelated'"));
        assert!(parse_err("[[container.cache]]\nname = \"pip\"\ntarget = \"/tmp/pip\"\nsize = 1\n").contains("unknown key 'size'"));
    }

    #[test]
    fn wrong_types_name_the_offending_key() {
        assert!(parse_err("[container.image]\npackages = \"curl\"\n").contains("must be an array of strings"));
        assert!(parse_err("[container.image]\npackages = [1]\n").contains("must contain only strings"));
        assert!(parse_err("[[container.cache]]\nname = 1\ntarget = \"/tmp/pip\"\n").contains("`name` must be a string"));
    }

    #[test]
    fn malformed_toml_is_reported_as_such() {
        assert!(parse_err("[container.image").contains("not valid TOML"));
    }

    #[test]
    fn a_mount_source_must_name_exactly_one_kind() {
        assert!(parse_err("[[container.mount]]\nname = \"m\"\nsource = {}\ntarget = \"/m\"\n").contains("must name one of repository"));
        assert!(
            parse_err("[[container.mount]]\nname = \"m\"\nsource = { sibling = \"a\", host = \"/b\" }\ntarget = \"/m\"\n")
                .contains("names 2 kinds")
        );
        assert!(
            parse_err("[[container.mount]]\nname = \"m\"\nsource = { elsewhere = \"a\" }\ntarget = \"/m\"\n")
                .contains("only one of repository")
        );
    }

    #[test]
    fn reserved_container_targets_are_rejected_in_both_directions() {
        for reserved in RESERVED_TARGETS {
            let text = format!("[[container.cache]]\nname = \"c\"\ntarget = \"{reserved}\"\n");
            assert!(
                parse_err(&text).contains("anvil owns"),
                "reserved target {reserved} must be rejected"
            );
        }
        // A descendant would be shadowed by the anvil-owned mount above it.
        assert!(
            parse_err("[[container.cache]]\nname = \"c\"\ntarget = \"/workspace/target/deps\"\n").contains("is inside"),
            "a descendant of an anvil-owned path must be rejected"
        );
        // An ancestor would shadow the anvil-owned mounts beneath it.
        assert!(
            parse_err("[[container.cache]]\nname = \"c\"\ntarget = \"/usr\"\n").contains("contains"),
            "an ancestor of an anvil-owned path must be rejected"
        );
    }

    #[test]
    fn declared_targets_may_not_overlap_each_other() {
        assert!(
            parse_err(
                "[[container.cache]]\nname = \"a\"\ntarget = \"/cache\"\n\n[[container.cache]]\nname = \"b\"\ntarget = \"/cache/inner\"\n"
            )
            .contains("overlaps")
        );
        assert!(
            parse_err(
                "[[container.cache]]\nname = \"a\"\ntarget = \"/shared\"\n\n[[container.mount]]\nname = \"b\"\nsource = { sibling = \"s\" }\ntarget = \"/shared\"\n"
            )
            .contains("overlaps")
        );
    }

    #[test]
    fn container_targets_must_be_absolute_and_normalized() {
        assert!(parse_err("[[container.cache]]\nname = \"c\"\ntarget = \"relative\"\n").contains("must be an absolute"));
        assert!(parse_err("[[container.cache]]\nname = \"c\"\ntarget = \"/a/../b\"\n").contains("must be normalized"));
        assert!(parse_err("[[container.cache]]\nname = \"c\"\ntarget = \"/a//b\"\n").contains("must be normalized"));
        assert!(parse_err("[[container.cache]]\nname = \"c\"\ntarget = \"/a/\"\n").contains("must not end with"));
    }

    #[test]
    fn repository_paths_may_not_escape_the_worktree() {
        assert!(
            parse_err("[[container.mount]]\nname = \"m\"\nsource = { repository = \"../outside\" }\ntarget = \"/m\"\n")
                .contains("escapes the worktree root")
        );
        assert!(
            parse_err("[[container.mount]]\nname = \"m\"\nsource = { repository = \"/abs\" }\ntarget = \"/m\"\n")
                .contains("must be relative")
        );
        // Re-entering the worktree is fine: it normalizes to somewhere inside.
        ContainerConfig::parse("[[container.mount]]\nname = \"m\"\nsource = { repository = \"a/../b\" }\ntarget = \"/m\"\n")
            .expect("a path that re-enters the worktree is inside it");
    }

    #[test]
    fn a_sibling_source_is_exactly_one_directory_name() {
        for bad in ["../elsewhere", "nested/path", "..", "."] {
            let text = format!("[[container.mount]]\nname = \"m\"\nsource = {{ sibling = \"{bad}\" }}\ntarget = \"/m\"\n");
            let message = parse_err(&text);
            assert!(
                message.contains("exactly one directory name") || message.contains("must not contain"),
                "sibling '{bad}' must be rejected, got: {message}"
            );
        }
    }

    #[test]
    fn a_host_source_must_be_absolute() {
        assert!(
            parse_err("[[container.mount]]\nname = \"m\"\nsource = { host = \"relative\" }\ntarget = \"/m\"\n")
                .contains("must be an absolute path")
        );
        ContainerConfig::parse("[[container.mount]]\nname = \"m\"\nsource = { host = \"/opt/tools\" }\ntarget = \"/m\"\n")
            .expect("an absolute POSIX host path is accepted");
        ContainerConfig::parse("[[container.mount]]\nname = \"m\"\nsource = { host = \"C:/tools\" }\ntarget = \"/m\"\n")
            .expect("a Windows absolute path is still an absolute host path");
    }

    #[test]
    fn shell_and_delimiter_metacharacters_are_rejected() {
        // The historical shape of the ownership container made a target like
        // this a root command; the drivers no longer build a shell string, and
        // the value is rejected here as well.
        assert!(parse_err("[[container.cache]]\nname = \"c\"\ntarget = \"/tmp/x;id\"\n").contains("must not contain ';'"));
        for bad in ["/tmp/a b", "/tmp/a,b", "/tmp/a=b", "/tmp/a$b", "/tmp/a|b", "/tmp/a`b"] {
            let text = format!("[[container.cache]]\nname = \"c\"\ntarget = \"{bad}\"\n");
            let message = parse_err(&text);
            assert!(
                message.contains("must not contain"),
                "target '{bad}' must be rejected, got: {message}"
            );
        }
    }

    #[test]
    fn reserved_environment_keys_are_rejected() {
        for key in RESERVED_ENV_KEYS {
            let text = format!("[container.image.env]\n{key} = \"x\"\n");
            assert!(
                parse_err(&text).contains("anvil sets it at runtime"),
                "reserved env key {key} must be rejected"
            );
        }
        assert!(parse_err("[container.image.env]\nANVIL_CONTAINER_IMAGE = \"x\"\n").contains("anvil sets it at runtime"));
    }

    #[test]
    fn duplicate_names_are_rejected_per_declaration_kind() {
        assert!(
            parse_err("[[container.cache]]\nname = \"c\"\ntarget = \"/a\"\n\n[[container.cache]]\nname = \"c\"\ntarget = \"/b\"\n")
                .contains("declared twice")
        );
        assert!(
            parse_err(
                "[[container.mount]]\nname = \"m\"\nsource = { sibling = \"a\" }\ntarget = \"/a\"\n\n[[container.mount]]\nname = \"m\"\nsource = { sibling = \"b\" }\ntarget = \"/b\"\n"
            )
            .contains("declared twice")
        );
        assert!(
            parse_err("[[container.command]]\nname = \"c\"\nrecipe = \"r\"\n\n[[container.command]]\nname = \"c\"\nrecipe = \"r\"\n")
                .contains("declared twice")
        );
    }

    #[test]
    fn command_names_may_not_claim_the_anvil_prefix() {
        for name in ["anvil-build", "_anvil-build"] {
            let text = format!("[[container.command]]\nname = \"{name}\"\nrecipe = \"r\"\n");
            let message = parse_err(&text);
            assert!(
                message.contains("must not start with") || message.contains("must start with a lowercase"),
                "command name '{name}' must be rejected, got: {message}"
            );
        }
    }

    #[test]
    fn required_arguments_must_precede_optional_ones() {
        let message = parse_err(
            r#"
[[container.command]]
name = "build"
recipe = "build"

[[container.command.arg]]
name = "first"
type = "token"
required = false

[[container.command.arg]]
name = "second"
type = "token"
"#,
        );
        assert!(message.contains("follows an optional argument"), "got: {message}");
    }

    #[test]
    fn an_enum_argument_needs_values() {
        let message = parse_err(
            "[[container.command]]\nname = \"b\"\nrecipe = \"b\"\n\n[[container.command.arg]]\nname = \"mode\"\ntype = \"enum\"\n",
        );
        assert!(message.contains("no `values`"), "got: {message}");
    }

    #[test]
    fn an_unknown_argument_type_is_rejected() {
        let message =
            parse_err("[[container.command]]\nname = \"b\"\nrecipe = \"b\"\n\n[[container.command.arg]]\nname = \"x\"\ntype = \"regex\"\n");
        assert!(message.contains("must be 'token', 'integer', 'path', or 'enum'"), "got: {message}");
    }

    #[test]
    fn a_recipe_name_must_be_a_plain_just_identifier() {
        for bad in ["--version", "a b", "with;semicolon"] {
            let text = format!("[[container.command]]\nname = \"c\"\nrecipe = \"{bad}\"\n");
            assert!(
                parse_err(&text).contains("plain just recipe name"),
                "recipe '{bad}' must be rejected"
            );
        }
    }

    #[test]
    fn a_workdir_may_not_escape_the_worktree() {
        let message = parse_err("[[container.command]]\nname = \"c\"\nrecipe = \"r\"\nworkdir = \"../outside\"\n");
        assert!(message.contains("escapes the worktree root"), "got: {message}");
    }

    #[test]
    fn duplicate_image_file_targets_are_rejected() {
        let message = parse_err(
            "[[container.image.file]]\nsource = \"a\"\ntarget = \"/etc/x\"\n\n[[container.image.file]]\nsource = \"b\"\ntarget = \"/etc/x\"\n",
        );
        assert!(message.contains("declared twice"), "got: {message}");
    }

    #[test]
    fn a_step_needs_a_name_and_a_body() {
        assert!(parse_err("[[container.image.step]]\nname = \"s\"\nrun = \"  \"\n").contains("empty `run` script"));
        assert!(parse_err("[[container.image.step]]\nrun = \"echo\"\n").contains("missing `name`"));
    }

    #[test]
    fn package_names_may_not_look_like_options() {
        assert!(parse_err("[container.image]\npackages = [\"--allow-downgrades\"]\n").contains("does not start with '-'"));
        assert!(parse_err("[container.image]\npackages = [\"\"]\n").contains("non-empty"));
    }

    #[test]
    fn an_empty_configuration_removes_both_markers_and_changes_nothing_else() {
        let rendered = render_containerfile(&ContainerConfig::default(), MARKED, "cargo-anvil").unwrap();
        assert_eq!(
            rendered, "FROM base\nCOPY . .\n",
            "a repository declaring nothing must get the Containerfile it has today"
        );
    }

    #[test]
    fn packages_use_the_command_the_marker_declares() {
        let config = ContainerConfig::parse("[container.image]\npackages = [\"protobuf-compiler\"]\n").unwrap();
        let rendered = render_containerfile(&config, MARKED, "cargo-anvil").unwrap();
        assert!(rendered.contains("RUN apt-get install -y 'protobuf-compiler'"), "got: {rendered}");
        assert!(!rendered.contains(PACKAGES_MARKER), "the marker itself must not survive");

        // A downstream catalog specializes the ecosystem by replacing the
        // Containerfile it already owns; no Rust change is involved.
        let tdnf = MARKED.replace("apt-get install -y {{packages}}", "tdnf install -y {{packages}}");
        let rendered = render_containerfile(&config, &tdnf, "cargo-forge").unwrap();
        assert!(rendered.contains("RUN tdnf install -y 'protobuf-compiler'"), "got: {rendered}");
    }

    #[test]
    fn extensions_render_in_the_documented_order() {
        let config = ContainerConfig::parse(
            r#"
[container.image]
packages = ["libpq-dev"]

[container.image.env]
PROTOC = "/usr/bin/protoc"

[[container.image.file]]
source = "build/pip.conf"
target = "/etc/pip.conf"

[[container.image.step]]
name = "first"
run = "echo one"

[[container.image.step]]
name = "second"
run = "echo two"
"#,
        )
        .unwrap();
        let rendered = render_containerfile(&config, MARKED, "cargo-anvil").unwrap();

        let position = |needle: &str| rendered.find(needle).unwrap_or_else(|| panic!("missing {needle} in: {rendered}"));
        assert!(position("apt-get install") < position("COPY [\"build/pip.conf\""));
        assert!(position("COPY [\"build/pip.conf\"") < position("echo one"));
        assert!(position("echo one") < position("echo two"), "steps keep declaration order");
        assert!(position("echo two") < position("ENV PROTOC="));
        assert!(position("ENV PROTOC=") < position("COPY . ."), "extensions precede anvil-setup");
    }

    #[test]
    fn steps_render_as_separate_named_instructions() {
        let config = ContainerConfig::parse("[[container.image.step]]\nname = \"install-kubectl\"\nrun = \"a\\nb\"\n").unwrap();
        let rendered = render_containerfile(&config, MARKED, "cargo-anvil").unwrap();
        assert!(rendered.contains("# anvil: step 'install-kubectl'"), "a failure must name its step");
        assert_eq!(rendered.matches("RUN ").count(), 1, "one RUN per step: {rendered}");
    }

    /// A multi-line script must keep its own structure. Joining lines with a
    /// continuation would turn `a\nb` into `a b` -- one command with extra
    /// arguments -- and `set -eu` is what makes a mid-script failure fail the
    /// build rather than being baked in as a success.
    #[test]
    fn a_multi_line_step_keeps_its_commands_separate_and_fails_fast() {
        let config = ContainerConfig::parse(
            "[[container.image.step]]\nname = \"s\"\nrun = \"curl -o /tmp/x https://example.invalid\\ninstall -m 0755 /tmp/x /usr/local/bin/x\"\n",
        )
        .unwrap();
        let rendered = render_containerfile(&config, MARKED, "cargo-anvil").unwrap();

        assert!(
            !rendered.contains("https://example.invalid \\"),
            "commands must not be joined into one: {rendered}"
        );
        assert!(rendered.contains("RUN <<'ANVIL-STEP-EOF'\nset -eu\n"), "got: {rendered}");
        assert!(
            rendered.contains("curl -o /tmp/x https://example.invalid\ninstall -m 0755"),
            "each command keeps its own line: {rendered}"
        );
        assert!(
            rendered.contains("\nANVIL-STEP-EOF\n"),
            "the heredoc must be terminated: {rendered}"
        );
    }

    #[test]
    fn a_step_may_not_terminate_its_own_heredoc() {
        let message = parse_err("[[container.image.step]]\nname = \"s\"\nrun = \"echo\\nANVIL-STEP-EOF\\nFROM evil\"\n");
        assert!(message.contains("reserved heredoc delimiter"), "got: {message}");
    }

    #[test]
    fn copy_uses_json_form_and_env_values_are_quoted() {
        let config = ContainerConfig::parse(
            "[[container.image.file]]\nsource = \"a/b.conf\"\ntarget = \"/etc/b.conf\"\n\n[container.image.env]\nK = \"v w\"\n",
        )
        .unwrap();
        let rendered = render_containerfile(&config, MARKED, "cargo-anvil").unwrap();
        assert!(
            rendered.contains(r#"COPY ["a/b.conf", "/etc/b.conf"]"#),
            "JSON form stops a path being re-split: {rendered}"
        );
        assert!(rendered.contains(r#"ENV K="v w""#), "got: {rendered}");
    }

    #[test]
    fn declaring_extensions_without_marker_support_names_the_owning_tool() {
        let config = ContainerConfig::parse("[container.image]\npackages = [\"curl\"]\n").unwrap();
        let message = render_err(&config, "FROM base\nCOPY . .\n");
        assert!(message.contains("cargo-anvil"), "the diagnostic must name the tool: {message}");
        assert!(message.contains("does not support consumer image extensions"), "got: {message}");
    }

    #[test]
    fn a_repository_declaring_nothing_tolerates_a_catalog_without_markers() {
        let rendered = render_containerfile(&ContainerConfig::default(), "FROM base\n", "cargo-anvil").unwrap();
        assert_eq!(rendered, "FROM base\n");
    }

    #[test]
    fn duplicate_markers_are_rejected_with_line_numbers() {
        let config = ContainerConfig::parse("[container.image]\npackages = [\"curl\"]\n").unwrap();
        let doubled = format!("FROM base\n{EXTENSIONS_MARKER}\n{EXTENSIONS_MARKER}\n");
        let message = render_err(&config, &doubled);
        assert!(message.contains("lines 2, 3"), "got: {message}");
    }

    #[test]
    fn packages_without_a_package_marker_are_rejected() {
        let config = ContainerConfig::parse("[container.image]\npackages = [\"curl\"]\n").unwrap();
        let message = render_err(&config, &format!("FROM base\n{EXTENSIONS_MARKER}\n"));
        assert!(message.contains("declares no package-install command"), "got: {message}");

        // Extensions that need no package manager still work.
        let config = ContainerConfig::parse("[[container.image.step]]\nname = \"s\"\nrun = \"echo\"\n").unwrap();
        render_containerfile(&config, &format!("FROM base\n{EXTENSIONS_MARKER}\n"), "cargo-anvil")
            .expect("a step needs no package-install command");
    }

    #[test]
    fn runtime_file_records_every_declaration_and_ends_with_coherence() {
        let config = ContainerConfig::parse(
            r#"
[[container.cache]]
name = "pip"
target = "/tmp/pip"
scope = "global"

[[container.mount]]
name = "protos"
source = { sibling = "protos" }
target = "/protos"
mode = "read-write"

[[container.command]]
name = "build-image"
recipe = "build"
workdir = "services/gateway"

[[container.command.arg]]
name = "tag"
type = "token"
"#,
        )
        .unwrap();
        let rendered = render_runtime_file(&config, "sha256:cfg", &[("Containerfile", "sha256:cf")]);

        assert!(rendered.contains("cache\tpip\t/tmp/pip\tglobal\n"), "got: {rendered}");
        assert!(
            rendered.contains("mount\tprotos\tsibling\tprotos\t/protos\tread-write\n"),
            "got: {rendered}"
        );
        assert!(
            rendered.contains("command\tbuild-image\tbuild\tservices/gateway\n"),
            "got: {rendered}"
        );
        assert!(rendered.contains("arg\tbuild-image\ttag\ttoken\trequired\t\n"), "got: {rendered}");
        assert!(rendered.contains("config\tsha256:cfg\n"), "got: {rendered}");
        assert!(rendered.contains("artifact\tContainerfile\tsha256:cf\n"), "got: {rendered}");

        // The coherence record vouches for artifacts written before it, so it
        // must come last: an interrupted generation then fails closed.
        let coherence = rendered.find("config\t").expect("coherence record is asserted above");
        for record in ["cache\t", "mount\t", "command\t", "arg\t"] {
            assert!(
                rendered.find(record).expect("record is asserted above") < coherence,
                "{record} must precede the coherence record"
            );
        }
    }

    #[test]
    fn a_command_without_a_workdir_still_emits_a_full_record() {
        let config = ContainerConfig::parse("[[container.command]]\nname = \"b\"\nrecipe = \"r\"\n").unwrap();
        let rendered = render_runtime_file(&config, "sha256:cfg", &[]);
        assert!(
            rendered.contains("command\tb\tr\t\n"),
            "an absent workdir is an empty field: {rendered}"
        );
    }

    #[test]
    fn optional_and_enum_arguments_round_trip_through_the_runtime_file() {
        let config = ContainerConfig::parse(
            "[[container.command]]\nname = \"b\"\nrecipe = \"r\"\n\n[[container.command.arg]]\nname = \"mode\"\ntype = \"enum\"\nvalues = [\"fast\", \"slow\"]\nrequired = false\n",
        )
        .unwrap();
        let rendered = render_runtime_file(&config, "sha256:cfg", &[]);
        assert!(rendered.contains("arg\tb\tmode\tenum\toptional\tfast,slow\n"), "got: {rendered}");
    }

    #[test]
    fn a_missing_file_and_an_empty_file_are_distinguishable() {
        assert_eq!(config_checksum(None), ABSENT_CONFIG_CHECKSUM);
        assert_ne!(
            config_checksum(Some("")),
            ABSENT_CONFIG_CHECKSUM,
            "adding an empty configuration must be detectable"
        );
    }

    #[test]
    fn the_configuration_checksum_ignores_line_endings() {
        assert_eq!(config_checksum(Some("a = 1\n")), config_checksum(Some("a = 1\r\n")));
    }
}
