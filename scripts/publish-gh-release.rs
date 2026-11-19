#!/usr/bin/env -S cargo +nightly -Zscript
---
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

# This script is invoked by the `Publish GitHub Release` GitHub Actions workflow
# when a version tag is pushed, or run locally with --dry-run to preview notes.

[package]
edition = "2024"

[dependencies]
argh = "0.1"
git-conventional = "1.1"
ohno = { version = "0.5", features = ["app-err"] }
semver = "1"
---

use std::collections::BTreeMap;
use std::fmt::{self, Display};
use std::path::Path;
use std::process::{Command, ExitCode};

use git_conventional::{Commit, Type};
use ohno::{AppError, app_err, bail};
use semver::Version;

/// Declaration order determines release-note category order.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Category {
    Breaking,
    Features,
    BugFixes,
    Performance,
    Documentation,
    Tasks,
    Refactoring,
    Build,
    Ci,
    Style,
    Miscellaneous,
    Custom(String),
}

impl From<Type<'_>> for Category {
    fn from(kind: Type<'_>) -> Self {
        match kind.as_str().to_ascii_lowercase().as_str() {
            "breaking" => Self::Breaking,
            "feat" => Self::Features,
            "fix" => Self::BugFixes,
            "perf" => Self::Performance,
            "doc" | "docs" => Self::Documentation,
            "chore" | "task" => Self::Tasks,
            "refactor" => Self::Refactoring,
            "build" => Self::Build,
            "ci" => Self::Ci,
            "style" => Self::Style,
            "misc" | "miscellaneous" => Self::Miscellaneous,
            other => Self::Custom(other.to_owned()),
        }
    }
}

impl Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Breaking => f.write_str("\u{26a0}\u{fe0f} Breaking"),
            Self::Features => f.write_str("\u{2728} Features"),
            Self::BugFixes => f.write_str("\u{1f41b} Bug Fixes"),
            Self::Performance => f.write_str("\u{26a1} Performance"),
            Self::Documentation => f.write_str("\u{1f4da} Documentation"),
            Self::Tasks => f.write_str("\u{2714}\u{fe0f} Tasks"),
            Self::Refactoring => f.write_str("\u{267b}\u{fe0f} Code Refactoring"),
            Self::Build => f.write_str("\u{1f3d7}\u{fe0f} Build System"),
            Self::Ci => f.write_str("\u{1f504} Continuous Integration"),
            Self::Style => f.write_str("\u{1f3a8} Styling"),
            Self::Miscellaneous => f.write_str("\u{1f9e9} Miscellaneous"),
            Self::Custom(kind) => {
                let mut chars = kind.chars();
                let first = chars.next().expect("custom categories come from nonempty parsed commit types");
                write!(f, "{}{}", first.to_uppercase(), chars.as_str())
            }
        }
    }
}

/// Create a GitHub release for a crate tag.
///
/// Parses a tag of the form `<crate>-v<major>.<minor>.<patch>`, generates
/// notes from the crate's commits through that tag, and calls `gh release create`
/// unless `--dry-run` is set.
#[derive(argh::FromArgs)]
struct Args {
    /// the git tag name (e.g. "bytesbuf-v1.2.3")
    #[argh(positional)]
    tag: String,

    /// the GitHub repository in "owner/name" format (required unless --dry-run)
    #[argh(option)]
    repo: Option<String>,

    /// print release notes without creating a GitHub release
    #[argh(switch)]
    dry_run: bool,
}

