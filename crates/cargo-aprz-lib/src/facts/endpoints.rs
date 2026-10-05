// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Overridable base addresses for the external services the providers talk to.

/// The Codecov API queried by the coverage provider.
pub use super::coverage::CODECOV_BASE_URL as DEFAULT_COVERAGE_URL;
/// The crates.io database dump downloaded by the crates provider.
pub use super::crates::DEFAULT_DUMP_URL;
/// The docs.rs API queried by the docs provider.
pub use super::docs::DOCS_BASE_URL as DEFAULT_DOCS_URL;

/// The GitHub REST API queried by the hosting provider.
pub const DEFAULT_GITHUB_URL: &str = "https://api.github.com";

/// The Codeberg REST API queried by the hosting provider.
pub const DEFAULT_CODEBERG_URL: &str = "https://codeberg.org/api/v1";

/// The git repository holding the `RustSec` advisory database.
pub const DEFAULT_ADVISORY_URL: &str = rustsec::repository::git::DEFAULT_URL;

/// Base addresses of the external services the providers talk to.
///
/// Every field defaults to the production service. Overriding one redirects the
/// corresponding provider elsewhere, which lets the whole tool run against a
/// local mirror, a GitHub Enterprise instance, or a mock server in tests.
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    clippy::struct_field_names,
    reason = "every field is an address, and the `_url` suffix is what makes each one readable at the use site"
)]
pub struct Endpoints {
    dump_url: String,
    docs_url: String,
    coverage_url: String,
    github_url: String,
    codeberg_url: String,
    advisory_url: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            dump_url: DEFAULT_DUMP_URL.to_owned(),
            docs_url: DEFAULT_DOCS_URL.to_owned(),
            coverage_url: DEFAULT_COVERAGE_URL.to_owned(),
            github_url: DEFAULT_GITHUB_URL.to_owned(),
            codeberg_url: DEFAULT_CODEBERG_URL.to_owned(),
            advisory_url: DEFAULT_ADVISORY_URL.to_owned(),
        }
    }
}

impl Endpoints {
    /// Address of the crates.io database dump.
    #[must_use]
    pub fn dump_url(&self) -> &str {
        &self.dump_url
    }

    /// Address of the docs.rs API.
    #[must_use]
    pub fn docs_url(&self) -> &str {
        &self.docs_url
    }

    /// Address of the coverage badge API.
    #[must_use]
    pub fn coverage_url(&self) -> &str {
        &self.coverage_url
    }

    /// Address of the API for the given hosting domain, if that domain is overridable.
    #[must_use]
    pub fn host_url(&self, host_domain: &str) -> Option<&str> {
        match host_domain {
            "github.com" => Some(&self.github_url),
            "codeberg.org" => Some(&self.codeberg_url),
            _ => None,
        }
    }

    /// Address of the `RustSec` advisory database repository.
    #[must_use]
    pub fn advisory_url(&self) -> &str {
        &self.advisory_url
    }

    /// Redirect the crates.io database dump.
    #[must_use]
    pub fn with_dump_url(mut self, url: impl Into<String>) -> Self {
        self.dump_url = url.into();
        self
    }

    /// Redirect the docs.rs API.
    #[must_use]
    pub fn with_docs_url(mut self, url: impl Into<String>) -> Self {
        self.docs_url = url.into();
        self
    }

    /// Redirect the coverage badge API.
    #[must_use]
    pub fn with_coverage_url(mut self, url: impl Into<String>) -> Self {
        self.coverage_url = url.into();
        self
    }

    /// Redirect the GitHub API.
    #[must_use]
    pub fn with_github_url(mut self, url: impl Into<String>) -> Self {
        self.github_url = url.into();
        self
    }

    /// Redirect the Codeberg API.
    #[must_use]
    pub fn with_codeberg_url(mut self, url: impl Into<String>) -> Self {
        self.codeberg_url = url.into();
        self
    }

