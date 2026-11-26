// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ffi::OsString;

use super::*;

pub(in crate::exec) enum Operation<'a> {
    Seal(&'a [String], &'a str),
    Invalidate,
    Identity(&'a Plan),
}

type Observer = Box<dyn for<'a> FnMut(Operation<'a>) -> Option<[u8; 32]>>;

thread_local! {
    static OPERATIONS: std::cell::RefCell<Option<Observer>> = const { std::cell::RefCell::new(None) };
}

pub(super) fn observe(operation: Operation<'_>) -> ControlFlow<Option<[u8; 32]>> {
    OPERATIONS.with_borrow_mut(|observer| match observer {
        Some(observer) => ControlFlow::Break(observer(operation)),
        None => ControlFlow::Continue(()),
    })
}

pub(in crate::exec) fn with_operations<T>(
    observer: impl for<'a> FnMut(Operation<'a>) -> Option<[u8; 32]> + 'static,
    operation: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<Observer>);
    impl Drop for Restore {
        fn drop(&mut self) {
            OPERATIONS.with_borrow_mut(|observer| *observer = self.0.take());
        }
    }
    let _restore = Restore(OPERATIONS.with_borrow_mut(|slot| slot.replace(Box::new(observer))));
    operation()
}

struct MemorySnapshot(BTreeMap<Utf8PathBuf, String>);

impl Snapshot for MemorySnapshot {
    fn content(&self, path: &Utf8Path) -> std::io::Result<Option<[u8; 32]>> {
        Ok(self.0.get(path).map(|text| *blake3::hash(text.as_bytes()).as_bytes()))
    }

    fn text(&self, path: &Utf8Path) -> Option<String> {
        self.0.get(path).cloned()
    }

    fn tree(&self, root: &Utf8Path, target: &Utf8Path) -> Option<Vec<Utf8PathBuf>> {
        Some(
            self.0
                .keys()
                .filter(|path| path.starts_with(root) && !path.starts_with(target))
                .cloned()
                .collect(),
        )
    }
}

fn drive() -> Utf8PathBuf {
    if cfg!(windows) {
        r"C:\".into()
    } else {
        std::path::MAIN_SEPARATOR_STR.into()
    }
}

fn fixture() -> (Context, MemorySnapshot, Utf8PathBuf, Utf8PathBuf) {
    let root = drive().join("scratch");
    let target = drive().join("outputs");
    let compiler = drive().join("toolchain").join("bin").join("rustc.exe");
    let cargo = drive().join("toolchain").join("bin").join("cargo.exe");
    let library = drive().join("toolchain").join("lib").join("libstd.rlib");
    let manifest = root.join("Cargo.toml");
    let source = root.join("lib.rs");
    let snapshot = MemorySnapshot(BTreeMap::from([
        (
            manifest.clone(),
            "[package]\nname='subject'\nversion='0.1.0'\n[lib]\npath='lib.rs'\n".into(),
        ),
        (source.clone(), "pub fn value() -> u32 { 1 }".into()),
        (compiler.clone(), "rustc identity".into()),
        (cargo.clone(), "Cargo identity".into()),
        (library.clone(), "sysroot identity".into()),
    ]));
    let shape = snapshot.tree(&root, &target).unwrap();
    let context = Context {
        command: [1; 32],
        args: vec!["check".into(), "--workspace".into(), "--all-targets".into()],
        immutable: vec![(manifest.clone(), snapshot.content(&manifest).unwrap())],
        shape,
        toolchain: [compiler, cargo.clone(), library]
            .into_iter()
            .map(|path| {
                let hash = snapshot.content(&path).unwrap().unwrap();
                (path, hash)
            })
            .collect(),
        units: vec![[2; 32]],
        options: b"features and profile".to_vec(),
        mutable: vec![source],
        runtime: None,
        cargo: (cargo.clone(), snapshot.content(&cargo).unwrap().unwrap()),
    };
    (context, snapshot, root, target)
}

fn identity(context: &Context, snapshot: &MemorySnapshot, root: &Utf8Path, target: &Utf8Path) -> Option<[u8; 32]> {
    context_identity(
        context,
        context.command,
        &context.options,
        &snapshot.tree(root, target)?,
        &context.immutable,
        target,
        snapshot,
    )
}

#[test]
fn closed_metadata_context_has_a_live_identity_and_ignores_only_workspace_outputs() {
    let (context, mut snapshot, root, target) = fixture();
    let first = identity(&context, &snapshot, &root, &target).expect("a closed source-only build is cacheable");
    let _previous = snapshot
        .0
        .insert(target.join("libsubject.rmeta"), "schema-specific metadata".into());
    assert_eq!(identity(&context, &snapshot, &root, &target), Some(first));
    let _previous = snapshot.0.insert(root.join("lib.rs"), "pub fn value() -> u32 { 2 }".into());
    let second = identity(&context, &snapshot, &root, &target).expect("a static schema remains closed");
    assert_eq!(
        first, second,
        "the caller separately keys frozen mutable sources and the active schema"
    );
}

#[test]
fn identity_reuses_the_validated_immutable_stamps_and_rechecks_only_mutable_source_syntax() {
    struct Observed<'a> {
        inner: &'a MemorySnapshot,
        contents: std::cell::RefCell<BTreeMap<Utf8PathBuf, usize>>,
        texts: std::cell::RefCell<BTreeMap<Utf8PathBuf, usize>>,
    }
    impl Snapshot for Observed<'_> {
        fn content(&self, path: &Utf8Path) -> std::io::Result<Option<[u8; 32]>> {
            *self.contents.borrow_mut().entry(path.to_owned()).or_default() += 1;
            self.inner.content(path)
        }
        fn text(&self, path: &Utf8Path) -> Option<String> {
            *self.texts.borrow_mut().entry(path.to_owned()).or_default() += 1;
            self.inner.text(path)
        }
        fn tree(&self, root: &Utf8Path, target: &Utf8Path) -> Option<Vec<Utf8PathBuf>> {
            self.inner.tree(root, target)
        }
    }
    let (mut context, mut snapshot, root, target) = fixture();
    let immutable_source = root.join("unchanged.rs");
    let _previous = snapshot.0.insert(immutable_source.clone(), "fn unchanged() {}".into());
    context.shape = snapshot.tree(&root, &target).unwrap();
    context
        .immutable
        .push((immutable_source.clone(), snapshot.content(&immutable_source).unwrap()));
    context.immutable.sort();
    let observed = Observed {
        inner: &snapshot,
        contents: std::cell::RefCell::default(),
        texts: std::cell::RefCell::default(),
    };
    assert!(
        context_identity(
            &context,
            context.command,
            &context.options,
            &context.shape,
            &context.immutable,
            &target,
            &observed
        )
        .is_some()
    );
    for (path, _) in &context.immutable {
        assert_eq!(observed.contents.borrow()[path], 1, "{path} must be observed live exactly once");
        assert!(
            !observed.texts.borrow().contains_key(path),
            "{path} passed closure validation at seal time"
        );
    }
    assert_eq!(observed.texts.borrow()[&context.mutable[0]], 1);
    for (path, count) in observed.contents.borrow().iter() {
        assert_eq!(*count, 1, "{path}");
    }
}

