// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![allow(clippy::expect_used, clippy::unwrap_used, reason = "panic-on-failure assertions in tests")]

//! Downstream-style engine coverage using only the supported crate-root API.
//! Fixtures are compiled into the executable; no host effects are performed.

#[path = "../examples/support/memory.rs"]
mod memory;

use std::cell::Cell;
use std::path::Path;

use cargo_anvil::{
    Artifact, Backend, Catalog, CliMeta, CommentSyntax, HostSelector, RegionId, RegionSpec, RepositoryEntry, UpdateIo, UpdateOptions,
    update,
};
use memory::MemoryRepository;
use ohno::{AppError, app_err};

const HISTORICAL_HOST: &str = include_str!("fixtures/update-0.20.1/gitattributes");
const HISTORICAL_LOCK: &str = include_str!("fixtures/update-0.20.1/anvil.lock");
const CARGO: &str = include_str!("fixtures/single-crate/Cargo.toml");

fn repository() -> MemoryRepository {
    let mut io = MemoryRepository::default();
    io.insert("Cargo.toml", CARGO);
    io
}

fn catalog(body: &str) -> Catalog {
    Catalog::builder(CliMeta::new("anvil"))
        .version("1.0.0")
        .with_artifact(Artifact::region(RegionSpec {
            host: HostSelector::Path(".gitattributes".to_owned()),
            id: RegionId::new("anvil-gitattributes"),
            body: body.to_owned(),
            syntax: CommentSyntax::Hash,
        }))
        .with_artifact(Artifact::owned_file("owned.just", "new recipe\n"))
        .with_artifact(Artifact::backend_file(Backend::GitHub, ".github/workflows/test.yml", "github\n"))
        .with_artifact(Artifact::backend_file(Backend::Ado, ".azuredevops/test.yml", "ado\n"))
        .build()
        .unwrap()
}

fn upgraded_catalog() -> Catalog {
    catalog("*.rs text eol=lf\n*.sh text eol=lf\n*.ps1 text eol=lf\n")
}

fn historical() -> MemoryRepository {
    let mut io = repository();
    io.insert(".gitattributes", HISTORICAL_HOST);
    io.insert(".anvil.lock", HISTORICAL_LOCK);
    io
}

#[test]
fn fresh_generation_selects_only_explicit_backends_and_deduplicates() {
    for (backends, github, ado) in [
        (vec![], false, false),
        (vec![Backend::GitHub], true, false),
        (vec![Backend::Ado], false, true),
        (vec![Backend::Ado, Backend::GitHub, Backend::Ado], true, true),
    ] {
        let mut io = repository();
        let mut options = UpdateOptions::default();
        options.backends = backends;
        let result = update(&upgraded_catalog(), &options, &mut io).unwrap();
        assert!(result.applied);
        assert!(result.has_changes);
        assert!(result.refusals.is_empty(), "{}", result.summary);
        assert_eq!(io.files.contains_key(".github/workflows/test.yml"), github);
        assert_eq!(io.files.contains_key(".azuredevops/test.yml"), ado);
        assert_eq!(result.backends.len(), usize::from(github) + usize::from(ado));
        assert_eq!(io.files["owned.just"], "new recipe\n");
        assert!(io.files[".anvil.lock"].contains("tool_version = \"1.0.0\""));
        assert!(io.files[".anvil.lock"].contains("catalog_checksum = \"sha256:"));
    }
}

#[test]
fn unchanged_historical_region_upgrades_and_preserves_surrounding_content() {
    for newline in ["\n", "\r\n"] {
        let mut io = historical();
        io.insert(".gitattributes", &HISTORICAL_HOST.replace("\r\n", "\n").replace('\n', newline));
        let result = update(&upgraded_catalog(), &UpdateOptions::default(), &mut io).unwrap();
        assert!(result.refusals.is_empty(), "{}", result.summary);
        assert!(result.summary.contains("Will update: 1 item(s)"), "{}", result.summary);
        let expected = "*.png binary\n\n# >>> anvil-managed: anvil-gitattributes\n*.rs text eol=lf\n*.sh text eol=lf\n*.ps1 text eol=lf\n# <<< anvil-managed: anvil-gitattributes\n\n# repository-owned suffix\n";
        assert_eq!(io.files[".gitattributes"], expected.replace('\n', newline));
        assert_ne!(io.files[".anvil.lock"], HISTORICAL_LOCK);
        let generated = io.files.clone();
        let again = update(&upgraded_catalog(), &UpdateOptions::default(), &mut io).unwrap();
        assert!(again.in_sync, "{}", again.summary);
        assert!(!again.has_changes);
        assert_eq!(io.files, generated);
    }
}

