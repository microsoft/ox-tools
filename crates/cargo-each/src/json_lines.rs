// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Strict JSON Lines input for record-driven command execution.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::error::{EachError, JsonLineParseError, JsonLinesFileReadError, JsonLinesFileUtf8Error, JsonRecordShapeError};

/// One object record and its diagnostic origin.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JsonRecord {
    pub(crate) fields: Map<String, Value>,
    pub(crate) source: Arc<str>,
    pub(crate) line: usize,
}

impl JsonRecord {
    pub(crate) fn label(&self) -> String {
        format!("{}:{}", self.source, self.line)
    }
}

/// Load inline inputs first, then file inputs, preserving record and duplicate
/// order within every source.
pub(crate) fn load(inline: &[String], files: &[PathBuf]) -> Result<Vec<JsonRecord>, EachError> {
    let mut records = Vec::new();
    for (index, text) in inline.iter().enumerate() {
        let source = Arc::from(format!("--json-lines #{}", index + 1));
        parse_inline(text, &source, &mut records)?;
    }
    for path in files {
        let display = path.display().to_string();
        let file = File::open(path).map_err(|error| JsonLinesFileReadError::caused_by(display.clone(), error))?;
        let source = Arc::from(display);
        parse_file(BufReader::new(file), &source, &mut records)?;
    }
    Ok(records)
}

fn parse_inline(text: &str, source: &Arc<str>, records: &mut Vec<JsonRecord>) -> Result<(), EachError> {
    for (index, raw_line) in text.lines().enumerate() {
        parse_line(raw_line, source, index + 1, records)?;
    }
    Ok(())
}

fn parse_file(mut reader: impl BufRead, source: &Arc<str>, records: &mut Vec<JsonRecord>) -> Result<(), EachError> {
    let mut bytes = Vec::new();
    let mut line_number = 0;
    loop {
        bytes.clear();
        let read = reader
            .read_until(b'\n', &mut bytes)
            .map_err(|error| JsonLinesFileReadError::caused_by(source.to_string(), error))?;
        if read == 0 {
            return Ok(());
        }
        line_number += 1;
        let line = std::str::from_utf8(&bytes).map_err(|error| JsonLinesFileUtf8Error::caused_by(source.to_string(), error))?;
        parse_line(line, source, line_number, records)?;
    }
}

fn parse_line(raw_line: &str, source: &Arc<str>, line_number: usize, records: &mut Vec<JsonRecord>) -> Result<(), EachError> {
    let line = if line_number == 1 {
        raw_line.strip_prefix('\u{feff}').unwrap_or(raw_line)
    } else {
        raw_line
    };
    if line.trim().is_empty() {
        return Ok(());
    }
    let value: Value = serde_json::from_str(line).map_err(|error| {
        let rendered = error.to_string();
        let location = format!(" at line {} column {}", error.line(), error.column());
        let reason = rendered.strip_suffix(&location).unwrap_or(&rendered).to_owned();
        JsonLineParseError::new(source.to_string(), line_number, error.column(), reason)
    })?;
    let Value::Object(fields) = value else {
        return Err(JsonRecordShapeError::new(source.to_string(), line_number).into());
    };
    records.push(JsonRecord {
        fields,
        source: Arc::clone(source),
        line: line_number,
    });
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    #[cfg_attr(miri, ignore = "uses temporary files; Miri isolation forbids filesystem access")]
    fn inline_and_file_records_preserve_source_order_and_duplicates() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let path = temp.path().join("records.jsonl");
        std::fs::write(&path, "\u{feff}{\"name\":\"file\"}\n\n{\"name\":\"file\"}\n").expect("write records");
        let records = load(&["{\"name\":\"inline\"}\n{\"name\":\"inline\"}\n".to_owned()], &[path]).expect("load records");
        let names = records
            .iter()
            .map(|record| record.fields["name"].as_str().expect("string name"))
            .collect::<Vec<_>>();
        assert_eq!(names, ["inline", "inline", "file", "file"]);
        assert_eq!(records[0].label(), "--json-lines #1:1");
        assert_eq!(records[3].line, 3);
        assert!(Arc::ptr_eq(&records[0].source, &records[1].source));
        assert!(Arc::ptr_eq(&records[2].source, &records[3].source));
    }

    #[test]
    fn invalid_json_and_non_objects_fail_loudly() {
        let invalid = load(&["{bad".to_owned()], &[]).expect_err("invalid JSON must fail").to_string();
        assert!(invalid.contains("--json-lines #1"));
        assert!(invalid.contains("line 1"));
        assert!(invalid.contains("column 2"));
        assert!(!invalid.contains("line 1 column"), "{invalid}");

        let indented = load(&["  {bad".to_owned()], &[])
            .expect_err("invalid indented JSON must fail")
            .to_string();
        assert!(indented.contains("line 1, column 4"), "{indented}");

        let array = load(&["[]".to_owned()], &[]).expect_err("array records must fail").to_string();
        assert!(array.contains("must be an object"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses temporary files; Miri isolation forbids filesystem access")]
    fn file_io_and_utf8_errors_name_the_file() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let missing = temp.path().join("missing.jsonl");
        let missing_error = load(&[], std::slice::from_ref(&missing))
            .expect_err("missing file must fail")
            .to_string();
        assert!(missing_error.contains("missing.jsonl"));

        let invalid = temp.path().join("invalid.jsonl");
        std::fs::write(&invalid, [0xff, 0xfe]).expect("write invalid UTF-8");
        let utf8_error = load(&[], &[invalid]).expect_err("invalid UTF-8 must fail").to_string();
        assert!(utf8_error.contains("invalid.jsonl"));
        assert!(utf8_error.contains("UTF-8"));
    }
}