#[test]
fn changed_compiler_sysroot_directory_shape_configuration_and_command_decline_identity() {
    let (context, mut snapshot, root, target) = fixture();
    assert!(identity(&context, &snapshot, &root, &target).is_some());
    let library = drive().join("toolchain").join("lib").join("new-std.rlib");
    let _previous = snapshot.0.insert(library, "new sysroot artifact".into());
    assert_eq!(identity(&context, &snapshot, &root, &target), None);
    let (context, mut snapshot, root, target) = fixture();
    let _previous = snapshot.0.insert(context.toolchain[0].0.clone(), "changed compiler".into());
    assert_eq!(identity(&context, &snapshot, &root, &target), None);
    let (context, snapshot, _root, target) = fixture();
    for (command, options, immutable) in [
        ([3; 32], context.options.clone(), context.immutable.clone()),
        (context.command, b"changed feature selection".to_vec(), context.immutable.clone()),
        (context.command, context.options.clone(), Vec::new()),
    ] {
        assert_eq!(
            context_identity(&context, command, &options, &context.shape, &immutable, &target, &snapshot),
            None,
        );
    }
}

#[test]
fn dynamic_source_expansion_and_dependency_manifests_are_not_assumed_closed() {
    for source in [
        "const X: &str = include_str!(\"outside.txt\");",
        "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));",
        "const X: &str = env!(\"CARGO_MAKEFLAGS\");",
        "macro_rules! indirectly { ($m:ident) => {$m!(\"outside.rs\")}; } indirectly!(include);",
        "#[path = \"../outside.rs\"] mod outside;",
    ] {
        let (context, mut snapshot, root, target) = fixture();
        let _previous = snapshot.0.insert(root.join("lib.rs"), source.into());
        assert_eq!(identity(&context, &snapshot, &root, &target), None, "{source}");
    }
    for manifest in [
        "[dependencies]\nother='1'",
        "[build-dependencies]\nother='1'",
        "[lib]\nproc-macro=true",
        "[package]\nbuild='custom-build.rs'",
    ] {
        assert!(
            !source_only_manifest(&toml::from_str::<toml::Value>(manifest).unwrap(), None),
            "{manifest}"
        );
    }
}

#[test]
fn only_the_exact_vendored_guard_dependency_is_accepted_in_a_source_only_manifest() {
    let runtime = drive().join("rt");
    let path = serde_json::to_string(runtime.as_str()).unwrap();
    let manifest = format!("[dependencies]\ngamma_rt={{path={path},package='cargo-gamma-rt'}}");
    let manifest = toml::from_str::<toml::Value>(&manifest).unwrap();
    assert!(source_only_manifest(&manifest, Some(&runtime)));
    assert!(!source_only_manifest(&manifest, Some(&drive().join("other-rt"))));
    assert!(!source_only_manifest(&manifest, None));
}

#[test]
fn source_manifest_guard_entries_do_not_hide_other_dependencies_or_invalid_package_aliases() {
    let runtime = drive().join("runtime");
    let path = serde_json::to_string(runtime.as_str()).unwrap();
    for manifest in [
        "[dependencies]\n".to_owned(),
        "[dependencies]\ngamma_rt={workspace=true}\n".to_owned(),
        format!("[dependencies]\ngamma_rt={{path={path}}}\n"),
        format!("[dependencies]\ncargo-gamma-rt={{path={path},package='cargo-gamma-rt'}}\n"),
    ] {
        assert!(
            source_only_manifest(&toml::from_str::<toml::Value>(&manifest).unwrap(), Some(&runtime)),
            "{manifest}"
        );
    }
    for manifest in [
        "[dependencies]\ngamma_rt={workspace=false}\n".to_owned(),
        "[dependencies]\ngamma_rt={workspace='true'}\n".to_owned(),
        format!("[dependencies]\ngamma_rt={{path={path},package='other'}}\n"),
        format!("[dependencies]\ngamma_rt={{path={path},package='cargo-gamma-rt'}}\nother='1'\n"),
    ] {
        let value = toml::from_str::<toml::Value>(&manifest).unwrap();
        assert!(!source_only_manifest(&value, Some(&runtime)), "{manifest}");
    }
    let allowed = toml::from_str::<toml::Value>("[package]\nbuild=false\n").unwrap();
    let rejected = toml::from_str::<toml::Value>("[package]\nbuild=true\n").unwrap();
    for value in [
        toml::Value::Array(Vec::new()),
        toml::Value::Array(vec![allowed.clone(), allowed.clone()]),
    ] {
        assert!(source_only_manifest(&value, Some(&runtime)));
    }
    for values in [vec![allowed.clone(), rejected.clone()], vec![rejected, allowed]] {
        assert!(!source_only_manifest(&toml::Value::Array(values), Some(&runtime)));
    }
}

#[test]
fn source_closure_reaches_unreadable_manifest_source_and_runtime_binding_boundaries() {
    let (context, mut snapshot, root, _target) = fixture();
    let manifest = root.join("Cargo.toml");
    let source = root.join("lib.rs");
    let runtime = drive().join("runtime");
    let path = serde_json::to_string(runtime.as_str()).unwrap();
    let _previous = snapshot.0.insert(
        manifest.clone(),
        format!("[package]\nname='subject'\n[dependencies]\ngamma_rt={{path={path},package='cargo-gamma-rt'}}"),
    );
    assert_eq!(closed_sources(&context.shape, &snapshot, Some(&runtime)), Some(()));
    assert_eq!(closed_sources(&context.shape, &snapshot, None), None);
    let _previous = snapshot.0.insert(manifest.clone(), "[package".into());
    assert_eq!(closed_sources(&context.shape, &snapshot, Some(&runtime)), None);
    let _removed = snapshot.0.remove(&manifest);
    assert_eq!(closed_sources(core::slice::from_ref(&manifest), &snapshot, Some(&runtime)), None);
    let _removed = snapshot.0.remove(&source);
    assert_eq!(closed_sources(core::slice::from_ref(&source), &snapshot, Some(&runtime)), None);
}

#[test]
fn source_closure_checks_each_hidden_expansion_identifier_without_matching_string_contents() {
    for identifier in [
        "include",
        "include_str",
        "include_bytes",
        "path",
        "global_asm",
        "asm",
        "env",
        "option_env",
    ] {
        let source = format!("macro_rules! deferred {{ () => {{ outer!({identifier}); }} }}");
        assert!(!source_only_tokens(source.parse().unwrap()), "{identifier}");
        let source = format!("const DESCRIPTION: &str = {identifier:?};");
        assert!(source_only_tokens(source.parse().unwrap()), "{identifier}");
    }
}

