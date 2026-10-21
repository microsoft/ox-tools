// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

enum Token {
    Literal(char),
    One,
    Star,
    RecursiveStar,
    RecursivePrefix,
}

fn tokenize(pattern: &str) -> Vec<Token> {
    let characters: Vec<_> = pattern.chars().collect();
    let mut tokens = Vec::with_capacity(characters.len());
    let mut index = 0;

    while index < characters.len() {
        match characters[index] {
            '?' => {
                tokens.push(Token::One);
                // #[gamma::skip(assign.add_to_sub, reason = "moving the tokenizer backwards after `?` makes this loop allocate tokens until memory exhaustion instead of producing a result")]
                // #[gamma::skip(stmt.delete_assign, tag = "outofmemory", reason = "not consuming the question mark allocates tokens forever")]
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
