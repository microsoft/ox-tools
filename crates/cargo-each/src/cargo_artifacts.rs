// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Strict Cargo JSON message input for executable test artifacts.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use cargo_metadata::Message;

use crate::error::{
    CargoArtifactMessageError, CargoArtifactMetadataError, CargoArtifactsFileReadError, CargoArtifactsFileUtf8Error, EachError,
};

/// One executable test artifact and the package context Cargo reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CargoArtifact {
    pub(crate) executable: PathBuf,
    pub(crate) manifest: PathBuf,
    pub(crate) package_dir: PathBuf,
    pub(crate) package_id: String,
    pub(crate) target: String,
}

/// Fully validated Cargo message input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CargoArtifactInput {
    pub(crate) artifacts: Vec<CargoArtifact>,
    pub(crate) diagnostics: Vec<String>,
}

/// Load and validate one Cargo JSON message file.
pub(crate) fn load(path: &Path) -> Result<CargoArtifactInput, EachError> {
    let display = path.display().to_string();
    let file = File::open(path).map_err(|error| CargoArtifactsFileReadError::caused_by(display.clone(), error))?;
    parse(BufReader::new(file), &display)
}

fn parse(mut reader: impl BufRead, source: &str) -> Result<CargoArtifactInput, EachError> {
    let mut artifacts = Vec::new();
    let mut diagnostics = Vec::new();
    let mut bytes = Vec::new();
    let mut line_number = 0;
    let mut build_finished = None;

    loop {
        bytes.clear();
        let read = reader
            .read_until(b'\n', &mut bytes)
            .map_err(|error| CargoArtifactsFileReadError::caused_by(source.to_owned(), error))?;
        if read == 0 {
            break;
        }
        line_number += 1;
        let raw_line =
            std::str::from_utf8(&bytes).map_err(|error| CargoArtifactsFileUtf8Error::caused_by(source.to_owned(), line_number, error))?;
        let line = if line_number == 1 {
            raw_line.strip_prefix('\u{feff}').unwrap_or(raw_line)
        } else {
            raw_line
        };
        if line.trim().is_empty() {
            continue;
        }

        let message = serde_json::from_str::<Message>(line)
            .map_err(|error| CargoArtifactMessageError::new(source.to_owned(), line_number, error.column(), error.to_string()))?;
        match message {
            Message::CompilerArtifact(artifact) if artifact.profile.test => {
                let Some(executable) = artifact.executable else {
                    continue;
                };
                let package_id = artifact.package_id.to_string();
                let target = artifact.target.name;
                let executable = executable.into_std_path_buf();
                let manifest = artifact.manifest_path.into_std_path_buf();
                validate_absolute_path(source, line_number, &executable, "executable")?;
                validate_absolute_path(source, line_number, &manifest, "manifest_path")?;
                validate_text_field(source, line_number, &package_id, "package_id")?;
                validate_text_field(source, line_number, &target, "target.name")?;
                let package_dir = manifest
                    .parent()
                    .ok_or_else(|| {
                        CargoArtifactMetadataError::new(
                            source.to_owned(),
                            line_number,
                            target.clone(),
                            "`manifest_path` has no package directory; regenerate the file directly from Cargo".to_owned(),
                        )
                    })?
                    .to_path_buf();
                artifacts.push(CargoArtifact {
                    executable,
                    manifest,
                    package_dir,
                    package_id,
                    target,
                });
            }
            Message::CompilerMessage(message) => {
                if let Some(rendered) = message.message.rendered {
                    diagnostics.push(rendered);
                }
            }
            Message::BuildFinished(finished) => {
                build_finished = Some(finished.success);
            }
            _ => {}
        }
    }

    match build_finished {
        Some(true) => Ok(CargoArtifactInput { artifacts, diagnostics }),
        Some(false) => Err(CargoArtifactMetadataError::new(
            source.to_owned(),
            line_number,
            "build-finished".to_owned(),
            "Cargo reported an unsuccessful build; fix the compilation failure and regenerate the artifact file".to_owned(),
        )
        .into()),
        None => Err(CargoArtifactMetadataError::new(
            source.to_owned(),
            line_number,
            "build-finished".to_owned(),
            "the stream has no `build-finished` message; capture the complete output of a Cargo command using `--message-format=json`"
                .to_owned(),
        )
        .into()),
    }
}