fn main() -> ExitCode {
    let args: Args = argh::from_env();

    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("::error::{e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<(), AppError> {
    let (crate_name, version, range) = determine_range(&args.tag, git_output)?;
    let commits = read_git_log(&range, crate_name, Path::is_dir, git_output)?;
    let categories = categorize_commits(&commits);
    let body = format_release_notes(categories)?;
    create_release(args, crate_name, &version.to_string(), &body, run_gh)
}

/// Splits a tag like `foo-v1.2.3` into a crate name and release version.
fn parse_tag(tag: &str) -> Result<(&str, Version), AppError> {
    let idx = tag
        .rfind("-v")
        .ok_or_else(|| app_err!("Tag '{tag}' does not match <crate>-v<version>"))?;

    let crate_name = &tag[..idx];
    let version = &tag[idx + 2..]; // skip "-v"

    if crate_name.is_empty() {
        bail!("Tag '{tag}' has an empty crate name");
    }

    let version = Version::parse(version).map_err(|error| app_err!("Tag '{tag}' must contain a major.minor.patch version: {error}"))?;
    if !version.pre.is_empty() || !version.build.is_empty() {
        bail!("Tag '{tag}' must contain a major.minor.patch version without a prerelease or build suffix");
    }

    Ok((crate_name, version))
}

/// Selects the greatest release version below the requested version for this crate.
fn previous_tag<'a>(tags: &'a str, crate_name: &str, version: &Version) -> Option<&'a str> {
    tags.lines()
        .filter_map(|tag| {
            let (name, candidate) = parse_tag(tag).ok()?;
            (name == crate_name && candidate < *version).then_some((candidate, tag))
        })
        .max_by(|(left, _), (right, _)| left.cmp(right))
        .map(|(_, tag)| tag)
}

#[ohno::enrich_err("failed to determine tag range for {tag}")]
fn determine_range(tag: &str, mut git: impl FnMut(&[&str]) -> Result<String, AppError>) -> Result<(&str, Version, String), AppError> {
    let (crate_name, version) = parse_tag(tag)?;
    let tags = git(&["tag", "--list", &format!("{crate_name}-v*")])?;
    let range = match previous_tag(&tags, crate_name, &version) {
        Some(previous) => format!("{previous}..{tag}"),
        None => tag.to_owned(),
    };
    Ok((crate_name, version, range))
}

