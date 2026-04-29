// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared helpers for the per-extension / per-license integration tests.
//!
//! Each integration test file is compiled as its own crate, so unused-item
//! warnings here are spurious — every test file pulls in only what it
//! needs.

#![allow(
    dead_code,
    reason = "each integration test file is its own crate; not all helpers are used by every file"
)]

use cargo_heather::{CheckResult, FileKind, check, fix};

pub const HEADER_MIT: &str = "Licensed under the MIT License.";

pub const HEADER_APACHE: &str = "\
Licensed under the Apache License, Version 2.0 (the \"License\");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an \"AS IS\" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.";

pub fn check_str(input: &str, header: &str, kind: FileKind) -> CheckResult {
    check(input.as_bytes(), header, kind).expect("check should not fail on in-memory input")
}

pub fn fix_to_string(input: &str, header: &str, kind: FileKind) -> (String, CheckResult) {
    let mut output: Vec<u8> = Vec::new();
    let result = fix(input.as_bytes(), &mut output, header, kind).expect("fix should not fail on in-memory input/output");
    let s = String::from_utf8(output).expect("fix output should be valid UTF-8");
    (s, result)
}
