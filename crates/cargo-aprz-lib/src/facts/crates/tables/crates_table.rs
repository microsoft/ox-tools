// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use chrono::{DateTime, Utc};
use url::Url;

use super::{CrateId, define_rows, define_table};

/// Log target for crates table
const LOG_TARGET: &str = "    crates";

define_rows! {
    CrateRow<'a> {
        pub id: CrateId,
        pub name: &'a str,
        pub created_at: DateTime<Utc>,
        pub updated_at: DateTime<Utc>,
        repository: &'a str,
    }
}

impl CrateRow<'_> {
    /// # Panics
    ///
    /// Panics if the repository URL in the database is malformed
    #[must_use]
    pub fn repository(&self) -> Option<Url> {
        if self.repository.is_empty() {
            None
        } else {
            Some(Url::parse(self.repository).expect("invalid URL in repository field"))
        }
    }
}

define_table! {
    crates {
        fn write_row(csv_row: &CsvCrateRow<'a>, writer: &mut RowWriter<impl Write>) -> Result<()> {
            writer.write_str(csv_row.name);
            writer.write_str_as_u64(csv_row.id)?;
            writer.write_str_as_datetime(csv_row.created_at)?;

            if let Err(e) = writer.write_str_as_url(csv_row.repository) {
                log::debug!(target: LOG_TARGET,
                    "invalid repository URL in crates table for crate '{}': {}",
                    csv_row.name,
                    e
                );
                writer.write_str("");
            }
            writer.write_str_as_datetime(csv_row.updated_at)?;

            Ok(())
        }

        fn read_row<'a>(reader: &mut RowReader<'a>) -> CrateRow<'a> {
            CrateRow {
                name: reader.read_str(),
                id: CrateId(reader.read_u64()),
                created_at: reader.read_datetime(),
                repository: reader.read_str(),
                updated_at: reader.read_datetime(),
            }
        }
    }
}
