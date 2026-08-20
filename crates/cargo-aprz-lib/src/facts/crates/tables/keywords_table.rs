// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::{KeywordId, define_rows, define_table};

define_rows! {
    KeywordRow<'a> {
        pub id: KeywordId,
        pub keyword: &'a str,
    }
}

define_table! {
    keywords {
        fn write_row(csv_row: &CsvKeywordRow<'a>, writer: &mut RowWriter<impl Write>) -> Result<()> {
            writer.write_str_as_u64(csv_row.id)?;
            writer.write_str(csv_row.keyword);

            Ok(())
        }

        fn read_row<'a>(reader: &mut RowReader<'a>) -> KeywordRow<'a> {
            KeywordRow {
                id: KeywordId(reader.read_u64()),
                keyword: reader.read_str(),
            }
        }
    }
}
