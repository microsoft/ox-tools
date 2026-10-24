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

mod effects;
mod fixed;
mod grouping;
mod patterns;
mod provenance;
mod sources;

pub(super) use effects::{StableCall, certify};
pub(super) use fixed::{Primitive, projection_sites};
pub(super) use patterns::{enclosing_pattern_shadows, supported_match};
pub(super) use sources::scalar_sources;
const CHAR_WIDTH: &str = "#standard-char-width";

pub(super) fn bool_cache_type(ty: &syn::Type) -> bool {
    ty == &syn::parse_quote!(std::collections::HashMap<u32, bool>)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod acceptance;
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod source_acceptance;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Binding {
    Bool,
    Integer,
    String,
    PeekableChars,
    StrReference,
    Vec,
    VecChar,
    VecString,
    Char,
    Primitive(Primitive),
    BoolCache,
    Opaque,
    CharReference,
    MaybeChar,
}

pub(super) struct LoopProof {
    span: Range<usize>,
    bindings: HashMap<String, Binding>,
    standard_char: bool,
    literals: HashMap<String, Literal>,
    initializers: HashMap<String, Expr>,
    original: Expr,
    helpers: HashMap<String, syn::ItemFn>,
    numeric_names: bool,
    stable_calls: HashMap<String, StableCall>,
    grouping: Option<grouping::Grouping>,
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
        let span = expression.span().byte_range();
        // Conservative defaults bound cloned loop syntax, abstract-state entries, and name storage.
        // These are not measured capacities or semantic limits; exhaustion retains the candidate.
        if span.len() > 4096 || bindings.len() > 64 || bindings.keys().any(|name| name.len() > 256) {
            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        }
        let mut proof = Self {
            span,
            bindings,
            standard_char,
            literals: HashMap::default(),
            initializers: HashMap::default(),
            original: expression.clone(),
            helpers: HashMap::default(),
            numeric_names: true,
            stable_calls: HashMap::default(),
            grouping: None,
        };
        Self::collect_literals(expression.to_token_stream(), &mut proof.literals);
        proof.proves(expression).is_none().then_some(proof)
    }

    pub(super) fn initializers(mut self, file: &syn::File) -> Self {
        self.initializers = fixed::initializers(file, &self.span);
        self.helpers = fixed::helpers(file, &self.span);
        self.numeric_names = fixed::numeric_names(file, &self.span);
        self
    }

    pub(super) fn stable_calls(mut self, calls: HashMap<String, StableCall>) -> Self {
        self.stable_calls = calls;
        self
    }

    pub(super) fn grouping(
        mut self,
        file: &syn::File,
        sources: &crate::ops::collect::resource_namespaces::Namespaces,
        source: &str,
        inline: &[String],
    ) -> Self {
        self.grouping = grouping::Grouping::new(file, &self.original, &self.bindings, sources, source, inline);
        self
    }

    pub(super) fn omits(&self, source: &str, span: &Range<usize>, replacement: &str) -> bool {
        if span.start < self.span.start || span.end > self.span.end {
            return false; // #[gamma::skip(return_value.default, reason = "false is the bool default.", tag = "equivalent")]
        }
        let Some(original) = source.get(self.span.clone()) else {
            return false; // #[gamma::skip(return_value.default, reason = "false is the bool default.", tag = "equivalent")]
        };
        let mut mutated = original.to_owned();
        mutated.replace_range(span.start - self.span.start..span.end - self.span.start, replacement);
        let Some(tokens) = Self::proof_tokens(&mutated, &self.literals) else {
            return false; // #[gamma::skip(return_value.default, reason = "false is the bool default.", tag = "equivalent")]
        };
        syn::parse2::<Expr>(tokens).is_ok_and(|expression| {
            self.grouping
                .as_ref()
                .is_some_and(|grouping| grouping.proves(&self.original, &expression))
                || self.proves(&expression).is_some()
                || (!fixed::proves_with_effects(
                    &self.original,
                    &self.original,
                    &self.bindings,
                    &self.initializers,
                    &self.helpers,
                    self.numeric_names,
                    &self.stable_calls,
                ) && fixed::proves_with_effects(
                    &self.original,
                    &expression,
                    &self.bindings,
                    &self.initializers,
                    &self.helpers,
                    self.numeric_names,
                    &self.stable_calls,
                ))
        })
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
            let spelling =
                // #[gamma::skip(try.propagate_to_unwrap, reason = "Lexer byte boundaries and terminated-char quotes are valid; the guarded token stack retains its base frame.", tag = "equivalent")]
                text.get(at..at + token.len)?;
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
                        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                    }
                    continue;
                }
                TokenKind::OpenParen | TokenKind::OpenBrace | TokenKind::OpenBracket => {
                    groups.push((delimiter?, TokenStream::new()));
                    continue;
                }
                TokenKind::CloseParen | TokenKind::CloseBrace | TokenKind::CloseBracket => {
                    if groups.len() == 1 {
                        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                    }
                    let (opened, tokens) = groups.pop()?;
                    if Some(opened) != delimiter {
                        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
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
                    } else if
                    // #[gamma::skip(logical.and_remove_right, reason = "A suffixed uncached char still fails the following closing-quote check.", tag = "equivalent")]
                    matches!(kind, LiteralKind::Char { terminated: true }) && suffix_start == spelling.len() {
                        Literal::character(
                            rustc_lexer::unescape::unescape_char(
                                // #[gamma::skip(try.propagate_to_unwrap, reason = "Lexer byte boundaries and terminated-char quotes are valid; the guarded token stack retains its base frame.", tag = "equivalent")]
                                spelling.strip_prefix('\'')?.strip_suffix('\'')?,
                            )
                            .ok()?,
                        )
                    } else if matches!(kind, LiteralKind::Int { empty_int: false, .. })
                        && suffix_start == spelling.len()
                        && let Ok(value) = spelling.replace('_', "").parse::<u128>()
                    {
                        Literal::u128_unsuffixed(value)
                    } else {
                        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                    };
                    TokenTree::Literal(literal)
                }
                _ if spelling.len() == 1 &&
                    // #[gamma::skip(iter.all_to_any, reason = "The enclosing one-byte spelling guard makes this iterator a singleton.", tag = "equivalent")]
                    spelling.chars().all(|character| "~!@#$%^&*-=+|;:,<>./?'".contains(character)) =>
                {
                    let character = spelling.chars().next()?;
                    let joint = text
                        .get(at..)
                        .and_then(|rest| rest.chars().next())
                        .is_some_and(|next| "~!@#$%^&*-=+|;:,<>./?'".contains(next));
                    TokenTree::Punct(Punct::new(character, if joint { Spacing::Joint } else { Spacing::Alone }))
                }
                _ => return None, // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
            };
            // #[gamma::skip(try.propagate_to_unwrap, reason = "Lexer byte boundaries and terminated-char quotes are valid; the guarded token stack retains its base frame.", tag = "equivalent")]
            groups.last_mut()?.1.extend([tree]);
        }
        (groups.len() == 1).then(|| groups.remove(0).1)
    }

    fn proves(&self, expression: &Expr) -> Option<Hazard> {
        let (condition, body) = match expression {
            Expr::While(node) if node.attrs.is_empty() => (Some(&*node.cond), &node.body),
            Expr::Loop(node) if node.attrs.is_empty() => (None, &node.body),
            _ => return None, // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        };
        if let Some(condition) = condition {
            for name in self.bindings.keys() {
                if self.at_eof(condition, name) == Some(true)
                    && let Some(grows) = self.exhausted_body(body, name)
                {
                    return Some(if grows { Hazard::UnboundedGrowth } else { Hazard::Nontermination });
                }
            }
        }
        if let Some(condition) = condition
            && (self.scalar(condition) != Some(Binding::Bool) || literal_bool(condition) == Some(false))
        {
            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
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
                return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
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
                    // Only constant branches are folded; unknown branches may hide an escape.
                    let iteration = if literal_bool(&node.cond)? {
                        Some(self.block(&node.then_branch)?)
                    } else if let Some((_, otherwise)) = &node.else_branch {
                        let Expr::Block(otherwise) = &**otherwise else {
                            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                        };
                        if !otherwise.attrs.is_empty() || otherwise.label.is_some() {
                            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
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
                if !matches!(kind, Binding::Bool | Binding::Integer | Binding::Primitive(_))
                    || bare_name(&node.right).as_ref() != Some(&name)
                {
                    return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                }
                Some(false)
            }
            Expr::Binary(node) if node.attrs.is_empty() && matches!(node.op, BinOp::AddAssign(_) | BinOp::SubAssign(_)) => {
                let name = bare_name(&node.left)?;
                if !matches!(
                    self.bindings.get(&name),
                    Some(Binding::Integer | Binding::Primitive(Primitive::Signed(_) | Primitive::Unsigned(_)))
                ) || !integer_zero(&node.right)
                {
                    return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                }
                Some(false)
            }
            Expr::MethodCall(node) if node.attrs.is_empty() && node.turbofish.is_none() && node.args.len() == 1 => {
                let name = bare_name(&node.receiver)?;
                if self.bindings.get(&name) != Some(&Binding::String) {
                    return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                }
                let Expr::Lit(argument) = &node.args[0] else {
                    return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                };
                if !argument.attrs.is_empty() {
                    return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
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
            Expr::Path(node) if
                // #[gamma::skip(match_guard.always_true, reason = "bare_name independently rejects path attributes before lookup.", tag = "equivalent")]
                node.attrs.is_empty() => {
                let name = bare_name(expression)?;
                let kind = match *self.bindings.get(&name)? {
                    Binding::Primitive(Primitive::Unsigned(_) | Primitive::Signed(_)) => Binding::Integer,
                    kind => kind,
                };
                if !matches!(kind, Binding::Bool | Binding::Integer) {
                    return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
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
            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        }
        let Expr::MethodCall(peek) = &*node.receiver else {
            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        };
        if !peek.attrs.is_empty() || peek.turbofish.is_some() || !peek.args.is_empty() || peek.method != "peek" {
            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        }
        let name = bare_name(&peek.receiver)?;
        if self.bindings.get(&name) != Some(&Binding::PeekableChars) {
            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
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
                    return false; // #[gamma::skip(return_value.default, reason = "false is the bool default.", tag = "equivalent")]
                };
                if parameter.by_ref.is_some() || parameter.subpat.is_some() {
                    return false; // #[gamma::skip(return_value.default, reason = "false is the bool default.", tag = "equivalent")]
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
                        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                    }
                    &*init.expr
                }
                _ => return None, // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
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
        Expr::Path(node) if
            // #[gamma::skip(logical.and_remove_right, reason = "Parsed qualified-self paths cannot pass the following get_ident check.", tag = "equivalent")]
            node.attrs.is_empty() && node.qself.is_none() => node.path.get_ident().map(ToString::to_string),
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
    use syn::{ExprGroup, ExprMethodCall, Item, parse_file, parse_quote, parse_str};

    use super::super::Collector;
    use super::*;
    use crate::cfg::CfgSet;
    use crate::ops::collect::Defaults;
    use crate::ops::registry::Selection;
    use crate::parse::SourceFile;

    #[test]
    fn collector_resource_proof_admits_the_exact_source_limit() {
        let loop_text = format!("loop {{ {}break; }}", " ".repeat(4096 - "loop { break; }".len()));
        let file = SourceFile::parse("fixture.rs", format!("fn f() {{ {loop_text} }}")).unwrap();
        let defaults = Defaults::of(&file.ast);
        let selection = Selection::everything();
        let cfg = CfgSet::parse("");
        let collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
        let Item::Fn(function) = &file.ast.items[0] else { unreachable!() };
        let Stmt::Expr(expression, _) = &function.block.stmts[0] else {
            unreachable!()
        };
        assert_eq!(expression.span().byte_range().len(), 4096);
        assert!(collector.resource_proof(expression).is_some());
    }

    #[test]
    fn collector_primitive_normalization_preserves_noninteger_alias_binding_kinds() {
        let file = SourceFile::parse("fixture.rs", "type u8 = bool; type f32 = f64; fn f() { loop { break; } }".into()).unwrap();
        let defaults = Defaults::of(&file.ast);
        let selection = Selection::everything();
        let cfg = CfgSet::parse("");
        let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
        collector.bind_type_with_trust("flag".into(), parse_quote!(u8), true);
        collector.bind_type_with_trust("float".into(), parse_quote!(f32), true);
        let Item::Fn(function) = &file.ast.items[2] else { unreachable!() };
        let Stmt::Expr(expression, _) = &function.block.stmts[0] else {
            unreachable!()
        };
        let proof = collector.resource_proof(expression).unwrap();
        assert_eq!(proof.bindings.get("flag"), Some(&Binding::Bool));
        assert_eq!(
            proof.bindings.get("float"),
            Some(&Binding::Primitive(Primitive::named("f64").unwrap()))
        );
    }

    #[test]
    fn collector_open_method_scopes_reject_any_collection_even_among_scalar_bindings() {
        for collection in [false, true] {
            let file = SourceFile::parse("fixture.rs", "use dependency::Extension; fn f() { loop { break; } }".into()).unwrap();
            let defaults = Defaults::of(&file.ast);
            let selection = Selection::everything();
            let cfg = CfgSet::parse("");
            let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
            collector.bind_type_with_trust("count".into(), parse_quote!(u8), true);
            if collection {
                collector.bind_type_with_trust("values".into(), parse_quote!(Vec<u8>), true);
            }
            let Item::Fn(function) = &file.ast.items[1] else { unreachable!() };
            let Stmt::Expr(expression, _) = &function.block.stmts[0] else {
                unreachable!()
            };
            assert!(!defaults.resource_sources().closed_methods_for("fixture.rs", &[]));
            assert_eq!(collector.resource_proof(expression).is_some(), !collection);
        }
    }

    #[test]
    fn collector_projection_bindings_skip_unknown_sites_and_admit_only_supported_direct_fields() {
        let source = "
            struct Inner { count: u8 }
            struct State {
                flag: bool, count: u8, items: Vec<u8>, text: String,
                unknown: dependency::Unknown, inner: Inner,
            }
            fn f() {
                loop {
                    consume(missing.count, untrusted.count, state.unknown, state.inner.count,
                            state.missing, state.flag, state.count, &state.items, &state.text);
                    break;
                }
            }
        ";
        for (ty, vector) in [("State", false), ("&State", true), ("&mut State", false)] {
            let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
            let defaults = Defaults::of(&file.ast);
            let selection = Selection::everything();
            let cfg = CfgSet::parse("");
            let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
            collector.bind_type_with_trust("state".into(), parse_str(ty).unwrap(), true);
            collector.bind_type_with_trust("untrusted".into(), parse_quote!(State), false);
            let Item::Fn(function) = &file.ast.items[2] else { unreachable!() };
            let Stmt::Expr(expression, _) = &function.block.stmts[0] else {
                unreachable!()
            };
            let proof = collector.resource_proof(expression).unwrap();
            assert_eq!(proof.bindings.get("state.flag"), Some(&Binding::Bool), "{ty}");
            assert_eq!(proof.bindings.get("state.count"), Some(&Binding::Integer), "{ty}");
            assert_eq!(proof.bindings.get("state.items"), vector.then_some(&Binding::Vec), "{ty}");
            for name in [
                "missing.count",
                "untrusted.count",
                "state.unknown",
                "state.inner.count",
                "state.missing",
                "state.text",
            ] {
                assert!(!proof.bindings.contains_key(name), "{ty}: {name}");
            }
        }
    }

    #[test]
    fn loop_proof_input_limits_are_inclusive_and_apply_to_source_bindings_and_names_independently() {
        for length in [4095, 4096, 4097] {
            let prefix = "loop { advance();";
            let source = format!("{prefix}{}}}", " ".repeat(length - prefix.len() - 1));
            let expression: Expr = parse_str(&source).unwrap();
            assert_eq!(expression.span().byte_range().len(), length);
            assert_eq!(LoopProof::new(&expression, HashMap::default(), true).is_some(), length <= 4096);
        }
        let expression: Expr = parse_quote!(loop {
            advance();
        });
        for count in [63, 64, 65] {
            let bindings = (0..count).map(|index| (format!("ready{index}"), Binding::Bool)).collect();
            assert_eq!(LoopProof::new(&expression, bindings, true).is_some(), count <= 64);
        }
        for length in [255, 256, 257] {
            let bindings = HashMap::from_iter([("x".repeat(length), Binding::Bool)]);
            assert_eq!(LoopProof::new(&expression, bindings, true).is_some(), length <= 256);
        }
        let proof = LoopProof::new(&expression, HashMap::default(), true).unwrap();
        assert!(proof.numeric_names);
    }

    #[test]
    fn compiler_groups_and_primitive_bindings_keep_scalar_types_and_reject_group_attributes() {
        let mut proof = scalar_proof();
        proof.bindings.insert("unsigned".into(), Binding::Primitive(Primitive::Unsigned(8)));
        assert_eq!(proof.scalar(&parse_quote!(unsigned)), Some(Binding::Integer));
        assert_eq!(bare_name(&parse_quote!(<Owner>::ready)), None);
        for source in ["ready", "0", "false", "chars.peek().is_none()"] {
            let group = Expr::Group(ExprGroup {
                attrs: vec![parse_quote!(#[expand])],
                group_token: syn::token::Group::default(),
                expr: Box::new(parse_str(source).unwrap()),
            });
            assert_eq!(proof.scalar(&group), None);
            assert_eq!(proof.at_eof(&group, "chars"), None);
            assert_eq!(bare_name(&group), None);
            assert!(!integer_zero(&group));
            assert_eq!(literal_bool(&group), None);
            assert_eq!(proof.at_eof(&group, "items"), None);
        }
    }

    #[test]
    fn nested_restart_flags_must_propagate_through_more_than_one_enclosing_block() {
        let proof = scalar_proof();
        for source in [
            "loop { { { continue; } unknown(); } unknown(); }",
            "loop { { if true { continue; } unknown(); } unknown(); }",
        ] {
            assert_eq!(proof.proves(&parse_str(source).unwrap()), Some(Hazard::Nontermination), "{source}");
        }
    }

    #[test]
    fn proof_node_attributes_and_else_labels_are_checked_on_the_exact_node_not_a_child() {
        let proof = scalar_proof();
        for source in ["i = i", "i += 0", "s.push('x')"] {
            let mut expression: Expr = parse_str(source).unwrap();
            assert!(proof.preserves(&expression).is_some());
            match &mut expression {
                Expr::Assign(node) => node.attrs.push(parse_quote!(#[expand])),
                Expr::Binary(node) => node.attrs.push(parse_quote!(#[expand])),
                Expr::MethodCall(node) => node.attrs.push(parse_quote!(#[expand])),
                _ => unreachable!(),
            }
            assert_eq!(proof.preserves(&expression), None, "{source}");
        }
        assert_eq!(proof.preserves(&parse_quote!(s.push::<char>('x'))), None);
        let mut expression: Expr = parse_quote!(ready && true);
        let Expr::Binary(binary) = &mut expression else { unreachable!() };
        binary.attrs.push(parse_quote!(#[expand]));
        assert_eq!(proof.scalar(&expression), None);
        for attributed in [false, true] {
            let mut expression: Expr = parse_quote!(loop {
                if false {
                } else {
                }
            });
            let Expr::Loop(looped) = &mut expression else { unreachable!() };
            let Stmt::Expr(Expr::If(conditional), _) = &mut looped.body.stmts[0] else {
                unreachable!()
            };
            let Expr::Block(otherwise) = &mut *conditional.else_branch.as_mut().unwrap().1 else {
                unreachable!()
            };
            if attributed {
                otherwise.attrs.push(parse_quote!(#[expand]));
            } else {
                otherwise.label = Some(parse_quote!('exit:));
            }
            assert_eq!(proof.proves(&expression), None);
        }
    }

    #[test]
    fn rejected_peek_predicates_return_none_rather_than_an_empty_or_unrelated_receiver_name() {
        let proof = scalar_proof();
        for source in [
            "chars.peek(1).is_none()",
            "s.peek().is_none()",
            "chars.peek().unknown()",
            "chars.peek().is_some_and(char::is_whitespace, other)",
        ] {
            let call: ExprMethodCall = parse_str(source).unwrap();
            assert_eq!(proof.peek_predicate(&call), None, "{source}");
        }
        let mut call: ExprMethodCall = parse_quote!(chars.peek().is_none());
        let Expr::MethodCall(peek) = &mut *call.receiver else {
            unreachable!()
        };
        peek.attrs.push(parse_quote!(#[expand]));
        assert_eq!(proof.peek_predicate(&call), None);
        assert!(!proof.char_predicate(&parse_quote!(<Owner as char>::is_whitespace)));
        for source in ["/*", "/**", "/*/", "'\\q'"] {
            assert!(LoopProof::proof_tokens(source, &HashMap::default()).is_none(), "{source}");
        }
    }

    #[test]
    fn initializer_metadata_respects_primitive_shadowing_before_retaining_numeric_name_evidence() {
        let file = parse_file("struct u8; fn f() { loop { advance(); } }").unwrap();
        let Item::Fn(function) = &file.items[1] else { unreachable!() };
        let Stmt::Expr(expression, _) = &function.block.stmts[0] else {
            unreachable!()
        };
        let proof = LoopProof::new(expression, HashMap::default(), true).unwrap().initializers(&file);
        assert!(!proof.numeric_names);
    }

    #[test]
    fn a_fixed_proof_of_the_original_hazard_does_not_exclude_an_equally_hazardous_replacement() {
        let source = "while true { let local = true; }";
        let original: Expr = parse_str(source).unwrap();
        let replaced: Expr = parse_str("while true { let local = false; }").unwrap();
        let proof = LoopProof::new(&original, HashMap::default(), true).unwrap();
        assert_eq!(proof.proves(&original), None);
        assert_eq!(proof.proves(&replaced), None);
        let fixed_proves = |expression: &Expr| {
            fixed::proves_with_effects(
                &original,
                expression,
                &proof.bindings,
                &proof.initializers,
                &proof.helpers,
                proof.numeric_names,
                &proof.stable_calls,
            )
        };
        assert!(fixed_proves(&original));
        assert!(fixed_proves(&replaced));
        assert_eq!(&source[25..29], "true");
        assert!(!proof.omits(source, &(25..29), "false"));
    }

    #[test]
    fn proof_tokens_reuse_raw_literals_and_preserve_punctuation_and_grouping() {
        use quote::ToTokens as _;
        for text in [
            "loop { r#type.push_str(r##\"text\"##); r#type.push('\\0'); break; }",
            "while i <= 0x2Ausize && ready { /* ordinary comment */ i += 1usize; }",
            "while !((i < n)) { if true { continue; } }",
        ] {
            let original: Expr = parse_str(text).unwrap();
            let mut literals = HashMap::default();
            LoopProof::collect_literals(original.to_token_stream(), &mut literals);
            let tokens = LoopProof::proof_tokens(text, &literals).unwrap();
            assert_eq!(syn::parse2::<Expr>(tokens).unwrap(), original);
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
            grouping: None,
            span: 0..0,
            bindings: HashMap::from_iter([("s".to_owned(), Binding::String)]),
            standard_char: true,
            literals: HashMap::default(),
            initializers: HashMap::default(),
            original: parse_quote!(loop {}),
            stable_calls: HashMap::default(),
            helpers: HashMap::default(),
            numeric_names: true,
        };
        for (expression, expected) in [
            ("loop { s.push_str(\"x\"); }", Some(Hazard::UnboundedGrowth)),
            ("loop { s.push('\\0'); }", Some(Hazard::UnboundedGrowth)),
            ("loop { s.push_str(\"\"); }", Some(Hazard::Nontermination)),
            ("loop { if true { continue; } s.push_str(\"x\"); }", Some(Hazard::Nontermination)),
            ("loop { s.push_str(\"x\"); s.clear(); }", None),
            ("loop { let s = String::new(); }", None),
        ] {
            assert_eq!(proof.proves(&parse_str(expression).unwrap()), expected, "{expression}");
        }
    }

    fn scalar_proof() -> LoopProof {
        LoopProof {
            grouping: None,
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
            initializers: HashMap::default(),
            original: parse_quote!(loop {}),
            stable_calls: HashMap::default(),
            helpers: HashMap::default(),
            numeric_names: true,
        }
    }

    #[test]
    fn proof_budget_and_whole_loop_edits_have_inclusive_boundaries() {
        for (length, admitted) in [(4095, true), (4096, true), (4097, false)] {
            let text = format!("loop {{ /*{}*/ break; }}", " ".repeat(length - "loop { /**/ break; }".len()));
            assert_eq!(text.len(), length);
            let expression: Expr = parse_str(&text).unwrap();
            let proof = LoopProof::new(&expression, HashMap::default(), true);
            assert_eq!(proof.is_some(), admitted, "length {length}");
            if let Some(proof) = proof {
                assert!(proof.omits(&text, &(0..length), "loop {}"));
                assert!(!proof.omits(&text, &(0..length), "loop { break; }"));
                assert!(!proof.omits(&text, &(0..length), "\"not a loop\""));
            }
        }
        let text = "prefix loop { break; } suffix";
        let mut proof = scalar_proof();
        proof.span = 7..22;
        assert!(!proof.omits(text, &(6..8), ""));
        assert!(!proof.omits(text, &(21..23), ""));
        assert!(!proof.omits("short", &(7..8), "x"));
    }

    #[test]
    fn attributed_and_labelled_proof_nodes_never_supply_unconditional_evidence() {
        let proof = scalar_proof();
        for text in [
            "#[cfg(any())] loop {}",
            "#[cfg(any())] while true {}",
            "loop { #[cfg(any())] continue; }",
            "loop { continue 'outer; }",
            "loop { #[cfg(any())] {} }",
            "loop { 'inner: {} }",
            "loop { { break; } }",
            "loop { #[cfg(any())] if true {} }",
            "loop { if false {} else { #[cfg(any())] {} } }",
            "loop { if false {} else { 'inner: {} } }",
            "loop { if false {} else { break; } }",
            "loop { if 1 {} }",
            "loop { if ready {} }",
        ] {
            assert_eq!(proof.proves(&parse_str(text).unwrap()), None, "{text}");
        }
        for text in [
            "loop { {} s.push('x'); }",
            "loop { if true {} s.push('x'); }",
            "loop { if false {} else {} s.push('x'); }",
            "loop { if false {} else { s.push('x'); } }",
            "loop { { s.push('x'); continue; } }",
            "loop { if true { s.push('x'); continue; } }",
        ] {
            assert_eq!(proof.proves(&parse_str(text).unwrap()), Some(Hazard::UnboundedGrowth), "{text}");
        }
        for text in [
            "loop { { continue; } break; }",
            "loop { if true { continue; } break; }",
            "loop { if false {} else { continue; } break; }",
            "loop { if false { s.push('x'); } }",
        ] {
            assert_eq!(proof.proves(&parse_str(text).unwrap()), Some(Hazard::Nontermination), "{text}");
        }
    }

    #[test]
    fn scalar_proof_rejects_effectful_or_attributed_syntax_at_every_layer() {
        let proof = scalar_proof();
        for (text, scalar, preserving) in [
            ("((ready))", Some(Binding::Bool), Some(false)),
            ("(0)", Some(Binding::Integer), Some(false)),
            ("true", Some(Binding::Bool), Some(false)),
            ("false", Some(Binding::Bool), Some(false)),
            ("7", Some(Binding::Integer), Some(false)),
            ("!ready", Some(Binding::Bool), Some(false)),
            ("!i", None, None),
            ("ready && true", Some(Binding::Bool), Some(false)),
            ("ready || false", Some(Binding::Bool), Some(false)),
            ("i == n", Some(Binding::Bool), Some(false)),
            ("i < n", Some(Binding::Bool), Some(false)),
            ("i + n", None, None),
            ("ready && i", None, None),
            ("i || ready", None, None),
            ("i == ready", None, None),
            ("-ready", None, None),
            ("unknown", None, None),
            ("module::i", None, None),
            ("<T as Trait>::i", None, None),
            ("s", None, None),
            ("i = i", None, Some(false)),
            ("s = s", None, None),
            ("i = n", None, None),
            ("unknown = unknown", None, None),
            ("s.field = s.field", None, None),
            ("s.field += 0", None, None),
            ("s.field.push('x')", None, None),
            ("s += 0", None, None),
            ("i *= 0", None, None),
            ("s.push_str(#[cfg(any())] \"x\")", None, None),
        ] {
            let expression: Expr = parse_str(text).unwrap();
            assert_eq!(proof.scalar(&expression), scalar, "{text}");
            assert_eq!(proof.preserves(&expression), preserving, "{text}");
        }
        for text in [
            "(ready)",
            "ready",
            "true",
            "!ready",
            "ready == true",
            "i = i",
            "i += 0",
            "s.push('x')",
        ] {
            let expression: Expr = parse_str(&format!("#[cfg(any())] {text}")).unwrap();
            assert_eq!(proof.scalar(&expression), None, "{text}");
            assert_eq!(proof.preserves(&expression), None, "{text}");
        }
        for text in ["#[cfg(any())] (i)", "#[cfg(any())] i", "<T as Trait>::i", "i + 1"] {
            assert_eq!(bare_name(&parse_str(text).unwrap()), None, "{text}");
        }
        for text in ["#[cfg(any())] (0)", "#[cfg(any())] 0", "1", "true", "i"] {
            assert!(!integer_zero(&parse_str(text).unwrap()), "{text}");
        }
        for text in ["#[cfg(any())] (false)", "#[cfg(any())] false", "0", "ready"] {
            assert_eq!(literal_bool(&parse_str(text).unwrap()), None, "{text}");
        }
    }

    #[test]
    fn exhausted_iterator_evidence_rejects_each_independent_unknown_effect() {
        let proof = scalar_proof();
        for text in [
            "chars.peek().is_none::<u8>()",
            "#[cfg(any())] chars.peek().is_none()",
            "(#[cfg(any())] chars.peek()).is_none()",
            "chars.peek::<u8>().is_none()",
            "chars.peek(1).is_none()",
            "chars.next().is_none()",
            "chars.peek().is_some(1)",
            "(s.field).peek().is_none()",
            "s.peek().is_none()",
            "chars.peek().is_some_and(async |c| c.is_whitespace())",
            "chars.peek().is_some_and(|c, d| c.is_whitespace())",
            "chars.peek().is_some_and(|c @ _| c.is_whitespace())",
            "chars.peek().is_some_and(|ref c| c.is_whitespace())",
            "chars.peek().is_some_and(|c| -c.is_whitespace())",
            "chars.peek().is_some_and(|c| c.is_whitespace(1))",
            "chars.peek().is_some_and(|c| c.is_whitespace::<u8>())",
            "-chars.peek().is_none()",
            "#[cfg(any())] (chars.peek().is_none())",
            "#[cfg(any())] !chars.peek().is_none()",
        ] {
            let expression: Expr = parse_str(text).unwrap();
            assert_eq!(proof.at_eof(&expression, "chars"), None, "{text}");
        }
        for condition in ["chars.peek().is_none()", "!chars.peek().is_some()"] {
            for body in [
                "chars.next(); break;",
                "let item = chars.next();",
                "#[cfg(any())] let _ = chars.next();",
                "let _;",
                "let _ = chars.next() else { break; };",
                "chars.next::<u8>();",
                "chars.next(1);",
                "unknown.next();",
                "chars.peek();",
            ] {
                let text = format!("while {condition} {{ {body} }}");
                assert_eq!(proof.proves(&parse_str(&text).unwrap()), None, "{text}");
            }
            let text = format!("while {condition} {{ chars.next(); s.push('x'); }}");
            assert_eq!(proof.proves(&parse_str(&text).unwrap()), Some(Hazard::UnboundedGrowth), "{text}");
        }
        for method in ["is_whitespace", "is_ascii_whitespace", "is_ascii_digit"] {
            let text = format!("chars.peek().is_some_and(char::{method})");
            assert_eq!(proof.at_eof(&parse_str(&text).unwrap(), "chars"), Some(false));
        }
        let mut shadowed_char = scalar_proof();
        shadowed_char.standard_char = false;
        assert_eq!(
            shadowed_char.at_eof(&parse_quote!(chars.peek().is_some_and(char::is_whitespace)), "chars"),
            None
        );
    }

    #[test]
    fn resource_leaf_evidence_preserves_primitive_values_and_rejects_independent_effects() {
        let proof = scalar_proof();
        for (text, expected) in [
            ("i += (0)", Some(false)),
            ("i -= 0", Some(false)),
            ("i += 1", None),
            ("i -= unknown", None),
            ("s.push_str(\"\")", Some(false)),
            ("s.push_str(\"nonempty\")", Some(true)),
            ("s.push('x', 'y')", None),
            ("s.push_str()", None),
            ("s.push(unknown)", None),
            ("s.push_str(unknown)", None),
            ("s.reserve(0)", None),
        ] {
            assert_eq!(proof.preserves(&parse_str(text).unwrap()), expected, "{text}");
        }
        for text in ["0", "(0)", "((0u8))"] {
            assert!(integer_zero(&parse_str(text).unwrap()), "{text}");
        }
        for text in ["1", "(1)", "340282366920938463463374607431768211456"] {
            assert!(!integer_zero(&parse_str(text).unwrap()), "{text}");
        }
        for (text, expected) in [("true", true), ("(false)", false), ("((true))", true)] {
            assert_eq!(literal_bool(&parse_str(text).unwrap()), Some(expected), "{text}");
        }
        for text in ["i", "(i)", "((i))"] {
            assert_eq!(bare_name(&parse_str(text).unwrap()).as_deref(), Some("i"));
        }
        for text in [
            "chars.peek().is_some_and(|c| c.is_whitespace())",
            "chars.peek().is_some_and(|c| !c.is_whitespace())",
        ] {
            let expression: Expr = parse_str(text).unwrap();
            assert_eq!(proof.at_eof(&expression, "chars"), Some(false));
            assert_eq!(proof.at_eof(&expression, "other"), None);
        }
        for text in [
            "chars.peek().is_some_and(#[transform] |c| c.is_whitespace())",
            "chars.peek().is_some_and(|c| #[transform] !c.is_whitespace())",
            "chars.peek().is_some_and(|c| #[transform] c.is_whitespace())",
            "chars.peek().is_some_and(|c| other.is_whitespace())",
            "chars.peek().is_some_and(#[transform] char::is_whitespace)",
            "chars.peek().is_some_and(char::<u8>::is_whitespace)",
            "chars.peek().is_some_and(char::is_whitespace::<u8>)",
            "chars.peek().is_some_and(char::unknown)",
        ] {
            assert_eq!(proof.at_eof(&parse_str(text).unwrap(), "chars"), None, "{text}");
        }
        for text in ["#[transform] chars.peek().is_none()", "(#[transform] chars.peek()).is_none()"] {
            assert_eq!(proof.at_eof(&parse_str(text).unwrap(), "chars"), None, "{text}");
        }
        let expression: Expr = parse_quote!(while chars.peek().is_none() {
            let _ = chars.next();
        });
        assert_eq!(proof.proves(&expression), Some(Hazard::Nontermination));
        let expression: Expr = parse_quote!(while chars.peek().is_some() {
            chars.next();
        });
        assert_eq!(proof.proves(&expression), None);
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
            assert_eq!(proof.scalar(&parse_str(text).unwrap()), expected, "{text}");
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
            assert_eq!(proof.preserves(&parse_str(text).unwrap()), expected, "{text}");
        }
        let grouped = Expr::Group(ExprGroup {
            attrs: Vec::new(),
            group_token: syn::token::Group::default(),
            expr: Box::new(parse_quote!(i)),
        });
        assert_eq!(proof.scalar(&grouped), Some(Binding::Integer));
        assert_eq!(bare_name(&grouped), Some("i".to_owned()));
        for (text, boolean, zero) in [("(true)", Some(true), false), ("(0)", None, true), ("(1)", None, false)] {
            let expression = Expr::Group(ExprGroup {
                attrs: Vec::new(),
                group_token: syn::token::Group::default(),
                expr: Box::new(parse_str(text).unwrap()),
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
            assert_eq!(proof.proves(&parse_str(text).unwrap()), expected, "{text}");
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
            assert_eq!(proof.at_eof(&parse_str(text).unwrap(), "chars"), expected, "{text}");
        }
        let grouped = Expr::Group(ExprGroup {
            attrs: Vec::new(),
            group_token: syn::token::Group::default(),
            expr: Box::new(parse_quote!(chars.peek().is_none())),
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
            assert_eq!(proof.proves(&parse_str(text).unwrap()), expected, "{text}");
        }
    }

    #[test]
    fn mutation_probes_reject_outside_invalid_and_unparseable_source() {
        let source = "while i < n { i += 1; }";
        let expression: Expr = parse_str(source).unwrap();
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
        let already_stalled: Expr = parse_quote!(loop {});
        assert!(LoopProof::new(&already_stalled, HashMap::default(), true).is_none());
    }
}
