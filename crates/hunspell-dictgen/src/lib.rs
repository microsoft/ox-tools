// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(docsrs, feature(doc_cfg))]

//! Generate deterministic Hunspell dictionaries from human-maintained word
//! lists.
//!
//! `hunspell-dictgen` converts a UTF-8 text file containing one entry per line
//! into Hunspell's `.dic` format. It removes empty and ASCII-decimal-only
//! lines, sorts the remaining entries using Rust string ordering, retains
//! duplicates, and prefixes the entries with their count.
//!
//! Missing input is accepted and produces an empty dictionary. This makes an
//! optional repository word list straightforward to use in build and CI
//! recipes without hiding other input failures.
//!
//! # Installation
//!
//! ```text
//! cargo install hunspell-dictgen
//! ```
//!
//! # Usage
//!
//! ```text
//! hunspell-dictgen --input .spelling --output target/spelling.dic
//! ```
//!
//! The output always uses LF line endings and ends with a newline. Parent
//! directories are created as needed, and the destination is replaced
//! atomically from a temporary file in the same directory.

use std::fs;
use std::io::Write;
use std::path::Path;

use tempfile::NamedTempFile;

/// An error encountered while generating or publishing a dictionary.
#[ohno::error]
#[from(
    ReadInputError,
    DecodeInputError,
    CreateOutputDirectoryError,
    CreateTemporaryOutputError,
    WriteTemporaryOutputError,
    FlushTemporaryOutputError,
    PublishOutputError
)]
pub struct GenerateError;

#[ohno::error]
#[display("failed to read input word list `{path}`")]
#[from(std::io::Error)]
struct ReadInputError {
    path: String,
}

#[ohno::error]
#[display("input word list `{path}` is not valid UTF-8")]
#[from(std::string::FromUtf8Error)]
struct DecodeInputError {
    path: String,
}

#[ohno::error]
#[display("failed to create parent directory `{parent}` for output dictionary `{output}`")]
#[from(std::io::Error)]
struct CreateOutputDirectoryError {
    parent: String,
    output: String,
}

#[ohno::error]
#[display("failed to create a temporary file beside output dictionary `{path}`")]
#[from(std::io::Error)]
struct CreateTemporaryOutputError {
    path: String,
}

#[ohno::error]
#[display("failed to write the temporary file for output dictionary `{path}`")]
#[from(std::io::Error)]
struct WriteTemporaryOutputError {
    path: String,
}

#[ohno::error]
#[display("failed to flush the temporary file for output dictionary `{path}`")]
#[from(std::io::Error)]
struct FlushTemporaryOutputError {
    path: String,
}

#[ohno::error]
#[display("failed to atomically replace output dictionary `{path}`")]
#[from(tempfile::PersistError)]
struct PublishOutputError {
    path: String,
}

/// Generate and atomically publish a Hunspell dictionary.
///
/// A missing `input_path` is treated as an empty word list. All other
/// filesystem and UTF-8 failures are returned with the affected path and
/// operation.
///
/// # Errors
///
/// Returns [`GenerateError`] if the input cannot be read or decoded, the
/// output directory cannot be created, or the staged output cannot be written
/// or atomically published.
pub fn generate(input_path: &Path, output_path: &Path) -> Result<(), GenerateError> {
    let input = read_input(input_path)?;
    let dictionary = render_dictionary(&input);

    publish_dictionary(output_path, dictionary.as_bytes())
}

/// Convert a UTF-8 word list into deterministic Hunspell `.dic` contents.
///
/// Empty lines and lines containing only ASCII decimal digits are removed.
/// Every other line is retained byte-for-byte except for its line ending.
/// Entries are sorted using [`str`] ordering and duplicates are retained.
#[must_use]
pub fn render_dictionary(input: &str) -> String {
    let mut entries: Vec<&str> = input.lines().filter(|line| is_dictionary_entry(line)).collect();
    entries.sort_unstable();

    let mut dictionary = entries.len().to_string();
    dictionary.push('\n');
    for entry in entries {
        dictionary.push_str(entry);
        dictionary.push('\n');
    }

    dictionary
}

fn is_dictionary_entry(line: &str) -> bool {
    !line.is_empty() && !line.bytes().all(|byte| byte.is_ascii_digit())
}

fn read_input(path: &Path) -> Result<String, GenerateError> {
    let display = path.display().to_string();
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(ReadInputError::caused_by(display, error).into()),
    };

    String::from_utf8(bytes).map_err(|error| DecodeInputError::caused_by(path.display().to_string(), error).into())
}

