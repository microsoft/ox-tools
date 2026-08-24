// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Builds a synthetic crates.io database dump.
//!
//! The real dump is a ~1.5 GB `db-dump.tar.gz` holding one CSV per database table, nested under a
//! dated top-level directory. The tool only reads a handful of those tables, and only a handful of
//! columns from each; the exact set is defined by the `CsvRow` structs in
//! `src/facts/crates/tables/*.rs`. This module produces a tarball with the same shape but with a
//! world of a few made-up crates, small enough to serve from a mock HTTP server in a test.
//!
//! Column values follow the `PostgreSQL` text encodings the real dump uses: `t`/`f` for booleans,
//! `YYYY-MM-DD HH:MM:SS.ffffff+00` for timestamps, `YYYY-MM-DD` for dates, and JSON objects for
//! feature maps.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use flate2::Compression;
use flate2::write::GzEncoder;

/// Top-level directory the real dump nests its files under.
const DUMP_DIR: &str = "2026-01-15-020017";

/// A crates.io user account that can own crates.
pub struct User {
    pub id: u64,
    pub gh_login: String,
    pub name: String,
}

/// A GitHub team that can own crates.
pub struct Team {
    pub id: u64,
    pub login: String,
    pub name: String,
}

/// A crates.io category a crate can be filed under.
pub struct Category {
    pub id: u64,
    pub slug: String,
}

/// A crates.io keyword a crate can be tagged with.
pub struct Keyword {
    pub id: u64,
    pub keyword: String,
}

/// An owner of a crate, which is either a user or a team.
pub enum Owner {
    User(u64),
    Team(u64),
}

/// One published version of a crate.
pub struct Version {
    pub id: u64,
    pub num: String,
    pub downloads: u64,
    pub edition: Option<u32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub description: String,
    pub features: String,
    pub license: String,
    /// The MSRV, written to the `rust_version` column.
    pub msrv: String,
    pub yanked: bool,
    pub documentation: String,
    pub homepage: String,
    /// Daily download counts, as `(day, downloads)` pairs.
    pub daily_downloads: Vec<(NaiveDate, u64)>,
    /// Ids of the crates this version depends on.
    pub dependencies: Vec<u64>,
}

impl Version {
    /// A version with plausible defaults, published `age_days` before `now`.
    pub fn new(id: u64, num: &str, now: DateTime<Utc>, age_days: i64) -> Self {
        let created_at = now - Duration::days(age_days);
        Self {
            id,
            num: num.to_owned(),
            downloads: 1000,
            edition: Some(2021),
            created_at,
            updated_at: created_at,
            description: format!("synthetic crate version {num}"),
            features: r#"{"default":["std"],"std":[]}"#.to_owned(),
            license: "MIT OR Apache-2.0".to_owned(),
            msrv: "1.70.0".to_owned(),
            yanked: false,
            documentation: String::new(),
            homepage: String::new(),
            daily_downloads: Vec::new(),
            dependencies: Vec::new(),
        }
    }
}

/// A crate, with all of its versions.
pub struct Crate {
    pub id: u64,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub repository: String,
    pub downloads: u64,
    pub owners: Vec<Owner>,
    pub categories: Vec<u64>,
    pub keywords: Vec<u64>,
    pub versions: Vec<Version>,
}

impl Crate {
    /// A crate with plausible defaults, first published `age_days` before `now`.
    pub fn new(id: u64, name: &str, now: DateTime<Utc>, age_days: i64) -> Self {
        let created_at = now - Duration::days(age_days);
        Self {
            id,
            name: name.to_owned(),
            created_at,
            updated_at: now - Duration::days(1),
            repository: String::new(),
            downloads: 0,
            owners: Vec::new(),
            categories: Vec::new(),
            keywords: Vec::new(),
            versions: Vec::new(),
        }
    }
}

/// A whole synthetic database dump.
pub struct Dump {
    pub crates: Vec<Crate>,
    pub users: Vec<User>,
    pub teams: Vec<Team>,
    pub categories: Vec<Category>,
    pub keywords: Vec<Keyword>,
}