#[test]
fn edited_historical_region_is_refused_not_overwritten() {
    let mut io = historical();
    let edited = HISTORICAL_HOST.replace("*.rs text eol=lf", "*.rs -text");
    io.insert(".gitattributes", &edited);
    let result = update(&upgraded_catalog(), &UpdateOptions::default(), &mut io).unwrap();
    assert!(!result.in_sync);
    assert!(!result.refusals.is_empty(), "{}", result.summary);
    assert_eq!(io.files[".gitattributes"], edited);
    assert!(io.files[".anvil.lock"].contains("b91854c7f0e6d14c74751f607d2c0e3dfcefef55f5178bbafae59d47e577aa1a"));
    assert_eq!(io.files["owned.just"], "new recipe\n");
}

#[test]
fn preexisting_and_customized_owned_files_are_preserved_with_real_proposals() {
    let mut preexisting = repository();
    preexisting.insert("owned.just", "host recipe\n");
    update(&upgraded_catalog(), &UpdateOptions::default(), &mut preexisting).unwrap();
    assert_eq!(preexisting.files["owned.just"], "host recipe\n");
    assert_eq!(preexisting.files["owned.just.anvil-proposed"], "new recipe\n");

    let mut io = repository();
    let old = Catalog::builder(CliMeta::new("anvil"))
        .with_artifact(Artifact::owned_file("owned.just", "old recipe\n"))
        .build()
        .unwrap();
    update(&old, &UpdateOptions::default(), &mut io).unwrap();
    io.insert("owned.just", "customized recipe\n");
    update(&upgraded_catalog(), &UpdateOptions::default(), &mut io).unwrap();
    assert_eq!(io.files["owned.just"], "customized recipe\n");
    assert_eq!(io.files["owned.just.anvil-proposed"], "new recipe\n");
    let repeat = update(&upgraded_catalog(), &UpdateOptions::default(), &mut io).unwrap();
    assert!(repeat.in_sync, "{}", repeat.summary);
}

#[test]
fn dry_run_has_identical_planning_without_mutations() {
    for mut io in [repository(), historical()] {
        let before = io.files.clone();
        let mut options = UpdateOptions::default();
        options.dry_run = true;
        let dry = update(&upgraded_catalog(), &options, &mut io).unwrap();
        assert!(!dry.applied);
        assert!(dry.has_changes);
        assert!(!dry.in_sync);
        assert_eq!(io.files, before);
        assert!(io.mutations.is_empty(), "unexpected mutations: {:?}", io.mutations);
        options.dry_run = false;
        let applied = update(&upgraded_catalog(), &options, &mut io).unwrap();
        assert_eq!(dry.summary, applied.summary);
    }
}

#[test]
fn dry_run_reports_refusals_without_publishing_even_safe_changes() {
    let mut io = historical();
    io.insert(".gitattributes", &HISTORICAL_HOST.replace("*.sh text eol=lf", "*.sh -text"));
    let before = io.files.clone();
    let mut options = UpdateOptions::default();
    options.dry_run = true;
    let result = update(&upgraded_catalog(), &options, &mut io).unwrap();
    assert!(!result.in_sync);
    assert!(!result.refusals.is_empty(), "{}", result.summary);
    assert!(!result.applied);
    assert_eq!(io.files, before);
    assert!(io.mutations.is_empty(), "unexpected mutations: {:?}", io.mutations);
}

#[test]
fn lock_only_version_update_is_detected_and_then_idempotent() {
    let mut io = repository();
    let initial = upgraded_catalog();
    update(&initial, &UpdateOptions::default(), &mut io).unwrap();
    let changed = initial.into_builder().version("1.1.0").build().unwrap();
    let artifacts = io.files.clone();
    let result = update(&changed, &UpdateOptions::default(), &mut io).unwrap();
    assert!(result.has_changes);
    assert!(io.files[".anvil.lock"].contains("tool_version = \"1.1.0\""));
    assert_eq!(io.files["owned.just"], artifacts["owned.just"]);
    assert_eq!(io.files[".gitattributes"], artifacts[".gitattributes"]);
    assert!(update(&changed, &UpdateOptions::default(), &mut io).unwrap().in_sync);
}

