// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Binary table infrastructure for crates.io database dump.
//!
//! This module provides efficient access to crates.io data via memory-mapped
//! binary tables. The main entry point is [`TableMgr`], which provides
//! methods to access individual tables.
//!
//! # Architecture Overview
//!
//! The tables infrastructure downloads the crates.io database dump (around 1 GB gzipped tarball
//! containing 15 CSV files), converts it to an optimized binary format, and provides
//! zero-copy access to those tables via memory-mapped files.
//!
//! When creating a `TableMgr`, it first attempts to open existing tables from disk.
//! If the tables are missing or stale (based on a configurable TTL), it streams
//! the download, decompresses it, extracts each CSV file, converts rows to binary,
//! and writes the binary tables to disk. Finally, it memory-maps the tables for
//! efficient access.
//!
//! This codebase tries to be as efficient as possible in terms of both speed and memory usage.
//! As the download is streamed off the network, it is decompressed and parsed line-by-line
//! on the fly and stored to disk in final form.
//!
//! # Binary Table Format
//!
//! Each table is stored as a binary file with the following structure:
//!
//! ```text
//! +------------------------------------------------------+
//! | FORMAT_MAGIC: u64 (8 bytes)                          |
//! |   - Magic number identifying format version          |
//! +------------------------------------------------------+
//! | ROW_COUNT: u64 (8 bytes)                             |
//! |   - Total number of rows in table                    |
//! +------------------------------------------------------+
//! | TIMESTAMP: u64 (8 bytes)                             |
//! |   - Unix epoch seconds when table was created        |
//! |   - Used for TTL validation                          |
//! +------------------------------------------------------+
//! | Row Data (variable length)                           |
//! |   - Variable-length encoded integers (vlen crate)    |
//! |   - Strings stored as length prefix + UTF-8 bytes    |
//! |   - Optional fields use discriminant byte            |
//! +------------------------------------------------------+
//! ```

mod categories_table;
mod crate_downloads_table;
mod crate_owners_table;
mod crates_categories_table;
mod crates_keywords_table;
mod crates_table;
mod dependencies_table;
mod ids;
mod keywords_table;
mod row_iter;
mod row_reader;
mod row_writer;
mod table;
mod table_mgr;
mod teams_table;
mod users_table;
mod version_downloads_table;
mod versions_table;

pub use categories_table::{CategoriesTable, CategoriesTableIndex};
pub use crate_downloads_table::CrateDownloadsTable;
pub use crate_owners_table::{CrateOwnersTable, OwnerKind};
pub use crates_categories_table::CratesCategoriesTable;
pub use crates_keywords_table::CratesKeywordsTable;
pub use crates_table::{CratesTable, CratesTableIndex};
pub use dependencies_table::DependenciesTable;
pub use ids::{CategoryId, CrateId, KeywordId, TeamId, UserId, VersionId};
pub use keywords_table::{KeywordsTable, KeywordsTableIndex};
pub use row_iter::RowIter;
use row_reader::RowReader;
use row_writer::RowWriter;
pub use table::Table;
use table::{TABLE_HEADER_SIZE, define_rows, define_table, validate_table_header};
pub use table_mgr::TableMgr;
pub use teams_table::{TeamsTable, TeamsTableIndex};
pub use users_table::{UsersTable, UsersTableIndex};
pub use version_downloads_table::VersionDownloadsTable;
pub use versions_table::{VersionsTable, VersionsTableIndex};
