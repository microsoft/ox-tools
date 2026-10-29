// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::time::Duration;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{BufWriter, Read as IoRead, Seek, SeekFrom, Write};
use std::path::Path;

use chrono::{DateTime, TimeZone, Utc};
use csv::{Reader, StringRecord};
use mmap_rs::{Mmap, MmapFlags, MmapOptions};
use ohno::IntoAppError;
use serde::de::Deserialize;

use super::{RowReader, RowWriter};
use crate::Result;

const FORMAT_MAGIC: u64 = 0xC0DE_C0DE_C0DE_0011;
const TABLE_WRITE_BUFFER_SIZE: usize = 1_048_576;

pub const TABLE_HEADER_SIZE: usize = 24; // 8 bytes magic + 8 bytes count + 8 bytes timestamp

pub trait Table: Sized {
    type CsvRow<'a>: Deserialize<'a>;
    type Row<'a>
    where
        Self: 'a;
    type Index: Copy + From<usize>;

    // Constants
    const CSV_NAME: &'static str;
    const TABLE_NAME: &'static str;

    // CSV serialization
    fn write_row(csv_row: &Self::CsvRow<'_>, writer: &mut RowWriter<impl Write>) -> Result<()>;
    fn read_row<'a>(reader: &mut RowReader<'a>) -> Self::Row<'a>;

    // Table construction and opening
    fn open_with(mmap: Mmap, max_ttl: Duration, now: DateTime<Utc>) -> Result<Self>;

    fn open(tables_root: impl AsRef<Path>, max_ttl: Duration, now: DateTime<Utc>) -> Result<Self> {
        open_table_with::<Self, _>(
            tables_root.as_ref(),
            max_ttl,
            now,
            |path| File::open(path),
            File::metadata,
            map_table_file,
        )
    }

    fn create_table(tables_root: impl AsRef<Path>, csv_entry: impl IoRead, now: DateTime<Utc>) -> Result<File> {
        let tables_root = tables_root.as_ref();
        let path = tables_root.join(Self::TABLE_NAME);

        // Open with read+write permissions so the completed file can be rewound and snapshotted.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .into_app_err_with(|| format!("creating table file: {}", path.display()))?;

        // Use a 1MB buffer for better performance with large tables
        let mut buf_writer = BufWriter::with_capacity(TABLE_WRITE_BUFFER_SIZE, file);

        write_table_contents::<Self>(&mut buf_writer, csv_entry, now)?;

        // Return the file handle so the caller can load an immutable snapshot.
        let file =
        // #[gamma::skip(try.propagate_to_unwrap, reason = "The preceding successful header write flushed the BufWriter; into_inner has no buffered bytes left to fail.", tag = "equivalent")]
        finish_buffered_writer(buf_writer)?;
        Ok(file)
    }

    // Runtime data access
    fn iter(&self) -> impl Iterator<Item = (Self::Row<'_>, Self::Index)>;
    fn get(&self, index: Self::Index) -> Self::Row<'_>;
    fn timestamp(&self) -> DateTime<Utc>;
}

fn open_table_with<T: Table, F>(
    tables_root: &Path,
    max_ttl: Duration,
    now: DateTime<Utc>,
    open: impl FnOnce(&Path) -> std::io::Result<F>,
    metadata: impl FnOnce(&F) -> std::io::Result<Metadata>,
    load_snapshot: impl FnOnce(&F, usize) -> Result<Mmap>,
) -> Result<T> {
    let path = tables_root.join(T::TABLE_NAME);
    let file = open(&path).into_app_err_with(|| format!("opening table file: {}", path.display()))?;
    let file_size = table_file_size(metadata(&file))?;
    let mmap = load_snapshot(&file, file_size)?;
    T::open_with(mmap, max_ttl, now)
}

fn table_file_size(metadata: std::io::Result<Metadata>) -> Result<usize> {
    let metadata = metadata.into_app_err("getting file metadata")?;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "Table files won't exceed usize::MAX on any supported platform"
    )]
    let file_size = metadata.len() as usize;
    Ok(file_size)
}

fn write_table_header_placeholder(writer: &mut impl Write) -> Result<()> {
    writer
        .write_all(&[0u8; TABLE_HEADER_SIZE])
        .into_app_err("writing table header placeholder")?;
    Ok(())
}

