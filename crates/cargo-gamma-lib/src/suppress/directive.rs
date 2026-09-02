// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::mem;
use core::ops::Range;

use proc_macro2::{Delimiter, TokenStream, TokenTree};

use super::intent::Intent;
use crate::Result;
use crate::error::Error;
use crate::model::{Channel, Mutant};
use crate::ops::registry::Selection;
use crate::parse::SourceFile;

/// One directive, resolved and located.
#[derive(Debug, Clone)]
pub struct Directive {
    /// What the directive asks for, if any.
    pub intent: Option<Intent>,

    /// The mutators it names, already resolved against the registry.
    pub selection: Selection,

    /// The selector text as written, for diagnostics.
    pub selectors: String,

    /// The stated reason, if any.
    pub reason: Option<String>,

    /// The stated tag, if any.
    pub tag: Option<String>,

    /// The timeout multiplier override, if any.
    pub test_timeout_multiplier: Option<f64>,

    /// How the directive arrived.
    pub channel: Channel,

    /// One-based line the directive appears on.
    pub line: usize,

    /// The byte range the directive governs.
    pub scope: Range<usize>,
}

impl Directive {
    /// Returns whether this directive governs a mutant.
    #[must_use]
    pub fn governs(&self, mutant: &Mutant) -> bool {
        mutant.span.start >= self.scope.start && mutant.span.start < self.scope.end && self.selection.contains(&mutant.mutator)
    }
}

/// Turns parsed arguments into a directive.
pub(super) fn build(
    intent: Option<Intent>,
    arguments: &TokenStream,
    channel: Channel,
    line: usize,
    scope: Range<usize>,
    file: &SourceFile,
) -> Result<Directive> {
    let parsed = parse_arguments(arguments);

    if let Some(err) = parsed.errors.first() {
        return Err(Error::new(format!("{}:{line}: {err}", file.path())).usage());
    }

    // An unrecognized `name = value` pair is a typo, and swallowing it is dangerous rather than
    // merely lossy: when it is the only argument the selector list is left empty, which is read
    // below as "suppress everything in scope". A misspelled `reason` would therefore silently
    // widen a directive from nothing to the whole item.
    if let Some(name) = parsed.unknown.first() {
        return Err(Error::new(format!(
            "{}:{line}: unknown argument `{name}` in directive, expected `reason`, `tag`, or `test_timeout_multiplier`",
            file.path()
        ))
        .usage());
    }

    let selection = if parsed.selectors.is_empty() {
        // A bare directive means all of them.
        Selection::everything()
    } else {
        let mut selection = Selection::empty();

        selection
            .apply(&parsed.selectors)
            .map_err(|error| Error::new(format!("{}:{line}: {error}", file.path())).usage())?;

        selection
    };

    Ok(Directive {
        intent,
        selection,
        selectors: parsed.selectors,
        reason: parsed.reason,
        tag: parsed.tag,
        test_timeout_multiplier: parsed.test_timeout_multiplier,
        channel,
        line,
        scope,
    })
}

/// The parts of a directive's argument list.
#[derive(Debug, Default)]
struct Arguments {
    selectors: String,
    reason: Option<String>,
    tag: Option<String>,
    test_timeout_multiplier: Option<f64>,

    /// Named arguments whose name is not recognized.
    unknown: Vec<String>,

    /// Errors encountered during parsing.
    errors: Vec<String>,
}

