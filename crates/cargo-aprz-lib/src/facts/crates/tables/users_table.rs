// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Reads user profiles and optional legacy GitHub logins from the database dump.
//!
//! [`UsersTable`] supplies profiles to the crates provider. Current GitHub logins
//! come from the sibling OAuth table; older dumps store them here.

use super::{UserId, define_table};

/// CSV user profile with an optional legacy GitHub login.
#[derive(Debug, serde::Deserialize)]
pub struct CsvUserRow<'a> {
    #[serde(borrow)]
    id: &'a str,
    #[serde(borrow)]
    gh_login: Option<&'a str>,
    #[serde(borrow)]
    name: &'a str,
}

/// Borrowed user profile from the binary table.
#[derive(Debug, Clone)]
pub struct UserRow<'a> {
    /// crates.io user identifier.
    pub id: UserId,
    /// GitHub login supplied by older dumps, or empty when absent.
    pub gh_login: &'a str,
    /// Display name, which may be empty.
    pub name: &'a str,
}

define_table! {
    users {
        fn write_row(csv_row: &CsvUserRow<'a>, writer: &mut RowWriter<impl Write>) -> Result<()> {
            writer.write_str_as_u64(csv_row.id)?;
            // An empty string encodes an absent legacy login without changing the cache layout.
            writer.write_str(csv_row.gh_login.unwrap_or_default());
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
