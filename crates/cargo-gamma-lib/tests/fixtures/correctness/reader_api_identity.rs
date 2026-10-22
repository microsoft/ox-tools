// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fs::File;
use std::io::Cursor;

trait Extension {
    fn take(self, count: usize) -> std::vec::IntoIter<u8>;
    fn skip(self, count: usize) -> std::vec::IntoIter<u8>;
}
impl Extension for File {
    fn take(self, count: usize) -> std::vec::IntoIter<u8> {
        drop(self);
        (0..4).take(count).collect::<Vec<_>>().into_iter()
    }
    fn skip(self, count: usize) -> std::vec::IntoIter<u8> {
        drop(self);
        (0..4).skip(count).collect::<Vec<_>>().into_iter()
    }
}
impl Extension for Cursor<Vec<u8>> {
    fn take(self, count: usize) -> std::vec::IntoIter<u8> {
        self.into_inner().into_iter().take(count).collect::<Vec<_>>().into_iter()
    }
    fn skip(self, count: usize) -> std::vec::IntoIter<u8> {
        self.into_inner().into_iter().skip(count).collect::<Vec<_>>().into_iter()
    }
}

pub struct Wrapper(pub Cursor<Vec<u8>>);
impl std::io::Read for Wrapper {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        std::io::Read::read(&mut self.0, buf)
    }
}
impl Extension for Wrapper {
    fn take(self, count: usize) -> std::vec::IntoIter<u8> {
        self.0.into_inner().into_iter().take(count).collect::<Vec<_>>().into_iter()
    }
    fn skip(self, count: usize) -> std::vec::IntoIter<u8> {
        self.0.into_inner().into_iter().skip(count).collect::<Vec<_>>().into_iter()
    }
}

pub fn file_take(value: File) -> impl Iterator<Item = u8> {
    value.take(2)
}
pub fn compiled_file_skip(value: File) -> impl Iterator<Item = u8> {
    value.skip(2)
}
pub fn cursor_take(value: Cursor<Vec<u8>>) -> impl Iterator<Item = u8> {
    value.take(2)
}
pub fn compiled_cursor_skip(value: Cursor<Vec<u8>>) -> impl Iterator<Item = u8> {
    value.skip(2)
}
pub fn wrapper_take(value: Wrapper) -> impl Iterator<Item = u8> {
    value.take(2)
}
pub fn compiled_wrapper_skip(value: Wrapper) -> impl Iterator<Item = u8> {
    value.skip(2)
}

pub mod read_cases {
    use std::fs::File;
    use std::io::Read as _;

    pub fn read_take(value: File) -> std::io::Take<File> {
        value.take(2)
    }
}

pub mod custom_view {
    use std::fs::File;
    use std::io::Read as _;

    trait IteratorView {
        fn by_ref(self) -> std::ops::Range<u8>;
    }
    impl IteratorView for File {
        fn by_ref(self) -> std::ops::Range<u8> {
            drop(self);
            0..4
        }
    }
    pub fn iterator_take(value: File) -> impl Iterator<Item = u8> {
        value.by_ref().take(2)
    }
    pub fn compiled_iterator_skip(value: File) -> impl Iterator<Item = u8> {
        value.by_ref().skip(2)
    }
    pub fn direct_read(value: File) -> std::io::Take<File> {
        value.take(2)
    }
}
