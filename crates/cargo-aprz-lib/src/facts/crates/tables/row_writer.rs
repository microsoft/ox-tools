// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::Write;

use chrono::{DateTime, Utc};
use ohno::{IntoAppError, bail};
use semver::Version;
use url::Url;

use crate::Result;

#[derive(Debug)]
pub struct RowWriter<'a, W: Write> {
    buffer: Vec<u8>,
    writer: &'a mut W,
    row_count: u64,
}

impl<'a, W: Write> RowWriter<'a, W> {
    pub fn new(writer: &'a mut W) -> Self {
        Self {
            buffer: Vec::with_capacity(256),
            writer,
            row_count: 0,
        }
    }

    #[must_use]
    pub const fn row_count(&self) -> u64 {
        self.row_count
    }

    pub fn row_done(&mut self) -> Result<()> {
        self.writer.write_all(&self.buffer)?;
        self.buffer.clear();
        self.row_count += 1;
        Ok(())
    }

    #[inline]
    pub fn write_byte(&mut self, byte: u8) {
        self.buffer.push(byte);
    }

    #[inline]
    pub fn write_u64(&mut self, value: u64) {
        let mut buf = [0u8; 9];
        let bytes_written = vu128::encode_u64(&mut buf, value);
        self.buffer.extend_from_slice(&buf[..bytes_written]);
    }

    pub fn write_str(&mut self, s: &str) {
        self.write_u64(s.len() as u64);
        self.buffer.extend_from_slice(s.as_bytes());
    }

    #[inline]
    pub fn write_bool(&mut self, value: bool) {
        self.buffer.push(u8::from(value));
    }

    pub fn write_optional_u64(&mut self, value: Option<u64>) {
        if let Some(v) = value {
            self.write_byte(1);
            self.write_u64(v);
        } else {
            self.write_byte(0);
        }
    }

    pub fn write_str_as_u64(&mut self, s: &str) -> Result<()> {
        let value = s.parse::<u64>().into_app_err_with(|| format!("parsing u64 from '{s}'"))?;
        self.write_u64(value);
        Ok(())
    }

    pub fn write_str_as_datetime(&mut self, s: &str) -> Result<()> {
        let timestamp = parse_pg_timestamp(s)?;
        self.write_u64(timestamp);
        Ok(())
    }

    pub fn write_str_as_date(&mut self, s: &str) -> Result<()> {
        let timestamp = parse_pg_date(s)?;
        self.write_u64(timestamp);
        Ok(())
    }

    pub fn write_str_as_url(&mut self, s: &str) -> Result<()> {
        if s.is_empty() {
            self.write_str(s);
            return Ok(());
        }

        // Try parsing the URL as-is
        if Url::parse(s).is_ok() {
            self.write_str(s);
            return Ok(());
        }

        // If that fails, try prepending https://
        let with_https = format!("https://{s}");
        if Url::parse(&with_https).is_ok() {
            self.write_str(&with_https);
            return Ok(());
        }

        // Both attempts failed, return error
        bail!("unable to parse URL from '{s}'");
    }

    pub fn write_optional_str_as_u64(&mut self, s: &str) -> Result<()> {
        if s.is_empty() {
            self.write_byte(0);
            return Ok(());
        }

        let v = s.parse::<u64>().into_app_err_with(|| format!("parsing u64 from '{s}'"))?;
        self.write_optional_u64(Some(v));
        Ok(())
    }

    pub fn write_str_as_bool(&mut self, s: &str) -> Result<()> {
        let value = match s {
            "t" | "true" => true,
            "f" | "false" | "" => false,
            _ => bail!("invalid boolean value: expected 't', 'true', 'f', 'false', or empty, got '{s}'"),
        };

        self.write_bool(value);
        Ok(())
    }

    pub fn write_str_as_version(&mut self, s: &str) -> Result<()> {
        let version = Version::parse(s).into_app_err_with(|| format!("parsing version '{s}'"))?;
        self.write_u64(version.major);
        self.write_u64(version.minor);
        self.write_u64(version.patch);
        self.write_str(version.pre.as_str());
        self.write_str(version.build.as_str());
        Ok(())
    }
}

fn parse_pg_timestamp(s: &str) -> Result<u64> {
    let dt = DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f%#z")
        .or_else(|_| DateTime::parse_from_rfc3339(s))
        .into_app_err_with(|| format!("parsing timestamp '{s}'"))?
        .with_timezone(&Utc);
    Ok(dt.timestamp().max(0).cast_unsigned())
}

fn parse_pg_date(s: &str) -> Result<u64> {
    Ok(u64::from(
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .into_app_err_with(|| format!("parsing date '{s}'"))?
            .to_epoch_days()
            .max(0)
            .cast_unsigned(),
    ))
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn encoded(value: u64) -> Vec<u8> {
        let mut buffer = Vec::new();
        {
            let mut writer = RowWriter::new(&mut buffer);
            writer.write_u64(value);
            writer.row_done().expect("writing to a Vec cannot fail");
        }

        buffer
    }

    /// The tables are a cache read back by later runs, so the integer encoding is an on-disk
    /// contract rather than an implementation detail. These bytes sample each of the four short
    /// `vu128` prefix forms plus the shortest and longest `u64` cases of its binary-length form,
    /// so replacing the encoder cannot silently invalidate every table an older build wrote.
    #[test]
    fn writes_the_documented_variable_length_encoding() {
        assert_eq!(encoded(0x7F), [0x7F]);
        assert_eq!(encoded(0x80), [0x80, 0x02]);
        assert_eq!(encoded(0x4000), [0xC0, 0x00, 0x02]);
        assert_eq!(encoded(0x0020_0000), [0xE0, 0x00, 0x00, 0x02]);
        assert_eq!(encoded(0x1000_0000), [0xF3, 0x00, 0x00, 0x00, 0x10]);
        assert_eq!(encoded(u64::MAX), [0xF7, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn absent_optional_u64_is_a_single_zero_byte() {
        let mut buffer = Vec::new();
        {
            let mut writer = RowWriter::new(&mut buffer);
            writer.write_optional_u64(None);
            writer.row_done().expect("writing to a Vec cannot fail");
        }

        assert_eq!(buffer, [0]);
    }

    #[test]
    fn rejects_a_value_that_is_not_a_boolean() {
        let mut buffer = Vec::new();
        let mut writer = RowWriter::new(&mut buffer);
        let error = writer.write_str_as_bool("maybe").expect_err("'maybe' is not a boolean");
        assert!(format!("{error:#}").contains("maybe"));
    }
}