fn write_table_contents<T: Table>(writer: &mut (impl Write + Seek), csv_entry: impl IoRead, now: DateTime<Utc>) -> Result<()> {
    write_table_header_placeholder(writer)?;

    let mut csv_reader = Reader::from_reader(csv_entry);
    let mut row_writer = RowWriter::new(&mut *writer);
    let headers = csv_reader.headers().into_app_err("reading CSV headers")?.clone();
    let mut record = StringRecord::new();
    while csv_reader.read_record(&mut record).into_app_err("reading CSV row")? {
        let row = record.deserialize(Some(&headers)).into_app_err("deserializing CSV row")?;
        T::write_row(&row, &mut row_writer).into_app_err("converting CSV row")?;
        row_writer.row_done().into_app_err("writing table row")?;
    }

    let count = row_writer.row_count();
    let timestamp = now.timestamp().max(0).cast_unsigned();
    drop(row_writer);

    writer.write_all(&[0u8; 10]).into_app_err("writing table padding")?;
    finish_table_header(writer, count, timestamp).into_app_err("finalizing table header")
}

fn finish_buffered_writer<W: Write>(writer: BufWriter<W>) -> Result<W> {
    writer
        .into_inner()
        .map_err(|error| ohno::app_err!("flushing table data: {}", error.error()))
}

pub(super) fn map_table_file(file: &File, file_size: usize) -> Result<Mmap> {
    map_table_file_with(
        file.try_clone(),
        file_size,
        MmapOptions::new,
        |options| {
            options
                .with_flags(MmapFlags::TRANSPARENT_HUGE_PAGES.union(MmapFlags::SEQUENTIAL))
                .map_mut()
        },
        |mmap| mmap.make_read_only().map_err(|(_mmap, cause)| cause),
    )
}

fn map_table_file_with<R, O, B, T, E>(
    reader: std::io::Result<R>,
    file_size: usize,
    options: impl FnOnce(usize) -> std::result::Result<O, E>,
    allocate: impl FnOnce(O) -> std::result::Result<B, E>,
    protect: impl FnOnce(B) -> std::result::Result<T, E>,
) -> Result<T>
where
    R: IoRead + Seek,
    B: AsMut<[u8]>,
    E: core::error::Error + Send + Sync + 'static,
{
    let mut reader = reader.into_app_err("cloning table file handle")?;
    let _ = reader.seek(SeekFrom::Start(0)).into_app_err("rewinding table file")?;
    let options = options(file_size).into_app_err("creating memory-map options")?;
    let mut mmap = allocate(options).into_app_err("allocating table memory")?;
    reader.read_exact(mmap.as_mut()).into_app_err("reading table file")?;
    protect(mmap).into_app_err("protecting table memory")
}

fn finish_table_header(writer: &mut (impl Write + Seek), count: u64, timestamp: u64) -> Result<()> {
    let _ = writer.seek(SeekFrom::Start(0))?;
    writer.write_all(&FORMAT_MAGIC.to_le_bytes()).into_app_err("writing table magic")?;
    writer.write_all(&count.to_le_bytes()).into_app_err("writing table row count")?;
    writer.write_all(&timestamp.to_le_bytes()).into_app_err("writing table timestamp")?;
    writer.flush().into_app_err("flushing table header")?;
    Ok(())
}