#[test]
fn native_proof_snapshot_preserves_missing_unreadable_and_non_utf8_input_distinctions() {
    use super::super::rustc_replay::snapshot_io::{self, FileData, Script};

    let path = drive().join("proof-snapshot");
    for case in ["present", "missing", "denied", "non-utf8"] {
        let mut script = Script::default();
        let bytes = if case == "non-utf8" { &[0xff][..] } else { &b"source bytes"[..] };
        let data = match case {
            "missing" => Err(std::io::ErrorKind::NotFound),
            "denied" => Err(std::io::ErrorKind::PermissionDenied),
            _ => Ok(FileData::bytes(bytes)),
        };
        let _previous = script.files.insert(path.clone().into_std_path_buf(), data);
        snapshot_io::with(script, || match case {
            "missing" => {
                assert_eq!(content(&path).unwrap(), None);
                assert_eq!(NativeSnapshot.content(&path).unwrap(), None);
                assert_eq!(NativeSnapshot.text(&path), None);
            }
            "denied" => {
                assert_eq!(content(&path).unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
                assert_eq!(
                    NativeSnapshot.content(&path).unwrap_err().kind(),
                    std::io::ErrorKind::PermissionDenied
                );
                assert_eq!(NativeSnapshot.text(&path), None);
            }
            _ => {
                let hash = Some(*blake3::hash(bytes).as_bytes());
                assert_eq!(content(&path).unwrap(), hash);
                assert_eq!(NativeSnapshot.content(&path).unwrap(), hash);
                assert_eq!(NativeSnapshot.text(&path), String::from_utf8(bytes.to_vec()).ok());
            }
        });
    }
}

#[test]
fn native_proof_tree_adapter_uses_the_replay_snapshot_and_excludes_only_target_outputs() {
    use super::super::rustc_replay::snapshot_io::{self, Script};

    let root = drive().join("proof-tree");
    let target = root.join("target");
    let source = root.join("lib.rs");
    let output = target.join("unit.rmeta");
    let script = Script::from_files([source.clone(), output].map(|path| (path.into_std_path_buf(), Vec::new())));
    snapshot_io::with(script, || {
        assert_eq!(tree(&root, &target), Some(vec![source.clone()]));
        assert_eq!(NativeSnapshot.tree(&root, &target), Some(vec![source]));
    });
    snapshot_io::with(Script::default(), || {
        assert_eq!(tree(&root, &target), None);
        assert_eq!(NativeSnapshot.tree(&root, &target), None);
    });
}

struct MetadataFixture {
    work: Workspace,
    plan: Plan,
    cargo: Utf8PathBuf,
    args: Vec<String>,
    files: BTreeMap<Utf8PathBuf, Vec<u8>>,
    directory: Utf8PathBuf,
    stdout: String,
}

impl MetadataFixture {
    fn add_runtime(&mut self) {
        use super::super::rustc_replay::snapshot_io;

        let root = self.work.compiler_runtime().to_owned();
        let manifest = root.join("Cargo.toml");
        let source = root.join("src").join("lib.rs");
        let metadata = self.work.target.join("libgamma_rt.rmeta");
        let archive = self.work.target.join("libgamma_rt.rlib");
        let _previous = self.files.insert(
            manifest,
            b"[package]\nname='cargo-gamma-rt'\nversion='0.1.0'\nbuild=false\n".to_vec(),
        );
        let _previous = self.files.insert(source, b"pub fn selected() -> bool { false }".to_vec());
        let _previous = self.files.insert(metadata.clone(), b"rust\0runtime metadata".to_vec());
        let _previous = self.files.insert(archive.clone(), b"!<arch>\nruntime archive".to_vec());
        let (record, runtime) = snapshot_io::with(self.script(&self.files), || {
            let stamp = |path: Utf8PathBuf| {
                let hash = NativeSnapshot.content(&path).unwrap().unwrap();
                (path, hash)
            };
            let compiler = self.cargo.with_file_name("rustc.exe");
            let mut record: serde_json::Value = serde_json::from_slice(&self.files[&self.directory.join("unit.json")]).unwrap();
            record["inventory"] = serde_json::to_value(snapshot_io::inventory(&self.work.root, &self.work.target).unwrap()).unwrap();
            record["externs"] = serde_json::json!([metadata]);
            let inputs = record["inputs"].as_array_mut().unwrap();
            inputs.push(serde_json::to_value(stamp(metadata.clone())).unwrap());
            inputs.sort_by(|left, right| Utf8Path::new(left[0].as_str().unwrap()).cmp(Utf8Path::new(right[0].as_str().unwrap())));
            let context = [6_u8; 32];
            let runtime = serde_json::json!({
                "root": root,
                "files": tree(&root, Utf8Path::new("")).unwrap().into_iter().map(stamp).collect::<Vec<_>>(),
                "shape": snapshot_io::inventory(&root, Utf8Path::new("")).unwrap(),
                "artifacts": [stamp(metadata), stamp(archive)],
                "compiler": stamp(compiler),
                "context": context,
            });
            (serde_json::to_vec(&record).unwrap(), serde_json::to_vec(&runtime).unwrap())
        });
        let _previous = self.files.insert(self.directory.join("unit.json"), record);
        let captures = self.work.rustc_captures().unwrap();
        let _previous = self.files.insert(captures.join("runtime-provenance"), runtime);
    }

    fn script(&self, files: &BTreeMap<Utf8PathBuf, Vec<u8>>) -> super::super::rustc_replay::snapshot_io::Script {
        use super::super::rustc_replay::snapshot_io::Script;

        let mut script = Script::from_files(files.iter().map(|(path, bytes)| (path.clone().into_std_path_buf(), bytes.clone())));
        script.environment = BTreeMap::from([
            ("CARGO".into(), self.cargo.clone().into_std_path_buf().into_os_string()),
            ("USERPROFILE".into(), drive().join("home").into_std_path_buf().into_os_string()),
        ]);
        script
    }

    fn generation_directory(&self) -> Utf8PathBuf {
        let mut command = self.work.cargo();
        let _command = command
            .args(&self.args)
            .env("CARGO_TERM_PROGRESS_WHEN", "always")
            .env("CARGO_TERM_PROGRESS_WIDTH", PROGRESS_WIDTH.to_string());
        self.work.configure_compiler_replay(&mut command);
        let generation = command
            .get_envs()
            .find_map(|(name, value)| (name == REPLAY_VAR).then_some(value).flatten())
            .unwrap()
            .to_str()
            .unwrap();
        self.work.rustc_captures().unwrap().join("replay").join(generation)
    }

    fn populate_current_generation(&self) {
        use super::super::rustc_replay::snapshot_io;

        snapshot_io::write(
            self.generation_directory().join("unit.json"),
            &self.files[&self.directory.join("unit.json")],
        )
        .unwrap();
    }
}

fn metadata_fixture() -> MetadataFixture {
    use super::super::rustc_replay::snapshot_io;

    let root = drive().join("native-proof-context");
    let target = root.join("target");
    let captures = target.join("captures");
    let cargo = drive().join("toolchain").join("bin").join("cargo.exe");
    let compiler = cargo.with_file_name("rustc.exe");
    let library = drive().join("toolchain").join("lib").join("libstd.rlib");
    let source = root.join("lib.rs");
    let manifest = root.join("Cargo.toml");
    let output = target.join("libsubject.rmeta");
    let mut work = Workspace::adopt(root.clone(), target.clone());
    work.set_rustc_captures(Some(captures));
    let plan = Plan {
        root: root.clone(),
        files: Vec::new(),
        mutants: Vec::new(),
        suppressed: 0,
        idle: Vec::new(),
        sharded_out: 0,
        settled_out: 0,
        digests: crate::HashMap::default(),
        skipped: Vec::new(),
        reach: crate::HashMap::default(),
        specs: crate::HashMap::default(),
    };
    let args = vec!["check".to_owned(), "--workspace".to_owned(), "--all-targets".to_owned()];
    let files = BTreeMap::from([
        (
            manifest,
            b"[package]\nname='subject'\nversion='0.1.0'\n[lib]\npath='lib.rs'\n".to_vec(),
        ),
        (source.clone(), b"pub fn value() -> u32 { 1 }".to_vec()),
        (cargo.clone(), b"cargo identity".to_vec()),
        (compiler.clone(), b"rustc identity".to_vec()),
        (library.clone(), b"sysroot identity".to_vec()),
        (output.clone(), b"rust\0metadata".to_vec()),
    ]);
    let mut fixture = MetadataFixture {
        work,
        plan,
        cargo: cargo.clone(),
        args,
        files,
        directory: Utf8PathBuf::new(),
        stdout: serde_json::json!({"reason": "compiler-artifact", "filenames": [output]}).to_string(),
    };
    let (directory, record) = snapshot_io::with(fixture.script(&fixture.files), || {
        let directory = fixture.generation_directory();
        let mut inputs = [source.clone(), compiler.clone(), cargo.clone(), library]
            .into_iter()
            .map(|path| {
                let hash = NativeSnapshot.content(&path).unwrap().unwrap();
                (path, hash)
            })
            .collect::<Vec<_>>();
        inputs.sort();
        let context = [9_u8; 32];
        let record = serde_json::json!({
            "context": context,
            "inputs": inputs,
            "outputs": [(output.clone(), NativeSnapshot.content(&output).unwrap().unwrap())],
            "inventory": snapshot_io::inventory(&root, &target).unwrap(),
            "stdout": [],
            "stderr": [],
            "compiler": compiler,
            "source": source,
            "externs": [],
            "searches": [],
        });
        (directory, serde_json::to_vec(&record).unwrap())
    });
    let _previous = fixture.files.insert(directory.join("unit.json"), record);
    fixture.directory = directory;
    fixture
}

#[test]
fn native_metadata_context_seals_publishes_reloads_and_invalidates_a_closed_capture() {
    use super::super::rustc_replay::snapshot_io;

    let fixture = metadata_fixture();
    let source = fixture.work.root.join("lib.rs");
    let manifest = fixture.work.root.join("Cargo.toml");
    snapshot_io::with(fixture.script(&fixture.files), || {
        assert_eq!(
            super::identity(&fixture.work, &fixture.plan),
            None,
            "an unpublished context is unavailable"
        );
        let context = seal_context(&fixture.work, &fixture.plan, &fixture.args, &fixture.stdout)
            .expect("all metadata captures and source controls are closed");
        assert_eq!(context.args, fixture.args);
        assert_eq!(context.units, vec![[9; 32]]);
        assert_eq!(context.shape, vec![manifest.clone(), source.clone()]);
        assert_eq!(context.cargo.0, fixture.cargo);
        seal(&fixture.work, &fixture.plan, &fixture.args, &fixture.stdout);
        let first = super::identity(&fixture.work, &fixture.plan).expect("the published context reloads through the native adapters");
        assert_eq!(super::identity(&fixture.work, &fixture.plan), Some(first));
        snapshot_io::write(&source, b"pub fn value() -> u32 { 2 }").unwrap();
        assert_eq!(
            super::identity(&fixture.work, &fixture.plan),
            None,
            "an immutable source change cannot reuse the earlier proof"
        );
        snapshot_io::write(&source, &fixture.files[&source]).unwrap();
        assert_eq!(super::identity(&fixture.work, &fixture.plan), Some(first));
        let control = drive().join("home").join(".cargo").join("config.toml");
        snapshot_io::write(&control, b"[build]\nrustflags=['--cfg=changed']").unwrap();
        assert_eq!(
            super::identity(&fixture.work, &fixture.plan),
            None,
            "ancestor and tool-home controls participate in the proof"
        );
        snapshot_io::remove_file(&control).unwrap();
        assert_eq!(super::identity(&fixture.work, &fixture.plan), Some(first));
        invalidate(&fixture.work);
        assert_eq!(super::identity(&fixture.work, &fixture.plan), None);
    });
}

#[test]
fn native_metadata_context_declines_incomplete_captures_unapproved_launchers_and_unresolved_controls() {
    use super::super::rustc_replay::snapshot_io;

    let fixture = metadata_fixture();
    for args in [vec!["build".to_owned()], Vec::new()] {
        snapshot_io::with(fixture.script(&fixture.files), || {
            assert!(seal_context(&fixture.work, &fixture.plan, &args, &fixture.stdout).is_none());
        });
    }
    snapshot_io::with(fixture.script(&fixture.files), || {
        let bad =
            serde_json::json!({"reason": "compiler-artifact", "filenames": [fixture.work.target.join("unrecorded.rmeta")]}).to_string();
        assert!(seal_context(&fixture.work, &fixture.plan, &fixture.args, &bad).is_none());
        seal(&fixture.work, &fixture.plan, &fixture.args, &bad);
        assert_eq!(super::identity(&fixture.work, &fixture.plan), None);
    });
    for program in [Utf8PathBuf::from("cargo"), drive().join("alternative-tools").join("other.exe")] {
        let mut changed = fixture.files.clone();
        let _previous = changed.insert(program.clone(), b"an unapproved Cargo launcher".to_vec());
        let mut script = fixture.script(&changed);
        let _previous = script
            .environment
            .insert("CARGO".into(), program.into_std_path_buf().into_os_string());
        snapshot_io::with(script, || {
            fixture.populate_current_generation();
            assert!(seal_context(&fixture.work, &fixture.plan, &fixture.args, &fixture.stdout).is_none());
        });
    }
    let alternate = drive().join("alternative-tools").join("cargo.exe");
    let mut changed = fixture.files.clone();
    let _previous = changed.insert(alternate.clone(), b"aliased Cargo launcher".to_vec());
    let mut script = fixture.script(&changed);
    let _previous = script
        .environment
        .insert("CARGO".into(), alternate.clone().into_std_path_buf().into_os_string());
    let _previous = script.symlinks.insert(alternate.into_std_path_buf(), Ok(true));
    snapshot_io::with(script, || {
        fixture.populate_current_generation();
        assert!(seal_context(&fixture.work, &fixture.plan, &fixture.args, &fixture.stdout).is_none());
    });
    for home in [None, Some(OsString::from("relative-home"))] {
        let mut script = fixture.script(&fixture.files);
        let _previous = script.environment.remove(std::ffi::OsStr::new("USERPROFILE"));
        if let Some(home) = home {
            let _previous = script.environment.insert("USERPROFILE".into(), home);
        }
        snapshot_io::with(script, || {
            fixture.populate_current_generation();
            assert!(seal_context(&fixture.work, &fixture.plan, &fixture.args, &fixture.stdout).is_none());
        });
    }
    let control = drive().join("home").join(".cargo").join("config.toml");
    let mut script = fixture.script(&fixture.files);
    let _previous = script
        .files
        .insert(control.into_std_path_buf(), Err(std::io::ErrorKind::PermissionDenied));
    snapshot_io::with(script, || {
        assert!(seal_context(&fixture.work, &fixture.plan, &fixture.args, &fixture.stdout).is_none());
    });
}

#[test]
fn source_controls_cover_ancestors_and_explicit_or_default_tool_homes() {
    let root = drive().join("workspace").join("member");
    let home = drive().join("home");
    for home_name in ["USERPROFILE", "HOME"] {
        let environment = BTreeMap::from([(home_name.to_owned(), home.clone().into_std_path_buf().into_os_string())]);
        let paths = controls_with(&root, |name| environment.get(name).cloned(), |_| true).unwrap();
        for ancestor in root.ancestors() {
            for control in [
                ancestor.join(".cargo").join("config"),
                ancestor.join(".cargo").join("config.toml"),
                ancestor.join("rust-toolchain"),
                ancestor.join("rust-toolchain.toml"),
            ] {
                assert!(paths.contains(&control), "{control}");
            }
        }
        for control in [
            home.join(".cargo").join("config"),
            home.join(".cargo").join("config.toml"),
            home.join(".rustup").join("settings.toml"),
        ] {
            assert!(paths.contains(&control), "{control}");
        }
        assert!(paths.windows(2).all(|pair| pair[0] < pair[1]));
    }
    let cargo = drive().join("custom-cargo");
    let rustup = drive().join("custom-rustup");
    let environment = BTreeMap::from([
        ("USERPROFILE", home.clone().into_std_path_buf().into_os_string()),
        ("HOME", drive().join("ignored-home").into_std_path_buf().into_os_string()),
        ("CARGO_HOME", cargo.clone().into_std_path_buf().into_os_string()),
        ("RUSTUP_HOME", rustup.clone().into_std_path_buf().into_os_string()),
    ]);
    let paths = controls_with(&root, |name| environment.get(name).cloned(), |_| true).unwrap();
    assert!(paths.contains(&cargo.join("config")));
    assert!(paths.contains(&cargo.join("config.toml")));
    assert!(paths.contains(&rustup.join("settings.toml")));
    assert!(!paths.contains(&home.join(".cargo").join("config")));
    assert!(!paths.contains(&home.join(".rustup").join("settings.toml")));
    assert!(controls_with(&root, |_| None, |_| true).is_none());
    for name in ["USERPROFILE", "CARGO_HOME", "RUSTUP_HOME"] {
        let mut environment = environment.clone();
        let _previous = environment.insert(name, "relative-home".into());
        assert!(
            controls_with(&root, |name| environment.get(name).cloned(), |_| true).is_none(),
            "{name}"
        );
    }
    for blocked in paths {
        assert!(
            controls_with(&root, |name| environment.get(name).cloned(), |path| path != blocked).is_none(),
            "{blocked}"
        );
    }
}

#[test]
fn native_metadata_sealing_uses_the_owned_generation_with_encoded_rustflags() {
    use super::super::rustc_replay::snapshot_io;

    let fixture = metadata_fixture();
    let mut script = fixture.script(&fixture.files);
    let _previous = script.environment.insert("CARGO_ENCODED_RUSTFLAGS".into(), "--cfg=selected".into());
    snapshot_io::with(script, || {
        fixture.populate_current_generation();
        assert!(seal_context(&fixture.work, &fixture.plan, &fixture.args, &fixture.stdout).is_some());
    });
}

#[test]
fn native_metadata_sealing_does_not_borrow_captures_from_a_different_command_generation() {
    use super::super::rustc_replay::snapshot_io;

    let mut fixture = metadata_fixture();
    let record = fixture.files.remove(&fixture.directory.join("unit.json")).unwrap();
    let stale = fixture.directory.parent().unwrap().join("b".repeat(64));
    assert_ne!(stale, fixture.directory);
    let _previous = fixture.files.insert(stale.join("unit.json"), record);
    snapshot_io::with(fixture.script(&fixture.files), || {
        assert!(seal_context(&fixture.work, &fixture.plan, &fixture.args, &fixture.stdout).is_none());
    });
}

#[test]
fn native_metadata_sealing_declines_an_unavailable_capture_generation() {
    use super::super::rustc_replay::snapshot_io;

    let fixture = metadata_fixture();
    let mut script = fixture.script(&fixture.files);
    let _previous = script
        .directory_errors
        .insert(fixture.directory.clone().into_std_path_buf(), std::io::ErrorKind::PermissionDenied);
    snapshot_io::with(script, || {
        assert!(seal_context(&fixture.work, &fixture.plan, &fixture.args, &fixture.stdout).is_none());
    });
}

#[test]
fn native_metadata_context_declines_non_unicode_environment() {
    use super::super::rustc_replay::snapshot_io;

    #[cfg(windows)]
    let invalid = {
        use std::os::windows::ffi::OsStringExt as _;
        OsString::from_wide(&[0xd800])
    };
    #[cfg(unix)]
    let invalid = {
        use std::os::unix::ffi::OsStringExt as _;
        OsString::from_vec(vec![0xff])
    };
    let mut fixture = metadata_fixture();
    {
        let mut script = fixture.script(&fixture.files);
        let _previous = script.environment.insert("OPAQUE_INPUT".into(), invalid.clone());
        snapshot_io::with(script, || {
            assert!(seal_context(&fixture.work, &fixture.plan, &fixture.args, &fixture.stdout).is_none());
        });
    }
    let path = fixture.work.rustc_captures().unwrap().join("proof-context");
    let bytes = snapshot_io::with(fixture.script(&fixture.files), || {
        seal(&fixture.work, &fixture.plan, &fixture.args, &fixture.stdout);
        assert!(super::identity(&fixture.work, &fixture.plan).is_some());
        snapshot_io::read(&path).unwrap()
    });
    let _previous = fixture.files.insert(path, bytes);
    let mut script = fixture.script(&fixture.files);
    let _previous = script.environment.insert("OPAQUE_INPUT".into(), invalid);
    snapshot_io::with(script, || {
        assert_eq!(super::identity(&fixture.work, &fixture.plan), None);
    });
}

#[test]
fn native_metadata_identity_declines_newly_unreadable_roots_and_controls() {
    use super::super::rustc_replay::snapshot_io;

    let mut fixture = metadata_fixture();
    let path = fixture.work.rustc_captures().unwrap().join("proof-context");
    let bytes = snapshot_io::with(fixture.script(&fixture.files), || {
        seal(&fixture.work, &fixture.plan, &fixture.args, &fixture.stdout);
        assert!(super::identity(&fixture.work, &fixture.plan).is_some());
        snapshot_io::read(&path).unwrap()
    });
    let _previous = fixture.files.insert(path, bytes);
    for boundary in ["tree", "control"] {
        let mut script = fixture.script(&fixture.files);
        if boundary == "tree" {
            let _previous = script.directories.insert(
                fixture.work.root.clone().into_std_path_buf(),
                Err(std::io::ErrorKind::PermissionDenied),
            );
        } else {
            let _previous = script.files.insert(
                drive().join("home").join(".cargo").join("config.toml").into_std_path_buf(),
                Err(std::io::ErrorKind::PermissionDenied),
            );
        }
        snapshot_io::with(script, || {
            assert_eq!(super::identity(&fixture.work, &fixture.plan), None, "{boundary}");
        });
    }
}

#[test]
fn native_metadata_identity_tracks_the_mutable_set_without_freezing_mutated_source_bytes() {
    use super::super::rustc_replay::snapshot_io;

    let mut fixture = metadata_fixture();
    let first = crate::discover::TargetFile {
        path: "lib.rs".into(),
        absolute: fixture.work.root.join("lib.rs"),
        package: "subject".into(),
        source: None,
    };
    let second = crate::discover::TargetFile {
        path: "a.rs".into(),
        absolute: fixture.work.root.join("a.rs"),
        package: "subject".into(),
        source: None,
    };
    let _previous = fixture.files.insert(second.absolute.clone(), b"pub fn extra() {}".to_vec());
    fixture.plan.files = vec![first.clone(), second.clone(), first.clone()];
    snapshot_io::with(fixture.script(&fixture.files), || {
        let path = fixture.directory.join("unit.json");
        let mut record: serde_json::Value = serde_json::from_slice(&snapshot_io::read(&path).unwrap()).unwrap();
        record["inventory"] = serde_json::to_value(snapshot_io::inventory(&fixture.work.root, &fixture.work.target).unwrap()).unwrap();
        snapshot_io::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
        seal(&fixture.work, &fixture.plan, &fixture.args, &fixture.stdout);
        let baseline = super::identity(&fixture.work, &fixture.plan).unwrap();
        let context_path = fixture.work.rustc_captures().unwrap().join("proof-context");
        let sealed = snapshot_io::read(&context_path).unwrap();
        let mut malformed: Context = serde_json::from_slice(&sealed).unwrap();
        malformed.mutable.clear();
        snapshot_io::write(&context_path, serde_json::to_vec(&malformed).unwrap()).unwrap();
        assert_eq!(super::identity(&fixture.work, &fixture.plan), None);
        snapshot_io::write(&context_path, sealed).unwrap();
        snapshot_io::write(fixture.work.root.join("lib.rs"), b"pub fn value() -> u32 { 99 }").unwrap();
        assert_eq!(super::identity(&fixture.work, &fixture.plan), Some(baseline));
        fixture.plan.files = vec![second, first];
        assert_eq!(super::identity(&fixture.work, &fixture.plan), Some(baseline));
        fixture.plan.files.clear();
        assert_eq!(super::identity(&fixture.work, &fixture.plan), None);
    });
}

#[test]
fn native_metadata_context_preserves_the_runtime_binding_and_requires_its_directory_snapshot() {
    use super::super::rustc_replay::snapshot_io;

    let mut fixture = metadata_fixture();
    fixture.add_runtime();
    snapshot_io::with(fixture.script(&fixture.files), || {
        let context = seal_context(&fixture.work, &fixture.plan, &fixture.args, &fixture.stdout).unwrap();
        assert_eq!(
            context.runtime.as_ref().map(|(root, _)| root.as_path()),
            Some(fixture.work.compiler_runtime())
        );
        seal(&fixture.work, &fixture.plan, &fixture.args, &fixture.stdout);
        assert!(super::identity(&fixture.work, &fixture.plan).is_some());
        snapshot_io::write(fixture.work.compiler_runtime().join("new.rs"), b"pub fn new() {}").unwrap();
        assert_eq!(super::identity(&fixture.work, &fixture.plan), None);
    });
    let mut script = fixture.script(&fixture.files);
    let _previous = script.directories.insert(
        fixture.work.compiler_runtime().to_owned().into_std_path_buf(),
        Err(std::io::ErrorKind::PermissionDenied),
    );
    snapshot_io::with(script, || {
        assert!(seal_context(&fixture.work, &fixture.plan, &fixture.args, &fixture.stdout).is_none());
    });
}

#[test]
fn native_metadata_identity_declines_runtime_inputs_that_look_like_misplaced_compilers() {
    use super::super::rustc_replay::snapshot_io;

    for (directory, name, reusable) in [("src", "helper.rs", true), ("bin", "rustc.rs", true), ("src", "rustc.rs", false)] {
        let mut fixture = metadata_fixture();
        let source = fixture.work.compiler_runtime().join(directory).join(name);
        let _previous = fixture.files.insert(source.clone(), b"pub fn helper() {}".to_vec());
        let _previous = fixture.files.insert(
            fixture.work.compiler_runtime().join("lib").join("resource.txt"),
            b"immutable runtime resource".to_vec(),
        );
        fixture.add_runtime();
        snapshot_io::with(fixture.script(&fixture.files), || {
            let context = seal_context(&fixture.work, &fixture.plan, &fixture.args, &fixture.stdout).unwrap();
            assert!(
                context.toolchain.iter().any(|(path, _)| path == &source),
                "the ambiguous input must come from an admitted runtime capture"
            );
            seal(&fixture.work, &fixture.plan, &fixture.args, &fixture.stdout);
            assert_eq!(
                super::identity(&fixture.work, &fixture.plan).is_some(),
                reusable,
                "{directory}/{name}"
            );
        });
    }
}

#[test]
fn native_metadata_sealing_declines_a_runtime_inventory_lost_after_provenance_validation() {
    use super::super::rustc_replay::snapshot_io;

    let mut fixture = metadata_fixture();
    fixture.add_runtime();
    snapshot_io::with(fixture.script(&fixture.files), || {
        let _context = seal_context(&fixture.work, &fixture.plan, &fixture.args, &fixture.stdout).unwrap();
    });
    for (successful_reads, reusable) in [(3, false), (4, true)] {
        let mut script = fixture.script(&fixture.files);
        let mut reads = vec![Ok(()); successful_reads];
        reads.push(Err(std::io::ErrorKind::PermissionDenied));
        let _previous = script
            .directory_reads
            .insert(fixture.work.compiler_runtime().to_owned().into_std_path_buf(), reads.into());
        snapshot_io::with(script, || {
            assert_eq!(
                seal_context(&fixture.work, &fixture.plan, &fixture.args, &fixture.stdout).is_some(),
                reusable,
                "runtime inventory after {successful_reads} successful reads"
            );
        });
    }
}

#[test]
fn native_metadata_sealing_checks_the_selected_command_after_a_complete_capture() {
    use super::super::rustc_replay::snapshot_io;

    for boundary in ["verb", "source", "tree", "cargo-missing", "cargo-unreadable"] {
        let mut fixture = metadata_fixture();
        if boundary == "verb" {
            fixture.args[0] = "build".into();
        }
        let mut script = fixture.script(&fixture.files);
        match boundary {
            "tree" => {
                let _previous = script.directories.insert(
                    fixture.work.root.clone().into_std_path_buf(),
                    Err(std::io::ErrorKind::PermissionDenied),
                );
            }
            "cargo-missing" | "cargo-unreadable" => {
                let kind = if boundary == "cargo-missing" {
                    std::io::ErrorKind::NotFound
                } else {
                    std::io::ErrorKind::PermissionDenied
                };
                let _previous = script.files.insert(fixture.cargo.clone().into_std_path_buf(), Err(kind));
            }
            _ => {}
        }
        snapshot_io::with(script, || {
            fixture.populate_current_generation();
            if boundary == "source" {
                let source = fixture.work.root.join("lib.rs");
                snapshot_io::write(&source, b"pub fn value() -> u32 { include!(\"hidden.rs\") }").unwrap();
                let mut record: serde_json::Value =
                    serde_json::from_slice(&snapshot_io::read(fixture.directory.join("unit.json")).unwrap()).unwrap();
                for input in record["inputs"].as_array_mut().unwrap() {
                    if input[0] == source.as_str() {
                        input[1] = serde_json::to_value(NativeSnapshot.content(&source).unwrap().unwrap()).unwrap();
                    }
                }
                snapshot_io::write(fixture.directory.join("unit.json"), serde_json::to_vec(&record).unwrap()).unwrap();
            }
            assert!(
                seal_context(&fixture.work, &fixture.plan, &fixture.args, &fixture.stdout).is_none(),
                "{boundary}"
            );
        });
    }
}

#[test]
fn native_metadata_identity_rejects_malformed_context_and_shape_changes_outside_the_immutable_list() {
    use super::super::rustc_replay::snapshot_io;

    let fixture = metadata_fixture();
    snapshot_io::with(fixture.script(&fixture.files), || {
        let path = fixture.work.rustc_captures().unwrap().join("proof-context");
        snapshot_io::write(&path, b"not JSON").unwrap();
        assert_eq!(super::identity(&fixture.work, &fixture.plan), None);
        seal(&fixture.work, &fixture.plan, &fixture.args, &fixture.stdout);
        let bytes = snapshot_io::read(&path).unwrap();
        let mut context: Context = serde_json::from_slice(&bytes).unwrap();
        let original = super::identity(&fixture.work, &fixture.plan).unwrap();
        context.shape.push(fixture.work.root.join("missing.rs"));
        snapshot_io::write(&path, serde_json::to_vec(&context).unwrap()).unwrap();
        assert_eq!(super::identity(&fixture.work, &fixture.plan), None);
        snapshot_io::write(&path, &bytes).unwrap();
        assert_eq!(super::identity(&fixture.work, &fixture.plan), Some(original));
        let mut context: Context = serde_json::from_slice(&bytes).unwrap();
        let root = drive().join("unowned-runtime");
        snapshot_io::write(root.join("lib.rs"), b"pub fn active() -> bool { false }").unwrap();
        context.runtime = Some((root.clone(), tree(&root, &fixture.work.target).unwrap()));
        snapshot_io::write(&path, serde_json::to_vec(&context).unwrap()).unwrap();
        assert_eq!(super::identity(&fixture.work, &fixture.plan), None);
    });
}

#[test]
fn context_requires_the_owned_cargo_stamp_even_when_it_is_not_a_rustc_toolchain_input() {
    let (mut context, mut snapshot, root, target) = fixture();
    context.cargo.0 = drive().join("cargo-home").join("cargo.exe");
    let _previous = snapshot.0.insert(context.cargo.0.clone(), "Cargo identity".into());
    assert!(identity(&context, &snapshot, &root, &target).is_some());
    let _previous = snapshot.0.insert(context.cargo.0.clone(), "changed Cargo executable".into());
    assert_eq!(identity(&context, &snapshot, &root, &target), None);
}

#[test]
fn source_controls_reject_each_non_unicode_home() {
    let root = drive().join("workspace");
    #[cfg(windows)]
    let invalid = {
        use std::os::windows::ffi::OsStringExt as _;
        OsString::from_wide(&[0xd800])
    };
    #[cfg(unix)]
    let invalid = {
        use std::os::unix::ffi::OsStringExt as _;
        OsString::from_vec(vec![0xff])
    };
    for boundary in ["USERPROFILE", "CARGO_HOME", "RUSTUP_HOME"] {
        let paths = controls_with(
            &root,
            |name| {
                if name == boundary {
                    Some(invalid.clone())
                } else if name == "USERPROFILE" {
                    Some(drive().join("home").into_std_path_buf().into_os_string())
                } else {
                    None
                }
            },
            |_| true,
        );
        assert_eq!(paths, None, "{boundary}");
    }
}

#[test]
fn context_rejects_missing_or_changed_immutable_inputs_and_toolchain_reads() {
    struct Unreadable<'a> {
        snapshot: &'a MemorySnapshot,
        path: &'a Utf8Path,
        missing: bool,
    }

    impl Snapshot for Unreadable<'_> {
        fn content(&self, path: &Utf8Path) -> std::io::Result<Option<[u8; 32]>> {
            if path == self.path {
                if self.missing {
                    Ok(None)
                } else {
                    Err(std::io::Error::other("scripted unreadable compiler input"))
                }
            } else {
                self.snapshot.content(path)
            }
        }
        fn text(&self, path: &Utf8Path) -> Option<String> {
            self.snapshot.text(path)
        }
        fn tree(&self, root: &Utf8Path, target: &Utf8Path) -> Option<Vec<Utf8PathBuf>> {
            self.snapshot.tree(root, target)
        }
    }
    let (context, snapshot, root, target) = fixture();
    assert!(identity(&context, &snapshot, &root, &target).is_some());
    for path in context
        .immutable
        .iter()
        .map(|(path, _)| path)
        .chain(context.toolchain.iter().map(|(path, _)| path))
    {
        let (_, mut changed, _, _) = fixture();
        let _removed = changed.0.remove(path);
        assert!(identity(&context, &changed, &root, &target).is_none(), "missing {path}");
        let (_, mut changed, _, _) = fixture();
        let _previous = changed.0.insert(path.clone(), "different input".into());
        assert!(identity(&context, &changed, &root, &target).is_none(), "changed {path}");
    }
    for path in context
        .immutable
        .iter()
        .map(|(path, _)| path)
        .chain([&context.cargo.0, &context.toolchain[0].0, &context.toolchain[2].0])
    {
        for missing in [false, true] {
            let unreadable = Unreadable {
                snapshot: &snapshot,
                path,
                missing,
            };
            assert!(
                context_identity(
                    &context,
                    context.command,
                    &context.options,
                    &context.shape,
                    &context.immutable,
                    &target,
                    &unreadable,
                )
                .is_none(),
                "{path}, missing={missing}"
            );
        }
    }
}

