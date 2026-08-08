// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::{TeamId, define_rows, define_table};
define_rows! {
    TeamRow<'a> {
        pub id: TeamId,
        pub login: &'a str,
        pub name: &'a str,
    }
}

define_table! {
    teams {
        fn write_row(csv_row: &CsvTeamRow<'a>, writer: &mut RowWriter<impl Write>) -> Result<()> {
            writer.write_str_as_u64(csv_row.id)?;
            writer.write_str(csv_row.login);
            writer.write_str(csv_row.name);

            Ok(())
        }

        fn read_row<'a>(reader: &mut RowReader<'a>) -> TeamRow<'a> {
            TeamRow {
                id: TeamId(reader.read_u64()),
                login: reader.read_str(),
                name: reader.read_str(),
            }
        }
    }
}
