// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Narrow proofs of mutant-induced stalled loops and retained-memory growth.

use core::ops::Range;

use proc_macro2::{Delimiter, Group, Ident, Literal, Punct, Spacing, TokenStream, TokenTree};
use quote::ToTokens as _;
use rustc_lexer::{LiteralKind, TokenKind};
use syn::spanned::Spanned as _;
use syn::{BinOp, Block, Expr, Lit, Stmt, UnOp};

use crate::HashMap;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Binding {
    Bool,
    Integer,
    String,
    PeekableChars,
    StrReference,
}

pub(super) struct LoopProof {
    span: Range<usize>,
    bindings: HashMap<String, Binding>,
    standard_char: bool,
    literals: HashMap<String, Literal>,
}

#[derive(Debug, PartialEq)]
enum Hazard {
    Nontermination,
    UnboundedGrowth,
}

struct Iteration {
    grows: bool,
    restarts: bool,
}

impl LoopProof {
    pub(super) fn new(expression: &Expr, bindings: HashMap<String, Binding>, standard_char: bool) -> Option<Self> {
        let mut proof = Self {
            span: expression.span().byte_range(),
            bindings,
            standard_char,
            literals: HashMap::default(),
        };
        // Keep proof work bounded; exceeding the budget never excludes a candidate.
        if proof.span.len() > 4096 {
            return None;
        }
        Self::collect_literals(expression.to_token_stream(), &mut proof.literals);
        proof.proves(expression).is_none().then_some(proof)
    }

    pub(super) fn omits(&self, source: &str, span: &Range<usize>, replacement: &str) -> bool {
        if span.start < self.span.start || span.end > self.span.end {
            return false;
        }
        let Some(original) = source.get(self.span.clone()) else {
            return false;
        };
        let mut mutated = original.to_owned();
        mutated.replace_range(span.start - self.span.start..span.end - self.span.start, replacement);
        let Some(tokens) = Self::proof_tokens(&mutated, &self.literals) else {
            return false;
        };
        syn::parse2::<Expr>(tokens).is_ok_and(|expression| self.proves(&expression).is_some())
    }

    fn collect_literals(tokens: TokenStream, literals: &mut HashMap<String, Literal>) {
        for token in tokens {
            match token {
                TokenTree::Group(group) => Self::collect_literals(group.stream(), literals),
                TokenTree::Literal(literal) => {
                    let _previous = literals.insert(literal.to_string(), literal);
                }
                TokenTree::Ident(_) | TokenTree::Punct(_) => {}
            }
        }
    }

    // Reuse source literals and construct only the tokens needed by the narrow proof language.
    // Parsing strings with proc-macro2 would retain a new source-map entry on every probe.
    fn proof_tokens(text: &str, literals: &HashMap<String, Literal>) -> Option<TokenStream> {
        let mut groups = vec![(Delimiter::None, TokenStream::new())];
        let mut at = 0;
        for token in rustc_lexer::tokenize(text) {
            let spelling = text.get(at..at + token.len)?;
            at += token.len;
            let delimiter = match token.kind {
                TokenKind::OpenParen | TokenKind::CloseParen => Some(Delimiter::Parenthesis),
                TokenKind::OpenBrace | TokenKind::CloseBrace => Some(Delimiter::Brace),
                TokenKind::OpenBracket | TokenKind::CloseBracket => Some(Delimiter::Bracket),
                _ => None,
            };
            let tree = match token.kind {
                TokenKind::Whitespace => continue,
                TokenKind::LineComment | TokenKind::BlockComment { terminated: true } => {
                    if (spelling.starts_with("///") && !spelling.starts_with("////"))
                        || spelling.starts_with("//!")
                        || (spelling.starts_with("/**") && !spelling.starts_with("/***"))
                        || spelling.starts_with("/*!")
                    {
                        return None;
                    }
                    continue;
                }
                TokenKind::OpenParen | TokenKind::OpenBrace | TokenKind::OpenBracket => {
                    groups.push((delimiter?, TokenStream::new()));
                    continue;
                }
                TokenKind::CloseParen | TokenKind::CloseBrace | TokenKind::CloseBracket => {
                    if groups.len() == 1 {
                        return None;
                    }
                    let (opened, tokens) = groups.pop()?;
                    if Some(opened) != delimiter {
                        return None;
                    }
                    TokenTree::Group(Group::new(opened, tokens))
                }
                TokenKind::Ident => TokenTree::Ident(Ident::new(spelling, proc_macro2::Span::call_site())),
                TokenKind::RawIdent => TokenTree::Ident(Ident::new_raw(spelling.strip_prefix("r#")?, proc_macro2::Span::call_site())),
                TokenKind::Literal { kind, suffix_start } => {
                    let literal = if let Some(literal) = literals.get(spelling) {
                        literal.clone()
                    } else if spelling == "\"\"" {
                        Literal::string("")
                    } else if matches!(kind, LiteralKind::Int { empty_int: false, .. })
                        && suffix_start == spelling.len()
                        && let Ok(value) = spelling.replace('_', "").parse::<u128>()
                    {
                        Literal::u128_unsuffixed(value)
                    } else {
                        return None;
                    };
                    TokenTree::Literal(literal)
                }
                _ if spelling.len() == 1 && spelling.chars().all(|character| "~!@#$%^&*-=+|;:,<>./?'".contains(character)) => {
                    let character = spelling.chars().next()?;
                    let joint = text
                        .get(at..)
                        .and_then(|rest| rest.chars().next())
                        .is_some_and(|next| "~!@#$%^&*-=+|;:,<>./?'".contains(next));
                    TokenTree::Punct(Punct::new(character, if joint { Spacing::Joint } else { Spacing::Alone }))
                }
                _ => return None,
            };
            groups.last_mut()?.1.extend([tree]);
        }
        (groups.len() == 1).then(|| groups.remove(0).1)
    }

