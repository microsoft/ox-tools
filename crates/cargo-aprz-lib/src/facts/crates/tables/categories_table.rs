// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::{CategoryId, define_rows, define_table};

define_rows! {
    CategoryRow<'a> {
        pub id: CategoryId,
        pub slug: &'a str,
    }
}

define_table! {
    categories {
        fn write_row(csv_row: &CsvCategoryRow<'a>, writer: &mut RowWriter<impl Write>) -> Result<()> {
            writer.write_str_as_u64(csv_row.id)?;
            writer.write_str(csv_row.slug);

            Ok(())
        }

        fn read_row<'a>(reader: &mut RowReader<'a>) -> CategoryRow<'a> {
            CategoryRow {
                id: CategoryId(reader.read_u64()),
                slug: reader.read_str(),
            }
        }
    }
}