/// Generates a table struct, index type, and implementation from a `snake_case` name and row conversion functions.
///
/// Creates:
/// - `{Name}Table` - Main table struct with zero-copy row access over an immutable snapshot
/// - `{Name}TableIndex` - Type-safe index for accessing rows
/// - Implementation of `Table` trait
/// - File name constants derived from the base name (`{name}.csv`, `{name}.table`)
///
/// See `crates_table.rs`, `versions_table.rs`, or any table file for usage examples.
macro_rules! define_table {
    (
        $name_snake:ident {
            fn write_row($csv_param:ident: &$csv_ty:ty, $writer:ident: &mut RowWriter<impl Write>) -> Result<()>
                $write_body:block

            fn read_row<'a>($reader:ident: &mut RowReader<'a>) -> $row_result:ty
                $read_body:block
        }
    ) => {
        pastey::paste! {
            // Derive all names from the snake_case base name
            // Table struct: snake_case -> PascalCase + "Table"
            // Row struct: snake_case -> PascalCase + "Row"
            // Index struct: snake_case -> PascalCase + "TableIndex"
            // CSV file: snake_case + ".csv"
            // Table file: snake_case + ".table"

            #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
            pub struct [<$name_snake:camel Table Index>](usize);

            impl From<usize> for [<$name_snake:camel Table Index>] {
                fn from(value: usize) -> Self {
                    Self(value)
                }
            }

            #[derive(Debug)]
            pub struct [<$name_snake:camel Table>] {
                mmap: mmap_rs::Mmap,
                count: u64,
                timestamp: chrono::DateTime<chrono::Utc>,
            }

            impl super::Table for [<$name_snake:camel Table>] {
                type CsvRow<'a> = $csv_ty;
                type Row<'a> = $row_result;
                type Index = [<$name_snake:camel Table Index>];

                const CSV_NAME: &'static str = concat!(stringify!($name_snake), ".csv");
                const TABLE_NAME: &'static str = concat!(stringify!($name_snake), ".table");

                fn write_row($csv_param: &Self::CsvRow<'_>, $writer: &mut super::RowWriter<impl std::io::Write>) -> crate::Result<()>
                    $write_body

                fn read_row<'a>($reader: &mut super::RowReader<'a>) -> Self::Row<'a>
                    $read_body

                fn open_with(mmap: mmap_rs::Mmap, max_ttl: core::time::Duration, now: chrono::DateTime<chrono::Utc>) -> crate::Result<Self> {
                    let (count, timestamp) = super::validate_table_header(&mmap, max_ttl, now)?;
                    Ok(Self { mmap, count, timestamp })
                }

                fn iter(&self) -> impl Iterator<Item = (Self::Row<'_>, Self::Index)> {
                    super::RowIter::new(
                        super::RowReader::new(&self.mmap[super::TABLE_HEADER_SIZE..]),
                        Self::read_row,
                        self.count
                    )
                }

                fn get(&self, index: Self::Index) -> Self::Row<'_> {
                    let mut reader = super::RowReader::new(&self.mmap[super::TABLE_HEADER_SIZE + index.0..]);
                    Self::read_row(&mut reader)
                }

                fn timestamp(&self) -> chrono::DateTime<chrono::Utc> {
                    self.timestamp
                }
            }
        }
    };
}

