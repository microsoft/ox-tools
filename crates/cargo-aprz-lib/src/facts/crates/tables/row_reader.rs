// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use chrono::{DateTime, TimeZone, Utc};
use semver::{BuildMetadata, Prerelease, Version};

#[derive(Debug)]
pub struct RowReader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> RowReader<'a> {
    pub const fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    #[must_use]
    pub const fn position(&self) -> usize {
        self.position
    }

    pub fn read_byte(&mut self) -> u8 {
        let byte = self.data[self.position];
        self.position += 1;
        byte
    }

    pub fn read_u64(&mut self) -> u64 {
        let chunk = self.data[self.position..]
            .first_chunk()
            .expect("table data is padded for u64 decoding");
        let (value, bytes) = vu128::decode_u64(chunk);
        self.position += bytes;
        value
    }

    pub fn read_bool(&mut self) -> bool {
        self.read_byte() != 0
    }

    pub fn read_str(&mut self) -> &'a str {
        let len = self.read_u64();
        let len = usize::try_from(len).expect("string length fits in usize");
        let end = self.position.checked_add(len).expect("no overflow in read_bytes");
        let bytes = &self.data[self.position..end];
        self.position = end;
        core::str::from_utf8(bytes).expect("valid UTF-8 in string")
    }

    pub fn read_optional_u64(&mut self) -> Option<u64> {
        (self.read_byte() != 0).then(|| self.read_u64())
    }

    pub fn read_datetime(&mut self) -> DateTime<Utc> {
        let timestamp = self.read_u64();
        let timestamp = i64::try_from(timestamp).expect("timestamp in range");
        Utc.timestamp_opt(timestamp, 0).single().expect("valid timestamp")
    }

    pub fn read_version(&mut self) -> Version {
        let major = self.read_u64();
        let minor = self.read_u64();
        let patch = self.read_u64();
        let pre = self.read_str();
        let build = self.read_str();

        // The components are already decoded, so the version is assembled directly rather than
        // formatted into a string and parsed back: `Prerelease`/`BuildMetadata` only validate
        // their input, which avoids both the intermediate allocation and a full semver parse.
        Version {
            major,
            minor,
            patch,
            pre: Prerelease::new(pre).expect("valid pre-release"),
            build: BuildMetadata::new(build).expect("valid build metadata"),
        }
    }

    /// Advances past a vu128-encoded `u64` without returning the value.
    pub fn skip_u64(&mut self) {
        let _ = self.read_u64();
    }

    /// Advances past a length-prefixed string without UTF-8 validation.
    pub fn skip_str(&mut self) {
        let len = self.read_u64();
        let len = usize::try_from(len).expect("string length fits in usize");
        self.position = self.position.checked_add(len).expect("no overflow in skip_str");
    }

    /// Advances past a serialized `Version` (3 vu128 `u64`s + 2 length-prefixed strings).
    pub fn skip_version(&mut self) {
        self.skip_u64(); // major
        self.skip_u64(); // minor
        self.skip_u64(); // patch
        self.skip_str(); // pre
        self.skip_str(); // build
    }

    pub const fn skip_bool(&mut self) {
        self.position += 1;
    }

    pub fn skip_optional_u64(&mut self) {
        if self.read_byte() != 0 {
            self.skip_u64();
        }
    }

    pub fn skip_datetime(&mut self) {
        self.skip_u64();
    }
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::facts::crates::tables::RowWriter;

    fn round_trip(version: &str) -> Version {
        let mut buffer = Vec::new();
        {
            let mut writer = RowWriter::new(&mut buffer);
            writer.write_str_as_version(version).unwrap();
            writer.row_done().unwrap();
        }

        // Tables pad their data so vu128 decoding never reads past the end of the mapping.
        buffer.extend_from_slice(&[0u8; 10]);

        RowReader::new(&buffer).read_version()
    }

    #[test]
    fn reads_back_every_version_shape() {
        for version in [
            "1.2.3",
            "0.0.0",
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-0.3.7",
            "1.0.0+build.1",
            "1.0.0+20130313144700",
            "1.0.0-beta.2+exp.sha.5114f85",
            "10.20.30-rc.1.2.3+meta-data.0",
        ] {
            assert_eq!(round_trip(version), Version::parse(version).unwrap(), "round trip of '{version}'");
        }
    }

    #[test]
    fn preserves_ordering_of_pre_release_versions() {
        assert!(round_trip("1.0.0-alpha") < round_trip("1.0.0"));
        assert!(round_trip("1.0.0-alpha") < round_trip("1.0.0-beta"));
        assert!(round_trip("1.0.0") < round_trip("2.0.0-alpha"));
    }
}
