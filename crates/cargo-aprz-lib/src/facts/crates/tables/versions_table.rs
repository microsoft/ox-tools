// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use compact_str::CompactString;
use ohno::IntoAppError;
use semver::Version;
use url::Url;

use super::super::rust_edition::RustEdition;
use super::{CrateId, VersionId, define_rows, define_table};

/// Log target for versions table
const LOG_TARGET: &str = "    crates";

define_rows! {
    VersionRow<'a> {
        pub id: VersionId,
        #[allow(dead_code)]
        pub crate_id: CrateId,
        // Read via `get_query` during scanning; the full row is only used for enrichment.
        #[allow(dead_code)]
        pub num: Version,
        pub downloads: u64,
        edition: Option<u64>,
        pub created_at: DateTime<Utc>,
        pub updated_at: DateTime<Utc>,
        pub description: &'a str,
        features: &'a str,
        pub license: &'a str,
        pub rust_version: &'a str,
        pub yanked: bool,
        documentation: &'a str,
        homepage: &'a str,
    }
}

impl VersionRow<'_> {
    /// # Panics
    ///
    /// Panics if the features JSON in the database is malformed
    #[must_use]
    pub fn features(&self) -> BTreeMap<CompactString, Vec<CompactString>> {
        serde_json::from_str(self.features).expect("invalid data in features field")
    }

    /// # Panics
    ///
    /// Panics if the homepage URL in the database is malformed
    #[must_use]
    pub fn homepage(&self) -> Option<Url> {
        if self.homepage.is_empty() {
            None
        } else {
            Some(Url::parse(self.homepage).expect("invalid URL in homepage field"))
        }
    }

    /// # Panics
    ///
    /// Panics if the documentation URL in the database is malformed
    #[must_use]
    pub fn documentation(&self) -> Option<Url> {
        if self.documentation.is_empty() {
            None
        } else {
            Some(Url::parse(self.documentation).expect("invalid URL in documentation field"))
        }
    }

    #[must_use]
    pub const fn edition(&self) -> Option<RustEdition> {
        match self.edition {
            None => None,
            Some(2015) => Some(RustEdition::Edition2015),
            Some(2018) => Some(RustEdition::Edition2018),
            Some(2021) => Some(RustEdition::Edition2021),
            Some(2024) => Some(RustEdition::Edition2024),
            _ => Some(RustEdition::Unknown),
        }
    }
}

define_table! {
    versions {
        fn write_row(csv_row: &CsvVersionRow<'a>, writer: &mut RowWriter<impl Write>) -> Result<()> {
            let _ = serde_json::from_str::<BTreeMap<CompactString, Vec<CompactString>>>(csv_row.features).into_app_err("parsing feature map")?;

            writer.write_str_as_u64(csv_row.id)?;
            writer.write_str_as_u64(csv_row.crate_id)?;
            writer.write_str_as_version(csv_row.num)?;
            writer.write_str_as_u64(csv_row.downloads)?;
            writer.write_optional_str_as_u64(csv_row.edition)?;
            writer.write_str_as_datetime(csv_row.created_at)?;
            writer.write_str_as_datetime(csv_row.updated_at)?;
            writer.write_str(csv_row.description);
            writer.write_str(csv_row.features);
            writer.write_str(csv_row.license);
            writer.write_str(csv_row.rust_version);
            writer.write_str_as_bool(csv_row.yanked)?;

            if let Err(e) = writer.write_str_as_url(csv_row.documentation) {
                log::debug!(target: LOG_TARGET,
                    "invalid documentation URL in versions table for version {} (crate '{}'): {}",
                    csv_row.num,
                    csv_row.crate_id,
                    e
                );
                writer.write_str("");
            }

            if let Err(e) = writer.write_str_as_url(csv_row.homepage) {
                log::debug!(target: LOG_TARGET,
                    "invalid homepage URL in versions table for version {} (crate '{}'): {}",
                    csv_row.num,
                    csv_row.crate_id,
                    e
                );
                writer.write_str("");
            }

            Ok(())
        }

        fn read_row<'a>(reader: &mut RowReader<'a>) -> VersionRow<'a> {
            VersionRow {
                id: VersionId(reader.read_u64()),
                crate_id: CrateId(reader.read_u64()),
                num: reader.read_version(),
                downloads: reader.read_u64(),
                edition: reader.read_optional_u64(),
                created_at: reader.read_datetime(),
                updated_at: reader.read_datetime(),
                description: reader.read_str(),
                features: reader.read_str(),
                license: reader.read_str(),
                rust_version: reader.read_str(),
                yanked: reader.read_bool(),
                documentation: reader.read_str(),
                homepage: reader.read_str(),
            }
        }
    }
}

