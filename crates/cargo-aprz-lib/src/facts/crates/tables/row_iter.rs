// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::fmt::{Debug, Formatter, Result as FmtResult};
use core::ptr::from_ref;

use super::RowReader;

pub struct RowIter<'a, Row, Index, F> {
    reader: RowReader<'a>,
    read_fn: F,
    rows_remaining: u64,
    _phantom: core::marker::PhantomData<fn() -> (Row, Index)>,
}

impl<'a, Row, Index, F> RowIter<'a, Row, Index, F> {
    pub(super) const fn new(reader: RowReader<'a>, read_fn: F, num_rows: u64) -> Self {
        Self {
            reader,
            read_fn,
            rows_remaining: num_rows,
            _phantom: core::marker::PhantomData,
        }
    }
}

impl<'a, Row, Index: From<usize>, F: Fn(&mut RowReader<'a>) -> Row> Iterator for RowIter<'a, Row, Index, F> {
    type Item = (Row, Index);

    fn next(&mut self) -> Option<Self::Item> {
        if self.rows_remaining == 0 {
            return None;
        }

        let row_position = self.reader.position();
        let row = (self.read_fn)(&mut self.reader);
        self.rows_remaining -= 1;
        let index = Index::from(row_position);

        Some((row, index))
    }

    #[expect(clippy::cast_possible_truncation, reason = "Tables won't exceed usize::MAX entries in practice")]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.rows_remaining as usize;
        (remaining, Some(remaining))
    }
}

impl<Row, Index, F> Debug for RowIter<'_, Row, Index, F> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.debug_struct("RowIter")
            .field("reader", &self.reader)
            .field("read_fn", &from_ref(&self.read_fn))
            .field("rows_remaining", &self.rows_remaining)
            .finish()
    }
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn size_hint_reports_the_remaining_rows() {
        let data = [1_u8, 2, 3];
        let mut it: RowIter<'_, u8, usize, _> = RowIter::new(RowReader::new(&data), |reader: &mut RowReader<'_>| reader.read_byte(), 3);
        assert_eq!(it.size_hint(), (3, Some(3)));

        let _ = it.next().expect("three rows were requested");
        assert_eq!(it.size_hint(), (2, Some(2)));

        assert_eq!(it.count(), 2);
    }

    #[test]
    fn debug_lists_the_iterator_state() {
        let data = [1_u8, 2, 3];
        let it: RowIter<'_, u8, usize, _> = RowIter::new(RowReader::new(&data), |reader: &mut RowReader<'_>| reader.read_byte(), 3);
        let rendered = format!("{it:?}");
        assert!(rendered.starts_with("RowIter {"), "unexpected rendering: {rendered}");
        assert!(rendered.contains("rows_remaining: 3"), "unexpected rendering: {rendered}");
    }
}