#[cfg_attr(
    miri,
    ignore = "full-catalog generation and hashing are expensive to interpret; safe orchestration is covered by native tests and smaller Miri engine cases"
)]
#[test]
fn full_catalog_workspace_discovery_and_case_resolution_use_memory() {
    let mut io = repository();
    io.insert(
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\", \"crates/alpha\", \"extras/*\"]\n",
    );
    io.insert("crates/alpha/Cargo.toml", "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\n");
    io.insert("crates/not-a-crate/notes.txt", "not a member");
    io.insert("crates/manifest-directory/Cargo.toml/notes.txt", "not a manifest");
    io.insert("crates/README.txt", "not a directory");
    io.insert("extras", "not a directory either");
    io.insert("justfile", "host:\n    @echo host\n");
    let result = update(&Catalog::anvil(), &UpdateOptions::default(), &mut io).unwrap();
    assert!(result.refusals.is_empty(), "{}", result.summary);
    assert!(io.files["justfile"].starts_with("host:\n    @echo host\n"));
    assert!(!io.files.contains_key("Justfile"));
    assert!(io.files["crates/alpha/Cargo.toml"].contains("workspace = true"));
    assert_eq!(io.files["crates/manifest-directory/Cargo.toml/notes.txt"], "not a manifest");
    assert!(update(&Catalog::anvil(), &UpdateOptions::default(), &mut io).unwrap().in_sync);
}

#[cfg_attr(
    miri,
    ignore = "full-catalog generation and hashing are expensive to interpret; safe orchestration is covered by native tests and smaller Miri engine cases"
)]
#[test]
fn equivalent_workspace_paths_resolve_to_the_same_memory_files() {
    for member in ["crates/alpha", "./crates/alpha", "crates/*", "./crates/*"] {
        let mut io = repository();
        io.insert("./Cargo.toml", &format!("[workspace]\nmembers = [\"{member}\"]\n"));
        io.insert("./crates/alpha/./Cargo.toml", "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\n");
        let result = update(&Catalog::anvil(), &UpdateOptions::default(), &mut io).unwrap();
        assert!(result.refusals.is_empty(), "{}", result.summary);
        assert!(io.files["crates/alpha/Cargo.toml"].contains("workspace = true"));
        assert!(update(&Catalog::anvil(), &UpdateOptions::default(), &mut io).unwrap().in_sync);
    }
}

#[test]
fn ownership_guard_precedes_workspace_reads_and_force_switches_owner() {
    let mut io = historical();
    io.insert(".anvil.lock", &HISTORICAL_LOCK.replace("tool = \"anvil\"", "tool = \"another\""));
    io.insert("Cargo.toml", "invalid TOML");
    let error = update(&upgraded_catalog(), &UpdateOptions::default(), &mut io).unwrap_err();
    assert!(error.to_string().contains("managed by 'another'"));
    assert!(io.mutations.is_empty(), "unexpected mutations: {:?}", io.mutations);
    io.insert("Cargo.toml", CARGO);
    let mut options = UpdateOptions::default();
    options.force = true;
    update(&upgraded_catalog(), &options, &mut io).unwrap();
    assert!(io.files[".anvil.lock"].contains("tool = \"anvil\""));
}

#[test]
fn disabling_a_backend_removes_only_unchanged_owned_files() {
    let mut io = repository();
    let mut options = UpdateOptions::default();
    options.backends = vec![Backend::GitHub, Backend::Ado];
    update(&upgraded_catalog(), &options, &mut io).unwrap();
    io.insert(".azuredevops/test.yml", "customized pipeline\n");
    options.backends.clear();
    update(&upgraded_catalog(), &options, &mut io).unwrap();
    assert!(!io.files.contains_key(".github/workflows/test.yml"));
    assert_eq!(io.files[".azuredevops/test.yml"], "customized pipeline\n");
    assert!(!io.files[".anvil.lock"].contains(".azuredevops/test.yml"));
    assert!(
        io.mutations
            .iter()
            .any(|operation| operation == "remove .github/workflows/test.yml")
    );
}

#[derive(Default)]
struct FailingIo {
    repository: MemoryRepository,
    read: Option<String>,
    directory: Option<String>,
    write: Option<String>,
    remove: Option<String>,
    fail_second_lock_read: bool,
    lock_reads: Cell<usize>,
}

impl UpdateIo for FailingIo {
    fn read_file(&self, path: &Path) -> Result<Option<String>, AppError> {
        if path == Path::new(".anvil.lock") {
            self.lock_reads.set(self.lock_reads.get() + 1);
            if self.fail_second_lock_read && self.lock_reads.get() == 2 {
                return Err(app_err!("late lock read failed"));
            }
        }
        if self.read.as_deref().is_some_and(|fail| path == Path::new(fail)) {
            return Err(app_err!("scripted read failure"));
        }
        self.repository.read_file(path)
    }