/// Ids of the crates in [`Dump::sample`], so tests can refer to them without repeating literals.
pub mod ids {
    pub const SERDE: u64 = 1;
    pub const ITOA: u64 = 2;
    pub const MINIZ_OXIDE: u64 = 3;
    pub const ADLER2: u64 = 4;
    pub const ONCE_CELL: u64 = 5;
    pub const SERDE_JSON: u64 = 6;
    pub const VERSIONLESS: u64 = 7;
    pub const SCHEMELESS_REPO: u64 = 8;
    pub const BROKEN_REPO: u64 = 9;
}

/// The `serde` crate of [`Dump::sample`]: several versions, both kinds of owner, categories,
/// keywords, and a daily download series that straddles the 90-day window.
fn serde_crate(now: DateTime<Utc>) -> Crate {
    let today = now.date_naive();

    let serde_current = Version {
        downloads: 900_000,
        edition: Some(2018),
        msrv: "1.61.0".to_owned(),
        license: "MIT OR Apache-2.0".to_owned(),
        description: "A generic serialization framework".to_owned(),
        documentation: "https://docs.rs/serde".to_owned(),
        homepage: "https://serde.rs".to_owned(),
        features: r#"{"default":["std"],"derive":["serde_derive"],"std":[]}"#.to_owned(),
        daily_downloads: vec![
            (today - Duration::days(1), 5000),
            (today - Duration::days(2), 4000),
            (today - Duration::days(45), 3000),
            // Outside the 90-day window: must not appear in the monthly series.
            (today - Duration::days(200), 999_999),
        ],
        dependencies: vec![ids::ITOA],
        ..Version::new(1001, "1.0.200", now, 40)
    };

    let serde_old = Version {
        downloads: 100_000,
        daily_downloads: vec![(today - Duration::days(3), 100)],
        ..Version::new(1002, "1.0.100", now, 400)
    };

    let serde_pre = Version {
        downloads: 42,
        ..Version::new(1003, "2.0.0-alpha.1", now, 10)
    };

    let serde_yanked = Version {
        downloads: 7,
        yanked: true,
        ..Version::new(1004, "1.0.150", now, 200)
    };

    Crate {
        downloads: 12_000_000,
        owners: vec![Owner::User(1), Owner::Team(10)],
        categories: vec![100, 101],
        keywords: vec![200, 201],
        versions: vec![serde_current, serde_old, serde_pre, serde_yanked],
        ..Crate::new(ids::SERDE, "serde", now, 4000)
    }
}

/// Every crate in [`Dump::sample`].
fn sample_crates(now: DateTime<Utc>) -> Vec<Crate> {
    let today = now.date_naive();

    let itoa_version = Version {
        downloads: 400_000,
        edition: None,
        msrv: String::new(),
        daily_downloads: vec![(today - Duration::days(5), 250)],
        ..Version::new(2001, "1.0.17", now, 70)
    };
    let itoa = Crate {
        downloads: 5_000_000,
        owners: vec![Owner::User(2)],
        keywords: vec![201],
        versions: vec![itoa_version],
        ..Crate::new(ids::ITOA, "itoa", now, 3000)
    };

    let miniz_version = Version {
        dependencies: vec![ids::ADLER2],
        ..Version::new(3001, "0.8.9", now, 120)
    };
    let miniz_oxide = Crate {
        downloads: 2_000_000,
        owners: vec![Owner::Team(10)],
        versions: vec![miniz_version],
        ..Crate::new(ids::MINIZ_OXIDE, "miniz_oxide", now, 2000)
    };

    let adler2 = Crate {
        downloads: 1_500_000,
        versions: vec![Version::new(4001, "2.0.1", now, 150)],
        ..Crate::new(ids::ADLER2, "adler2", now, 1000)
    };

    let once_cell = Crate {
        downloads: 3_000_000,
        versions: vec![Version::new(5001, "1.21.3", now, 60)],
        ..Crate::new(ids::ONCE_CELL, "once_cell", now, 2500)
    };

    let serde_json_version = Version {
        dependencies: vec![ids::SERDE, ids::ITOA],
        ..Version::new(6001, "1.0.140", now, 30)
    };
    let serde_json = Crate {
        downloads: 8_000_000,
        versions: vec![serde_json_version],
        ..Crate::new(ids::SERDE_JSON, "serde_json", now, 3500)
    };

    // A crate that exists but has no published version.
    let versionless = Crate::new(ids::VERSIONLESS, "versionless-crate", now, 500);

    // The repository URL has no scheme, so the table writer prepends `https://`.
    let schemeless = Crate {
        repository: "github.com/fake-org/schemeless-repo-crate".to_owned(),
        versions: vec![Version::new(8001, "0.1.0", now, 20)],
        ..Crate::new(ids::SCHEMELESS_REPO, "schemeless-repo-crate", now, 300)
    };

    // The repository URL cannot be parsed at all, so it is stored as empty.
    let broken = Crate {
        repository: "not a valid url".to_owned(),
        versions: vec![Version::new(9001, "0.1.0", now, 20)],
        ..Crate::new(ids::BROKEN_REPO, "broken-repo-crate", now, 300)
    };

    vec![
        serde_crate(now),
        itoa,
        miniz_oxide,
        adler2,
        once_cell,
        serde_json,
        versionless,
        schemeless,
        broken,
    ]
}