#[ohno::enrich_err("failed to read Git log for {range}")]
fn read_git_log(
    range: &str,
    crate_name: &str,
    is_dir: impl FnOnce(&Path) -> bool,
    mut git: impl FnMut(&[&str]) -> Result<String, AppError>,
) -> Result<Vec<String>, AppError> {
    if crate_name.contains("..") || crate_name.contains('/') || crate_name.contains('\\') {
        bail!("Invalid crate name: '{crate_name}'");
    }

    let crate_path = Path::new("crates").join(crate_name);
    if !is_dir(&crate_path) {
        bail!("Crate directory not found: '{}'", crate_path.display());
    }

    let crate_path = crate_path
        .to_str()
        .ok_or_else(|| app_err!("Crate path must be valid UTF-8: '{}'", crate_path.display()))?;
    let log = git(&["log", "--format=%B", "--encoding=UTF-8", "-z", range, "--", crate_path])?;
    Ok(log
        .split_terminator('\0')
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Groups raw commit messages by category, excluding conventional test commits.
fn categorize_commits(commits: &[String]) -> BTreeMap<Category, Vec<String>> {
    let mut categories: BTreeMap<Category, Vec<String>> = BTreeMap::new();

    for message in commits {
        let (category, description) = match Commit::parse(message) {
            Ok(commit) => {
                if commit.type_() == Type::TEST {
                    continue;
                }

                (
                    if commit.breaking() {
                        Category::Breaking
                    } else {
                        Category::from(commit.type_())
                    },
                    commit.description(),
                )
            }
            Err(_) => (
                Category::Miscellaneous,
                message.lines().next().expect("read_git_log filters out empty commit messages"),
            ),
        };
        categories.entry(category).or_default().push(description.to_owned());
    }

    categories
}

/// Formats categorized commits as grouped Markdown.
///
/// # Errors
///
/// Returns an error if no categories are supplied.
fn format_release_notes(categories: BTreeMap<Category, Vec<String>>) -> Result<String, AppError> {
    if categories.is_empty() {
        bail!("No relevant commits found; check the release tag and crate history");
    }

    Ok(categories
        .into_iter()
        .map(|(category, descriptions)| format!("- {category}\n\n  - {}", descriptions.join("\n  - ")))
        .collect::<Vec<_>>()
        .join("\n\n"))
}

#[ohno::enrich_err("failed to run Git with arguments {args:?}")]
fn git_output(args: &[&str]) -> Result<String, AppError> {
    let output = Command::new("git").args(args).output()?;
    if !output.status.success() {
        bail!(
            "Git exited with {}: {}. Ensure the release tag and full history have been fetched",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)?)
}

/// Previews release notes or invokes `gh release create` to publish them.
///
/// # Errors
///
/// Returns an error if publishing without `--repo`, or if `gh` fails.
fn create_release(
    args: &Args,
    crate_name: &str,
    version: &str,
    body: &str,
    gh: impl FnOnce(&[&str]) -> Result<(), AppError>,
) -> Result<(), AppError> {
    let title = format!("{crate_name} v{version}");
    println!("Release {title} with body:\n{body}");

    if args.dry_run {
        println!("\nDry run: GitHub release creation skipped.");
        return Ok(());
    }

    let repo = args
        .repo
        .as_deref()
        .ok_or_else(|| app_err!("--repo is required to create a release; pass --repo <owner/name> or use --dry-run to preview notes"))?;
    gh(&["release", "create", &args.tag, "--repo", repo, "--title", &title, "--notes", body])
}

#[ohno::enrich_err("failed to run `gh`")]
fn run_gh(args: &[&str]) -> Result<(), AppError> {
    let status = Command::new("gh").args(args).status()?;
    if !status.success() {
        bail!("`gh` exited with {status}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_preceding_version_numerically_for_the_same_crate() -> Result<(), AppError> {
        let (_, version) = parse_tag("sample-v1.11.0")?;
        let tags = "sample-v1.9.9\nsample-v1.10.0\nsample-v1.11.0\nsample-v2.0.0\n\
                    other-v1.10.9\nsample-v1.10.5-rc.1\nsample-v1.10.6+build\nsample-vpreview";
        assert_eq!(previous_tag(tags, "sample", &version), Some("sample-v1.10.0"));
        Ok(())
    }

    #[test]
    fn determines_range_from_the_preceding_tag() -> Result<(), AppError> {
        let release = determine_range("sample-v1.1.0", |args| {
            assert_eq!(args, ["tag", "--list", "sample-v*"]);
            Ok("sample-v1.0.0\nsample-v1.1.0\nsample-v2.0.0".to_owned())
        })?;
        assert_eq!(
            release,
            ("sample", Version::new(1, 1, 0), "sample-v1.0.0..sample-v1.1.0".to_owned())
        );
        Ok(())
    }

    #[test]
    fn uses_requested_tag_alone_when_no_preceding_tag_exists() -> Result<(), AppError> {
        let release = determine_range("sample-v1.1.0", |args| {
            assert_eq!(args, ["tag", "--list", "sample-v*"]);
            Ok("sample-vpreview\nsample-v1.1.0\nsample-v2.0.0".to_owned())
        })?;
        assert_eq!(release, ("sample", Version::new(1, 1, 0), "sample-v1.1.0".to_owned()));
        Ok(())
    }

    #[test]
    fn propagates_tag_range_git_failures() {
        let tag = "sample-v1.0.0";
        let error = determine_range(tag, |_| Err(app_err!("scripted Git failure")))
            .expect_err("a failed Git command must abort tag-range selection");
        assert!(error.to_string().contains("scripted Git failure"));
    }

    #[test]
    fn reads_normalized_full_commit_messages() -> Result<(), AppError> {
        let range = "sample-v1.0.0..sample-v1.1.0";
        let crate_path = Path::new("crates").join("sample");
        let path = crate_path.to_str().expect("the fixture path consists entirely of ASCII characters");
        let commits = read_git_log(
            range,
            "sample",
            |path| {
                assert_eq!(path, crate_path.as_path());
                true
            },
            |args| {
                assert_eq!(args, ["log", "--format=%B", "--encoding=UTF-8", "-z", range, "--", path]);
                Ok("  feat(sample): first change\n\nBREAKING CHANGE: new contract\n\0test(sample): more coverage  \0".to_owned())
            },
        )?;
        assert_eq!(
            commits,
            [
                "feat(sample): first change\n\nBREAKING CHANGE: new contract",
                "test(sample): more coverage"
            ]
        );
        Ok(())
    }

    #[test]
    fn reads_empty_or_blank_history_as_no_commits() -> Result<(), AppError> {
        let tag = "sample-v1.0.0";
        let crate_path = Path::new("crates").join("sample");
        let path = crate_path.to_str().expect("the fixture path consists entirely of ASCII characters");
        for log in ["", "\0 \n\t\0"] {
            let commits = read_git_log(
                tag,
                "sample",
                |path| {
                    assert_eq!(path, crate_path.as_path());
                    true
                },
                |args| {
                    assert_eq!(args, ["log", "--format=%B", "--encoding=UTF-8", "-z", tag, "--", path]);
                    Ok(log.to_owned())
                },
            )?;
            assert!(commits.is_empty());
        }
        Ok(())
    }

    #[test]
    fn rejects_invalid_crate_names_before_checking_paths_or_reading_git() {
        for crate_name in ["..", "sample/other", "sample\\other"] {
            let error = read_git_log(
                "sample-v1.0.0",
                crate_name,
                |_| panic!("invalid crate names must fail before checking the filesystem"),
                |_| panic!("invalid crate names must fail before reading Git history"),
            )
            .expect_err("crate names must not escape the crates directory");
            let message = error.to_string();
            assert_eq!(
                message.lines().next().expect("AppError should include an error message"),
                format!("Invalid crate name: '{crate_name}'")
            );
        }
    }

    #[test]
    fn rejects_missing_crate_directory_before_reading_git() {
        let mut directory_checked = false;
        let error = read_git_log(
            "sample-v1.0.0",
            "sample",
            |path| {
                assert_eq!(path, Path::new("crates").join("sample"));
                directory_checked = true;
                false
            },
            |_| panic!("a missing crate directory must fail before reading Git history"),
        )
        .expect_err("the crate directory must exist");
        assert!(directory_checked);
        let message = error.to_string();
        assert_eq!(
            message.lines().next().expect("AppError should include an error message"),
            format!("Crate directory not found: '{}'", Path::new("crates").join("sample").display())
        );
    }

    #[test]
    fn propagates_commit_log_git_failures() {
        let tag = "sample-v1.0.0";
        let crate_path = Path::new("crates").join("sample");
        let error = read_git_log(
            tag,
            "sample",
            |path| {
                assert_eq!(path, crate_path.as_path());
                true
            },
            |_| Err(app_err!("scripted Git failure")),
        )
        .expect_err("a failed Git command must abort log reading");
        assert!(error.to_string().contains("scripted Git failure"));
    }

    #[test]
    fn categorizes_and_groups_commits() {
        let commits = [
            "chore: update tooling",
            "docs(api): explain usage  (#9)",
            "Feat(parser): accept new syntax (#12)",
            "fix(config)!: reject invalid input",
            "feat(api): change the contract\n\nBREAKING CHANGE: new contract",
            "plain subject (#13)\n\nBody must not become a bullet.",
            "doc: document migration",
            "misc: update examples",
            "UNKNOWN: title",
        ]
        .map(str::to_owned);
        assert_eq!(
            categorize_commits(&commits),
            BTreeMap::from([
                (
                    Category::Breaking,
                    vec!["reject invalid input".to_owned(), "change the contract".to_owned()]
                ),
                (Category::Features, vec!["accept new syntax (#12)".to_owned()]),
                (
                    Category::Documentation,
                    vec!["explain usage  (#9)".to_owned(), "document migration".to_owned()]
                ),
                (Category::Tasks, vec!["update tooling".to_owned()]),
                (
                    Category::Miscellaneous,
                    vec!["plain subject (#13)".to_owned(), "update examples".to_owned()]
                ),
                (Category::Custom("unknown".to_owned()), vec!["title".to_owned()]),
            ])
        );
    }

    #[test]
    fn excludes_test_commits_during_categorization() {
        let commits = [
            "test: add coverage",
            "test(api): add scoped coverage",
            "test!: change test infrastructure",
            "TEST: add more coverage",
            "Test!: change shared test infrastructure",
            "feat: preview release notes",
        ]
        .map(str::to_owned);
        assert_eq!(
            categorize_commits(&commits),
            BTreeMap::from([(Category::Features, vec!["preview release notes".to_owned()])])
        );
    }

    #[test]
    fn formats_categorized_commits() -> Result<(), AppError> {
        let categories = BTreeMap::from([
            (Category::Custom("zeta".to_owned()), vec!["last category".to_owned()]),
            (Category::Tasks, vec!["update tooling".to_owned()]),
            (Category::Documentation, vec!["explain usage  (#9)".to_owned()]),
            (Category::Features, vec!["accept new syntax (#12)".to_owned()]),
            (
                Category::Custom("alpha".to_owned()),
                vec!["first custom change".to_owned(), "second custom change".to_owned()],
            ),
            (Category::Breaking, vec!["reject invalid input".to_owned()]),
            (Category::Miscellaneous, vec!["plain subject (#13)".to_owned()]),
        ]);
        assert_eq!(
            format_release_notes(categories)?,
            "- \u{26a0}\u{fe0f} Breaking\n\n  - reject invalid input\n\n\
             - \u{2728} Features\n\n  - accept new syntax (#12)\n\n\
             - \u{1f4da} Documentation\n\n  - explain usage  (#9)\n\n\
             - \u{2714}\u{fe0f} Tasks\n\n  - update tooling\n\n\
             - \u{1f9e9} Miscellaneous\n\n  - plain subject (#13)\n\n\
             - Alpha\n\n  - first custom change\n  - second custom change\n\n\
             - Zeta\n\n  - last category"
        );
        Ok(())
    }

    #[test]
    fn rejects_empty_categories_during_formatting() {
        let no_category = BTreeMap::new();
        let error = format_release_notes(no_category).expect_err("formatting without any categories must fail");
        assert_eq!(
            error.to_string(),
            "No relevant commits found; check the release tag and crate history"
        );
    }

    #[test]
    fn dry_run_does_not_invoke_gh() -> Result<(), AppError> {
        for repo in [None, Some("microsoft/ox-tools".to_owned())] {
            let args = Args {
                tag: "sample-v1.0.0".to_owned(),
                repo,
                dry_run: true,
            };
            create_release(&args, "sample", "1.0.0", "release notes", |_| panic!("dry runs must not invoke gh"))?;
        }
        Ok(())
    }

    #[test]
    fn publishes_with_the_requested_repository_tag_title_and_notes() -> Result<(), AppError> {
        let args = Args {
            tag: "sample-v1.0.0".to_owned(),
            repo: Some("microsoft/ox-tools".to_owned()),
            dry_run: false,
        };
        let mut called = false;
        create_release(&args, "sample", "1.0.0", "release notes", |args| {
            assert_eq!(
                args,
                [
                    "release",
                    "create",
                    "sample-v1.0.0",
                    "--repo",
                    "microsoft/ox-tools",
                    "--title",
                    "sample v1.0.0",
                    "--notes",
                    "release notes"
                ]
            );
            called = true;
            Ok(())
        })?;
        assert!(called);
        Ok(())
    }

    #[test]
    fn publishing_requires_a_repository() {
        let args = Args {
            tag: "sample-v1.0.0".to_owned(),
            repo: None,
            dry_run: false,
        };
        let error = create_release(&args, "sample", "1.0.0", "release notes", |_| {
            panic!("a missing repository must fail before invoking gh")
        })
        .expect_err("publishing without --repo must fail");
        assert_eq!(
            error.to_string(),
            "--repo is required to create a release; pass --repo <owner/name> or use --dry-run to preview notes"
        );
    }
}
