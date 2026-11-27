// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, coverage(off))]

use std::io::{self, Cursor, Read, Seek, SeekFrom};

use super::soname;

struct Fixture {
    bytes: Vec<u8>,
    wide: bool,
    big: bool,
}

impl Fixture {
    fn new(wide: bool, big: bool, name: &[u8]) -> Self {
        let mut fixture = Self {
            bytes: vec![0; 0x400.max(0x301 + name.len() + 1)],
            wide,
            big,
        };
        fixture.bytes[..7].copy_from_slice(&[0x7f, b'E', b'L', b'F', if wide { 2 } else { 1 }, if big { 2 } else { 1 }, 1]);
        fixture.put(16, 3, 2);
        fixture.put(18, if wide { 62 } else { 3 }, 2);
        fixture.put(20, 1, 4);
        let header = if wide { 64_u64 } else { 52 };
        let ph_size = if wide { 56_u64 } else { 32 };
        fixture.put(if wide { 32 } else { 28 }, header, if wide { 8 } else { 4 });
        fixture.put(if wide { 52 } else { 40 }, header, 2);
        fixture.put(if wide { 54 } else { 42 }, ph_size, 2);
        fixture.put(if wide { 56 } else { 44 }, 2, 2);
        fixture.segment(0, 1, 0, 0x10000, fixture.bytes.len() as u64);
        fixture.segment(1, 2, 0x200, 0x10200, if wide { 64 } else { 32 });
        fixture.dynamic(0, 5, 0x10300);
        fixture.dynamic(1, 10, name.len() as u64 + 2);
        fixture.dynamic(2, 14, 1);
        fixture.dynamic(3, 0, 0);
        fixture.bytes[0x301..0x301 + name.len()].copy_from_slice(name);
        fixture
    }

    fn put(&mut self, offset: usize, value: u64, width: usize) {
        let bytes = if self.big { value.to_be_bytes() } else { value.to_le_bytes() };
        let range = if self.big { 8 - width..8 } else { 0..width };
        self.bytes[offset..offset + width].copy_from_slice(&bytes[range]);
    }

    fn segment(&mut self, index: usize, kind: u64, offset: u64, address: u64, size: u64) {
        let start = if self.wide { 64 + index * 56 } else { 52 + index * 32 };
        self.put(start, kind, 4);
        if self.wide {
            self.put(start + 8, offset, 8);
            self.put(start + 16, address, 8);
            self.put(start + 32, size, 8);
            self.put(start + 40, size, 8);
        } else {
            self.put(start + 4, offset, 4);
            self.put(start + 8, address, 4);
            self.put(start + 16, size, 4);
            self.put(start + 20, size, 4);
        }
    }

    fn dynamic(&mut self, index: usize, tag: u64, value: u64) {
        let width = if self.wide { 8 } else { 4 };
        self.put(0x200 + index * width * 2, tag, width);
        self.put(0x200 + index * width * 2 + width, value, width);
    }
}

#[test]
fn sectionless_elf_runtime_names_support_both_classes_and_byte_orders() {
    for (wide, big) in [(false, false), (false, true), (true, false), (true, true)] {
        let fixture = Fixture::new(wide, big, b"libclang.so.18");
        assert_eq!(soname(&mut Cursor::new(fixture.bytes)).unwrap(), Some(b"libclang.so.18".to_vec()));
    }
}

#[test]
fn string_table_at_a_load_boundary_uses_the_segment_starting_there() {
    let mut fixture = Fixture::new(true, false, b"libclang.so.18");
    fixture.put(56, 3, 2);
    fixture.segment(0, 1, 0, 0x10000, 0x300);
    fixture.segment(1, 1, 0x300, 0x10300, 0x100);
    fixture.segment(2, 2, 0x200, 0x10200, 64);
    assert_eq!(soname(&mut Cursor::new(fixture.bytes)).unwrap(), Some(b"libclang.so.18".to_vec()));
}

#[test]
fn missing_soname_is_distinct_from_missing_dynamic_metadata() {
    let mut fixture = Fixture::new(true, false, b"libclang.so.18");
    fixture.dynamic(2, 1, 1);
    assert_eq!(soname(&mut Cursor::new(fixture.bytes)).unwrap(), None);
    let mut fixture = Fixture::new(true, false, b"libclang.so.18");
    fixture.segment(1, 1, 0x200, 0x10200, 64);
    soname(&mut Cursor::new(fixture.bytes)).unwrap_err();
}

