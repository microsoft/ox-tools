// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use cargo_gamma_lib::internals::commands::Cli;
use clap::Parser as _;

#[test]
fn every_manual_command_parses() {
    let mut checked = 0;
    let mut lines = include_str!("../src/main.rs").lines();

    while let Some(line) = lines.next() {
        let Some(command) = line.strip_prefix("//! cargo gamma ") else {
            continue;
        };

        let mut command = command
            .split_once(" #")
            .map_or(command, |(command, _comment)| command)
            .trim()
            .to_owned();

        while command.ends_with('\\') {
            command.pop();
            let continuation = lines
                .next()
                .and_then(|line| line.strip_prefix("//!"))
                .expect("a continued manual command has another rustdoc line");
            command.push_str(continuation.trim());
        }

        let mut arguments = vec!["cargo gamma"];

        let words = shell_words(&command);

        arguments.extend(words.iter().map(String::as_str));

        let _cli =
            Cli::try_parse_from(arguments).unwrap_or_else(|error| panic!("manual command `cargo gamma {command}` does not parse: {error}"));
        checked += 1;
    }

    assert!(checked >= 20, "only {checked} manual commands were checked");
}

fn shell_words(command: &str) -> Vec<String> {
    let command = without_command_substitutions(command);
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;

    for character in command.chars() {
        match (quote, character) {
            (Some(open), close) if open == close => quote = None,
            (None, '\'' | '"') => quote = Some(character),
            (None, whitespace) if whitespace.is_whitespace() => {
                if !word.is_empty() {
                    words.push(core::mem::take(&mut word));
                }
            }
            _ => word.push(character),
        }
    }

    assert!(quote.is_none(), "unclosed quote in manual command `{command}`");

    if !word.is_empty() {
        words.push(word);
    }

    words
}

fn without_command_substitutions(command: &str) -> String {
    let mut normalized = String::with_capacity(command.len());
    let mut characters = command.chars().peekable();

    while let Some(character) = characters.next() {
        if character != '$' || characters.peek() != Some(&'(') {
            normalized.push(character);
            continue;
        }

        let _first = characters.next();
        let mut depth = 1;

        for nested in characters.by_ref() {
            match nested {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;

                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }

        assert_eq!(depth, 0, "unclosed command substitution in manual command `{command}`");
        normalized.push('0');
    }

    normalized
}
