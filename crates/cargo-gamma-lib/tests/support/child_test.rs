// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Checks that a re-executed libtest harness actually completed its selected test.

pub fn passed_exactly_one(stdout: &[u8]) -> bool {
    String::from_utf8_lossy(stdout)
        .lines()
        .rfind(|line| line.starts_with("test result: "))
        .is_some_and(|summary| summary.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_one_completed_passing_test() {
        assert!(passed_exactly_one(
            b"running 1 test\ntest selected ... ok\n\
              test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 7 filtered out; finished in 0.01s\n"
        ));
    }

    #[test]
    fn rejects_missing_failed_ignored_or_multiple_tests() {
        for report in [
            "",
            "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 8 filtered out;",
            "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 7 filtered out;",
            "test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 7 filtered out;",
            "test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 6 filtered out;",
        ] {
            assert!(!passed_exactly_one(report.as_bytes()), "{report}");
        }
    }

    #[test]
    fn checks_the_final_harness_summary() {
        assert!(!passed_exactly_one(
            b"test result: ok. 1 passed; 0 failed; 0 ignored;\n\
              test result: ok. 0 passed; 0 failed; 0 ignored;\n"
        ));
    }
}