    /// Redirect the `RustSec` advisory database repository.
    #[must_use]
    pub fn with_advisory_url(mut self, url: impl Into<String>) -> Self {
        self.advisory_url = url.into();
        self
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;

    #[test]
    fn defaults_point_at_production() {
        let endpoints = Endpoints::default();

        assert_eq!(endpoints.dump_url(), DEFAULT_DUMP_URL);
        assert_eq!(endpoints.docs_url(), DEFAULT_DOCS_URL);
        assert_eq!(endpoints.coverage_url(), DEFAULT_COVERAGE_URL);
        assert_eq!(endpoints.host_url("github.com"), Some(DEFAULT_GITHUB_URL));
        assert_eq!(endpoints.host_url("codeberg.org"), Some(DEFAULT_CODEBERG_URL));
        assert_eq!(endpoints.advisory_url(), DEFAULT_ADVISORY_URL);
    }

    #[test]
    fn unknown_host_has_no_override() {
        assert_eq!(Endpoints::default().host_url("example.com"), None);
    }

    #[test]
    fn each_url_can_be_redirected() {
        let endpoints = Endpoints::default()
            .with_dump_url("http://localhost/dump.tar.gz")
            .with_docs_url("http://localhost/docs")
            .with_coverage_url("http://localhost/badge")
            .with_github_url("http://localhost/github")
            .with_codeberg_url("http://localhost/codeberg")
            .with_advisory_url("http://localhost/advisories.git");

        assert_eq!(endpoints.dump_url(), "http://localhost/dump.tar.gz");
        assert_eq!(endpoints.docs_url(), "http://localhost/docs");
        assert_eq!(endpoints.coverage_url(), "http://localhost/badge");
        assert_eq!(endpoints.host_url("github.com"), Some("http://localhost/github"));
        assert_eq!(endpoints.host_url("codeberg.org"), Some("http://localhost/codeberg"));
        assert_eq!(endpoints.advisory_url(), "http://localhost/advisories.git");
    }

    #[test]
    fn overrides_are_independent() {
        let endpoints = Endpoints::default().with_github_url("http://localhost/github");

        assert_eq!(endpoints.host_url("github.com"), Some("http://localhost/github"));
        assert_eq!(endpoints.host_url("codeberg.org"), Some(DEFAULT_CODEBERG_URL));
        assert_eq!(endpoints.dump_url(), DEFAULT_DUMP_URL);
    }

    #[test]
    fn with_dump_url_sets_only_the_dump_url() {
        let endpoints = Endpoints::default().with_dump_url("http://example.invalid/dump.tar.gz");

        assert_eq!(endpoints.dump_url(), "http://example.invalid/dump.tar.gz");
        assert_eq!(endpoints.docs_url(), DEFAULT_DOCS_URL);
        assert_eq!(endpoints.coverage_url(), DEFAULT_COVERAGE_URL);
        assert_eq!(endpoints.host_url("github.com"), Some(DEFAULT_GITHUB_URL));
        assert_eq!(endpoints.host_url("codeberg.org"), Some(DEFAULT_CODEBERG_URL));
        assert_eq!(endpoints.advisory_url(), DEFAULT_ADVISORY_URL);
    }

    #[test]
    fn with_docs_url_sets_only_the_docs_url() {
        let endpoints = Endpoints::default().with_docs_url("http://example.invalid/docs");

        assert_eq!(endpoints.dump_url(), DEFAULT_DUMP_URL);
        assert_eq!(endpoints.docs_url(), "http://example.invalid/docs");
        assert_eq!(endpoints.coverage_url(), DEFAULT_COVERAGE_URL);
        assert_eq!(endpoints.host_url("github.com"), Some(DEFAULT_GITHUB_URL));
        assert_eq!(endpoints.host_url("codeberg.org"), Some(DEFAULT_CODEBERG_URL));
        assert_eq!(endpoints.advisory_url(), DEFAULT_ADVISORY_URL);
    }

    #[test]
    fn with_coverage_url_sets_only_the_coverage_url() {
        let endpoints = Endpoints::default().with_coverage_url("http://example.invalid/coverage");

        assert_eq!(endpoints.dump_url(), DEFAULT_DUMP_URL);
        assert_eq!(endpoints.docs_url(), DEFAULT_DOCS_URL);
        assert_eq!(endpoints.coverage_url(), "http://example.invalid/coverage");
        assert_eq!(endpoints.host_url("github.com"), Some(DEFAULT_GITHUB_URL));
        assert_eq!(endpoints.host_url("codeberg.org"), Some(DEFAULT_CODEBERG_URL));
        assert_eq!(endpoints.advisory_url(), DEFAULT_ADVISORY_URL);
    }

    #[test]
    fn with_github_url_sets_only_the_github_url() {
        let endpoints = Endpoints::default().with_github_url("http://example.invalid/github");

        assert_eq!(endpoints.dump_url(), DEFAULT_DUMP_URL);
        assert_eq!(endpoints.docs_url(), DEFAULT_DOCS_URL);
        assert_eq!(endpoints.coverage_url(), DEFAULT_COVERAGE_URL);
        assert_eq!(endpoints.host_url("github.com"), Some("http://example.invalid/github"));
        assert_eq!(endpoints.host_url("codeberg.org"), Some(DEFAULT_CODEBERG_URL));
        assert_eq!(endpoints.advisory_url(), DEFAULT_ADVISORY_URL);
    }

    #[test]
    fn with_codeberg_url_sets_only_the_codeberg_url() {
        let endpoints = Endpoints::default().with_codeberg_url("http://example.invalid/codeberg");

        assert_eq!(endpoints.dump_url(), DEFAULT_DUMP_URL);
        assert_eq!(endpoints.docs_url(), DEFAULT_DOCS_URL);
        assert_eq!(endpoints.coverage_url(), DEFAULT_COVERAGE_URL);
        assert_eq!(endpoints.host_url("github.com"), Some(DEFAULT_GITHUB_URL));
        assert_eq!(endpoints.host_url("codeberg.org"), Some("http://example.invalid/codeberg"));
        assert_eq!(endpoints.advisory_url(), DEFAULT_ADVISORY_URL);
    }

    #[test]
    fn with_advisory_url_sets_only_the_advisory_url() {
        let endpoints = Endpoints::default().with_advisory_url("http://example.invalid/advisories.git");

        assert_eq!(endpoints.dump_url(), DEFAULT_DUMP_URL);
        assert_eq!(endpoints.docs_url(), DEFAULT_DOCS_URL);
        assert_eq!(endpoints.coverage_url(), DEFAULT_COVERAGE_URL);
        assert_eq!(endpoints.host_url("github.com"), Some(DEFAULT_GITHUB_URL));
        assert_eq!(endpoints.host_url("codeberg.org"), Some(DEFAULT_CODEBERG_URL));
        assert_eq!(endpoints.advisory_url(), "http://example.invalid/advisories.git");
    }
}
