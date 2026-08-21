// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::{UserId, define_rows, define_table};

define_rows! {
    UserRow<'a> {
        pub id: UserId,
        pub gh_login: &'a str,
        pub name: &'a str,
    }
}

define_table! {
    users {
        fn write_row(csv_row: &CsvUserRow<'a>, writer: &mut RowWriter<impl Write>) -> Result<()> {
            writer.write_str_as_u64(csv_row.id)?;
            writer.write_str(csv_row.gh_login);
            writer.write_str(csv_row.name);

            Ok(())
        }

        fn read_row<'a>(reader: &mut RowReader<'a>) -> UserRow<'a> {
            UserRow {
                id: UserId(reader.read_u64()),
                gh_login: reader.read_str(),
                name: reader.read_str(),
            }
        }
    }
}