    fn read_directory(&self, path: &Path) -> Result<Option<Vec<RepositoryEntry>>, AppError> {
        if self.directory.as_deref().is_some_and(|fail| path == Path::new(fail)) {
            return Err(app_err!("scripted directory failure"));
        }
        self.repository.read_directory(path)
    }

    fn write_file(&mut self, path: &Path, content: &str) -> Result<(), AppError> {
        if self.write.as_deref().is_some_and(|fail| path == Path::new(fail)) {
            return Err(app_err!("scripted write failure"));
        }
        self.repository.write_file(path, content)
    }

    fn remove_file(&mut self, path: &Path) -> std::io::Result<()> {
        if self.remove.as_deref().is_some_and(|fail| path == Path::new(fail)) {
            return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "scripted delete failure"));
        }
        self.repository.remove_file(path)
    }
}

#[test]
fn read_directory_and_late_lock_failures_stop_before_publication() {
    for (read, directory, late, expected) in [
        (Some("Cargo.toml"), None, false, "scripted read failure"),
        (Some(".anvil.lock"), None, false, "scripted read failure"),
        (Some(".gitattributes"), None, false, "scripted read failure"),
        (Some("owned.just"), None, false, "scripted read failure"),
        (None, Some(""), false, "scripted directory failure"),
        (None, None, true, "late lock read failed"),
    ] {
        let mut io = FailingIo {
            repository: historical(),
            read: read.map(str::to_owned),
            directory: directory.map(str::to_owned),
            fail_second_lock_read: late,
            ..FailingIo::default()
        };
        let before = io.repository.files.clone();
        let error = update(&upgraded_catalog(), &UpdateOptions::default(), &mut io).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        if late {
            assert_eq!(io.lock_reads.get(), 2);
        }
        assert_eq!(io.repository.files, before);
        assert!(
            io.repository.mutations.is_empty(),
            "unexpected mutations: {:?}",
            io.repository.mutations
        );
    }
}

#[test]
fn failed_artifact_or_lock_writes_do_not_publish_a_successful_lock() {
    for path in [".gitattributes", "owned.just", ".anvil.lock"] {
        let mut io = FailingIo {
            repository: historical(),
            write: Some(path.to_owned()),
            ..FailingIo::default()
        };
        let error = update(&upgraded_catalog(), &UpdateOptions::default(), &mut io).unwrap_err();
        assert!(error.to_string().contains("scripted write failure"));
        assert_eq!(io.repository.files[".anvil.lock"], HISTORICAL_LOCK);
    }
}

#[test]
fn removal_failure_prevents_lock_publication() {
    let mut io = FailingIo {
        repository: repository(),
        ..FailingIo::default()
    };
    let mut options = UpdateOptions::default();
    options.backends = vec![Backend::GitHub];
    update(&upgraded_catalog(), &options, &mut io).unwrap();
    let lock = io.repository.files[".anvil.lock"].clone();
    io.remove = Some(".github/workflows/test.yml".to_owned());
    options.backends.clear();
    let error = update(&upgraded_catalog(), &options, &mut io).unwrap_err();
    assert!(error.to_string().contains("scripted delete failure"));
    assert_eq!(io.repository.files[".anvil.lock"], lock);
}

#[test]
fn proposal_failure_preserves_the_original_and_previous_lock() {
    let mut io = FailingIo {
        repository: historical(),
        write: Some("owned.just.anvil-proposed".to_owned()),
        ..FailingIo::default()
    };
    io.repository.insert("owned.just", "host recipe\n");
    let error = update(&upgraded_catalog(), &UpdateOptions::default(), &mut io).unwrap_err();
    assert!(error.to_string().contains("scripted write failure"));
    assert_eq!(io.repository.files["owned.just"], "host recipe\n");
    assert!(!io.repository.files.contains_key("owned.just.anvil-proposed"));
    assert_eq!(io.repository.files[".anvil.lock"], HISTORICAL_LOCK);
}

#[test]
fn missing_root_manifest_returns_an_error_without_mutation() {
    let mut io = MemoryRepository::default();
    let error = update(&upgraded_catalog(), &UpdateOptions::default(), &mut io).unwrap_err();
    assert!(error.to_string().contains("no Cargo.toml at the repository root"), "{error}");
    assert!(io.files.is_empty(), "unexpected files: {:?}", io.files);
    assert!(io.mutations.is_empty(), "unexpected mutations: {:?}", io.mutations);
}