#[test]
fn live_immutable_content_changes_cannot_reuse_an_identity_from_the_earlier_snapshot() {
    let (context, mut snapshot, root, target) = fixture();
    let original = identity(&context, &snapshot, &root, &target).unwrap();
    let manifest = root.join("Cargo.toml");
    let changed = snapshot.0[&manifest].replace("version='0.1.0'", "version='0.2.0'");
    let _previous = snapshot.0.insert(manifest, changed);
    let current = context_identity(
        &context,
        context.command,
        &context.options,
        &context.shape,
        &context.immutable,
        &target,
        &snapshot,
    );
    assert_eq!(current, None, "a live change cannot reuse the sealed immutable stamp {original:?}");
}

#[test]
fn proof_identity_declines_unreadable_toolchain_and_runtime_directory_inventories() {
    struct UnreadableTree<'a> {
        snapshot: &'a MemorySnapshot,
        failed: &'a Utf8Path,
    }
    impl Snapshot for UnreadableTree<'_> {
        fn content(&self, path: &Utf8Path) -> std::io::Result<Option<[u8; 32]>> {
            self.snapshot.content(path)
        }

        fn text(&self, path: &Utf8Path) -> Option<String> {
            self.snapshot.text(path)
        }

        fn tree(&self, root: &Utf8Path, target: &Utf8Path) -> Option<Vec<Utf8PathBuf>> {
            if root == self.failed {
                None
            } else {
                self.snapshot.tree(root, target)
            }
        }
    }
    let (mut context, snapshot, root, target) = fixture();
    let runtime = drive().join("runtime");
    context.runtime = Some((runtime.clone(), Vec::new()));
    assert!(identity(&context, &snapshot, &root, &target).is_some());
    for failed in [
        context.toolchain[0].0.parent().unwrap().to_owned(),
        drive().join("toolchain").join("lib"),
        runtime,
    ] {
        let unreadable = UnreadableTree {
            snapshot: &snapshot,
            failed: &failed,
        };
        assert_eq!(
            context_identity(
                &context,
                context.command,
                &context.options,
                &context.shape,
                &context.immutable,
                &target,
                &unreadable,
            ),
            None,
            "{failed}"
        );
    }
}