    fn proves(&self, expression: &Expr) -> Option<Hazard> {
        let (condition, body) = match expression {
            Expr::While(node) if node.attrs.is_empty() => (Some(&*node.cond), &node.body),
            Expr::Loop(node) if node.attrs.is_empty() => (None, &node.body),
            _ => return None,
        };
        if let Some(condition) = condition {
            for (name, kind) in &self.bindings {
                if *kind == Binding::PeekableChars
                    && self.at_eof(condition, name) == Some(true)
                    && let Some(grows) = self.exhausted_body(body, name)
                {
                    return Some(if grows { Hazard::UnboundedGrowth } else { Hazard::Nontermination });
                }
            }
        }
        if let Some(condition) = condition
            && (self.scalar(condition) != Some(Binding::Bool) || literal_bool(condition) == Some(false))
        {
            return None;
        }
        // Every supported body operation preserves scalar bindings. The condition is therefore
        // invariant once true. String appends may fail allocation, but cannot restore progress.
        self.block(body).map(|iteration| {
            if iteration.grows {
                Hazard::UnboundedGrowth
            } else {
                Hazard::Nontermination
            }
        })
    }

    fn block(&self, block: &Block) -> Option<Iteration> {
        let mut grows = false;
        for statement in &block.stmts {
            let Stmt::Expr(expression, _) = statement else {
                return None;
            };
            match expression {
                Expr::Continue(node) if node.attrs.is_empty() && node.label.is_none() => {
                    return Some(Iteration { grows, restarts: true });
                }
                Expr::Block(node) if node.attrs.is_empty() && node.label.is_none() => {
                    let iteration = self.block(&node.block)?;
                    grows |= iteration.grows;
                    if iteration.restarts {
                        return Some(Iteration { grows, restarts: true });
                    }
                }
                Expr::If(node) if node.attrs.is_empty() => {
                    if self.scalar(&node.cond) != Some(Binding::Bool) {
                        return None;
                    }
                    // Only constant branches are folded; unknown branches may hide an escape.
                    let iteration = if literal_bool(&node.cond)? {
                        Some(self.block(&node.then_branch)?)
                    } else if let Some((_, otherwise)) = &node.else_branch {
                        let Expr::Block(otherwise) = &**otherwise else {
                            return None;
                        };
                        if !otherwise.attrs.is_empty() || otherwise.label.is_some() {
                            return None;
                        }
                        Some(self.block(&otherwise.block)?)
                    } else {
                        None
                    };
                    if let Some(iteration) = iteration {
                        grows |= iteration.grows;
                        if iteration.restarts {
                            return Some(Iteration { grows, restarts: true });
                        }
                    }
                }
                _ => grows |= self.preserves(expression)?,
            }
        }
        Some(Iteration { grows, restarts: false })
    }

