// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::{UserId, define_table};

#[derive(Debug, serde::Deserialize)]
pub struct CsvUserRow<'a> {
    #[serde(borrow)]
    id: &'a str,
    #[serde(borrow, default)]
    username: Option<&'a str>,
    #[serde(borrow, default)]
    gh_login: Option<&'a str>,
    #[serde(borrow)]
    name: &'a str,
}

#[derive(Debug, Clone)]
pub struct UserRow<'a> {
    pub id: UserId,
    pub username: &'a str,
    pub name: &'a str,
}

define_table! {
    users {
        fn write_row(csv_row: &CsvUserRow<'a>, writer: &mut RowWriter<impl Write>) -> Result<()> {
            let username = csv_row.username.or(csv_row.gh_login).ok_or_else(|| {
                ohno::app_err!("users.csv is missing username (or legacy gh_login); use a compatible crates.io database dump")
            })?;
            writer.write_str_as_u64(csv_row.id)?;
            writer.write_str(username);
            writer.write_str(csv_row.name);

            Ok(())
        }

        fn read_row<'a>(reader: &mut RowReader<'a>) -> UserRow<'a> {
            UserRow {
                id: UserId(reader.read_u64()),
                username: reader.read_str(),
                name: reader.read_str(),
            }
        }
    }
}
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::super::{RowReader, RowWriter, Table};
    use super::*;

    fn read_user(csv: &str) -> crate::Result<(u64, String, String)> {
        let mut reader = csv::Reader::from_reader(csv.as_bytes());
        let headers = reader.headers()?.clone();
        let record = reader.records().next().expect("the fixture contains one user")?;
        let row: CsvUserRow<'_> = record.deserialize(Some(&headers))?;
        let mut bytes = Vec::new();
        let mut writer = RowWriter::new(&mut bytes);
        UsersTable::write_row(&row, &mut writer)?;
        writer.row_done()?;
        drop(writer);
        bytes.extend_from_slice(&[0; 10]);
        let row = UsersTable::read_row(&mut RowReader::new(&bytes));
        Ok((row.id.0, row.username.to_owned(), row.name.to_owned()))
    }

    #[test]
    fn reads_current_crates_io_usernames() {
        let row = read_user("created_at,gh_id,id,name,username\n2020-01-01,123,42,Alice,crates-alice\n").unwrap();

        assert_eq!(row, (42, "crates-alice".to_owned(), "Alice".to_owned()));
    }

    #[test]
    fn reads_legacy_github_logins() {
        let row = read_user("id,gh_login,name\n42,github-alice,Alice\n").unwrap();

        assert_eq!(row, (42, "github-alice".to_owned(), "Alice".to_owned()));
    }

    #[test]
    fn prefers_crates_io_username_when_both_identity_columns_are_present() {
        let row = read_user("id,gh_login,name,username\n42,github-alice,Alice,crates-alice\n").unwrap();

        assert_eq!(row, (42, "crates-alice".to_owned(), "Alice".to_owned()));
    }

    #[test]
    fn missing_user_identity_is_an_error() {
        let error = read_user("id,name\n42,Alice\n").unwrap_err();

        assert!(error.to_string().contains("users.csv is missing username"), "{error}");
    }
}
