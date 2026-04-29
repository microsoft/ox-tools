// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fixture-driven integration tests for cargo-script files (shebang +
//! `---` frontmatter) against an MIT-style single-line license header.

mod common;

use cargo_heather::{CheckResult, FileKind};
use common::{HEADER_MIT, check_str, fix_to_string};

#[test]
fn script_ok() {
    const INPUT: &str = "\
#!/usr/bin/env cargo
---
# Licensed under the MIT License.

fn main() {}
";
    const EXPECTED: &str = "\
#!/usr/bin/env cargo
---
# Licensed under the MIT License.

fn main() {}
";

    assert_eq!(check_str(INPUT, HEADER_MIT, FileKind::CargoScript), CheckResult::Ok);
    let (out, _) = fix_to_string(INPUT, HEADER_MIT, FileKind::CargoScript);
    assert_eq!(out, EXPECTED);
}

#[test]
fn script_missing() {
    const INPUT: &str = "\
#!/usr/bin/env cargo
---
fn main() {}
";
    const EXPECTED: &str = "\
#!/usr/bin/env cargo
---
# Licensed under the MIT License.

fn main() {}
";

    assert_eq!(check_str(INPUT, HEADER_MIT, FileKind::CargoScript), CheckResult::Missing);
    let (out, _) = fix_to_string(INPUT, HEADER_MIT, FileKind::CargoScript);
    assert_eq!(out, EXPECTED);
}

#[test]
fn script_mismatch() {
    const INPUT: &str = "\
#!/usr/bin/env cargo
---
# Licensed under the Apache License.

fn main() {}
";
    const EXPECTED: &str = "\
#!/usr/bin/env cargo
---
# Licensed under the MIT License.

fn main() {}
";

    let result = check_str(INPUT, HEADER_MIT, FileKind::CargoScript);
    assert!(matches!(result, CheckResult::Mismatch { .. }), "expected Mismatch, got {result:?}");
    let (out, _) = fix_to_string(INPUT, HEADER_MIT, FileKind::CargoScript);
    assert_eq!(out, EXPECTED);
}
