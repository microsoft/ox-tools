// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::path::Path;

use ohno::{AppError, IntoAppError};

use super::{Environment, Platform, Probe, Target};

pub(super) fn validate(target: &Target, path: &Path, probe: &mut impl Probe) -> Result<(), AppError> {
    match target.platform {
        Platform::Linux => {
            // #[gamma::skip(literal.str_to_empty, literal.str_to_xyzzy, tag = "diagnostic", reason = "Only changes read-error context; rejection and the candidate path are tested without pinning prose.")]
            let header = probe.read(path, 0, 5).into_app_err("cannot read ELF header")?;
            let expected = if target.pointer_width == 64 { 2 } else { 1 };
            if header.get(..4) != Some(b"\x7fELF") || header.get(4) != Some(&expected) {
                return Err(AppError::new(format!(
                    "invalid ELF header or class; expected {}-bit libclang",
                    target.pointer_width
                )));
            }
        }
        Platform::Windows => {
            // #[gamma::skip(literal.str_to_empty, literal.str_to_xyzzy, tag = "diagnostic", reason = "Only changes error context; tests reject failed and truncated offset reads.")]
            let offset = probe.read(path, 0x3c, 4).into_app_err("cannot read PE header offset")?;
            // #[gamma::skip(literal.str_to_empty, literal.str_to_xyzzy, tag = "diagnostic", reason = "Only changes error context; tests reject truncated offset reads.")]
            let offset = <[u8; 4]>::try_from(offset.as_slice()).into_app_err("truncated PE header offset")?;
            let offset = u64::from(u32::from_le_bytes(offset));
            // #[gamma::skip(literal.str_to_empty, literal.str_to_xyzzy, tag = "diagnostic", reason = "Only changes error context; tests reject failed header reads.")]
            let header = probe.read(path, offset, 26).into_app_err("cannot read PE header")?;
            if header.get(..4) != Some(b"PE\0\0") {
                // #[gamma::skip(literal.str_to_empty, literal.str_to_xyzzy, tag = "diagnostic", reason = "Only changes refusal prose; malformed signatures must still be rejected.")]
                return Err(AppError::new("invalid PE signature"));
            }
            let machine = word(&header, 4)?;
            let magic = word(&header, 24)?;
            let expected = if target.pointer_width == 64 { 0x20b } else { 0x10b };
            let architecture_matches = match machine {
                0x14c => matches!(target.arch.as_str(), "i586" | "i686"),
                0x8664 => target.arch == "x86_64",
                0xaa64 => target.arch == "aarch64",
                _ => true,
            };
            if magic != expected || !architecture_matches {
                return Err(AppError::new(format!("DLL architecture does not match `{}`", target.triple)));
            }
        }
        Platform::MacOs => {}
    }
    Ok(())
}

fn word(header: &[u8], offset: usize) -> Result<u16, AppError> {
    // #[gamma::skip(literal.str_to_empty, literal.str_to_xyzzy, tag = "diagnostic", reason = "Only changes refusal prose; truncated fields must still be rejected.")]
    let bytes = header.get(offset..offset + 2).ok_or_else(|| AppError::new("truncated PE header"))?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

pub(super) fn version(path: &Path) -> Vec<u32> {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("libclang-"))
        // #[gamma::skip(literal.str_to_empty, tag = "equivalent", reason = "All versioned candidates end in .so; keeping that suffix appends the same parsed zero component to every version and preserves lexicographic ranking, including prefix ties.")]
        .and_then(|name| name.strip_suffix(".so"))
        .map(|version| version.split('.').map(|part| part.parse().unwrap_or(0)).collect())
        .unwrap_or_default()
}

pub(super) fn check_link_requirements(environment: &Environment, library: &Path, probe: &mut impl Probe) -> Result<(), AppError> {
    if environment.target.platform != Platform::Windows {
        return Ok(());
    }
    let directory = library
        .parent()
        // #[gamma::skip(literal.str_to_empty, literal.str_to_xyzzy, tag = "diagnostic", reason = "Only changes error context; a parentless library is rejected in-process.")]
        .ok_or_else(|| AppError::new("selected libclang DLL has no parent directory"))?;
    if environment.target.msvc {
        let import_directory = if directory.ends_with("bin") {
            directory
                .parent()
                .expect("a directory ending in the normal component bin has a parent, possibly the empty relative path")
                .join("lib")
        } else {
            directory.to_path_buf()
        };
        let mut found = false;
        for name in ["libclang.lib", "libclang.dll.a"] {
            let path = import_directory.join(name);
            match probe.is_file(&path) {
                Ok(true) => {
                    found = true;
                    break;
                }
                Ok(false) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).into_app_err(format!("cannot inspect libclang import library `{}`", path.display())),
            }
        }
        if !found {
            return Err(AppError::new(format!(
                "libclang DLL `{}` requires `libclang.lib` or `libclang.dll.a` in `{}` for MSVC linking. \
                 Install the matching LLVM development distribution, not a DLL-only copy.",
                library.display(),
                import_directory.display()
            )));
        }
    }
    let resolved_directory = probe
        .absolute_path(directory)
        .into_app_err(format!("cannot resolve libclang DLL directory `{}`", directory.display()))?;
    for entry in &environment.path {
        let resolved_entry = probe
            .absolute_path(entry)
            .into_app_err(format!("cannot resolve PATH entry `{}`", entry.display()))?;
        if normalized_path(&resolved_entry) == normalized_path(&resolved_directory) {
            return Ok(());
        }
    }
    Err(AppError::new(format!(
        "libclang DLL directory `{}` is not on PATH; add it to PATH so the linked bindgen build script can load the DLL.",
        directory.display()
    )))
}

fn normalized_path(path: &Path) -> Vec<u8> {
    let mut bytes: Vec<_> = path
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        // #[gamma::skip(string.lower_to_upper, tag = "equivalent", reason = "Both sides of the equality undergo the same ASCII case normalization; uppercase and lowercase induce the same equivalence classes.")]
        .map(|byte| if *byte == b'/' { b'\\' } else { byte.to_ascii_lowercase() })
        .collect();
    while bytes.last() == Some(&b'\\') {
        // #[gamma::skip(stmt.delete_call, tag = "timeout", reason = "Removing pop leaves the trailing separator in place forever; a deterministic assertion cannot observe a nonreturning loop.")]
        bytes.pop();
    }
    bytes
}
