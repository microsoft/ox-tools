// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))] // These tests execute Cargo, rustc and git.
#![expect(clippy::unwrap_used, reason = "integration tests favor concise assertions over Result plumbing")]

//! Real Cargo packaging against a disposable, read-only loopback registry.
//! Nothing is published: the only registry archive is seeded with `cargo package`.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use std::{env, fs};

use sha2::{Digest, Sha256};
use tempfile::TempDir;
use toml_edit::DocumentMut;

const A: &str = "anvil-packaging-a";
const B: &str = "anvil-packaging-b";
const GROUP: &[&str] = &["package", "-p", A, "-p", B, "--all-features", "--allow-dirty"];

type Routes = Arc<Mutex<HashMap<String, Vec<u8>>>>;

struct Registry {
    address: std::net::SocketAddr,
    routes: Routes,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Registry {
    fn new() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let routes = Arc::new(Mutex::new(HashMap::<String, Vec<u8>>::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let routes = Arc::clone(&routes);
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream.set_nonblocking(false).unwrap();
                            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                            let mut reader = BufReader::new(&stream);
                            let mut line = String::new();
                            reader.read_line(&mut line).unwrap();
                            if line.is_empty() {
                                continue;
                            }
                            requests.lock().unwrap().push(line.trim().to_owned());
                            let mut parts = line.split_whitespace();
                            let method = parts.next().unwrap();
                            let path = parts.next().unwrap();
                            let body = routes.lock().unwrap().get(path).cloned();
                            let mut header = String::new();
                            loop {
                                header.clear();
                                if reader.read_line(&mut header).unwrap() == 0 || header == "\r\n" {
                                    break;
                                }
                            }
                            let (status, body) = match (method, body) {
                                ("GET", Some(body)) => ("200 OK", body),
                                ("GET", None) => ("404 Not Found", Vec::new()),
                                _ => ("405 Method Not Allowed", Vec::new()),
                            };
                            write!(
                                stream,
                                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            )
                            .unwrap();
                            stream.write_all(&body).unwrap();
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => {
                            assert!(stop.load(Ordering::Relaxed), "registry accept failed: {error}");
                        }
                    }
                }
            })
        };
        let registry = Self {
            address,
            routes,
            requests,
            stop,
            worker: Some(worker),
        };
        let config = format!(
            r#"{{"dl":"http://{address}/crates/{{crate}}/{{version}}/download","api":"http://{address}/api","auth-required":false}}"#
        );
        for index in ["index", "other-index"] {
            registry.put(&format!("/{index}/config.json"), config.as_bytes().to_vec());
        }
        registry
    }

    fn put(&self, path: &str, body: Vec<u8>) {
        self.routes.lock().unwrap().insert(path.to_owned(), body);
    }

    fn seed(&self, archive: Vec<u8>) {
        let mut checksum = String::with_capacity(64);
        for byte in Sha256::digest(&archive) {
            write!(checksum, "{byte:02x}").unwrap();
        }
        let entry = format!(r#"{{"name":"{A}","vers":"0.1.0","deps":[],"cksum":"{checksum}","features":{{}},"yanked":false}}"#);
        for index in ["index", "other-index"] {
            self.put(&format!("/{index}/an/vi/{A}"), format!("{entry}\n").into_bytes());
        }
        self.put(&format!("/crates/{A}/0.1.0/download"), archive);
    }

    fn assert_read_only(&self) {
        for request in self.requests.lock().unwrap().iter() {
            assert!(request.starts_with("GET "), "unexpected registry write: {request}");
            assert!(!request.contains("/api"), "unexpected registry API request: {request}");
        }
        assert_eq!(
            self.routes
                .lock()
                .unwrap()
                .keys()
                .filter(|path| path.starts_with("/crates/"))
                .count(),
            1,
            "only the original registry A 0.1.0 archive may exist"
        );
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let result = self.worker.take().unwrap().join();
        if !thread::panicking() {
            result.unwrap();
        }
    }
}

struct Fixture {
    directory: TempDir,
    registry: Registry,
}

