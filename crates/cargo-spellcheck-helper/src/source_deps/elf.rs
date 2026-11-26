// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};

use ::elf::ElfStream;
use ::elf::abi::{DT_NULL, DT_SONAME, DT_STRSZ, DT_STRTAB, PT_DYNAMIC, PT_LOAD};
use ::elf::dynamic::Dyn;
use ::elf::endian::AnyEndian;
use ::elf::parse::ParseAt;
use ohno::{AppError, IntoAppError};

// #[gamma::skip(literal.str_to_empty, literal.str_to_xyzzy, tag = "diagnostic", reason = "Every string literal in this function is error or expect prose; tests verify parsing outcomes, bounded reads and typed I/O causes, not copied diagnostic constants.")]
pub(super) fn soname(reader: &mut (impl Read + Seek)) -> Result<Option<Vec<u8>>, AppError> {
    let (class, endian, segments) = {
        let elf = ElfStream::<AnyEndian, _>::open_stream(&mut *reader).into_app_err("cannot parse selected ELF library")?;
        (elf.ehdr.class, elf.ehdr.endianness, elf.segments().clone())
    };
    let dynamic = segments
        .iter()
        .find(|segment| segment.p_type == PT_DYNAMIC)
        .ok_or_else(|| AppError::new("ELF shared library has no dynamic segment"))?;
    let entry_size = Dyn::size_for(class);
    reader
        .seek(SeekFrom::Start(dynamic.p_offset))
        .into_app_err("cannot seek to ELF dynamic segment")?;
    let mut bytes = vec![0; entry_size];
    let mut name_offset = None;
    let mut string_address = None;
    let mut string_size = None;
    let mut terminated = false;
    for _ in 0..dynamic.p_filesz / u64::try_from(entry_size).expect("ELF dynamic entry size fits u64") {
        reader.read_exact(&mut bytes).into_app_err("cannot read ELF dynamic entry")?;
        let entry = Dyn::parse_at(endian, class, &mut 0, &bytes)
            .expect("read_exact fills Dyn::size_for bytes and ElfStream validates the class and endianness");
        match entry.d_tag {
            DT_NULL => {
                terminated = true;
                break;
            }
            DT_SONAME => name_offset = Some(entry.d_val()),
            DT_STRTAB => string_address = Some(entry.d_ptr()),
            DT_STRSZ => string_size = Some(entry.d_val()),
            _ => {}
        }
    }
    if !terminated {
        return Err(AppError::new("ELF dynamic segment has no terminating entry"));
    }
    let Some(name_offset) = name_offset else {
        return Ok(None);
    };
    let address = string_address.ok_or_else(|| AppError::new("ELF SONAME has no dynamic string table"))?;
    let size = string_size.ok_or_else(|| AppError::new("ELF SONAME has no dynamic string-table size"))?;
    let (segment, relative) = segments
        .iter()
        .filter(|segment| segment.p_type == PT_LOAD)
        .find_map(|segment| {
            let relative = address.checked_sub(segment.p_vaddr)?;
            (relative < segment.p_filesz).then_some((segment, relative))
        })
        .ok_or_else(|| AppError::new("ELF dynamic string table is outside file-backed load segments"))?;
    // #[gamma::skip(relational.ge_to_gt, tag = "equivalent", reason = "When the name starts exactly at table end, the bounded reader has zero bytes and the final empty/unterminated-name check still rejects it.")]
    if size > segment.p_filesz - relative || name_offset >= size {
        return Err(AppError::new("ELF SONAME is outside the dynamic string table"));
    }
    let offset = segment
        .p_offset
        .checked_add(relative)
        .and_then(|offset| offset.checked_add(name_offset))
        .ok_or_else(|| AppError::new("ELF SONAME file offset overflows"))?;
    reader.seek(SeekFrom::Start(offset)).into_app_err("cannot seek to ELF SONAME")?;
    let mut name = Vec::new();
    // Linux paths, including the terminating NUL, cannot exceed PATH_MAX (4096).
    BufReader::new(reader.take((size - name_offset).min(4096)))
        .read_until(0, &mut name)
        .into_app_err("cannot read ELF SONAME")?;
    if name.last() != Some(&0) || name.len() == 1 {
        return Err(AppError::new("ELF SONAME is empty, unterminated, or exceeds the Linux path limit"));
    }
    name.pop();
    Ok(Some(name))
}

#[cfg(test)]
#[path = "elf_tests.rs"]
mod tests;