    fn preserves(&self, expression: &Expr) -> Option<bool> {
        match expression {
            Expr::Assign(node) if node.attrs.is_empty() => {
                let name = bare_name(&node.left)?;
                let kind = *self.bindings.get(&name)?;
                if !matches!(kind, Binding::Bool | Binding::Integer) || bare_name(&node.right).as_ref() != Some(&name) {
                    return None;
                }
                Some(false)
            }
            Expr::Binary(node) if node.attrs.is_empty() && matches!(node.op, BinOp::AddAssign(_) | BinOp::SubAssign(_)) => {
                let name = bare_name(&node.left)?;
                if self.bindings.get(&name) != Some(&Binding::Integer) || !integer_zero(&node.right) {
                    return None;
                }
                Some(false)
            }
            Expr::MethodCall(node) if node.attrs.is_empty() && node.turbofish.is_none() && node.args.len() == 1 => {
                let name = bare_name(&node.receiver)?;
                if self.bindings.get(&name) != Some(&Binding::String) {
                    return None;
                }
                let Expr::Lit(argument) = &node.args[0] else {
                    return None;
                };
                if !argument.attrs.is_empty() {
                    return None;
                }
                match (&*node.method.to_string(), &argument.lit) {
                    ("push", Lit::Char(_)) => Some(true),
                    ("push_str", Lit::Str(value)) => Some(!value.value().is_empty()),
                    _ => None,
                }
            }
            _ => self.scalar(expression).map(|_| false),
        }
    }

    fn scalar(&self, expression: &Expr) -> Option<Binding> {
        match expression {
            Expr::Paren(node) if node.attrs.is_empty() => self.scalar(&node.expr),
            Expr::Group(node) if node.attrs.is_empty() => self.scalar(&node.expr),
            Expr::Path(node) if node.attrs.is_empty() => {
                let name = bare_name(expression)?;
                let kind = *self.bindings.get(&name)?;
                if !matches!(kind, Binding::Bool | Binding::Integer) {
                    return None;
                }
                Some(kind)
            }
            Expr::Lit(node) if node.attrs.is_empty() => match node.lit {
                Lit::Bool(_) => Some(Binding::Bool),
                Lit::Int(_) => Some(Binding::Integer),
                _ => None,
            },
            Expr::Unary(node) if node.attrs.is_empty() && matches!(node.op, UnOp::Not(_)) => {
                (self.scalar(&node.expr)? == Binding::Bool).then_some(Binding::Bool)
            }
            Expr::Binary(node) if node.attrs.is_empty() => {
                let left = self.scalar(&node.left)?;
                let right = self.scalar(&node.right)?;
                match node.op {
                    BinOp::And(_) | BinOp::Or(_) if left == Binding::Bool && right == Binding::Bool => Some(Binding::Bool),
                    BinOp::Eq(_) | BinOp::Ne(_) | BinOp::Lt(_) | BinOp::Le(_) | BinOp::Gt(_) | BinOp::Ge(_) if left == right => {
                        Some(Binding::Bool)
                    }
                    _ => None,
                }
            }
            Expr::MethodCall(node) if self.peek_predicate(node).is_some() => Some(Binding::Bool),
            _ => None,
        }
    }

    fn peek_predicate(&self, node: &syn::ExprMethodCall) -> Option<String> {
        if !node.attrs.is_empty() || node.turbofish.is_some() {
            return None;
        }
        let Expr::MethodCall(peek) = &*node.receiver else {
            return None;
        };
        if !peek.attrs.is_empty() || peek.turbofish.is_some() || !peek.args.is_empty() || peek.method != "peek" {
            return None;
        }
        let name = bare_name(&peek.receiver)?;
        if self.bindings.get(&name) != Some(&Binding::PeekableChars) {
            return None;
        }
        match node.method.to_string().as_str() {
            "is_none" | "is_some" if node.args.is_empty() => Some(name),
            "is_some_and" if node.args.len() == 1 && self.char_predicate(&node.args[0]) => Some(name),
            _ => None,
        }
    }

    fn char_predicate(&self, expression: &Expr) -> bool {
        match expression {
            Expr::Path(node) if self.standard_char && node.attrs.is_empty() && node.qself.is_none() => {
                let segments = node.path.segments.iter().collect::<Vec<_>>();
                matches!(segments.as_slice(), [ty, method]
                    if ty.ident == "char" && matches!(ty.arguments, syn::PathArguments::None)
                        && matches!(method.arguments, syn::PathArguments::None) && char_method(&method.ident.to_string()))
            }
            Expr::Closure(node) if node.attrs.is_empty() && node.asyncness.is_none() && node.inputs.len() == 1 => {
                let syn::Pat::Ident(parameter) = &node.inputs[0] else {
                    return false;
                };
                if parameter.by_ref.is_some() || parameter.subpat.is_some() {
                    return false;
                }
                let body = match &*node.body {
                    Expr::Unary(unary) if unary.attrs.is_empty() && matches!(unary.op, UnOp::Not(_)) => &*unary.expr,
                    body => body,
                };
                matches!(body, Expr::MethodCall(call)
                    if call.attrs.is_empty() && call.turbofish.is_none() && call.args.is_empty()
                        && bare_name(&call.receiver).as_deref() == Some(&parameter.ident.to_string())
                        && char_method(&call.method.to_string()))
            }
            _ => false,
        }
    }

