// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Preserves Cargo's environment flag precedence for embedded test projects.

use std::env;
use std::ffi::OsString;
use std::process::Command;
use std::sync::OnceLock;

#[path = "flag_values.rs"]
mod values;

pub use values::append;

/// The fixtures use the compiler's host unless Cargo's target override selects another triple.
fn target_variable() -> String {
    static HOST: OnceLock<String> = OnceLock::new();
    let configured = env::var_os("CARGO_BUILD_TARGET");
    let target = configured
        .as_deref()
        .map(|value| value.to_str().expect("Cargo's target override must contain UTF-8"));
    let target = match target {
        Some(target) if target != "host-tuple" => target,
        _ => HOST.get_or_init(|| {
            let output = Command::new(env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
                .arg("-vV")
                .output()
                .expect("the selected compiler must be runnable to identify the fixture target");
            assert!(
                output.status.success(),
                "the selected compiler must report its host:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout)
                .expect("rustc version information is UTF-8")
                .lines()
                .find_map(|line| line.strip_prefix("host: "))
                .expect("rustc -vV reports a host triple")
                .trim()
                .to_owned()
        }),
    };
    format!("CARGO_TARGET_{}_RUSTFLAGS", target.to_uppercase().replace(['-', '.'], "_"))
}

/// Encodes the effective inherited source, excluding variables for unrelated targets.
pub fn inherited() -> OsString {
    values::inherited_from(|name| env::var_os(name), target_variable)
}
