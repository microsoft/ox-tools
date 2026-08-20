// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use ohno::bail;

use super::{CrateId, TeamId, UserId, define_rows, define_table};

define_rows! {
    CrateOwnerRow {
        pub crate_id: CrateId,
        owner_kind: u64,
        owner_id: u64,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerKind {
    User(UserId),
    Team(TeamId),
}

impl CrateOwnerRow {
    /// The `owner_kind` column is validated when the table is written (`write_row` below rejects
    /// anything other than `0` or `1`), so the final arm cannot be reached from a table this
    /// crate produced; coverage is turned off rather than fabricating a corrupt row for it.
    #[cfg_attr(coverage_nightly, coverage(off))]
    #[must_use]
    pub fn owner(&self) -> OwnerKind {
        match self.owner_kind {
            0 => OwnerKind::User(UserId(self.owner_id)),
            1 => OwnerKind::Team(TeamId(self.owner_id)),
            _ => unreachable!("invalid owner_kind: {}", self.owner_kind),
        }
    }
}

define_table! {
    crate_owners {
        fn write_row(csv_row: &CsvCrateOwnerRow<'a>, writer: &mut RowWriter<impl Write>) -> Result<()> {
            if csv_row.owner_kind != "0" && csv_row.owner_kind != "1" {
                bail!("invalid owner_kind: {}", csv_row.owner_kind);
            }

            writer.write_str_as_u64(csv_row.crate_id)?;
            writer.write_str_as_u64(csv_row.owner_kind)?;
            writer.write_str_as_u64(csv_row.owner_id)?;

            Ok(())
        }

        fn read_row<'a>(reader: &mut RowReader<'a>) -> CrateOwnerRow {
            CrateOwnerRow {
                crate_id: CrateId(reader.read_u64()),
                owner_kind: reader.read_u64(),
                owner_id: reader.read_u64(),
            }
        }
    }
}