    fn at_eof(&self, expression: &Expr, iterator: &str) -> Option<bool> {
        match expression {
            Expr::Paren(node) if node.attrs.is_empty() => self.at_eof(&node.expr, iterator),
            Expr::Group(node) if node.attrs.is_empty() => self.at_eof(&node.expr, iterator),
            Expr::Unary(node) if node.attrs.is_empty() && matches!(node.op, UnOp::Not(_)) => {
                self.at_eof(&node.expr, iterator).map(|value| !value)
            }
            Expr::MethodCall(node) if self.peek_predicate(node).as_deref() == Some(iterator) => Some(node.method == "is_none"),
            _ => None,
        }
    }

    fn exhausted_body(&self, block: &Block, iterator: &str) -> Option<bool> {
        let mut grows = false;
        for statement in &block.stmts {
            let expression = match statement {
                Stmt::Expr(expression, _) => expression,
                Stmt::Local(local) if local.attrs.is_empty() && matches!(local.pat, syn::Pat::Wild(_)) => {
                    let init = local.init.as_ref()?;
                    if init.diverge.is_some() {
                        return None;
                    }
                    &*init.expr
                }
                _ => return None,
            };
            if matches!(expression, Expr::MethodCall(call)
                if call.attrs.is_empty() && call.turbofish.is_none() && call.args.is_empty()
                    && call.method == "next" && bare_name(&call.receiver).as_deref() == Some(iterator))
            {
                continue;
            }
            grows |= self.preserves(expression)?;
        }
        Some(grows)
    }
}

fn char_method(method: &str) -> bool {
    matches!(method, "is_whitespace" | "is_ascii_whitespace" | "is_ascii_digit")
}

fn bare_name(expression: &Expr) -> Option<String> {
    match expression {
        Expr::Paren(node) if node.attrs.is_empty() => bare_name(&node.expr),
        Expr::Group(node) if node.attrs.is_empty() => bare_name(&node.expr),
        Expr::Path(node) if node.attrs.is_empty() && node.qself.is_none() => node.path.get_ident().map(ToString::to_string),
        _ => None,
    }
}

fn integer_zero(expression: &Expr) -> bool {
    match expression {
        Expr::Paren(node) if node.attrs.is_empty() => integer_zero(&node.expr),
        Expr::Group(node) if node.attrs.is_empty() => integer_zero(&node.expr),
        Expr::Lit(node) if node.attrs.is_empty() => matches!(&node.lit, Lit::Int(value) if value.base10_parse::<u128>().ok() == Some(0)),
        _ => false,
    }
}