impl Fixture {
    fn new() -> Self {
        // Keep scratch files inside the project, rather than using the system temp directory.
        let scratch = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("release-packaging-fixtures");
        fs::create_dir_all(&scratch).unwrap();
        let directory = tempfile::Builder::new().prefix("release-packaging-").tempdir_in(scratch).unwrap();
        let fixture = Self {
            directory,
            registry: Registry::new(),
        };
        let mut config = DocumentMut::new();
        let index = format!("sparse+http://{}/index/", fixture.registry.address);
        config["registries"]["fixture"]["index"] = toml_edit::value(&index);
        config["registries"]["other"]["index"] = toml_edit::value(format!("sparse+http://{}/other-index/", fixture.registry.address));
        // Any accidental crates.io dependency must still stay on loopback.
        config["source"]["crates-io"]["replace-with"] = toml_edit::value("fixture-source");
        config["source"]["fixture-source"]["registry"] = toml_edit::value(index);
        config["http"]["proxy"] = toml_edit::value("");
        config["net"]["retry"] = toml_edit::value(0);
        write(&fixture.root().join("cargo-home").join("config.toml"), &config.to_string());
        write(
            &fixture.root().join("seed").join("Cargo.toml"),
            &format!(
                "[package]\nname = \"{A}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
                 description = \"Disposable packaging fixture\"\nlicense = \"MIT\"\n[workspace]\n"
            ),
        );
        write(
            &fixture.root().join("seed").join("src").join("lib.rs"),
            "pub fn old_api() -> u32 { 1 }\n",
        );
        fixture.success("seed", &["package", "--no-verify", "--allow-dirty", "--registry", "fixture"]);
        fixture
            .registry
            .seed(fs::read(fixture.target("seed").join("package").join(format!("{A}-0.1.0.crate"))).unwrap());
        fixture.workspace("0.1.0", "[\"fixture\"]");
        fixture
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn target(&self, project: &str) -> PathBuf {
        self.root().join(format!("{project}-target"))
    }

    fn workspace(&self, a_version: &str, publish: &str) {
        let workspace = self.root().join("workspace");
        write(
            &workspace.join("Cargo.toml"),
            &format!(
                "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n\
                 [workspace.package]\npublish = {publish}\n"
            ),
        );
        write(
            &workspace.join("a").join("Cargo.toml"),
            &format!(
                "[package]\nname = \"{A}\"\nversion = \"{a_version}\"\nedition = \"2021\"\n\
                 description = \"Disposable packaging fixture\"\nlicense = \"MIT\"\npublish.workspace = true\n"
            ),
        );
        write(
            &workspace.join("a").join("src").join("lib.rs"),
            "pub fn old_api() -> u32 { 1 }\npub fn new_api() -> u32 { 2 }\n",
        );
        write(
            &workspace.join("b").join("Cargo.toml"),
            &format!(
                "[package]\nname = \"{B}\"\nversion = \"0.2.0\"\nedition = \"2021\"\n\
                 description = \"Disposable packaging fixture\"\nlicense = \"MIT\"\npublish.workspace = true\n\
                 [features]\nrelease-api = []\n[dependencies]\n\
                 {A} = {{ path = \"../a\", version = \"{a_version}\", registry = \"fixture\" }}\n"
            ),
        );
        write(
            &workspace.join("b").join("src").join("lib.rs"),
            "#[cfg(feature = \"release-api\")]\npub fn use_new_api() -> u32 { anvil_packaging_a::new_api() }\n",
        );
    }

    fn cargo(&self, project: &str, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO"));
        for (key, _) in env::vars_os() {
            let key_text = key.to_string_lossy();
            if key_text.starts_with("CARGO_")
                || matches!(
                    key_text.as_ref(),
                    "RUSTFLAGS" | "RUSTDOCFLAGS" | "RUSTC_WRAPPER" | "RUSTC_WORKSPACE_WRAPPER" | "RUSTC" | "RUSTDOC"
                )
            {
                command.env_remove(key);
            }
        }
        command
            .args(args)
            .current_dir(self.root().join(project))
            .env("CARGO_HOME", self.root().join("cargo-home"))
            .env("CARGO_TARGET_DIR", self.target(project))
            .env("CARGO_TERM_COLOR", "never")
            .env("CARGO_INCREMENTAL", "0")
            .env("CARGO_NET_OFFLINE", "false")
            .env("CARGO_HTTP_PROXY", "")
            .output()
            .unwrap()
    }

    fn success(&self, project: &str, args: &[&str]) -> String {
        let output = self.cargo(project, args);
        let text = output_text(&output);
        assert!(output.status.success(), "cargo {args:?} failed:\n{text}");
        text
    }

    fn failure(&self, args: &[&str], expected: &str) -> String {
        let output = self.cargo("workspace", args);
        let text = output_text(&output);
        assert!(!output.status.success(), "cargo {args:?} unexpectedly succeeded:\n{text}");
        assert!(text.contains(expected), "cargo {args:?} did not report {expected:?}:\n{text}");
        text
    }

    fn lockfile(&self) -> Option<Vec<u8>> {
        let path = self.root().join("workspace").join("Cargo.lock");
        path.exists().then(|| fs::read(path).unwrap())
    }

    fn assert_packed_candidates(&self, output: &str) {
        for package in [A, B] {
            assert!(output.contains(&format!("Verifying {package} v0.2.0")), "{output}");
            assert!(
                self.target("workspace")
                    .join("package")
                    .join(format!("{package}-0.2.0.crate"))
                    .is_file(),
                "missing packed candidate {package}"
            );
        }
        let packed = self.target("workspace").join("package").join(format!("{B}-0.2.0"));
        let manifest = fs::read_to_string(packed.join("Cargo.toml"))
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert!(
            manifest["dependencies"][A].get("path").is_none(),
            "packed B must not retain a local path"
        );
        assert_eq!(manifest["dependencies"][A]["version"].as_str(), Some("0.2.0"));
        self.registry.assert_read_only();
    }
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn tools_available() -> bool {
    for tool in [env!("CARGO"), "rustc", "git"] {
        let output = Command::new(tool).arg("--version").output();
        if !output.as_ref().is_ok_and(|output| output.status.success()) {
            eprintln!("skipping: required tool '{tool}' not available");
            return false;
        }
        if tool == env!("CARGO") {
            eprintln!("fixture uses {}", String::from_utf8_lossy(&output.unwrap().stdout).trim());
        }
    }
    true
}

#[test]
fn unselected_same_version_dependency_uses_registry_source_not_workspace_source() {
    if !tools_available() {
        return;
    }
    let fixture = Fixture::new();
    fixture.success("workspace", &["build", "--workspace", "--all-features"]);
    let lock = fixture.lockfile().unwrap();

    // Without all features B never references the API missing from registry A.
    fixture.success("workspace", &["package", "-p", B, "--allow-dirty"]);
    fixture.failure(
        &["package", "-p", B, "--all-features", "--allow-dirty"],
        "cannot find function `new_api`",
    );
    fixture.failure(
        &["package", "-p", B, "--all-features", "--allow-dirty", "--locked"],
        "cannot find function `new_api`",
    );
    assert_eq!(fixture.lockfile().unwrap(), lock, "package must not rewrite the workspace lockfile");
    assert!(
        fixture
            .registry
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.contains(&format!("/crates/{A}/0.1.0/download"))),
        "the failing verification must actually fetch registry A"
    );
    fixture.registry.assert_read_only();

    fixture.workspace("0.2.0", "[\"fixture\"]");
    let mut locked_args = GROUP.to_vec();
    locked_args.push("--locked");
    fixture.failure(&locked_args, "--locked");
    assert_eq!(
        fixture.lockfile().unwrap(),
        lock,
        "locked packaging must preserve a stale workspace lockfile"
    );
    // Registry A 0.2.0 does not exist: success requires the grouped packed candidate.
    let output = fixture.success("workspace", GROUP);
    fixture.assert_packed_candidates(&output);
    let updated_lock = fixture.lockfile().unwrap();
    assert_ne!(updated_lock, lock, "unlocked packaging refreshes a stale workspace lockfile");
    let document = String::from_utf8(updated_lock).unwrap().parse::<DocumentMut>().unwrap();
    let a = document["package"]
        .as_array_of_tables()
        .unwrap()
        .iter()
        .find(|package| package["name"].as_str() == Some(A))
        .unwrap();
    assert_eq!(a["version"].as_str(), Some("0.2.0"));
}

#[test]
fn grouped_packaging_locked_preserves_an_existing_valid_workspace_lockfile() {
    if !tools_available() {
        return;
    }
    let fixture = Fixture::new();
    fixture.workspace("0.2.0", "[\"fixture\"]");
    fixture.success("workspace", &["generate-lockfile"]);
    let lock = fixture.lockfile().unwrap();
    let mut args = GROUP.to_vec();
    args.push("--locked");
    let output = fixture.success("workspace", &args);
    fixture.assert_packed_candidates(&output);
    assert_eq!(
        fixture.lockfile().unwrap(),
        lock,
        "package --locked must not rewrite the workspace lockfile"
    );
    fixture.success("workspace", GROUP);
    assert_eq!(
        fixture.lockfile().unwrap(),
        lock,
        "unlocked packaging preserves a valid workspace lockfile"
    );
}

#[test]
fn grouped_packaging_locked_without_a_workspace_lockfile() {
    if !tools_available() {
        return;
    }
    let fixture = Fixture::new();
    fixture.workspace("0.2.0", "[\"fixture\"]");
    assert!(fixture.lockfile().is_none());
    let mut args = GROUP.to_vec();
    args.push("--locked");
    let output = fixture.success("workspace", &args);
    fixture.assert_packed_candidates(&output);
    assert!(
        fixture.lockfile().is_none(),
        "package --locked must not create a workspace lockfile"
    );
    fixture.success("workspace", GROUP);
    assert!(
        fixture.lockfile().is_none(),
        "unlocked packaging also leaves a missing workspace lockfile absent"
    );
}

#[test]
fn ambiguous_publish_allowlist_requires_an_explicit_registry() {
    if !tools_available() {
        return;
    }
    let fixture = Fixture::new();
    fixture.workspace("0.2.0", "[\"fixture\", \"other\"]");
    fixture.failure(GROUP, "--registry");
    let mut args = GROUP.to_vec();
    args.extend(["--registry", "fixture"]);
    let output = fixture.success("workspace", &args);
    fixture.assert_packed_candidates(&output);
}

#[test]
fn conflicting_publish_allowlists_are_not_silently_grouped() {
    if !tools_available() {
        return;
    }
    let fixture = Fixture::new();
    fixture.workspace("0.2.0", "[\"fixture\"]");
    let manifest = fixture.root().join("workspace").join("b").join("Cargo.toml");
    let mut document = fs::read_to_string(&manifest).unwrap().parse::<DocumentMut>().unwrap();
    document["package"]["publish"] = toml_edit::value({
        let mut array = toml_edit::Array::new();
        array.push("other");
        array
    });
    write(&manifest, &document.to_string());
    fixture.failure(GROUP, "conflicts between `package.publish`");
    let mut args = GROUP.to_vec();
    args.extend(["--registry", "fixture"]);
    fixture.failure(&args, "publish");
    fixture.registry.assert_read_only();
}

#[test]
fn dirty_workspace_requires_allow_dirty_but_still_verifies_the_packed_sources() {
    if !tools_available() {
        return;
    }
    let fixture = Fixture::new();
    fixture.workspace("0.2.0", "[\"fixture\"]");
    let workspace = fixture.root().join("workspace");
    write(&workspace.join(".gitignore"), "Cargo.lock\n");
    for args in [
        vec!["init", "--quiet"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Packaging Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    ] {
        let mut command = Command::new("git");
        for (key, _) in env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                command.env_remove(key);
            }
        }
        let output = command
            .args(["-c", "core.hooksPath=", "-c", "init.templateDir="])
            .args(&args)
            .env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .current_dir(&workspace)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?} failed:\n{}", output_text(&output));
    }
    write(
        &workspace.join("b").join("src").join("lib.rs"),
        "#[cfg(feature = \"release-api\")]\npub fn use_new_api() -> u32 { anvil_packaging_a::new_api() + 1 }\n",
    );
    fixture.failure(&["package", "-p", A, "-p", B, "--all-features"], "uncommitted");
    let output = fixture.success("workspace", GROUP);
    fixture.assert_packed_candidates(&output);
    assert!(
        fs::read_to_string(
            fixture
                .target("workspace")
                .join("package")
                .join(format!("{B}-0.2.0"))
                .join("src")
                .join("lib.rs")
        )
        .unwrap()
        .contains("new_api() + 1")
    );
}
