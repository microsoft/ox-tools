// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Reads current GitHub logins keyed by crates.io user identifiers.
//!
//! [`OauthGithubTable`] joins `oauth_github.csv` with [`super::UsersTable`]
//! when the crates provider assembles user owners, including renamed accounts.

use super::{UserId, define_rows, define_table};

define_rows! {
    OauthGithubRow<'a> {
        /// crates.io user identifier, not the GitHub account identifier.
        pub user_id: UserId,
        /// Current GitHub login.
        pub login: &'a str,
    }
}

define_table! {
    oauth_github {
        fn write_row(csv_row: &CsvOauthGithubRow<'a>, writer: &mut RowWriter<impl Write>) -> Result<()> {
            writer.write_str_as_u64(csv_row.user_id)?;
            writer.write_str(csv_row.login);
            Ok(())
        }

        fn read_row<'a>(reader: &mut RowReader<'a>) -> OauthGithubRow<'a> {
            OauthGithubRow {
                user_id: UserId(reader.read_u64()),
                login: reader.read_str(),
            }
        }
    }
}
