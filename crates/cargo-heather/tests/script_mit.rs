// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fixture-driven integration tests for cargo-script files (shebang +
//! `---` frontmatter) against an MIT-style single-line license header.

#![cfg(not(miri))] // miri can't sandbox FS ops these tests do (TempDir, assert_cmd, etc.)
mod common;

use cargo_heather::{CheckResult, FileKind, fix};
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

#[test]
fn public_fix_handles_empty_script_content() {
    let mut output = Vec::new();
    let result = fix(std::io::empty(), &mut output, HEADER_MIT, FileKind::CargoScript).expect("in-memory public API call should succeed");

    assert_eq!(result, CheckResult::Missing);
    assert_eq!(
        String::from_utf8(output).expect("fix output should be UTF-8"),
        "\n---\n# Licensed under the MIT License.\n"
    );
}

#[test]
fn public_fix_handles_script_missing_frontmatter_opener() {
    const INPUT: &str = "#!/usr/bin/env cargo\n";

    let mut output = Vec::new();
    let result = fix(INPUT.as_bytes(), &mut output, HEADER_MIT, FileKind::CargoScript).expect("in-memory public API call should succeed");

    assert_eq!(result, CheckResult::Missing);
    assert_eq!(
        String::from_utf8(output).expect("fix output should be UTF-8"),
        "#!/usr/bin/env cargo\n---\n# Licensed under the MIT License.\n"
    );
}