/// Lean version row containing only the fields needed for phase 4 scanning.
///
/// The full `VersionRow` has 14+ fields including large strings (description, features JSON, etc.)
/// that require expensive UTF-8 validation. This lean variant reads only `id` and `crate_id`,
/// skipping all other fields. For the `~0.1%` of rows belonging to queried crates, the caller
/// uses `get(index)` to do a full read.
#[derive(Debug, Clone, Copy)]
pub struct VersionRowLean {
    pub id: VersionId,
    pub crate_id: CrateId,
}

/// Reads only `id` and `crate_id` from a version row, skipping all remaining fields.
fn read_row_lean(reader: &mut super::RowReader<'_>) -> VersionRowLean {
    let id = VersionId(reader.read_u64());
    let crate_id = CrateId(reader.read_u64());
    reader.skip_version(); // num
    reader.skip_u64(); // downloads
    reader.skip_optional_u64(); // edition
    reader.skip_datetime(); // created_at
    reader.skip_datetime(); // updated_at
    reader.skip_str(); // description
    reader.skip_str(); // features
    reader.skip_str(); // license
    reader.skip_str(); // rust_version
    reader.skip_bool(); // yanked
    reader.skip_str(); // documentation
    reader.skip_str(); // homepage
    VersionRowLean { id, crate_id }
}

/// Version row holding only the fields phase 4 needs from a row it has matched.
///
/// A full read validates and returns every string field, including the `features` JSON, which
/// can run to kilobytes per row. Phase 4 matches a row on its crate and then only looks at the
/// version number, creation time and yanked flag, so it reads just those.
#[derive(Debug, Clone)]
pub struct VersionRowQuery {
    pub num: Version,
    pub created_at: DateTime<Utc>,
    pub yanked: bool,
}

/// Reads the query fields of a version row, skipping the rest.
///
/// The field order must match `read_row`.
fn read_row_query(reader: &mut super::RowReader<'_>) -> VersionRowQuery {
    reader.skip_u64(); // id
    reader.skip_u64(); // crate_id
    let num = reader.read_version();
    reader.skip_u64(); // downloads
    reader.skip_optional_u64(); // edition
    let created_at = reader.read_datetime();
    reader.skip_datetime(); // updated_at
    reader.skip_str(); // description
    reader.skip_str(); // features
    reader.skip_str(); // license
    reader.skip_str(); // rust_version
    let yanked = reader.read_bool();

    VersionRowQuery { num, created_at, yanked }
}

impl VersionsTable {
    /// Reads only the fields needed while scanning for matches.
    ///
    /// See [`VersionRowQuery`] for why this exists.
    pub fn get_query(&self, index: VersionsTableIndex) -> VersionRowQuery {
        let mut reader = super::RowReader::new(&self.mmap[super::TABLE_HEADER_SIZE + index.0..]);
        read_row_query(&mut reader)
    }

    /// Returns a lean iterator that only deserializes `id` and `crate_id` per row,
    /// skipping all string fields (no UTF-8 validation) and version parsing.
    pub fn iter_lean(&self) -> impl Iterator<Item = (VersionRowLean, VersionsTableIndex)> {
        super::RowIter::new(
            super::RowReader::new(&self.mmap[super::TABLE_HEADER_SIZE..]),
            read_row_lean,
            self.count,
        )
    }
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use chrono::TimeZone as _;

    use super::*;

    #[test]
    fn edition_maps_rust_2021_explicitly() {
        let row = VersionRow {
            id: VersionId(1),
            crate_id: CrateId(2),
            num: semver::Version::parse("1.2.3").expect("valid version literal"),
            downloads: 3,
            edition: Some(2021),
            created_at: Utc.timestamp_opt(1, 0).single().expect("valid timestamp"),
            updated_at: Utc.timestamp_opt(2, 0).single().expect("valid timestamp"),
            description: "description",
            features: "{}",
            license: "MIT",
            rust_version: "1.70",
            yanked: false,
            documentation: "",
            homepage: "",
        };

        assert_eq!(row.edition(), Some(RustEdition::Edition2021));
    }
}