impl Dump {
    /// The world every test shares.
    ///
    /// It contains the crates the fixture workspaces depend on (`itoa`, `miniz_oxide`, `adler2`,
    /// `once_cell`), a well-populated `serde` with several versions and both kinds of owner, a
    /// `serde_json` that depends on it, and a few crates that exercise awkward corners: one with
    /// no versions at all, one whose repository URL has no scheme, and one whose repository URL
    /// cannot be parsed at all.
    ///
    /// Everything is dated relative to `now` so the recency-sensitive facts (downloads over the
    /// last 90 days, versions published in the last 90/180/365 days) are stable.
    pub fn sample(now: DateTime<Utc>) -> Self {
        Self {
            crates: sample_crates(now),
            users: vec![
                User {
                    id: 1,
                    gh_login: "alice".to_owned(),
                    name: "Alice Example".to_owned(),
                },
                User {
                    id: 2,
                    gh_login: "bob".to_owned(),
                    // An owner with no display name; the provider reports `None` for it.
                    name: String::new(),
                },
            ],
            teams: vec![Team {
                id: 10,
                login: "github:fake-org:maintainers".to_owned(),
                name: "Fake Org Maintainers".to_owned(),
            }],
            categories: vec![
                Category {
                    id: 100,
                    slug: "encoding".to_owned(),
                },
                Category {
                    id: 101,
                    slug: "parser-implementations".to_owned(),
                },
            ],
            keywords: vec![
                Keyword {
                    id: 200,
                    keyword: "serialization".to_owned(),
                },
                Keyword {
                    id: 201,
                    keyword: "no-std".to_owned(),
                },
            ],
        }
    }

    /// The CSV files of the dump, as `(file name, contents)` pairs.
    pub fn csv_files(&self) -> Vec<(String, String)> {
        vec![
            ("crates.csv".to_owned(), self.crates_csv()),
            ("versions.csv".to_owned(), self.versions_csv()),
            ("version_downloads.csv".to_owned(), self.version_downloads_csv()),
            ("dependencies.csv".to_owned(), self.dependencies_csv()),
            ("crate_downloads.csv".to_owned(), self.crate_downloads_csv()),
            ("crates_categories.csv".to_owned(), self.crates_categories_csv()),
            ("crates_keywords.csv".to_owned(), self.crates_keywords_csv()),
            ("categories.csv".to_owned(), self.categories_csv()),
            ("keywords.csv".to_owned(), self.keywords_csv()),
            ("teams.csv".to_owned(), self.teams_csv()),
            ("users.csv".to_owned(), self.users_csv()),
            ("crate_owners.csv".to_owned(), self.crate_owners_csv()),
            // Files the real dump ships that the tool does not read.
            ("README.md".to_owned(), "# synthetic dump\n".to_owned()),
            ("default_versions.csv".to_owned(), "version_id,crate_id\n1001,1\n".to_owned()),
        ]
    }