fn validate_absolute_path(source: &str, line: usize, path: &Path, field: &str) -> Result<(), EachError> {
    if path.to_string_lossy().contains('\0') {
        return Err(CargoArtifactMetadataError::new(
            source.to_owned(),
            line,
            field.to_owned(),
            format!("`{field}` contains a NUL byte and cannot be passed to a child process; regenerate the file directly from Cargo"),
        )
        .into());
    }
    if path.is_absolute() || has_windows_absolute_prefix(path) {
        return Ok(());
    }
    Err(CargoArtifactMetadataError::new(
        source.to_owned(),
        line,
        field.to_owned(),
        format!("`{field}` must be an absolute path; regenerate the file directly from Cargo instead of editing its messages"),
    )
    .into())
}

fn validate_text_field(source: &str, line: usize, value: &str, field: &str) -> Result<(), EachError> {
    if value.contains('\0') {
        Err(CargoArtifactMetadataError::new(
            source.to_owned(),
            line,
            field.to_owned(),
            format!("`{field}` contains a NUL byte and cannot be passed to a child process; regenerate the file directly from Cargo"),
        )
        .into())
    } else {
        Ok(())
    }
}

fn has_windows_absolute_prefix(path: &Path) -> bool {
    let text = path.to_string_lossy();
    let bytes = text.as_bytes();
    matches!(
        bytes,
        [drive, b':', b'\\' | b'/', ..] if drive.is_ascii_alphabetic()
    ) || bytes.starts_with(br"\\")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::io;

    use super::*;

    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/cargo-artifacts.jsonl");

    #[test]
    fn parses_test_executables_diagnostics_order_and_duplicates() {
        let input = parse(io::Cursor::new(FIXTURE), "cargo-artifacts.jsonl").expect("valid Cargo message fixture");
        assert_eq!(input.artifacts.len(), 3);
        assert_eq!(
            input.artifacts.iter().map(|artifact| artifact.target.as_str()).collect::<Vec<_>>(),
            ["alpha_tests", "beta_integration", "alpha_tests"]
        );
        assert_eq!(input.artifacts[0], input.artifacts[2]);
        assert_eq!(input.diagnostics, ["warning: fixture diagnostic\n"]);
    }

    #[test]
    fn empty_successful_stream_has_no_artifacts() {
        let input =
            parse(io::Cursor::new("{\"reason\":\"build-finished\",\"success\":true}\n"), "empty.jsonl").expect("successful empty stream");
        assert_eq!(input.artifacts, Vec::<CargoArtifact>::new());
        assert_eq!(input.diagnostics, Vec::<String>::new());
    }

    #[test]
    fn malformed_failed_incomplete_and_relative_inputs_are_actionable() {
        for (input, expected) in [
            ("{bad\n", "invalid Cargo JSON message"),
            ("{\"reason\":\"build-finished\",\"success\":false}\n", "fix the compilation failure"),
            ("{\"reason\":\"compiler-message\"}\n", "invalid Cargo JSON message"),
            ("{\"reason\":\"future-message\"}\n", "invalid Cargo JSON message"),
            (
                "{\"reason\":\"compiler-artifact\",\"package_id\":\"path+file:///ws/a#0.1.0\",\"manifest_path\":\"relative/Cargo.toml\",\"target\":{\"kind\":[\"test\"],\"crate_types\":[\"bin\"],\"name\":\"a\",\"src_path\":\"/ws/a/tests/a.rs\",\"edition\":\"2024\",\"doc\":false,\"doctest\":false,\"test\":true},\"profile\":{\"opt_level\":\"0\",\"debuginfo\":2,\"debug_assertions\":true,\"overflow_checks\":true,\"test\":true},\"features\":[],\"filenames\":[],\"executable\":\"relative/a\",\"fresh\":false}\n{\"reason\":\"build-finished\",\"success\":true}\n",
                "must be an absolute path",
            ),
            ("", "no `build-finished` message"),
        ] {
            let error = parse(io::Cursor::new(input), "invalid.jsonl").expect_err("invalid input must fail");
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[test]
    fn reader_and_utf8_errors_name_the_source() {
        struct FailingReader;
        impl io::Read for FailingReader {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("injected read failure"))
            }
        }
        impl BufRead for FailingReader {
            fn fill_buf(&mut self) -> io::Result<&[u8]> {
                Err(io::Error::other("injected read failure"))
            }
            fn consume(&mut self, _: usize) {}
        }

        let read_error = parse(FailingReader, "artifacts.jsonl").expect_err("reader failure");
        assert!(read_error.to_string().contains("artifacts.jsonl"), "{read_error}");
        let utf8_error = parse(io::Cursor::new([0xff, b'\n']), "artifacts.jsonl").expect_err("invalid UTF-8");
        let rendered = utf8_error.to_string();
        assert!(rendered.contains("artifacts.jsonl") && rendered.contains("line 1") && rendered.contains("UTF-8"));
    }
}