/// Generates both CSV row and table row structs from a single row definition.
///
/// Creates:
/// - `Csv{RowName}` - Struct for deserializing from CSV, with all fields as `&'a str`
/// - `{RowName}` - Table row struct with the specified field types
///
/// The macro has two variants:
/// - With `<'a>` lifetime parameter - generates `#[derive(Clone)]` row struct
/// - Without lifetime - generates `#[derive(Clone, Copy)]` row struct
///
/// See `categories_table.rs`, `users_table.rs`, or `versions_table.rs` for usage examples.
macro_rules! define_rows {
    (
        $row_name:ident<'a> {
            $(
                $(#[$field_meta:meta])*
                $vis:vis $field:ident: $field_type:ty
            ),* $(,)?
        }
    ) => {
        pastey::paste! {
            #[derive(Debug, serde::Deserialize)]
            pub struct [<Csv $row_name>]<'a> {
                $(
                    $(#[$field_meta])*
                    #[serde(borrow)]
                    $field: &'a str,
                )*
            }
        }

        #[derive(Debug, Clone)]
        pub struct $row_name<'a> {
            $(
                $(#[$field_meta])*
                $vis $field: $field_type,
            )*
        }
    };

    // Variant for rows without lifetimes
    (
        $row_name:ident {
            $(
                $(#[$field_meta:meta])*
                $vis:vis $field:ident: $field_type:ty
            ),* $(,)?
        }
    ) => {
        pastey::paste! {
            #[derive(Debug, serde::Deserialize)]
            pub struct [<Csv $row_name>]<'a> {
                $(
                    $(#[$field_meta])*
                    #[serde(borrow)]
                    $field: &'a str,
                )*
            }
        }

        #[derive(Debug, Clone, Copy)]
        pub struct $row_name {
            $(
                $(#[$field_meta])*
                $vis $field: $field_type,
            )*
        }
    };
}

pub(crate) use define_rows;
pub(crate) use define_table;

pub fn validate_table_header(mmap: &Mmap, max_ttl: Duration, now: DateTime<Utc>) -> Result<(u64, DateTime<Utc>)> {
    use ohno::bail;

    if mmap.len() < TABLE_HEADER_SIZE {
        bail!("invalid table: file too short (need at least 24 bytes for header)");
    }
    assert!(mmap.len() >= TABLE_HEADER_SIZE, "Length check above guarantees at least 24 bytes");
    assert!(mmap.len() > 23, "mmap length sufficient for all header indexing operations");

    // Validate format magic identifier
    let magic = read_header_u64(mmap, 0);
    if magic != FORMAT_MAGIC {
        bail!("invalid table format: expected magic 0x{FORMAT_MAGIC:016X}, found 0x{magic:016X}. Database may need regeneration.");
    }

    // Read row count
    let count = read_header_u64(mmap, 8);

    // Read and validate creation timestamp
    let table_timestamp = read_header_u64(mmap, 16);

    // Check TTL
    let now_secs = now.timestamp().max(0).cast_unsigned();
    let age_seconds = now_secs.saturating_sub(table_timestamp);
    let age = Duration::from_secs(age_seconds);

    if age > max_ttl {
        bail!("table is stale: age {}s exceeds TTL {}s", age.as_secs(), max_ttl.as_secs());
    }

    let dt = Utc
        .timestamp_opt(i64::try_from(table_timestamp).into_app_err("timestamp out of range for i64")?, 0)
        .single()
        .ok_or_else(|| ohno::app_err!("invalid or out-of-range timestamp"))?;

    Ok((count, dt))
}

fn read_header_u64(mmap: &Mmap, offset: usize) -> u64 {
    let end = offset + size_of::<u64>();
    let bytes: [u8; size_of::<u64>()] = mmap[offset..end]
        .try_into()
        .expect("header length validation guarantees a complete u64");
    u64::from_le_bytes(bytes)
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::fs;
    use std::io::{Cursor, Error, ErrorKind};

    use chrono::TimeZone as _;
    use serde::Deserialize;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn table_open_returns_metadata_failure_after_the_file_was_successfully_opened() {
        let error = open_table_with::<TestTable, _>(
            Path::new("tables"),
            Duration::ZERO,
            DateTime::UNIX_EPOCH,
            |path| {
                assert_eq!(path, Path::new("tables").join(TestTable::TABLE_NAME));
                Ok("opened table")
            },
            |file| {
                assert_eq!(*file, "opened table");
                Err(Error::new(ErrorKind::PermissionDenied, "injected table metadata failure"))
            },
            |_, _| panic!("mapping must not run after metadata failure"),
        )
        .unwrap_err();
        assert_eq!(error.message(), "injected table metadata failure");
        assert_eq!(error.find_source::<Error>().unwrap().kind(), ErrorKind::PermissionDenied);
        assert_eq!(
            error.to_string().lines().nth(1).unwrap().split_once(" (at ").unwrap().0,
            "> getting file metadata"
        );
    }

    #[derive(Deserialize)]
    struct TestCsvRow<'a> {
        value: &'a str,
    }

    #[derive(Debug)]
    struct TestTable;

    impl Table for TestTable {
        type CsvRow<'a> = TestCsvRow<'a>;
        type Row<'a> = &'a str;
        type Index = usize;

        const CSV_NAME: &'static str = "test.csv";
        const TABLE_NAME: &'static str = "test.table";

        fn write_row(csv_row: &Self::CsvRow<'_>, writer: &mut RowWriter<impl Write>) -> Result<()> {
            if csv_row.value == "reject" {
                ohno::bail!("synthetic row conversion failure");
            }
            writer.write_str(csv_row.value);
            Ok(())
        }

        fn read_row<'a>(reader: &mut RowReader<'a>) -> Self::Row<'a> {
            reader.read_str()
        }

        fn open_with(_mmap: Mmap, _max_ttl: Duration, _now: DateTime<Utc>) -> Result<Self> {
            Ok(Self)
        }

        fn iter(&self) -> impl Iterator<Item = (Self::Row<'_>, Self::Index)> {
            core::iter::empty()
        }

        fn get(&self, _index: Self::Index) -> Self::Row<'_> {
            ""
        }

        fn timestamp(&self) -> DateTime<Utc> {
            DateTime::UNIX_EPOCH
        }
    }

    struct FailingIo;

    impl Write for FailingIo {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(Error::other("synthetic table write failure"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(Error::other("synthetic table flush failure"))
        }
    }

    impl Seek for FailingIo {
        fn seek(&mut self, _pos: SeekFrom) -> std::io::Result<u64> {
            Err(Error::other("synthetic table seek failure"))
        }
    }

    impl IoRead for FailingIo {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            panic!("a failed rewind must prevent reading table data")
        }
    }

    #[test]
    fn table_snapshot_clone_and_rewind_failures_preserve_context_without_panicking() {
        for (reader, expected_context, expected_cause) in [
            (
                Err(Error::new(ErrorKind::PermissionDenied, "synthetic table clone failure")),
                "cloning table file handle",
                "synthetic table clone failure",
            ),
            (Ok(FailingIo), "rewinding table file", "synthetic table seek failure"),
        ] {
            let error = map_table_file_with(
                reader,
                3,
                |_size| -> std::io::Result<()> { panic!("reader failure must prevent creating mapping options") },
                |()| -> std::io::Result<Vec<u8>> { panic!("reader failure must prevent allocating a mapping") },
                |_bytes| -> std::io::Result<()> { panic!("reader failure must prevent protecting a mapping") },
            )
            .expect_err("reader setup must return its IO error");
            assert_eq!(error.message(), expected_cause);
            let display = error.to_string();
            let context = display.lines().nth(1).unwrap().split_once(" (at ").unwrap().0;
            assert_eq!(context, format!("> {expected_context}"));
            assert_eq!(
                error.find_source::<Error>().unwrap().kind(),
                if expected_context == "cloning table file handle" {
                    ErrorKind::PermissionDenied
                } else {
                    ErrorKind::Other
                }
            );
        }
    }

    #[derive(Debug)]
    struct ScriptedIo {
        writes_before_failure: Option<usize>,
        fail_flush: bool,
    }

    impl ScriptedIo {
        const fn fail_write_after(successful_writes: usize) -> Self {
            Self {
                writes_before_failure: Some(successful_writes),
                fail_flush: false,
            }
        }

        const fn fail_flush() -> Self {
            Self {
                writes_before_failure: None,
                fail_flush: true,
            }
        }
    }

    impl Write for ScriptedIo {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Some(remaining) = &mut self.writes_before_failure {
                if *remaining == 0 {
                    return Err(Error::other("synthetic staged write failure"));
                }
                *remaining -= 1;
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            if self.fail_flush {
                Err(Error::other("synthetic staged flush failure"))
            } else {
                Ok(())
            }
        }
    }

    impl Seek for ScriptedIo {
        fn seek(&mut self, _pos: SeekFrom) -> std::io::Result<u64> {
            Ok(0)
        }
    }

    struct HeaderThenError {
        header_sent: bool,
    }

    impl IoRead for HeaderThenError {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.header_sent {
                return Err(Error::other("synthetic CSV read failure"));
            }
            self.header_sent = true;
            let header = b"value\n";
            buf[..header.len()].copy_from_slice(header);
            Ok(header.len())
        }
    }

    fn map_header(bytes: &[u8]) -> (TempDir, Mmap) {
        let dir = TempDir::new().expect("creating a temporary directory");
        let path = dir.path().join("header.table");
        fs::write(&path, bytes).expect("writing table header");
        let file = File::open(&path).expect("opening table header");
        // SAFETY: The file is created in this test's private temporary directory, opened
        // read-only, and never written again after mapping. The returned `TempDir` keeps the
        // backing file alive for at least as long as the returned mapping.
        let mmap = unsafe {
            MmapOptions::new(bytes.len())
                .expect("valid mmap size")
                .with_flags(MmapFlags::SEQUENTIAL)
                .with_file(&file, 0)
                .map()
                .expect("mapping table header")
        };
        (dir, mmap)
    }

    fn header(count: u64, timestamp: u64) -> [u8; TABLE_HEADER_SIZE] {
        let mut bytes = [0u8; TABLE_HEADER_SIZE];
        bytes[0..8].copy_from_slice(&FORMAT_MAGIC.to_le_bytes());
        bytes[8..16].copy_from_slice(&count.to_le_bytes());
        bytes[16..24].copy_from_slice(&timestamp.to_le_bytes());
        bytes
    }

    #[test]
    fn exactly_sized_table_headers_are_valid() {
        let now = Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap();
        let bytes = header(42, now.timestamp().cast_unsigned());
        let (_dir, mmap) = map_header(&bytes);

        let (count, timestamp) = validate_table_header(&mmap, Duration::from_mins(1), now).expect("header is exactly complete");

        assert_eq!(count, 42);
        assert_eq!(timestamp, now);
    }

    #[test]
    fn table_age_equal_to_ttl_is_still_fresh() {
        let now = Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap();
        let table_timestamp = (now - chrono::TimeDelta::seconds(60)).timestamp().cast_unsigned();
        let bytes = header(7, table_timestamp);
        let (_dir, mmap) = map_header(&bytes);

        let (count, timestamp) = validate_table_header(&mmap, Duration::from_mins(1), now).expect("age equal to TTL is allowed");

        assert_eq!(count, 7);
        assert_eq!(timestamp, now - chrono::TimeDelta::seconds(60));
    }

    #[test]
    fn pre_epoch_clock_is_clamped_to_the_epoch() {
        let now = Utc.timestamp_opt(-1, 0).unwrap();
        let bytes = header(1, 0);
        let (_dir, mmap) = map_header(&bytes);
        let (_, timestamp) = validate_table_header(&mmap, Duration::ZERO, now).expect("a pre-epoch clock cannot make an epoch table stale");
        assert_eq!(timestamp, DateTime::UNIX_EPOCH);
    }

    #[test]
    fn invalid_magic_is_rejected() {
        let now = Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap();
        let bytes = [0u8; TABLE_HEADER_SIZE];
        let (_dir, mmap) = map_header(&bytes);

        let error = validate_table_header(&mmap, Duration::ZERO, now).expect_err("zero is not the table magic");

        assert!(format!("{error:#}").contains("invalid table format"));
    }

    #[test]
    fn out_of_range_timestamp_is_returned_with_context() {
        let bytes = header(1, u64::MAX);
        let (_dir, mmap) = map_header(&bytes);

        let error = validate_table_header(&mmap, Duration::MAX, DateTime::<Utc>::MAX_UTC)
            .expect_err("u64::MAX cannot be represented as an i64 timestamp");

        assert!(format!("{error:#}").contains("timestamp out of range for i64"));
    }

    #[test]
    fn chrono_rejects_an_i64_timestamp_outside_its_calendar_range() {
        let bytes = header(1, i64::MAX.cast_unsigned());
        let (_dir, mmap) = map_header(&bytes);

        let error = validate_table_header(&mmap, Duration::MAX, DateTime::<Utc>::MAX_UTC)
            .expect_err("chrono cannot represent the largest i64 timestamp");

        assert!(format!("{error:#}").contains("invalid or out-of-range timestamp"));
    }

    #[test]
    fn metadata_errors_are_returned_with_context() {
        let error = table_file_size(Err(Error::new(ErrorKind::PermissionDenied, "synthetic metadata failure")))
            .expect_err("metadata failures must be returned");

        assert!(format!("{error:#}").contains("getting file metadata"));
    }

    #[test]
    fn opening_an_empty_table_returns_the_mapping_error_with_context() {
        let dir = TempDir::new().expect("creating a temporary directory");
        fs::write(dir.path().join(TestTable::TABLE_NAME), []).expect("creating an empty table");

        let error =
            TestTable::open(dir.path(), Duration::ZERO, DateTime::UNIX_EPOCH).expect_err("an empty file cannot be opened as a table");

        let diagnostic = format!("{error:#}");
        assert!(
            diagnostic.contains("creating memory-map options") || diagnostic.contains("allocating table memory"),
            "{diagnostic}"
        );
    }

    #[test]
    fn creating_a_table_returns_path_errors_with_context() {
        let dir = TempDir::new().expect("creating a temporary directory");
        let missing_root = dir.path().join("missing");

        let error =
            TestTable::create_table(&missing_root, &b"value\n"[..], DateTime::UNIX_EPOCH).expect_err("the table root does not exist");

        assert!(format!("{error:#}").contains("creating table file"));
        assert!(format!("{error:#}").contains(TestTable::TABLE_NAME));
    }

    #[test]
    fn recreating_a_table_truncates_existing_contents() {
        let dir = TempDir::new().expect("creating a temporary directory");
        let path = dir.path().join(TestTable::TABLE_NAME);
        fs::write(&path, vec![0xAA; 4096]).expect("writing old table contents");

        drop(TestTable::create_table(dir.path(), &b"value\n"[..], DateTime::UNIX_EPOCH).expect("creating the replacement table"));

        assert_eq!(
            fs::metadata(path).expect("reading replacement metadata").len(),
            u64::try_from(TABLE_HEADER_SIZE + 10).expect("small fixed size fits in u64")
        );
    }

    #[test]
    fn loaded_table_bytes_do_not_depend_on_the_backing_file_after_open() {
        let file = tempfile::tempfile().expect("temporary file");
        file.set_len(4).expect("size temporary file");
        (&file).write_all(b"data").expect("write temporary file");

        let mmap = map_table_file(&file, 4).expect("load table bytes");
        file.set_len(0).expect("truncate backing file");

        assert_eq!(&*mmap, b"data");
    }

    #[test]
    fn creating_a_table_writes_exact_count_timestamp_and_zero_padding() {
        let dir = TempDir::new().expect("creating a temporary directory");
        let now = Utc.with_ymd_and_hms(2026, 9, 28, 12, 34, 56).unwrap();

        drop(TestTable::create_table(dir.path(), &b"value\nhello\n"[..], now).expect("creating the table"));
        let bytes = fs::read(dir.path().join(TestTable::TABLE_NAME)).expect("reading the table");

        assert_eq!(u64::from_le_bytes(bytes[8..16].try_into().unwrap()), 1);
        assert_eq!(
            u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
            now.timestamp().cast_unsigned()
        );
        assert_eq!(&bytes[bytes.len() - 10..], &[0; 10]);
    }

    #[test]
    fn creating_a_table_clamps_pre_epoch_timestamps_to_zero() {
        let dir = TempDir::new().expect("creating a temporary directory");
        let before_epoch = Utc.timestamp_opt(-1, 0).unwrap();

        drop(TestTable::create_table(dir.path(), &b"value\n"[..], before_epoch).expect("creating the table"));
        let bytes = fs::read(dir.path().join(TestTable::TABLE_NAME)).expect("reading the table");

        assert_eq!(u64::from_le_bytes(bytes[16..24].try_into().unwrap()), 0);
    }

    #[test]
    fn table_creation_returns_csv_and_row_conversion_errors() {
        let dir = TempDir::new().expect("creating a temporary directory");

        let error = TestTable::create_table(dir.path(), &b"value\n\xFF\n"[..], DateTime::UNIX_EPOCH)
            .expect_err("invalid UTF-8 in the header must be rejected");
        assert!(format!("{error:#}").contains("reading CSV"));

        let error = write_table_contents::<TestTable>(
            &mut Cursor::new(Vec::new()),
            HeaderThenError { header_sent: false },
            DateTime::UNIX_EPOCH,
        )
        .expect_err("reader failures after the header must be returned");
        assert!(format!("{error:#}").contains("reading CSV row"));

        let error = write_table_contents::<TestTable>(&mut Cursor::new(Vec::new()), &b"other\nvalue\n"[..], DateTime::UNIX_EPOCH)
            .expect_err("missing required fields must fail deserialization");
        assert!(format!("{error:#}").contains("deserializing CSV row"));

        let error = TestTable::create_table(dir.path(), &b"value\nreject\n"[..], DateTime::UNIX_EPOCH)
            .expect_err("row conversion failures must be returned");
        assert!(format!("{error:#}").contains("converting CSV row"));
        assert!(format!("{error:#}").contains("synthetic row conversion failure"));
    }

    #[test]
    fn table_write_helpers_return_exact_io_contexts() {
        let placeholder_error = write_table_header_placeholder(&mut FailingIo).expect_err("placeholder writes must return errors");
        assert!(format!("{placeholder_error:#}").contains("writing table header placeholder"));
        assert!(format!("{placeholder_error:#}").contains("synthetic table write failure"));

        let finish_error = finish_table_header(&mut FailingIo, 1, 2).expect_err("header finalization must return errors");
        assert!(format!("{finish_error:#}").contains("synthetic table seek failure"));

        for (successful_writes, expected) in [
            (0, "writing table magic"),
            (1, "writing table row count"),
            (2, "writing table timestamp"),
        ] {
            let error = finish_table_header(&mut ScriptedIo::fail_write_after(successful_writes), 1, 2)
                .expect_err("the selected header write must fail");
            assert!(format!("{error:#}").contains(expected), "{error:#}");
        }

        let error = finish_table_header(&mut ScriptedIo::fail_flush(), 1, 2).expect_err("header flushing must fail");
        assert!(format!("{error:#}").contains("flushing table header"));
    }

    #[test]
    fn table_content_writes_return_stage_contexts() {
        let error = write_table_contents::<TestTable>(&mut FailingIo, &b"value\nhello\n"[..], DateTime::UNIX_EPOCH)
            .expect_err("the initial placeholder write must be propagated by table construction");
        assert!(format!("{error:#}").contains("writing table header placeholder"));

        let error = write_table_contents::<TestTable>(&mut ScriptedIo::fail_write_after(1), &b"value\nhello\n"[..], DateTime::UNIX_EPOCH)
            .expect_err("writing the first row must fail");
        assert!(format!("{error:#}").contains("writing table row"));

        let error = write_table_contents::<TestTable>(&mut ScriptedIo::fail_write_after(2), &b"value\nhello\n"[..], DateTime::UNIX_EPOCH)
            .expect_err("writing padding must fail");
        assert!(format!("{error:#}").contains("writing table padding"));

        let error = write_table_contents::<TestTable>(&mut ScriptedIo::fail_write_after(3), &b"value\nhello\n"[..], DateTime::UNIX_EPOCH)
            .expect_err("finalizing the header must fail");
        assert!(format!("{error:#}").contains("finalizing table header"));
        assert!(format!("{error:#}").contains("writing table magic"));
    }

    #[test]
    fn buffered_writer_flush_errors_have_exact_context() {
        let mut writer = BufWriter::new(ScriptedIo::fail_write_after(0));
        writer.write_all(b"pending").expect("the buffered write does not flush yet");

        let error = finish_buffered_writer(writer).expect_err("flushing the buffered data must fail");
        assert!(format!("{error:#}").contains("flushing table data"));
    }

    #[test]
    fn table_header_and_snapshot_errors_are_propagated_at_the_call_sites() {
        let error = write_table_contents::<TestTable>(&mut Cursor::new(Vec::new()), &b"\xFF\n"[..], DateTime::UNIX_EPOCH)
            .expect_err("invalid UTF-8 in the CSV header must be rejected");
        assert!(format!("{error:#}").contains("reading CSV headers"), "{error:#}");

        for (stage, expected, cause, kind) in [
            (
                0,
                "creating memory-map options",
                "scripted options failure",
                std::io::ErrorKind::Other,
            ),
            (
                1,
                "allocating table memory",
                "scripted allocation failure",
                std::io::ErrorKind::Other,
            ),
            (
                2,
                "reading table file",
                "failed to fill whole buffer",
                std::io::ErrorKind::UnexpectedEof,
            ),
            (
                3,
                "protecting table memory",
                "scripted protection failure",
                std::io::ErrorKind::Other,
            ),
        ] {
            let file_size = if stage == 3 { 2 } else { 3 };
            let calls = std::cell::RefCell::new(Vec::new());
            let error = map_table_file_with(
                Ok(Cursor::new([1_u8, 2])),
                file_size,
                |size| {
                    assert_eq!(size, file_size);
                    calls.borrow_mut().push("options");
                    if stage == 0 {
                        Err(Error::other("scripted options failure"))
                    } else {
                        Ok(size)
                    }
                },
                |size| {
                    assert_ne!(stage, 0, "options failure must stop allocation");
                    assert_eq!(size, file_size);
                    calls.borrow_mut().push("allocate");
                    if stage == 1 {
                        Err(Error::other("scripted allocation failure"))
                    } else {
                        Ok(vec![0; size])
                    }
                },
                |bytes| {
                    assert_eq!(stage, 3, "earlier failure must stop protection");
                    assert_eq!(bytes, [1, 2]);
                    calls.borrow_mut().push("protect");
                    Err::<(), _>(Error::other("scripted protection failure"))
                },
            )
            .expect_err("the selected snapshot stage must fail");
            assert_eq!(error.message(), cause);
            assert_eq!(
                error.find_source::<Error>().expect("the scripted IO error is retained").kind(),
                kind
            );
            let display = error.to_string();
            let context = display.lines().nth(1).expect("the error carries its stage context");
            let (context, _location) = context
                .split_once(" (at ")
                .expect("ohno annotates context with its source location");
            assert_eq!(context, format!("> {expected}"));
            assert_eq!(
                calls.into_inner(),
                match stage {
                    0 => vec!["options"],
                    1 | 2 => vec!["options", "allocate"],
                    3 => vec!["options", "allocate", "protect"],
                    _ => unreachable!(),
                }
            );
        }
    }
}
