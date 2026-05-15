// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fixture-driven integration tests for `.rs` files against a multi-line
//! Apache-2.0 license header.

mod common;

use cargo_heather::{CheckResult, FileKind};
use common::{HEADER_APACHE, check_str, fix_to_string};

#[test]
fn rust_apache_ok() {
    const INPUT: &str = "\
// Licensed under the Apache License, Version 2.0 (the \"License\");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an \"AS IS\" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

fn main() {}
";
    const EXPECTED: &str = "\
// Licensed under the Apache License, Version 2.0 (the \"License\");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an \"AS IS\" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

fn main() {}
";

    assert_eq!(check_str(INPUT, HEADER_APACHE, FileKind::Rust), CheckResult::Ok);
    let (out, _) = fix_to_string(INPUT, HEADER_APACHE, FileKind::Rust);
    assert_eq!(out, EXPECTED);
}

#[test]
fn rust_apache_missing() {
    const INPUT: &str = "fn main() {}\n";
    const EXPECTED: &str = "\
// Licensed under the Apache License, Version 2.0 (the \"License\");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an \"AS IS\" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

fn main() {}
";

    assert_eq!(check_str(INPUT, HEADER_APACHE, FileKind::Rust), CheckResult::Missing);
    let (out, _) = fix_to_string(INPUT, HEADER_APACHE, FileKind::Rust);
    assert_eq!(out, EXPECTED);
}

#[test]
fn rust_apache_mismatch() {
    // MIT header present, Apache expected — Mismatch. The MIT block is
    // stripped and the multi-line Apache header is prepended.
    const INPUT: &str = "\
// Licensed under the MIT License.

fn main() {}
";
    const EXPECTED: &str = "\
// Licensed under the Apache License, Version 2.0 (the \"License\");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an \"AS IS\" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

fn main() {}
";

    let result = check_str(INPUT, HEADER_APACHE, FileKind::Rust);
    assert!(matches!(result, CheckResult::Mismatch { .. }), "expected Mismatch, got {result:?}");
    let (out, _) = fix_to_string(INPUT, HEADER_APACHE, FileKind::Rust);
    assert_eq!(out, EXPECTED);
}
