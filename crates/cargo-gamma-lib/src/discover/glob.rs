// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Matching source paths against the include and exclude patterns a selection carries.

use core::cell::RefCell;
use std::borrow::Cow;

thread_local! {
    static SCRATCH: RefCell<Scratch> = RefCell::new(Scratch::default());
}

#[derive(Debug, Default)]
struct Scratch {
    text: Vec<char>,
    next: Vec<bool>,
    current: Vec<bool>,
}

/// A normalized, tokenized glob that can match many paths without recompiling the pattern.
#[derive(Debug, Clone)]
pub(crate) struct Glob {
    tokens: Box<[Token]>,
    basename_only: bool,
}

impl Glob {
    #[must_use]
    pub(crate) fn new(pattern: &str) -> Self {
        let pattern = normalized(pattern);

        Self {
            tokens: tokenize(&pattern).into_boxed_slice(),
            basename_only: !pattern.contains('/'),
        }
    }

    #[must_use]
    pub(crate) fn matches(&self, path: &str) -> bool {
        let path = normalized(path);
        let text = if self.basename_only {
            path.rsplit('/').next().unwrap_or(&path)
        } else {
            &path
        };

        SCRATCH.with_borrow_mut(|scratch| glob_match(&self.tokens, text, scratch))
    }
}

/// Matches a path against a glob pattern supporting `*`, `**` and `?`.
///
/// A pattern with no separator matches against the file name alone, so `--file lexer.rs` does what
/// it looks like it does regardless of how deep the file is.
///
/// Both sides are normalised to `/` first. Paths are walked with the platform's own separator, so
/// on Windows they arrive with `\`, whereas patterns are written with `/` — they are typed on a
/// command line, checked into a config file and shared across platforms. Without normalisation
/// every pattern silently matches nothing there, and the run reports zero mutants and succeeds.
#[must_use]
pub fn matches_glob(pattern: &str, path: &str) -> bool {
    Glob::new(pattern).matches(path)
}

/// Rewrites a path or pattern so that `/` is the only separator.
///
/// Only done on Windows: a backslash is a legal character in a Unix file name, and rewriting it
/// there would make `--file "odd\name.rs"` match a file that does not exist.
pub(super) fn normalize_separators(text: &str) -> String {
    normalized(text).into_owned()
}

fn normalized(text: &str) -> Cow<'_, str> {
    if cfg!(windows) {
        Cow::Owned(text.replace('\\', "/"))
    } else {
        Cow::Borrowed(text)
    }
}

#[derive(Debug, Clone, Copy)]
enum Token {
    Literal(char),
    One,
    Star,
    RecursiveStar,
    RecursivePrefix,
}

/// Dynamic-programming glob matcher in which `*` stops at a separator and `**` does not.
///
/// Each token/path-scalar pair is considered at most once, avoiding the exponential suffix
/// retries of recursive backtracking.
fn glob_match(tokens: &[Token], text: &str, scratch: &mut Scratch) -> bool {
    scratch.text.clear();
    scratch.text.extend(text.chars());
    let width = scratch.text.len() + 1;
    scratch.next.clear();
    scratch.next.resize(width, false);
    scratch.current.clear();
    scratch.current.resize(width, false);
    scratch.next[scratch.text.len()] = true;

    for token in tokens.iter().rev() {
        scratch.current.fill(false);

        match token {
            Token::Literal(expected) => {
                for (index, character) in scratch.text.iter().enumerate() {
                    scratch.current[index] = character == expected && scratch.next[index + 1];
                }
            }
            Token::One => {
                for (index, character) in scratch.text.iter().enumerate() {
                    scratch.current[index] = *character != '/' && scratch.next[index + 1];
                }
            }
            Token::Star => {
                for index in (0..=scratch.text.len()).rev() {
                    scratch.current[index] =
                        scratch.next[index] || (index < scratch.text.len() && scratch.text[index] != '/' && scratch.current[index + 1]);
                }
            }
            Token::RecursiveStar => {
                for index in (0..=scratch.text.len()).rev() {
                    scratch.current[index] = scratch.next[index] || (index < scratch.text.len() && scratch.current[index + 1]);
                }
            }
            Token::RecursivePrefix => {
                let mut directory_match = false;

                for index in (0..=scratch.text.len()).rev() {
                    if index < scratch.text.len() && scratch.text[index] == '/' && scratch.next[index + 1] {
                        directory_match = true;
                    }

                    scratch.current[index] = scratch.next[index] || directory_match;
                }
            }
        }

        core::mem::swap(&mut scratch.next, &mut scratch.current);
    }

    scratch.next[0]
}

fn tokenize(pattern: &str) -> Vec<Token> {
    let characters: Vec<_> = pattern.chars().collect();
    let mut tokens = Vec::with_capacity(characters.len());
    let mut index = 0;

    while index < characters.len() {
        match characters[index] {
            '?' => {
                tokens.push(Token::One);
                index += 1;
            }
            '*' => {
                let start = index;
                while index < characters.len() && characters[index] == '*' {
                    index += 1;
                }

                if index - start == 1 {
                    tokens.push(Token::Star);
                } else if characters.get(index) == Some(&'/') {
                    tokens.push(Token::RecursivePrefix);
                    index += 1;
                } else {
                    tokens.push(Token::RecursiveStar);
                }
            }
            literal => {
                tokens.push(Token::Literal(literal));
                index += 1;
            }
        }
    }

    tokens
}