#[test]
fn workspace_member_read_and_enumeration_failures_do_not_mutate() {
    for (member, read, directory, expected) in [
        ("crates/*", Some("crates/alpha/Cargo.toml"), None, "scripted read failure"),
        ("crates/alpha", Some("crates/alpha/Cargo.toml"), None, "scripted read failure"),
        ("crates/*", None, Some("crates"), "scripted directory failure"),
        ("crates/*", None, Some("crates/alpha/Cargo.toml"), "scripted directory failure"),
    ] {
        let mut io = FailingIo {
            repository: repository(),
            read: read.map(str::to_owned),
            directory: directory.map(str::to_owned),
            ..FailingIo::default()
        };
        io.repository
            .insert("Cargo.toml", &format!("[workspace]\nmembers = [\"{member}\"]\n"));
        io.repository
            .insert("crates/alpha/Cargo.toml", "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\n");
        let before = io.repository.files.clone();
        let error = update(&upgraded_catalog(), &UpdateOptions::default(), &mut io).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert_eq!(io.repository.files, before);
        assert!(
            io.repository.mutations.is_empty(),
            "unexpected mutations: {:?}",
            io.repository.mutations
        );
    }
}

#[test]
fn artifact_reads_reject_directories_and_file_parents_before_mutation() {
    for (artifact, obstruction, expected) in [
        ("owned.just", "owned.just/notes.txt", "is a directory"),
        ("nested/owned.just", "nested", "parent 'nested' is a file"),
    ] {
        let mut io = repository();
        io.insert(obstruction, "repository content\n");
        let before = io.files.clone();
        let catalog = Catalog::builder(CliMeta::new("anvil"))
            .with_artifact(Artifact::owned_file(artifact, "generated\n"))
            .build()
            .unwrap();
        let error = update(&catalog, &UpdateOptions::default(), &mut io).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert!(error.to_string().contains(artifact), "{error}");
        assert_eq!(io.files, before);
        assert!(io.mutations.is_empty(), "unexpected mutations: {:?}", io.mutations);
    }
}

#[test]
fn artifact_writes_reject_file_directory_collisions_before_lock_publication() {
    for (first, second, expected) in [
        ("generated", "generated/owned.just", "parent 'generated' is a file"),
        ("generated/owned.just", "generated", "is a directory"),
    ] {
        let mut io = repository();
        let catalog = Catalog::builder(CliMeta::new("anvil"))
            .with_artifact(Artifact::owned_file(first, "first\n"))
            .with_artifact(Artifact::owned_file(second, "second\n"))
            .build()
            .unwrap();
        let error = update(&catalog, &UpdateOptions::default(), &mut io).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert_eq!(io.files[first], "first\n");
        assert!(!io.files.contains_key(second));
        assert!(!io.files.contains_key(".anvil.lock"));
        assert_eq!(io.mutations, vec![format!("write {first}")]);
    }
}

#[test]
fn invalid_manifests_and_ambiguous_casing_fail_without_mutation() {
    for (path, content, expected) in [
        ("Cargo.toml", "not TOML", "failed to parse Cargo.toml as TOML"),
        (
            "Cargo.toml",
            "[workspace]\nmembers = []\n",
            "workspace at the repository root resolved to zero members",
        ),
        (".anvil.lock", "not valid manifest TOML", "failed to parse manifest at .anvil.lock"),
        (
            ".anvil.lock",
            "version = 999\n",
            "manifest schema version 999 is newer than supported",
        ),
        (
            ".anvil.lock",
            "version = 2\n[[file]]\npath = \"../escape\"\nchecksum = \"old\"\n",
            "[[file]] entry '../escape' must be a relative path inside the repository",
        ),
    ] {
        let mut io = repository();
        io.insert(path, content);
        let error = update(&upgraded_catalog(), &UpdateOptions::default(), &mut io).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert!(io.mutations.is_empty(), "unexpected mutations: {:?}", io.mutations);
    }
    let mut io = repository();
    io.insert("OWNED.JUST", "one");
    io.insert("Owned.just", "two");
    let error = update(&upgraded_catalog(), &UpdateOptions::default(), &mut io).unwrap_err();
    assert!(error.to_string().contains("differ only by ASCII case"));
    assert!(error.to_string().contains("in the repository root because"), "{error}");
    assert!(io.mutations.is_empty(), "unexpected mutations: {:?}", io.mutations);
}
