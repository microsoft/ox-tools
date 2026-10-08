// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! In-process selection and encoding of scripted compiler-flag values.

use std::ffi::{OsStr, OsString};

fn encode_plain(flags: &OsStr) -> OsString {
    OsString::from(
        flags
            .to_str()
            .expect("Cargo's plain rustflags variables must contain UTF-8")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("\u{1f}"),
    )
}

pub fn inherited_from(read: impl Fn(&str) -> Option<OsString>, target: impl FnOnce() -> String) -> OsString {
    if let Some(encoded) = read("CARGO_ENCODED_RUSTFLAGS") {
        return encoded;
    }
    if let Some(plain) = read("RUSTFLAGS") {
        return encode_plain(&plain);
    }
    if let Some(flags) = read(&target()) {
        let encoded = encode_plain(&flags);
        if !encoded.is_empty() {
            return encoded;
        }
    }
    read("CARGO_BUILD_RUSTFLAGS").map_or_else(OsString::new, |flags| encode_plain(&flags))
}

/// Adds one argument without splitting spaces inside it.
pub fn append(encoded: &mut OsString, flag: &str) {
    if !encoded.is_empty() {
        encoded.push("\u{1f}");
    }
    encoded.push(flag);
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: &str = "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS";

    fn selected(variables: &[(&str, &str)]) -> OsString {
        inherited_from(
            |name| {
                variables
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            },
            || TARGET.to_owned(),
        )
    }

    #[test]
    fn target_flags_outrank_build_flags() {
        assert_eq!(
            selected(&[(TARGET, "--cfg target_fixture"), ("CARGO_BUILD_RUSTFLAGS", "--cfg build_fixture")]),
            OsString::from("--cfg\u{1f}target_fixture")
        );
    }

    #[test]
    fn unrelated_target_flags_are_not_selected() {
        assert_eq!(
            selected(&[
                ("CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS", "--cfg unrelated"),
                ("CARGO_BUILD_RUSTFLAGS", "--cfg build_fixture"),
            ]),
            OsString::from("--cfg\u{1f}build_fixture")
        );
    }

    #[test]
    fn empty_target_flags_fall_back_to_build_flags() {
        assert_eq!(
            selected(&[(TARGET, " \t"), ("CARGO_BUILD_RUSTFLAGS", "--cfg build_fixture")]),
            OsString::from("--cfg\u{1f}build_fixture")
        );
    }

    #[test]
    fn encoded_flags_keep_precedence_and_argument_boundaries() {
        let encoded = "--cfg\u{1f}label=\"two words\"";
        assert_eq!(
            inherited_from(
                |name| (name == "CARGO_ENCODED_RUSTFLAGS").then(|| OsString::from(encoded)),
                || panic!("global flags must not require a target lookup"),
            ),
            OsString::from(encoded)
        );
    }

    #[test]
    fn plain_global_flags_outrank_target_flags() {
        assert_eq!(
            selected(&[("RUSTFLAGS", "--cfg global_fixture"), (TARGET, "--cfg target_fixture")]),
            OsString::from("--cfg\u{1f}global_fixture")
        );
    }

    #[test]
    fn appended_arguments_keep_spaces() {
        let mut encoded = selected(&[(TARGET, "--cfg target_fixture")]);
        append(&mut encoded, "--extern");
        append(&mut encoded, "gamma=path with spaces/gamma.dll");
        assert_eq!(
            encoded,
            OsString::from("--cfg\u{1f}target_fixture\u{1f}--extern\u{1f}gamma=path with spaces/gamma.dll")
        );
    }
}