#[test]
fn malformed_dynamic_metadata_and_names_are_rejected() {
    for case in 0..11 {
        let mut fixture = Fixture::new(true, false, b"libclang.so.18");
        match case {
            0 => fixture.bytes[0] = 0,
            1 => fixture.dynamic(0, 1, 0x10300),
            2 => fixture.dynamic(1, 1, 15),
            3 => fixture.dynamic(0, 5, 0xffff),
            4 => fixture.dynamic(0, 5, 0x10400),
            5 => fixture.dynamic(1, 10, 0x101),
            6 => fixture.dynamic(2, 14, 16),
            7 => fixture.dynamic(2, 14, 0),
            8 => fixture.bytes[0x30f] = b'x',
            9 => fixture.dynamic(3, 1, 0),
            10 => {
                fixture.dynamic(2, 14, 17);
                fixture.bytes[0x311] = b'x';
            }
            _ => unreachable!("the loop uses only the eleven listed malformed fixtures"),
        }
        soname(&mut Cursor::new(fixture.bytes)).unwrap_err();
    }
}

#[test]
fn file_offset_overflow_is_reported_instead_of_wrapping() {
    let mut fixture = Fixture::new(true, false, b"libclang.so.18");
    fixture.segment(0, 1, u64::MAX, 0x10000, 0x400);
    soname(&mut Cursor::new(fixture.bytes)).unwrap_err();
}

#[test]
fn dynamic_entries_after_null_do_not_change_the_runtime_name() {
    let mut fixture = Fixture::new(true, false, b"libclang.so.18");
    fixture.segment(1, 2, 0x200, 0x10200, 80);
    fixture.dynamic(4, 14, 2);
    assert_eq!(soname(&mut Cursor::new(fixture.bytes)).unwrap(), Some(b"libclang.so.18".to_vec()));
}

#[test]
fn runtime_name_length_is_bounded_by_the_linux_path_limit() {
    for (length, valid) in [(4095, true), (4096, false)] {
        let name = vec![b'x'; length];
        let fixture = Fixture::new(true, false, &name);
        let result = soname(&mut Cursor::new(fixture.bytes));
        assert_eq!(result.is_ok(), valid);
        if valid {
            assert_eq!(result.unwrap(), Some(name));
        }
    }
}

struct Reader {
    cursor: Cursor<Vec<u8>>,
    fail_read_at: Option<u64>,
    fail_seek_at: Option<u64>,
    bytes_read: usize,
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.fail_read_at == Some(self.cursor.position()) {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let count = self.cursor.read(buf)?;
        self.bytes_read += count;
        Ok(count)
    }
}

impl Seek for Reader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        if matches!(pos, SeekFrom::Start(offset) if Some(offset) == self.fail_seek_at) {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        self.cursor.seek(pos)
    }
}

#[test]
fn metadata_and_name_io_failures_retain_their_typed_cause() {
    for (read, seek) in [(Some(0x200), None), (Some(0x301), None), (None, Some(0x200)), (None, Some(0x301))] {
        let mut reader = Reader {
            cursor: Cursor::new(Fixture::new(true, false, b"libclang.so.18").bytes),
            fail_read_at: read,
            fail_seek_at: seek,
            bytes_read: 0,
        };
        let error = soname(&mut reader).unwrap_err();
        assert_eq!(error.find_source::<io::Error>().unwrap().kind(), io::ErrorKind::PermissionDenied);
    }
}

#[test]
fn only_metadata_and_runtime_name_are_read_not_library_payload() {
    let mut fixture = Fixture::new(true, false, b"libclang.so.18");
    fixture.bytes.resize(16 * 1024, 0xa5);
    fixture.segment(0, 1, 0, 0x10000, fixture.bytes.len() as u64);
    let mut reader = Reader {
        cursor: Cursor::new(fixture.bytes),
        fail_read_at: None,
        fail_seek_at: None,
        bytes_read: 0,
    };
    assert_eq!(soname(&mut reader).unwrap(), Some(b"libclang.so.18".to_vec()));
    assert!(reader.bytes_read < 512);
}
