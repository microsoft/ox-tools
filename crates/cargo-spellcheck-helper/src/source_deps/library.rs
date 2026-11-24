// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::path::Path;

use ohno::{AppError, IntoAppError};

use super::{Environment, Platform, Probe, Target};

pub(super) fn validate(target: &Target, path: &Path, probe: &mut impl Probe) -> Result<(), AppError> {
    match target.platform {
        Platform::Linux => {
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
            let offset = probe.read(path, 0x3c, 4).into_app_err("cannot read PE header offset")?;
            let offset = <[u8; 4]>::try_from(offset.as_slice()).into_app_err("truncated PE header offset")?;
            let offset = u64::from(u32::from_le_bytes(offset));
            let header = probe.read(path, offset, 26).into_app_err("cannot read PE header")?;
            if header.get(..4) != Some(b"PE\0\0") {
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
    let bytes = header.get(offset..offset + 2).ok_or_else(|| AppError::new("truncated PE header"))?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

pub(super) fn version(path: &Path) -> Vec<u32> {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("libclang-"))
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
        .ok_or_else(|| AppError::new("selected libclang DLL has no parent directory"))?;
    if environment.target.msvc {
        let import_directory = if directory.ends_with("bin") {
            directory
                .parent()
                .ok_or_else(|| AppError::new("LLVM bin directory has no parent"))?
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
    if !environment
        .path
        .iter()
        .any(|entry| normalized_path(entry) == normalized_path(directory))
    {
        return Err(AppError::new(format!(
            "libclang DLL directory `{}` is not on PATH; add it to PATH so the linked bindgen build script can load the DLL.",
            directory.display()
        )));
    }
    Ok(())
}

fn normalized_path(path: &Path) -> Vec<u8> {
    let mut bytes: Vec<_> = path
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .map(|byte| if *byte == b'/' { b'\\' } else { byte.to_ascii_lowercase() })
        .collect();
    while bytes.last() == Some(&b'\\') {
        bytes.pop();
    }
    bytes
}