#[test]
fn context_identity_includes_units_arguments_and_runtime_inventory() {
    let (mut context, mut snapshot, root, target) = fixture();
    let original = identity(&context, &snapshot, &root, &target).unwrap();
    context.args.push("--features=other".into());
    assert_ne!(identity(&context, &snapshot, &root, &target), Some(original));
    let (mut context, _, _, _) = fixture();
    context.units.push([9; 32]);
    assert_ne!(identity(&context, &snapshot, &root, &target), Some(original));
    let runtime = drive().join("runtime");
    let file = runtime.join("src").join("lib.rs");
    let _previous = snapshot.0.insert(file.clone(), "pub fn active() -> bool { false }".into());
    context.runtime = Some((runtime.clone(), vec![file]));
    assert!(identity(&context, &snapshot, &root, &target).is_some());
    let _previous = snapshot.0.insert(runtime.join("new.rs"), "pub fn helper() {}".into());
    assert!(identity(&context, &snapshot, &root, &target).is_none());
}

#[test]
fn workspace_identity_forwards_the_owned_plan_and_preserves_unavailable_contexts() {
    let root = drive().join("owned-context");
    let work = Workspace::adopt(root.clone(), root.join("target"));
    let plan = Plan {
        root: root.clone(),
        files: Vec::new(),
        mutants: Vec::new(),
        suppressed: 0,
        idle: Vec::new(),
        sharded_out: 0,
        settled_out: 0,
        digests: crate::HashMap::default(),
        skipped: Vec::new(),
        reach: crate::HashMap::default(),
        specs: crate::HashMap::default(),
    };
    for expected in [Some([7; 32]), None] {
        let expected_root = root.clone();
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let observed = std::rc::Rc::clone(&calls);
        with_operations(
            move |operation| {
                let Operation::Identity(actual) = operation else {
                    panic!("identity lookup cannot publish or invalidate captures");
                };
                assert_eq!(actual.root, expected_root);
                observed.set(observed.get() + 1);
                expected
            },
            || assert_eq!(work.proof_context_identity(&plan), expected),
        );
        assert_eq!(calls.get(), 1);
    }
}

