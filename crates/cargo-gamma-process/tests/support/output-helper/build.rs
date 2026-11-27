// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=src/helper.rs");
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo provides the build output directory"));
    let helper = output.join(format!("gamma-process-helper{}", env::consts::EXE_SUFFIX));
    let rustc = env::var_os("RUSTC").expect("Cargo provides the compiler used for this test build");
    let status = Command::new(rustc)
        .args(["--edition=2024", "-C", "debuginfo=0", "src/helper.rs", "-o"])
        .arg(&helper)
        .status()
        .expect("Cargo's compiler can build the native test fixture");
    assert!(
        status.success(),
        "the native process test fixture must compile before tests execute"
    );
    println!("cargo:rustc-env=CARGO_GAMMA_PROCESS_HELPER={}", helper.display());
}
