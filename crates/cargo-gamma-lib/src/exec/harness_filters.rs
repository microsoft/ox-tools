// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Composition of the run's own test selection with the one the user asked for.
//!
//! libtest takes its filters as *positional* arguments and runs a test that any one of them
//! matches. That `or` is the whole problem this module exists for: appending the test this run
//! chose to the arguments the user supplied does not narrow the set, it widens it, and a mutant can
//! then be convicted by a test the user deliberately excluded. `--exact` makes it worse rather than
//! better, because it is global — one appended to pin the run's own name also converts the user's
//! substring filter into an exact match, which typically then matches nothing at all.
//!
//! The composition that is actually wanted is an intersection, and libtest cannot express one. So
//! it is computed here instead: the user's positional filters are read, applied to the names this
//! run chose, and the survivors passed on their own. The user's *flags* — including `--skip`, which
//! only ever removes tests — are passed through untouched, so everything that narrows still
//! narrows and only the widening is gone.

/// The user's harness arguments, split into the parts that select tests and the parts that do not.
#[derive(Debug, Default)]
pub(super) struct HarnessFilters<'args> {
    /// Everything that is not a positional filter, in the order it was given.
    flags: Vec<&'args str>,

    /// The positional filters, which libtest matches with `or`.
    filters: Vec<&'args str>,

    /// Whether the user asked for whole-name matching rather than substring matching.
    exact: bool,
}

/// libtest options whose value is the *next* argument rather than part of the same one.
///
/// Needed so that the value is not mistaken for a positional filter: in `--test-threads 4` the `4`
/// selects nothing, and treating it as a filter would silently run no tests. The `--opt=value`
/// spelling needs no entry here, because it is a single argument that already starts with `-`.
const VALUED: &[&str] = &[
    "--test-threads",
    "--logfile",
    "--skip",
    "--color",
    "--format",
    "--shuffle-seed",
    "-Z",
];

impl<'args> HarnessFilters<'args> {
    /// Reads the user's harness arguments.
    pub(super) fn parse(args: &'args [String]) -> Self {
        let mut parsed = Self::default();
        let mut index = 0;

        while let Some(arg) = args.get(index) {
            let arg = arg.as_str();

            if arg.starts_with('-') {
                parsed.flags.push(arg);

                if arg == "--exact" {
                    parsed.exact = true;
                }

                // The value belongs to the option, not to the filters, so it is consumed here and
                // travels with it.
                if VALUED.contains(&arg)
                    && let Some(value) = args.get(index + 1)
                {
                    parsed.flags.push(value.as_str());
                    index += 1;
                }
            } else {
                parsed.filters.push(arg);
            }

            index += 1;
        }

        parsed
    }

    /// Whether the user's filters would have let this test run.
    ///
    /// No positional filter is not an empty selection — it is the absence of one, and libtest runs
    /// everything. Anything else is matched the way libtest would have matched it, so that the
    /// answer here and the answer the harness would have given cannot differ.
    pub(super) fn admits(&self, name: &str) -> bool {
        if self.filters.is_empty() {
            return true;
        }

        if self.exact {
            return self.filters.contains(&name);
        }

        self.filters.iter().any(|filter| name.contains(filter))
    }

    /// The arguments to pass alongside a selection this run chose.
    ///
    /// The user's positional filters are deliberately *not* among them. They have already been
    /// applied by [`Self::admits`], so passing them again would restore the `or` this module exists
    /// to avoid — and leaving them out is what lets the `--exact` that pins the run's own names be
    /// added without silently redefining the user's filter as well.
    pub(super) fn flags(&self) -> &[&'args str] {
        &self.flags
    }

    /// The user's arguments as a listing pass should carry them.
    ///
    /// A listing has to see exactly the population the user's filters allow, or the census records
    /// tests that are never going to run. That population is decided by more than the positional
    /// filters — `--include-ignored` and `--exclude-should-panic` move it too — so everything is
    /// carried rather than an allowlist that would silently drop the next such option. The one
    /// exception is `--format`, which would fight with the one the listing asks for.
    pub(super) fn selecting(&self) -> Vec<&'args str> {
        // Each stored argument is pushed at most once, and the tail appends every filter, so this
        // sum is an upper bound available up front.
        let mut args: Vec<&str> = Vec::with_capacity(self.flags.len() + self.filters.len());
        let mut index = 0;

        while let Some(flag) = self.flags.get(index) {
            let value = VALUED.contains(flag).then(|| self.flags.get(index + 1)).flatten();

            if *flag != "--format" && !flag.starts_with("--format=") {
                args.push(flag);
                args.extend(value);
            }

            index += 1 + usize::from(value.is_some());
        }

        args.extend_from_slice(&self.filters);
        args
    }
}

#[cfg(test)]
mod tests {
    use super::HarnessFilters;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn an_option_value_is_not_mistaken_for_a_filter() {
        // `--test-threads 4` reading `4` as a filter would run only tests whose name contains a
        // four, which is very nearly none of them.
        let raw = args(&["--test-threads", "4", "--nocapture", "parser"]);
        let parsed = HarnessFilters::parse(&raw);

        assert_eq!(parsed.flags(), ["--test-threads", "4", "--nocapture"]);
        assert!(parsed.admits("tests::parser_works"));
        assert!(!parsed.admits("tests::lexer_works"));
    }

    #[test]
    fn the_joined_spelling_of_an_option_needs_no_lookahead() {
        let raw = args(&["--test-threads=4", "parser"]);
        let parsed = HarnessFilters::parse(&raw);

        assert_eq!(parsed.flags(), ["--test-threads=4"]);
        assert!(parsed.admits("tests::parser_works"));
    }

    #[test]
    fn no_positional_filter_admits_everything() {
        let raw = args(&["--nocapture"]);
        let parsed = HarnessFilters::parse(&raw);

        assert!(parsed.admits("anything at all"));
    }

    #[test]
    fn the_users_exact_flag_makes_their_filters_whole_name_matches() {
        let raw = args(&["--exact", "tests::parser"]);
        let parsed = HarnessFilters::parse(&raw);

        assert!(parsed.admits("tests::parser"));
        assert!(!parsed.admits("tests::parser_works"), "`--exact` is a whole-name match");
    }

    #[test]
    fn several_filters_are_matched_with_or_exactly_as_libtest_does() {
        let raw = args(&["parser", "lexer"]);
        let parsed = HarnessFilters::parse(&raw);

        assert!(parsed.admits("tests::parser_works"));
        assert!(parsed.admits("tests::lexer_works"));
        assert!(!parsed.admits("tests::writer_works"));
    }

    #[test]
    fn a_listing_drops_only_the_format_the_user_asked_for() {
        // A `--format` here would fight with the listing's own; everything else is carried, because
        // options such as `--include-ignored` decide the population the listing must see.
        let raw = args(&[
            "--test-threads",
            "4",
            "--format",
            "json",
            "--include-ignored",
            "--skip",
            "slow",
            "--exact",
            "parser",
        ]);
        let parsed = HarnessFilters::parse(&raw);

        assert_eq!(
            parsed.selecting(),
            ["--test-threads", "4", "--include-ignored", "--skip", "slow", "--exact", "parser"]
        );
    }

    #[test]
    fn the_joined_spelling_of_a_format_is_dropped_from_a_listing_too() {
        let raw = args(&["--format=json", "--skip=slow", "parser"]);
        let parsed = HarnessFilters::parse(&raw);

        assert_eq!(parsed.selecting(), ["--skip=slow", "parser"]);
    }
}