fn publish_dictionary(path: &Path, contents: &[u8]) -> Result<(), GenerateError> {
    let output = path.display().to_string();
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    fs::create_dir_all(parent)
        .map_err(|error| CreateOutputDirectoryError::caused_by(parent.display().to_string(), output.clone(), error))?;

    let mut staged = NamedTempFile::new_in(parent).map_err(|error| CreateTemporaryOutputError::caused_by(output.clone(), error))?;
    staged
        .write_all(contents)
        .map_err(|error| WriteTemporaryOutputError::caused_by(output.clone(), error))?;
    staged
        .flush()
        .map_err(|error| FlushTemporaryOutputError::caused_by(output.clone(), error))?;
    staged
        .persist(path)
        .map(|_| ())
        .map_err(|error| PublishOutputError::caused_by(output, error))?;

    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::error::Error as _;
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src").join("test_data").join(name)
    }

    #[test]
    fn empty_input_renders_an_empty_dictionary() {
        assert_eq!(render_dictionary(""), "0\n");
    }

    #[test]
    fn line_endings_are_removed_without_changing_entries() {
        assert_eq!(render_dictionary("beta\r\nalpha\n"), "2\nalpha\nbeta\n");
    }

    #[test]
    fn empty_lines_are_removed() {
        assert_eq!(render_dictionary("\nalpha\n\nbeta\n"), "2\nalpha\nbeta\n");
    }

    #[test]
    fn ascii_decimal_only_lines_are_removed() {
        assert_eq!(render_dictionary("123\n٠١٢\n42a\n"), "2\n42a\n٠١٢\n");
    }

    #[test]
    fn mixed_alphanumeric_entries_are_retained() {
        assert_eq!(render_dictionary("abc123\n123abc\n"), "2\n123abc\nabc123\n");
    }

    #[test]
    fn duplicate_entries_are_retained() {
        assert_eq!(render_dictionary("beta\nalpha\nalpha\n"), "3\nalpha\nalpha\nbeta\n");
    }

    #[test]
    fn entries_use_rust_string_ordering() {
        assert_eq!(render_dictionary("😀\nβ\né\na\nZ\n"), "5\nZ\na\né\nβ\n😀\n");
    }

    #[test]
    fn surrounding_and_whitespace_only_content_is_retained() {
        assert_eq!(render_dictionary("word \n word\n   \n123 \n"), "4\n   \n word\n123 \nword \n");
    }

    #[test]
    fn output_count_and_final_newline_match_retained_entries() {
        assert_eq!(render_dictionary("z\n\n1\na"), "2\na\nz\n");
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn absent_input_writes_an_empty_dictionary() {
        let temporary = TempDir::new().expect("temporary directory creation should succeed");
        let input = temporary.path().join("missing.spelling");
        let output = temporary.path().join("target").join("spelling.dic");

        generate(&input, &output).expect("a missing input is a supported empty dictionary");

        assert_eq!(fs::read_to_string(output).expect("generated dictionary should be readable"), "0\n");
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn generation_creates_parent_directories() {
        let temporary = TempDir::new().expect("temporary directory creation should succeed");
        let input = fixture("words.txt");
        let output = temporary.path().join("nested").join("target").join("words.dic");

        generate(&input, &output).expect("generation should create missing output parents");

        assert_eq!(
            fs::read_to_string(output).expect("generated dictionary should be readable"),
            "2\nalpha\nbeta\n"
        );
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn generation_replaces_an_existing_output() {
        let temporary = TempDir::new().expect("temporary directory creation should succeed");
        let input = fixture("words.txt");
        let output = temporary.path().join("words.dic");
        fs::write(&output, "stale partial content").expect("existing output fixture should be writable");

        generate(&input, &output).expect("generation should atomically replace an existing output");

        assert_eq!(
            fs::read_to_string(output).expect("replacement dictionary should be readable"),
            "2\nalpha\nbeta\n"
        );
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn invalid_utf8_names_the_input_path() {
        let temporary = TempDir::new().expect("temporary directory creation should succeed");
        let input = fixture("invalid-utf8.txt");
        let output = temporary.path().join("words.dic");

        let error = generate(&input, &output).expect_err("invalid UTF-8 must fail");
        let cause = error
            .source()
            .and_then(|source| source.downcast_ref::<DecodeInputError>())
            .expect("the generation error should preserve the decode stage");

        assert_eq!(cause.path, input.display().to_string());
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn input_read_failure_names_the_input_path() {
        let temporary = TempDir::new().expect("temporary directory creation should succeed");
        let output = temporary.path().join("words.dic");

        let error = generate(temporary.path(), &output).expect_err("reading a directory as input must fail");
        let cause = error
            .source()
            .and_then(|source| source.downcast_ref::<ReadInputError>())
            .expect("the generation error should preserve the input read stage");

        assert_eq!(cause.path, temporary.path().display().to_string());
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn output_parent_creation_failure_names_the_output_path() {
        let temporary = TempDir::new().expect("temporary directory creation should succeed");
        let input = temporary.path().join("missing.txt");
        let parent_file = temporary.path().join("not-a-directory");
        let output = parent_file.join("words.dic");
        fs::write(&parent_file, "blocking file").expect("blocking fixture should be writable");

        let error = generate(&input, &output).expect_err("a file cannot be used as an output parent");
        let cause = error
            .source()
            .and_then(|source| source.downcast_ref::<CreateOutputDirectoryError>())
            .expect("the generation error should preserve the output directory stage");

        assert_eq!(
            (&cause.parent, &cause.output),
            (&parent_file.display().to_string(), &output.display().to_string())
        );
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn publication_failure_does_not_replace_the_existing_directory() {
        let temporary = TempDir::new().expect("temporary directory creation should succeed");
        let input = temporary.path().join("missing.txt");
        let output = temporary.path().join("words.dic");
        let marker = output.join("marker.txt");
        fs::create_dir(&output).expect("output directory fixture should be creatable");
        fs::write(&marker, "keep").expect("marker fixture should be writable");

        let error = generate(&input, &output).expect_err("a directory cannot be replaced by the staged file");
        let cause = error
            .source()
            .and_then(|source| source.downcast_ref::<PublishOutputError>())
            .expect("the generation error should preserve the publication stage");

        assert_eq!(
            (
                &cause.path,
                fs::read_to_string(marker).expect("failed publication must preserve the destination"),
            ),
            (&output.display().to_string(), "keep".to_owned())
        );
    }
}