    /// The whole dump, as the bytes of a gzipped tarball.
    pub fn to_tar_gz(&self) -> Vec<u8> {
        tar_gz(&self.csv_files())
    }

    fn crates_csv(&self) -> String {
        let mut writer = csv_writer(&[
            "id",
            "name",
            "created_at",
            "updated_at",
            "description",
            "homepage",
            "documentation",
            "repository",
            "readme",
            "max_upload_size",
        ]);

        for krate in &self.crates {
            write_record(
                &mut writer,
                &[
                    &krate.id.to_string(),
                    &krate.name,
                    &timestamp(krate.created_at),
                    &timestamp(krate.updated_at),
                    &format!("the {} crate", krate.name),
                    "",
                    "",
                    &krate.repository,
                    "",
                    "",
                ],
            );
        }

        finish(writer)
    }

    fn versions_csv(&self) -> String {
        let mut writer = csv_writer(&[
            "id",
            "crate_id",
            "num",
            "created_at",
            "updated_at",
            "downloads",
            "features",
            "yanked",
            "license",
            "crate_size",
            "checksum",
            "links",
            "rust_version",
            "has_lib",
            "bin_names",
            "edition",
            "description",
            "homepage",
            "documentation",
        ]);

        for krate in &self.crates {
            for version in &krate.versions {
                write_record(
                    &mut writer,
                    &[
                        &version.id.to_string(),
                        &krate.id.to_string(),
                        &version.num,
                        &timestamp(version.created_at),
                        &timestamp(version.updated_at),
                        &version.downloads.to_string(),
                        &version.features,
                        boolean(version.yanked),
                        &version.license,
                        "12345",
                        "abc123",
                        "",
                        &version.msrv,
                        "t",
                        "{}",
                        &version.edition.map(|e| e.to_string()).unwrap_or_default(),
                        &version.description,
                        &version.homepage,
                        &version.documentation,
                    ],
                );
            }
        }

        finish(writer)
    }

    fn version_downloads_csv(&self) -> String {
        let mut writer = csv_writer(&["version_id", "downloads", "date"]);

        for krate in &self.crates {
            for version in &krate.versions {
                for (date, downloads) in &version.daily_downloads {
                    write_record(
                        &mut writer,
                        &[
                            &version.id.to_string(),
                            &downloads.to_string(),
                            &date.format("%Y-%m-%d").to_string(),
                        ],
                    );
                }
            }
        }

        finish(writer)
    }

    fn dependencies_csv(&self) -> String {
        let mut writer = csv_writer(&[
            "id",
            "version_id",
            "crate_id",
            "req",
            "optional",
            "default_features",
            "features",
            "target",
            "kind",
            "explicit_name",
        ]);

        let mut id = 0;
        for krate in &self.crates {
            for version in &krate.versions {
                for dependency in &version.dependencies {
                    id += 1;
                    write_record(
                        &mut writer,
                        &[
                            &id.to_string(),
                            &version.id.to_string(),
                            &dependency.to_string(),
                            "^1",
                            "f",
                            "t",
                            "{}",
                            "",
                            "0",
                            "",
                        ],
                    );
                }
            }
        }

        finish(writer)
    }

    fn crate_downloads_csv(&self) -> String {
        let mut writer = csv_writer(&["crate_id", "downloads"]);

        for krate in &self.crates {
            write_record(&mut writer, &[&krate.id.to_string(), &krate.downloads.to_string()]);
        }

        finish(writer)
    }

    fn crates_categories_csv(&self) -> String {
        let mut writer = csv_writer(&["crate_id", "category_id"]);

        for krate in &self.crates {
            for category in &krate.categories {
                write_record(&mut writer, &[&krate.id.to_string(), &category.to_string()]);
            }
        }

        finish(writer)
    }

    fn crates_keywords_csv(&self) -> String {
        let mut writer = csv_writer(&["crate_id", "keyword_id"]);

        for krate in &self.crates {
            for keyword in &krate.keywords {
                write_record(&mut writer, &[&krate.id.to_string(), &keyword.to_string()]);
            }
        }

        finish(writer)
    }