fn literal_bool(expression: &Expr) -> Option<bool> {
    match expression {
        Expr::Paren(node) if node.attrs.is_empty() => literal_bool(&node.expr),
        Expr::Group(node) if node.attrs.is_empty() => literal_bool(&node.expr),
        Expr::Lit(node) if node.attrs.is_empty() => match &node.lit {
            Lit::Bool(value) => Some(value.value),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn proof_tokens_reuse_raw_literals_and_preserve_punctuation_and_grouping() {
        use quote::ToTokens as _;
        for text in [
            "loop { r#type.push_str(r##\"text\"##); r#type.push('\\0'); break; }",
            "while i <= 0x2Ausize && ready { /* ordinary comment */ i += 1usize; }",
            "while !((i < n)) { if true { continue; } }",
        ] {
            let original: syn::Expr = syn::parse_str(text).unwrap();
            let mut literals = HashMap::default();
            LoopProof::collect_literals(original.to_token_stream(), &mut literals);
            let tokens = LoopProof::proof_tokens(text, &literals).unwrap();
            assert_eq!(syn::parse2::<syn::Expr>(tokens).unwrap(), original);
        }
        for text in [
            "loop {",
            "loop }",
            "loop ([)]",
            "loop { \\ }",
            "loop { /* unfinished",
            "loop { /// documented\n break; }",
            "loop { /** documented */ break; }",
            "'label: loop { break 'label; }",
            "loop { s.push_str(\"unknown\"); }",
        ] {
            assert!(LoopProof::proof_tokens(text, &HashMap::default()).is_none(), "{text}");
        }
        assert!(LoopProof::proof_tokens("loop { i += 0; s.push_str(\"\"); }", &HashMap::default()).is_some());
    }

    #[test]
    fn resource_proofs_distinguish_growth_from_spinning_and_unreachable_appends() {
        let proof = LoopProof {
            span: 0..0,
            bindings: HashMap::from_iter([("s".to_owned(), Binding::String)]),
            standard_char: true,
            literals: HashMap::default(),
        };
        for (expression, expected) in [
            ("loop { s.push_str(\"x\"); }", Some(Hazard::UnboundedGrowth)),
            ("loop { s.push('\\0'); }", Some(Hazard::UnboundedGrowth)),
            ("loop { s.push_str(\"\"); }", Some(Hazard::Nontermination)),
            ("loop { if true { continue; } s.push_str(\"x\"); }", Some(Hazard::Nontermination)),
            ("loop { s.push_str(\"x\"); s.clear(); }", None),
            ("loop { let s = String::new(); }", None),
        ] {
            assert_eq!(proof.proves(&syn::parse_str(expression).unwrap()), expected, "{expression}");
        }
    }

    fn scalar_proof() -> LoopProof {
        LoopProof {
            span: 0..0,
            bindings: HashMap::from_iter([
                ("i".to_owned(), Binding::Integer),
                ("n".to_owned(), Binding::Integer),
                ("ready".to_owned(), Binding::Bool),
                ("s".to_owned(), Binding::String),
                ("chars".to_owned(), Binding::PeekableChars),
            ]),
            standard_char: true,
            literals: HashMap::default(),
        }
    }

    #[test]
    fn scalar_and_preserving_operations_require_exact_binding_evidence() {
        let proof = scalar_proof();
        for (text, expected) in [
            ("(i)", Some(Binding::Integer)),
            ("ready", Some(Binding::Bool)),
            ("s", None),
            ("unknown", None),
            ("true", Some(Binding::Bool)),
            ("7", Some(Binding::Integer)),
            ("\"text\"", None),
            ("!ready", Some(Binding::Bool)),
            ("!i", None),
            ("ready && true", Some(Binding::Bool)),
            ("ready || false", Some(Binding::Bool)),
            ("i == n", Some(Binding::Bool)),
            ("i != n", Some(Binding::Bool)),
            ("i <= n", Some(Binding::Bool)),
            ("i >= n", Some(Binding::Bool)),
            ("i > n", Some(Binding::Bool)),
            ("i == ready", None),
            ("i + n", None),
        ] {
            assert_eq!(proof.scalar(&syn::parse_str(text).unwrap()), expected, "{text}");
        }
        for (text, expected) in [
            ("i = i", Some(false)),
            ("ready = ready", Some(false)),
            ("s = s", None),
            ("i = n", None),
            ("i += (0)", Some(false)),
            ("i -= 0", Some(false)),
            ("i += 1", None),
            ("ready += 0", None),
            ("s.push_str(\"x\")", Some(true)),
            ("s.push_str(\"\")", Some(false)),
            ("s.push('x')", Some(true)),
            ("s.push_str(unknown)", None),
            ("s.clear(\"x\")", None),
            ("i.push('x')", None),
        ] {
            assert_eq!(proof.preserves(&syn::parse_str(text).unwrap()), expected, "{text}");
        }
        let grouped = Expr::Group(syn::ExprGroup {
            attrs: Vec::new(),
            group_token: syn::token::Group::default(),
            expr: Box::new(syn::parse_quote!(i)),
        });
        assert_eq!(proof.scalar(&grouped), Some(Binding::Integer));
        assert_eq!(bare_name(&grouped), Some("i".to_owned()));
        for (text, boolean, zero) in [("(true)", Some(true), false), ("(0)", None, true), ("(1)", None, false)] {
            let expression = Expr::Group(syn::ExprGroup {
                attrs: Vec::new(),
                group_token: syn::token::Group::default(),
                expr: Box::new(syn::parse_str(text).unwrap()),
            });
            assert_eq!(literal_bool(&expression), boolean);
            assert_eq!(integer_zero(&expression), zero);
        }
    }

    #[test]
    fn constant_branch_proofs_preserve_restart_and_escape_semantics() {
        let proof = scalar_proof();
        for (text, expected) in [
            ("i", None),
            ("while false {}", None),
            ("while i {}", None),
            ("while ready { i = i; }", Some(Hazard::Nontermination)),
            ("loop { { s.push('x'); } }", Some(Hazard::UnboundedGrowth)),
            ("loop { { continue; } s.push('x'); }", Some(Hazard::Nontermination)),
            ("loop { if false { s.push('x'); } }", Some(Hazard::Nontermination)),
            ("loop { if false {} else { s.push('x'); } }", Some(Hazard::UnboundedGrowth)),
            ("loop { if false {} else { continue; } s.push('x'); }", Some(Hazard::Nontermination)),
            ("loop { if false {} else if true {} }", None),
            ("loop { if i {} }", None),
            ("loop { if ready {} }", None),
            ("loop { break; }", None),
            ("loop { let value = 1; }", None),
        ] {
            assert_eq!(proof.proves(&syn::parse_str(text).unwrap()), expected, "{text}");
        }
    }

    #[test]
    fn exhausted_iterator_proofs_require_a_known_predicate_and_nonprogressing_body() {
        let proof = scalar_proof();
        for (text, expected) in [
            ("(chars.peek().is_none())", Some(true)),
            ("!chars.peek().is_some()", Some(true)),
            ("chars.peek().is_some_and(char::is_whitespace)", Some(false)),
            ("chars.peek().is_some_and(|c| c.is_ascii_digit())", Some(false)),
            ("chars.peek().is_some_and(|c| !c.is_whitespace())", Some(false)),
            ("chars.peek().is_some_and(|ref c| c.is_whitespace())", None),
            ("chars.peek().is_some_and(|(c,)| c.is_whitespace())", None),
            ("chars.peek().is_some_and(|c| other.is_whitespace())", None),
            ("chars.peek().is_some_and(|c| c.unknown())", None),
            ("chars.peek().is_some_and(char::unknown)", None),
            ("chars.peek().is_some_and(unknown)", None),
            ("chars.peek().is_none(1)", None),
            ("chars.peek(1).is_none()", None),
            ("unknown.peek().is_none()", None),
        ] {
            assert_eq!(proof.at_eof(&syn::parse_str(text).unwrap(), "chars"), expected, "{text}");
        }
        let grouped = Expr::Group(syn::ExprGroup {
            attrs: Vec::new(),
            group_token: syn::token::Group::default(),
            expr: Box::new(syn::parse_quote!(chars.peek().is_none())),
        });
        assert_eq!(proof.at_eof(&grouped, "chars"), Some(true));
        for (text, expected) in [
            ("while chars.peek().is_none() { chars.next(); }", Some(Hazard::Nontermination)),
            (
                "while chars.peek().is_none() { let _ = chars.next(); s.push('x'); }",
                Some(Hazard::UnboundedGrowth),
            ),
            ("while chars.peek().is_none() { let _; }", None),
            ("while chars.peek().is_none() { let _ = chars.next() else { break; }; }", None),
            ("while chars.peek().is_none() { let value = chars.next(); }", None),
            ("while chars.peek().is_none() { chars.next(1); }", None),
        ] {
            assert_eq!(proof.proves(&syn::parse_str(text).unwrap()), expected, "{text}");
        }
    }

    #[test]
    fn mutation_probes_reject_outside_invalid_and_unparseable_source() {
        let source = "while i < n { i += 1; }";
        let expression: Expr = syn::parse_str(source).unwrap();
        let proof = LoopProof::new(&expression, scalar_proof().bindings, true).expect("an advancing loop is a proof candidate");
        let offset = source.find('1').unwrap();
        assert!(proof.omits(source, &(offset..offset + 1), "0"));
        assert!(!proof.omits(source, &(0..source.len() + 1), "0"));
        assert!(!proof.omits("", &(offset..offset + 1), "0"));
        assert!(!proof.omits(source, &(offset..offset + 1), "\"unknown\""));
        assert!(!proof.omits(source, &(offset..offset + 1), "let"));
        let mut shifted = scalar_proof();
        shifted.span = 1..2;
        assert!(!shifted.omits(source, &(0..1), "0"));
        let already_stalled: Expr = syn::parse_quote!(loop {});
        assert!(LoopProof::new(&already_stalled, HashMap::default(), true).is_none());
    }
}
