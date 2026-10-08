// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Supplies the real public macro library to dependency-free offline consumers.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::{env, fs};

use cargo_metadata::{Message, TargetKind};

fn macro_artifact() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();

    BUILT
        .get_or_init(|| {
            let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("the gamma test crates are workspace siblings")
                .join("cargo-gamma-attrs")
                .join("Cargo.toml");
            let built = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
                .current_dir(manifest.parent().expect("the constructed macro manifest has a parent directory"))
                .args([
                    "build",
                    "--offline",
                    "--locked",
                    "--lib",
                    "--message-format=json",
                    "--manifest-path",
                ])
                .arg(&manifest)
                .output()
                .expect("Cargo must be runnable to locate the real macro library");
            assert!(
                built.status.success(),
                "the macro library must be available before checking consumers:\n{}",
                String::from_utf8_lossy(&built.stderr)
            );

            Message::parse_stream(built.stdout.as_slice())
                .find_map(|message| match message.expect("Cargo artifact messages must be readable") {
                    Message::CompilerArtifact(artifact)
                        if artifact.target.name == "gamma" && artifact.target.kind.contains(&TargetKind::ProcMacro) =>
                    {
                        artifact
                            .filenames
                            .into_iter()
                            .find(|path| path.as_str().ends_with(env::consts::DLL_SUFFIX))
                            .map(PathBuf::from)
                    }
                    _ => None,
                })
                .expect("Cargo must report the compiled gamma proc-macro library")
        })
        .as_path()
}

/// Copies Cargo's selected public macro library into the owned consumer directory.
pub fn copy_gamma_macro(root: &Path) -> PathBuf {
    let artifact = macro_artifact();
    let local = root.join(artifact.file_name().expect("Cargo reports a file name for its proc-macro artifact"));
    fs::copy(artifact, &local).expect("the real macro library must be copyable into the consumer");
    local
}