/// Splits a directive's arguments into selectors and named values.
///
/// The tokens are read directly rather than through `syn`'s meta parser, because a selector like
/// `arith.add_to_sub` or `@default` or `!bitwise` is a perfectly good token sequence but not a
/// well-formed meta path. Reading tokens keeps the directive grammar identical to the one
/// `--mutators` accepts, which is the whole point of having a single vocabulary.
fn parse_arguments(tokens: &TokenStream) -> Arguments {
    let mut arguments = Arguments::default();
    let mut selectors: Vec<String> = Vec::new();
    let mut current: Vec<TokenTree> = Vec::new();

    let mut flush = |current: &mut Vec<TokenTree>, arguments: &mut Arguments| {
        if current.is_empty() {
            return;
        }

        let taken = mem::take(current);

        // Check for `name = value` named argument.
        if taken.len() >= 3
            && let TokenTree::Ident(name) = &taken[0]
            && let TokenTree::Punct(equals) = &taken[1]
            && equals.as_char() == '='
        {
            let name = name.to_string();
            let value = &taken[2..];
            let trailing = match name.as_str() {
                "reason" | "tag" => value.len() != 1,
                "test_timeout_multiplier" | "timeout_multiplier" | "multiplier" | "factor" => {
                    value.len() != 1
                        && !(value.len() == 2 && matches!(&value[0], TokenTree::Punct(punct) if matches!(punct.as_char(), '+' | '-')))
                }
                _ => false,
            };

            if trailing {
                arguments.errors.push(format!("trailing tokens after `{name}` value"));

                return;
            }

            if matches!(name.as_str(), "reason" | "tag") && !is_string_literal(value) {
                arguments.errors.push(format!("`{name}` must be a string literal"));

                return;
            }

            let rhs = value
                .iter()
                .map(|token| match token {
                    TokenTree::Literal(literal) => unquote(&literal.to_string()),
                    other => other.to_string(),
                })
                .collect::<String>();
            let text = rhs.trim().to_owned();

            match name.as_str() {
                "reason" => arguments.reason = Some(text),
                "tag" => arguments.tag = Some(text),
                "test_timeout_multiplier" | "timeout_multiplier" | "multiplier" | "factor" => match text.parse::<f64>() {
                    // Bounded through `bounds::factor` rather than by a local positivity check, so
                    // that a directive cannot smuggle in a multiplier the command line and the
                    // config file would both refuse. An unbounded one reaches `Duration::mul_f64`,
                    // which panics — turning a typo in a comment into an internal error that
                    // discards a completed build and baseline.
                    Ok(val) => match crate::bounds::factor(&text, val) {
                        Ok(bounded) => state_multiplier(arguments, bounded),
                        Err(message) => arguments.errors.push(format!("timeout multiplier {message}")),
                    },
                    Err(_cause) => {
                        arguments
                            .errors
                            .push(format!("timeout multiplier must be a positive number, got `{text}`"));
                    }
                },
                other => arguments.unknown.push(other.to_owned()),
            }

            return;
        }

        let rendered: String = taken
            .iter()
            .map(|token| match token {
                TokenTree::Literal(literal) => unquote(&literal.to_string()),
                other => other.to_string(),
            })
            .collect::<String>();

        let cleaned: String = rendered.chars().filter(|character| !character.is_whitespace()).collect();

        if !cleaned.is_empty() {
            if let Ok(val) = cleaned.parse::<f64>() {
                // As with the named form: a value that parses as a number is a multiplier and must
                // clear the same bound the other two entry points apply. One that does not parse is
                // a selector, not a bad multiplier, so it is not an error here.
                match crate::bounds::factor(&cleaned, val) {
                    Ok(bounded) => state_multiplier(arguments, bounded),
                    Err(message) => arguments.errors.push(format!("timeout multiplier {message}")),
                }
            } else {
                selectors.push(cleaned);
            }
        }
    };

    for token in tokens.clone() {
        match &token {
            TokenTree::Punct(punct) if punct.as_char() == ',' => flush(&mut current, &mut arguments),
            TokenTree::Group(group) if group.delimiter() == Delimiter::None => {
                current.extend(group.stream());
            }
            _ => current.push(token),
        }
    }

    flush(&mut current, &mut arguments);
    arguments.selectors = selectors.join(",");
    arguments
}

/// Records the one timeout multiplier a directive may state, refusing a second.
///
/// An item has one timeout, so a second multiplier — positional or keyed, and in whichever order
/// the two were written — can only mean the author believes something other than what would
/// happen. Keeping whichever arrived last hides that behind a directive that looks like it says two
/// things and quietly does one. Refusing says which argument to delete, and matches the proc-macro
/// validator in `cargo-gamma-attrs-impl`, so uncommenting a directive cannot change the verdict on
/// text that is otherwise character-for-character identical.
fn state_multiplier(arguments: &mut Arguments, value: f64) {
    if arguments.test_timeout_multiplier.is_some() {
        arguments
            .errors
            .push("a timeout multiplier is stated a second time; only one may apply to an item".to_owned());

        return;
    }

    arguments.test_timeout_multiplier = Some(value);
}

/// Returns whether `tokens` are exactly one Rust string literal.
///
/// A `Literal` can also be a number, character, byte string, or C string. Metadata is displayed
/// as text, so it accepts only the `&str` forms that the attribute macro accepts.
fn is_string_literal(tokens: &[TokenTree]) -> bool {
    matches!(tokens, [TokenTree::Literal(literal)] if syn::parse_str::<syn::LitStr>(&literal.to_string()).is_ok())
}

