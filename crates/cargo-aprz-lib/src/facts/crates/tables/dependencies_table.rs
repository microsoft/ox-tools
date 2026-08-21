// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::{CrateId, VersionId, define_rows, define_table};

define_rows! {
    DependencyRow {
        pub version_id: VersionId,
        pub crate_id: CrateId,
    }
}

define_table! {
    dependencies {
        fn write_row(csv_row: &CsvDependencyRow<'a>, writer: &mut RowWriter<impl Write>) -> Result<()> {
            writer.write_str_as_u64(csv_row.version_id)?;
            writer.write_str_as_u64(csv_row.crate_id)?;

            Ok(())
        }

        fn read_row<'a>(reader: &mut RowReader<'a>) -> DependencyRow {
            DependencyRow {
                version_id: VersionId(reader.read_u64()),
                crate_id: CrateId(reader.read_u64()),
            }
        }
    }
}