#[test]
fn source_closure_requires_a_manifest_and_rejects_hidden_expansions_and_native_builds() {
    let (context, mut snapshot, root, target) = fixture();
    assert!(closed_sources(&context.shape, &snapshot, None).is_some());
    let _removed = snapshot.0.remove(&root.join("Cargo.toml"));
    let shape = snapshot.tree(&root, &target).unwrap();
    assert!(closed_sources(&shape, &snapshot, None).is_none());
    for source in [
        "extern crate unknown;",
        "macro_rules! hidden { () => { extern crate plugin; }; }",
        "#![feature(compiler_private)] pub fn value() {}",
        "fn value() { asm!(\"nop\"); }",
        "global_asm!(\"nop\");",
        "fn value() { option_env!(\"SETTING\"); }",
        "mod outer { mod inner { include!(\"other.rs\"); } }",
        "this is not valid Rust {",
    ] {
        let (_, mut snapshot, _, _) = fixture();
        let _previous = snapshot.0.insert(root.join("lib.rs"), source.into());
        assert!(closed_sources(&context.shape, &snapshot, None).is_none(), "{source}");
    }
    for file in ["build.rs", "nested/build.rs"] {
        let (_, mut snapshot, _, _) = fixture();
        let _previous = snapshot.0.insert(root.join(file), "fn main() {}".into());
        assert!(
            closed_sources(&snapshot.tree(&root, &target).unwrap(), &snapshot, None).is_none(),
            "{file}"
        );
    }
    for manifest in [
        "[package]\nname='subject'\nbuild=true\n",
        "[lib]\nproc-macro=true\n",
        "[target.'cfg(windows)'.build-dependencies]\nother='1'\n",
        "[dev-dependencies]\ngamma_rt='1'\n",
        "[dependencies]\ngamma_rt={path='elsewhere',package='cargo-gamma-rt'}\n",
        "[dependencies]\nother={workspace=true}\n",
    ] {
        assert!(
            !source_only_manifest(&toml::from_str::<toml::Value>(manifest).unwrap(), None),
            "{manifest}"
        );
    }
}
