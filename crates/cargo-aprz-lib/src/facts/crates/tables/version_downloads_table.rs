// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use chrono::NaiveDate;

use super::{VersionId, define_rows, define_table};

define_rows! {
    VersionDownloadRow {
        pub version_id: VersionId,
        pub downloads: u64,
        /// Day the downloads were recorded, as a count of days since the Unix epoch.
        ///
        /// This is by far the largest table (on the order of `10^8` rows) and a full scan of it
        /// rejects nearly every row, so the day count is left encoded rather than converted to a
        /// [`NaiveDate`] while reading. The conversion runs a proleptic-calendar computation that
        /// is wasted on a row that is about to be discarded, so callers filter on this raw count
        /// and call [`VersionDownloadRow::date_naive`] only for the rows they keep.
        pub date: u64,
    }
}

impl VersionDownloadRow {
    /// Converts the row's raw day count into a calendar date.
    ///
    /// Returns `None` when cached data contains a day count the table writer cannot produce.
    #[must_use]
    pub fn date_naive(&self) -> Option<NaiveDate> {
        i32::try_from(self.date).ok().and_then(NaiveDate::from_epoch_days)
    }
}

define_table! {
    version_downloads {
        fn write_row(csv_row: &CsvVersionDownloadRow<'a>, writer: &mut RowWriter<impl Write>) -> Result<()> {
            writer.write_str_as_u64(csv_row.version_id)?;
            writer.write_str_as_u64(csv_row.downloads)?;
            writer.write_str_as_date(csv_row.date)?;
            Ok(())
        }

        fn read_row<'a>(reader: &mut RowReader<'a>) -> VersionDownloadRow {
            VersionDownloadRow {
                version_id: VersionId(reader.read_u64()),
                downloads: reader.read_u64(),
                date: reader.read_u64(),
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::super::{RowReader, RowWriter, Table};
    use super::*;

    #[test]
    fn download_rows_round_trip_date_as_epoch_days() {
        let csv_row = CsvVersionDownloadRow {
            version_id: "123",
            downloads: "4567",
            date: "2024-07-08",
        };
        let mut buffer = Vec::new();
        {
            let mut writer = RowWriter::new(&mut buffer);
            VersionDownloadsTable::write_row(&csv_row, &mut writer).expect("download row is valid");
            writer.row_done().expect("writing to Vec cannot fail");
        }
        buffer.extend_from_slice(&[0u8; 10]);

        let row = VersionDownloadsTable::read_row(&mut RowReader::new(&buffer));

        assert_eq!(row.version_id, VersionId(123));
        assert_eq!(row.downloads, 4567);
        assert_eq!(
            row.date_naive(),
            Some(NaiveDate::from_ymd_opt(2024, 7, 8).expect("valid date literal"))
        );
    }

    #[test]
    fn out_of_range_day_counts_are_rejected() {
        for date in [u64::MAX, i32::MAX as u64] {
            let row = VersionDownloadRow {
                version_id: VersionId(1),
                downloads: 2,
                date,
            };

            assert_eq!(row.date_naive(), None, "day count {date} cannot be emitted by the table writer");
        }
    }
}