    fn categories_csv(&self) -> String {
        let mut writer = csv_writer(&["id", "category", "slug", "description", "crates_cnt", "created_at", "path"]);

        for category in &self.categories {
            write_record(
                &mut writer,
                &[
                    &category.id.to_string(),
                    &category.slug,
                    &category.slug,
                    "a category",
                    "1",
                    "2015-05-01 10:00:00.000000+00",
                    &category.slug,
                ],
            );
        }

        finish(writer)
    }

    fn keywords_csv(&self) -> String {
        let mut writer = csv_writer(&["id", "keyword", "crates_cnt", "created_at"]);

        for keyword in &self.keywords {
            write_record(
                &mut writer,
                &[&keyword.id.to_string(), &keyword.keyword, "1", "2015-05-01 10:00:00.000000+00"],
            );
        }

        finish(writer)
    }

    fn teams_csv(&self) -> String {
        let mut writer = csv_writer(&["id", "login", "github_id", "name", "avatar", "org_id"]);

        for team in &self.teams {
            write_record(&mut writer, &[&team.id.to_string(), &team.login, "555", &team.name, "", "777"]);
        }

        finish(writer)
    }

    fn users_csv(&self) -> String {
        let mut writer = csv_writer(&["id", "gh_login", "name", "gh_avatar", "gh_id"]);

        for user in &self.users {
            write_record(
                &mut writer,
                &[&user.id.to_string(), &user.gh_login, &user.name, "", &user.id.to_string()],
            );
        }

        finish(writer)
    }

    fn crate_owners_csv(&self) -> String {
        let mut writer = csv_writer(&["crate_id", "owner_id", "created_at", "created_by", "owner_kind"]);

        for krate in &self.crates {
            for owner in &krate.owners {
                let (owner_id, kind) = match owner {
                    Owner::User(id) => (*id, "0"),
                    Owner::Team(id) => (*id, "1"),
                };

                write_record(
                    &mut writer,
                    &[
                        &krate.id.to_string(),
                        &owner_id.to_string(),
                        "2016-06-01 10:00:00.000000+00",
                        "1",
                        kind,
                    ],
                );
            }
        }

        finish(writer)
    }
}

/// Packs the given `(file name, contents)` pairs into a gzipped tarball shaped like the real dump.
pub fn tar_gz(files: &[(String, String)]) -> Vec<u8> {
    let encoder = GzEncoder::new(Vec::new(), Compression::fast());
    let mut builder = tar::Builder::new(encoder);

    for (name, contents) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();

        builder
            .append_data(&mut header, format!("{DUMP_DIR}/data/{name}"), contents.as_bytes())
            .expect("writing to an in-memory tar cannot fail");
    }

    builder
        .into_inner()
        .expect("finishing an in-memory tar cannot fail")
        .finish()
        .expect("finishing an in-memory gzip stream cannot fail")
}

/// Formats a timestamp the way `PostgreSQL` renders `timestamptz` in the dump.
fn timestamp(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%d %H:%M:%S%.6f+00").to_string()
}

/// Formats a boolean the way `PostgreSQL` renders it in the dump.
const fn boolean(value: bool) -> &'static str {
    if value { "t" } else { "f" }
}

fn csv_writer(headers: &[&str]) -> csv::Writer<Vec<u8>> {
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer.write_record(headers).expect("writing to a Vec cannot fail");
    writer
}

fn write_record(writer: &mut csv::Writer<Vec<u8>>, fields: &[&str]) {
    writer.write_record(fields).expect("writing to a Vec cannot fail");
}

fn finish(writer: csv::Writer<Vec<u8>>) -> String {
    let bytes = writer.into_inner().expect("flushing a Vec writer cannot fail");
    String::from_utf8(bytes).expect("every field written is valid UTF-8")
}

/// Truncates the given bytes to a prefix, producing a stream that ends mid-gzip.
pub fn truncate(bytes: &[u8]) -> Vec<u8> {
    let keep = bytes.len() / 2;
    bytes[..keep].to_vec()
}