/// Strips the quotes from a string literal's rendered form.
///
/// A raw string carries its own delimiters — an `r`, then a run of hashes, then the quote, mirrored
/// at the end — and all of them are punctuation rather than content. They are removed by matching
/// the run rather than by trimming, because trimming would eat characters that belong to the text:
/// `r#"say "no""#` ends in two quotes and a hash, only one of which is a delimiter.
///
/// Anything that is not a string literal passes through unchanged — the *whole* input, not the
/// remains of the prefix strip, which is what the selector rendering path relies on.
fn unquote(text: &str) -> String {
    let body = text.strip_prefix('r').unwrap_or(text);
    let hashes = body.len() - body.trim_start_matches('#').len();

    let Some(body) = body.get(hashes..body.len().saturating_sub(hashes)) else {
        return text.to_owned();
    };

    body.strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .map_or_else(|| text.to_owned(), ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use proc_macro2::{Group, Ident, Literal, Punct, Spacing};

    use super::super::directives;
    use super::*;

    fn file(source: &str) -> SourceFile {
        SourceFile::parse("test.rs", source.to_owned()).unwrap()
    }

    /// Text that is not a string literal comes back exactly as it went in.
    ///
    /// The selector rendering path hands every token through here, so anything this mangles
    /// becomes a selector naming a mutator nobody wrote — which is a directive silencing the wrong
    /// family, the one failure this subsystem must not have. An identifier beginning with `r` is
    /// the shape that gets it wrong, because the raw-string prefix is stripped before anything has
    /// established that there is a string here at all.
    #[test]
    fn anything_that_is_not_a_string_literal_passes_through_unquote_unchanged() {
        for text in ["range", "relational", "r", "r#raw", "arith.add_to_sub", "1", ""] {
            assert_eq!(unquote(text), text);
        }
    }

    /// The quoted forms still lose exactly their delimiters and nothing else.
    #[test]
    fn a_string_literal_loses_its_delimiters_and_keeps_its_text() {
        assert_eq!(unquote("\"why\""), "why");
        assert_eq!(unquote("r\"why\""), "why");
        assert_eq!(unquote("r#\"say \"no\"\"#"), "say \"no\"");
    }

    #[test]
    fn a_dotted_selector_survives_being_read_as_tokens() {
        let source = "#[gamma::skip(arith.add_to_sub)]\nfn f(a: i32) -> i32 { a + 1 }";
        let found = directives(&file(source)).unwrap();

        assert_eq!(found[0].selectors, "arith.add_to_sub");
    }

    #[test]
    fn a_profile_selector_is_accepted() {
        let source = "#[gamma::skip(@arithmetic)]\nfn f(a: i32) -> i32 { a + 1 }";
        let found = directives(&file(source)).unwrap();

        assert_eq!(found[0].selectors, "@arithmetic");
        assert!(found[0].selection.contains("arith.add_to_sub"));
    }

    #[test]
    fn a_negated_selector_is_accepted() {
        let source = "#[gamma::skip(arith, !arith.add_to_sub)]\nfn f(a: i32) -> i32 { a + 1 }";
        let found = directives(&file(source)).unwrap();

        assert!(!found[0].selection.contains("arith.add_to_sub"));
        assert!(found[0].selection.contains("arith.mul_to_div"));
    }

    #[test]
    fn a_reason_is_captured() {
        let source = "#[gamma::skip(arith, reason = \"fixed point\")]\nfn f(a: i32) -> i32 { a + 1 }";
        let found = directives(&file(source)).unwrap();

        assert_eq!(found[0].reason.as_deref(), Some("fixed point"));
        assert_eq!(found[0].selectors, "arith");
    }

    #[test]
    fn a_tag_is_captured() {
        let source = "#[gamma::skip(arith, tag = \"perf\")]\nfn f(a: i32) -> i32 { a + 1 }";
        let found = directives(&file(source)).unwrap();

        assert_eq!(found[0].tag.as_deref(), Some("perf"));
    }

    /// A raw string is a string, and it is how a reason embeds a quote without escaping it. The
    /// delimiters are punctuation and must not reach the report, and the hardest case is the one
    /// whose text itself ends in a quote: `r#"say "no""#` closes with two quotes and a hash, and
    /// only the last of each is a delimiter.
    #[test]
    fn a_raw_string_reason_keeps_its_text_and_loses_its_delimiters() {
        let source = "#[gamma::skip(arith, reason = r#\"say \"no\"\"#)]\nfn f(a: i32) -> i32 { a + 1 }";
        let found = directives(&file(source)).unwrap();

        assert_eq!(found[0].reason.as_deref(), Some("say \"no\""));

        let source = "#[gamma::skip(arith, tag = r\"perf\")]\nfn f(a: i32) -> i32 { a + 1 }";
        let found = directives(&file(source)).unwrap();

        assert_eq!(found[0].tag.as_deref(), Some("perf"));
    }

    /// An ordinary string whose text ends in a hash must not have it stripped: the hash count is
    /// read from the opening delimiter, and there is none here.
    #[test]
    fn a_hash_inside_an_ordinary_string_is_left_alone() {
        let source = "#[gamma::skip(arith, reason = \"#1\")]\nfn f(a: i32) -> i32 { a + 1 }";
        let found = directives(&file(source)).unwrap();

        assert_eq!(found[0].reason.as_deref(), Some("#1"));
    }

    /// Dropping an unrecognised `name = "value"` pair on the floor would be worse than lossy:
    /// `#[gamma::skip(op = "arith")]` leaves no selectors behind, and an empty selector list
    /// means "everything in scope", so a single typo would silently escalate a narrow suppression
    /// into a total one — in a tool whose entire job is to not silently suppress mutants.
    #[test]
    fn an_unknown_named_argument_is_rejected_rather_than_silently_widening_the_directive() {
        let source = "#[gamma::skip(op = \"arith\")]\nfn f(a: i32) -> i32 { a + 1 }";
        let error = directives(&file(source)).expect_err("an unknown named argument is a usage error");

        assert!(error.is_usage(), "{error}");
        assert!(error.to_string().contains("unknown argument `op`"), "{error}");
    }

    /// The two names that do exist are the ones worth misspelling, and dropping the value of
    /// either loses a diagnostic the user wrote on purpose.
    #[test]
    fn a_misspelled_reason_or_tag_is_rejected_rather_than_losing_its_value() {
        for (typo, source) in [
            ("reasn", "#[gamma::skip(arith, reasn = \"x\")]\nfn f(a: i32) -> i32 { a + 1 }"),
            ("tga", "#[gamma::skip(arith, tga = \"x\")]\nfn f(a: i32) -> i32 { a + 1 }"),
        ] {
            let error = directives(&file(source)).expect_err("a misspelled named argument is a usage error");

            assert!(error.is_usage(), "{typo}: {error}");
            assert!(error.to_string().contains(&format!("unknown argument `{typo}`")), "{error}");
        }
    }

    /// Metadata on comment directives has the same string-only schema as real attributes. Without
    /// this, `reason = performance` is not a selector, leaving a bare directive that suppresses
    /// everything in scope.
    #[test]
    fn non_string_comment_metadata_is_rejected_rather_than_widening_a_suppression() {
        for (name, value) in [("reason", "performance"), ("tag", "42"), ("reason", "b\"bytes\"")] {
            let source = format!("// #[gamma::skip({name} = {value})]\nfn f(a: i32) -> i32 {{ a + 1 }}");
            let error = directives(&file(&source)).expect_err("metadata must be a string literal");

            assert!(error.is_usage(), "{name} = {value}: {error}");
            assert!(
                error.to_string().contains(&format!("`{name}` must be a string literal")),
                "{name} = {value}: {error}"
            );
        }
    }

    #[test]
    fn literal_selectors_and_none_delimited_groups_are_rendered() {
        let literal = TokenTree::Literal(Literal::string("arith.add_to_sub"));
        let comma = TokenTree::Punct(Punct::new(',', Spacing::Alone));
        let grouped = TokenTree::Group(Group::new(
            Delimiter::None,
            TokenStream::from(TokenTree::Ident(Ident::new("literal", proc_macro2::Span::call_site()))),
        ));
        let tokens = [literal, comma, grouped].into_iter().collect();
        let arguments = parse_arguments(&tokens);

        assert_eq!(arguments.selectors, "arith.add_to_sub,literal");
    }

    #[test]
    fn an_unknown_selector_is_a_hard_error() {
        let source = "#[gamma::skip(arith.add_to_multiply)]\nfn f(a: i32) -> i32 { a + 1 }";
        let error = directives(&file(source)).unwrap_err();

        assert!(error.is_usage());
        assert!(error.to_string().contains("add_to_multiply"));
    }

    #[test]
    fn an_unknown_directive_name_is_a_hard_error() {
        let source = "// #[gamma::skipp(arith)]\nfn f(a: i32) -> i32 { a + 1 }";

        _ = directives(&file(source)).expect_err("the directive was expected to be rejected");
    }

    #[test]
    fn a_malformed_comment_directive_is_a_hard_error() {
        let source = "// #[gamma::skip(arith\nfn f(a: i32) -> i32 { a + 1 }";

        _ = directives(&file(source)).expect_err("the directive was expected to be rejected");
    }

    #[test]
    fn a_directive_governing_nothing_is_a_hard_error() {
        let source = "fn f(a: i32) -> i32 { a + 1 }\n// #[gamma::skip(arith)]\n";

        _ = directives(&file(source)).expect_err("the directive was expected to be rejected");
    }
}
