// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Whether a proposed replacement would leave the program as it was.

use rustc_lexer::{LiteralKind, TokenKind};
use syn::{Expr, ItemFn, PatIdent, ReturnType, Type};

use super::super::defaults::DefaultPaths;
use super::indexes::ScopePath;
use super::types::{Types, constructor_argument_count};
use super::values::Kind;
use crate::ops::collect::Shape;
use crate::{HashMap, HashSet};

/// Positive callee evidence kept separate from the optimistic name-only return index.
#[derive(Default)]
pub(super) struct LocalCalls {
    declarations: HashMap<String, Option<ScopePath>>,
    bound_names: HashSet<String>,
    opaque_scopes: HashSet<ScopePath>,
    module_scopes: HashSet<ScopePath>,
    imported_names: HashSet<String>,
}

/// Recognizes standard type constructors, not same-named inherent methods on local types.
pub(super) fn is_standard_default_constructor(expression: &Expr, expected: &Type, types: &Types<'_>, defaults: &DefaultPaths) -> bool {
    if types.abstracts.iter().chain(types.defaulted).any(|name| name == "Default")
        || !defaults.is_standard_default_segments(&["Default".into(), "default".into()])
    {
        return false;
    }
    let expression = match expression {
        Expr::Paren(paren) => return is_standard_default_constructor(&paren.expr, expected, types, defaults),
        Expr::Group(group) => return is_standard_default_constructor(&group.expr, expected, types, defaults),
        expression => expression,
    };
    let Expr::Call(call) = expression else {
        return false;
    };
    let Expr::Path(callee) = &*call.func else {
        return false;
    };
    if !call.attrs.is_empty() || !call.args.is_empty() || callee.qself.is_some() {
        return false;
    }
    let mut path = callee.path.clone();
    let Some(method) = path.segments.pop() else {
        return false;
    };
    if !matches!(method.arguments, syn::PathArguments::None) || !matches!(method.ident.to_string().as_str(), "new" | "default") {
        return false;
    }
    let _separator = path.segments.pop_punct();
    if path.segments.is_empty() {
        return false;
    }
    let constructor = Type::Path(syn::TypePath {
        attrs: Vec::new(),
        qself: None,
        path,
    });
    let Type::Path(constructor_path) = &constructor else {
        return false;
    };
    let Some(arguments) = constructor_argument_count(&constructor_path.path) else {
        return false;
    };
    let first = constructor_path
        .path
        .segments
        .first()
        .expect("the empty constructor path was rejected above")
        .ident
        .to_string();
    if types.abstracts.contains(&first)
        || types.defaulted.contains(&first)
        || types.aliases.is_some_and(|aliases| aliases.contains_key(&first))
    {
        return false;
    }
    let expected = types.resolve_alias(expected);
    if !types.standard_constructor_root(&constructor) || !types.standard_constructor_root(expected) {
        return false;
    }
    if method.ident == "new" {
        return types.supports_empty_default_constructor(&constructor)
            && types.supports_empty_default_constructor(expected)
            && same_standard_type(&constructor, expected, types);
    }
    if types.supports_empty_default_constructor(&constructor) && types.supports_empty_default_constructor(expected) {
        return same_standard_type(&constructor, expected, types);
    }
    let expected_arguments = match expected {
        Type::Path(path) => constructor_argument_count(&path.path),
        _ => None,
    };
    (arguments == 0
        && expected_arguments == Some(0)
        && types.supports_primitive_kind(&constructor, Kind::Bool)
        && types.supports_primitive_kind(expected, Kind::Bool))
        || (arguments <= 1
            && expected_arguments.is_some_and(|arguments| arguments <= 1)
            && types.supports_option_api(&constructor)
            && types.supports_option_api(expected))
}

fn same_standard_type(left: &Type, right: &Type, types: &Types<'_>) -> bool {
    // Imported renames are allowed only when both paths resolve to the same standard type.
    types
        .standard_type_name(left)
        .is_some_and(|left| types.standard_type_name(right).as_ref() == Some(&left))
}

pub(super) fn is_resolved_trait_default(expression: &Expr, types: &Types<'_>) -> bool {
    match expression {
        Expr::Paren(paren) => return is_resolved_trait_default(&paren.expr, types),
        Expr::Group(group) => return is_resolved_trait_default(&group.expr, types),
        _ => {}
    }
    let Expr::Call(call) = expression else {
        return false;
    };
    if !call.args.is_empty() || !call.attrs.is_empty() {
        return false;
    }
    let mut callee = &*call.func;
    while let Expr::Paren(paren) = callee {
        callee = &paren.expr;
    }
    let Expr::Path(callee) = callee else {
        return false;
    };
    if callee.qself.is_some() {
        return false;
    }
    let mut path = callee.path.clone();
    let Some(method) = path.segments.pop() else {
        return false;
    };
    if method.ident != "default" || !matches!(method.arguments, syn::PathArguments::None) {
        return false;
    }
    let _separator = path.segments.pop_punct();
    let generic = path.get_ident().is_some_and(|ident| types.defaulted.contains(&ident.to_string()));
    generic
        || types.supports_default_trait(&Type::Path(syn::TypePath {
            attrs: Vec::new(),
            qself: None,
            path,
        }))
}

impl LocalCalls {
    pub(super) fn function(&mut self, node: &ItemFn, scope: &ScopePath) {
        let unit = node.sig.asyncness.is_none()
            && (matches!(&node.sig.output, ReturnType::Default)
                || matches!(&node.sig.output, ReturnType::Type(_, ty) if matches!(&**ty, Type::Tuple(tuple) if tuple.elems.is_empty())));
        let ordinary_attributes = Self::nontransforming_attributes(&node.attrs);
        if !ordinary_attributes {
            self.opaque(scope);
        }
        let declaration = (unit && ordinary_attributes).then(|| scope.clone());
        let _previous = self
            .declarations
            .entry(node.sig.ident.to_string())
            .and_modify(|known| *known = None)
            .or_insert(declaration);
    }

    pub(super) fn nontransforming_attributes(attributes: &[syn::Attribute]) -> bool {
        attributes.iter().all(|attribute| {
            attribute.path().get_ident().is_some_and(|name| {
                matches!(
                    name.to_string().as_str(),
                    "cfg" | "doc" | "allow" | "expect" | "warn" | "deny" | "forbid" | "inline" | "cold" | "track_caller" | "must_use"
                )
            })
        })
    }

    pub(super) fn binding(&mut self, pattern: &PatIdent) {
        self.value_name(&pattern.ident);
    }

    pub(super) fn value_name(&mut self, name: &syn::Ident) {
        let _bound = self.bound_names.insert(name.to_string());
    }

    pub(super) fn opaque(&mut self, scope: &ScopePath) {
        let _opaque = self.opaque_scopes.insert(scope.clone());
    }

    pub(super) fn module(&mut self, scope: &ScopePath) {
        let _module = self.module_scopes.insert(scope.clone());
    }

    pub(super) fn imported(&mut self, name: &str) {
        let _imported = self.imported_names.insert(name.to_owned());
    }

    pub(super) fn scope_is_transparent(&self, scope: &ScopePath) -> bool {
        !self.opaque_scopes.iter().any(|opaque| scope.starts_with(opaque))
    }

    pub(super) fn resource_scope_is_transparent(&self, scope: &ScopePath, modules_verified: bool) -> bool {
        // Verified namespace closure supplies declarations hidden behind root/module opacity.
        // It does not resolve block-local transformations or shadowing, so inner opacity still wins.
        !self
            .opaque_scopes
            .iter()
            .any(|opaque| scope.starts_with(opaque) && !(modules_verified && (opaque.is_empty() || self.module_scopes.contains(opaque))))
    }

    pub(super) fn returns_unit(&self, callee: &Expr, scope: &ScopePath, visible_imports: &HashMap<String, Option<Vec<String>>>) -> bool {
        let Expr::Path(path) = callee else {
            return false;
        };
        if path.qself.is_some() || path.path.leading_colon.is_some() {
            return false;
        }
        let Some(ident) = path.path.get_ident() else {
            return false;
        };
        let name = ident.to_string();
        if self.bound_names.contains(&name)
            || self.imported_names.contains(&name)
            || visible_imports.contains_key("*")
            || !self.scope_is_transparent(scope)
        {
            return false;
        }
        self.declarations.get(&name).and_then(Option::as_ref).is_some_and(|declaration| {
            scope.starts_with(declaration)
                && !self
                    .module_scopes
                    .iter()
                    .any(|module| module.len() > declaration.len() && scope.starts_with(module))
        })
    }
}

/// Returns whether a replacement reproduces the code it would replace.
///
/// Such a mutant is not a weak one, it is not a mutant at all: the compiled program is unchanged,
/// so no test can distinguish it and it survives every suite that will ever be written. Left in, it
/// is charged for like any other mutant and then reported as a survivor, which is an accusation
/// against tests that had nothing to answer for.
///
/// It arises whenever a function already returns one of the values the `fn_value` family offers —
/// `fn ready() -> bool { true }` is the everyday case, and `{ 0 }`, `{ None }` and `{ "" }` are the
/// others. The comparison is made on tokens rather than text so that layout, comments and the
/// braces around a body do not decide the answer; a body is stripped of its braces first, since
/// what replaces it is an expression rather than a block.
///
/// Both sides must parse for the answer to be yes. A replacement this tool cannot itself tokenise
/// is not one it can claim to have recognised as a no-op.
///
/// One shape is compared more closely than tokens allow. The reference family wraps its values in
/// `Box::leak(Box::new(...))`, and a body already written that way differs from the replacement
/// only in how the value inside spells its default: a type parameter explicitly bounded by the
/// standard `Default` trait can write `T::default()` instead of `Default::default()`. Those are
/// the same call, so the mutant is the original program under another name.
pub(super) fn is_noop(replacement: &str, original: &str, shape: Shape, defaults: &DefaultPaths, defaulted_types: &[String]) -> bool {
    let original = if matches!(shape, Shape::Block | Shape::IterBlock | Shape::IterWrappedBlock { .. }) {
        let trimmed = original.trim();

        trimmed
            .strip_prefix('{')
            .and_then(|rest| rest.strip_suffix('}'))
            .unwrap_or(original)
    } else {
        original
    };

    if same_tokens(replacement, original) {
        return true;
    }

    is_same_leak(replacement, original, defaults, defaulted_types)
}

/// Returns whether two expressions are the same `Box::leak(Box::new(...))`, up to how the value
/// inside names its `Default`.
///
/// Deliberately narrow: it answers for this one shape and nothing else, rather than pretending to
/// decide equivalence in general.
pub(super) fn is_same_leak(replacement: &str, original: &str, defaults: &DefaultPaths, defaulted_types: &[String]) -> bool {
    // Every match requires the terminal method identifier checked later by `path_ends_with` in
    // both expressions, so its absence from either raw text rules out a match without tokenizing
    // either one. Almost no replacement or original has this shape, so this skips the two token
    // passes below for the overwhelming majority of candidates.
    // #[gamma::skip(cond.always_false, logical.or_remove_left, logical.or_remove_right, logical.or_to_and, literal.str_to_empty, reason = "both parsed calls must end in Box::leak; weakening this substring prefilter only performs extra parsing before the same answer")]
    if !replacement.contains("leak") || !original.contains("leak") {
        return false;
    }

    let (Some(replacement), Some(original)) = (leaked_value(replacement), leaked_value(original)) else {
        return false;
    };

    if is_default_call(replacement, defaults, defaulted_types) && is_default_call(original, defaults, defaulted_types) {
        return true;
    }

    same_tokens(replacement, original)
}

/// The value a `Box::leak(Box::new(value))` leaks, or `None` for any other expression.
///
/// A leading `&*` is stripped first. A shared reference is offered as `&*Box::leak(..)`, and
/// without this the reborrow would hide the shape from the no-op check — so a body that already
/// leaks a default would be handed a mutant that is the same program, and it would survive every
/// suite that will ever be written.
pub(super) fn leaked_value(text: &str) -> Option<&str> {
    let tokens = lexemes(text)?;
    let mut expression = strip_parentheses(&tokens);

    if expression.first()?.kind == TokenKind::And && expression.get(1)?.kind == TokenKind::Star {
        // #[gamma::skip(try.propagate_to_unwrap, reason = "the preceding checks found two tokens, so removing those tokens always yields a valid suffix slice")]
        expression = strip_parentheses(expression.get(2..)?);
    }

    let leaked = call_argument(text, expression, "Box", "leak")?;
    // #[gamma::skip(try.propagate_to_unwrap, reason = "call_argument returns a complete token-bounded subslice of the text already accepted by lexemes, including any intervening comments intact")]
    let tokens = lexemes(leaked)?;

    call_argument(leaked, strip_parentheses(&tokens), "Box", "new")
}

#[derive(Clone, Copy, Debug)]
struct Lexeme<'a> {
    kind: TokenKind,
    text: &'a str,
    start: usize,
    end: usize,
}

fn call_argument<'a>(text: &'a str, expression: &[Lexeme<'a>], qualifier: &str, name: &str) -> Option<&'a str> {
    let expression = strip_parentheses(expression);
    let open = expression.iter().position(|token| token.kind == TokenKind::OpenParen)?;
    let (callee, call) = expression.split_at(open);

    // #[gamma::skip(try.propagate_to_unwrap, reason = "position found an opening parenthesis, so split_at leaves a nonempty call suffix and last cannot be None")]
    if !path_ends_with(callee, qualifier, name) || call.last()?.kind != TokenKind::CloseParen {
        return None;
    }

    let mut depth = 0_usize;
    for (index, token) in call.iter().enumerate() {
        match token.kind {
            // #[gamma::skip(try.propagate_to_unwrap, reason = "each increment consumes a distinct token in a slice, so depth cannot overflow usize")]
            TokenKind::OpenParen => depth = depth.checked_add(1)?,
            TokenKind::CloseParen => {
                // #[gamma::skip(try.propagate_to_unwrap, reason = "the call starts with an opener and returns at the first zero depth, before any unmatched closer can underflow")]
                depth = depth.checked_sub(1)?;
                if depth == 0 && index + 1 != call.len() {
                    return None;
                }
            }
            TokenKind::Comma if depth == 1 => return None,
            _ => {}
        }
    }

    // #[gamma::skip(logical.or_remove_right, literal.int_decrement, literal.int_to_one, literal.int_to_zero, reason = "a two-token empty call yields an empty argument slice and first returns None; relaxing this size check cannot create an argument")]
    if depth != 0 || call.len() < 3 {
        return None;
    }

    // #[gamma::skip(try.propagate_to_unwrap, reason = "the preceding length guard guarantees both slice bounds exist and are ordered")]
    let argument = call.get(1..call.len() - 1)?;
    // #[gamma::skip(try.propagate_to_unwrap, reason = "call.len is at least three, so stripping two delimiters leaves a nonempty argument slice with both endpoints")]
    text.get(argument.first()?.start..argument.last()?.end)
}

fn path_ends_with(path: &[Lexeme<'_>], qualifier: &str, name: &str) -> bool {
    let Some(segments) = path_segments(path) else {
        return false;
    };

    matches!(segments.as_slice(), [.., found_qualifier, found_name] if *found_qualifier == qualifier && *found_name == name)
}

fn path_segments<'a>(path: &[Lexeme<'a>]) -> Option<Vec<&'a str>> {
    let path = strip_parentheses(path);
    let mut segments = Vec::new();
    let mut index = usize::from(
        path.first().is_some_and(|token| token.kind == TokenKind::Colon) && path.get(1).is_some_and(|token| token.kind == TokenKind::Colon),
    ) * 2;

    loop {
        let token = path.get(index)?;
        if !matches!(token.kind, TokenKind::Ident | TokenKind::RawIdent) {
            return None;
        }
        segments.push(token.text);
        index += 1;

        if index == path.len() {
            return Some(segments);
        }
        if path.get(index)?.kind != TokenKind::Colon || path.get(index + 1)?.kind != TokenKind::Colon {
            return None;
        }
        index += 2;
    }
}

fn strip_parentheses<'slice, 'text>(mut expression: &'slice [Lexeme<'text>]) -> &'slice [Lexeme<'text>] {
    loop {
        if expression.first().is_none_or(|token| token.kind != TokenKind::OpenParen)
            || expression.last().is_none_or(|token| token.kind != TokenKind::CloseParen)
        {
            return expression;
        }

        let mut depth = 0_usize;
        let mut closes_at_end = false;
        for (index, token) in expression.iter().enumerate() {
            match token.kind {
                TokenKind::OpenParen => depth += 1,
                TokenKind::CloseParen => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        closes_at_end = index + 1 == expression.len();
                        break;
                    }
                }
                _ => {}
            }
        }
        if !closes_at_end {
            return expression;
        }
        // #[gamma::skip(stmt.delete_assign, reason = "not removing the enclosing pair leaves the loop on the same expression forever")]
        expression = &expression[1..expression.len() - 1];
    }
}

fn is_default_call(text: &str, defaults: &DefaultPaths, defaulted_types: &[String]) -> bool {
    let Some(tokens) = lexemes(text) else {
        return false;
    };
    let expression = strip_parentheses(&tokens);
    let Some(open) = expression.iter().position(|token| token.kind == TokenKind::OpenParen) else {
        return false;
    };
    let (callee, call) = expression.split_at(open);
    if !matches!(call, [open, close] if open.kind == TokenKind::OpenParen && close.kind == TokenKind::CloseParen) {
        return false;
    }
    let Some(segments) = path_segments(callee) else {
        return false;
    };
    let segments: Vec<String> = segments.into_iter().map(str::to_owned).collect();

    defaults.is_standard_default_segments(&segments)
        || matches!(segments.as_slice(), [qualifier, method] if method == "default" && defaulted_types.contains(qualifier))
}

fn same_tokens(left: &str, right: &str) -> bool {
    let (Some(left), Some(right)) = (lexemes(left), lexemes(right)) else {
        return false;
    };

    if left.len() != right.len()
        || !left.iter().zip(&right).all(|(left, right)| {
            // #[gamma::skip(logical.and_remove_left, reason = "the deterministic lexer assigns the same token kind to identical complete token text, so text equality already proves kind equality")]
            left.kind == right.kind && left.text == right.text
        })
    {
        return false;
    }

    left.windows(2).zip(right.windows(2)).all(|(left, right)| {
        let left_first = &left[0];
        let left_second = &left[1];
        let right_first = &right[0];
        let right_second = &right[1];
        let spacing_matters = spacing_matters(left_first.kind, left_second.kind);

        !spacing_matters || (left_first.end == left_second.start) == (right_first.end == right_second.start)
    })
}

fn lexemes(text: &str) -> Option<Vec<Lexeme<'_>>> {
    let mut at = 0;
    let mut lexemes = Vec::new();

    for token in rustc_lexer::tokenize(text) {
        let start = at;
        at += token.len;

        match token.kind {
            TokenKind::Whitespace | TokenKind::LineComment | TokenKind::BlockComment { terminated: true } => {}
            kind if valid(kind) => lexemes.push(Lexeme {
                kind,
                // #[gamma::skip(try.propagate_to_unwrap, reason = "rustc_lexer token lengths partition this same UTF-8 source into in-bounds character-boundary slices")]
                text: text.get(start..at)?,
                start,
                end: at,
            }),
            _invalid => return None,
        }
    }

    Some(lexemes)
}

const fn valid(kind: TokenKind) -> bool {
    match kind {
        TokenKind::Unknown | TokenKind::BlockComment { terminated: false } | TokenKind::Lifetime { starts_with_number: true } => false,
        TokenKind::Literal { kind, .. } => valid_literal(kind),
        _ => true,
    }
}

const fn valid_literal(kind: LiteralKind) -> bool {
    matches!(
        kind,
        LiteralKind::Int { empty_int: false, .. }
            | LiteralKind::Float { empty_exponent: false, .. }
            | LiteralKind::Char { terminated: true }
            | LiteralKind::Byte { terminated: true }
            | LiteralKind::Str { terminated: true }
            | LiteralKind::ByteStr { terminated: true }
            | LiteralKind::RawStr {
                started: true,
                terminated: true,
                ..
            }
            | LiteralKind::RawByteStr {
                started: true,
                terminated: true,
                ..
            }
    )
}

const fn is_punctuation(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Semi
            | TokenKind::Comma
            | TokenKind::Dot
            | TokenKind::At
            | TokenKind::Pound
            | TokenKind::Tilde
            | TokenKind::Question
            | TokenKind::Colon
            | TokenKind::Dollar
            | TokenKind::Eq
            | TokenKind::Not
            | TokenKind::Lt
            | TokenKind::Gt
            | TokenKind::Minus
            | TokenKind::And
            | TokenKind::Or
            | TokenKind::Plus
            | TokenKind::Star
            | TokenKind::Slash
            | TokenKind::Caret
            | TokenKind::Percent
    )
}

const fn spacing_matters(left: TokenKind, right: TokenKind) -> bool {
    if is_punctuation(left) && is_punctuation(right) {
        return true;
    }

    is_word(left) && is_word(right)
}

const fn is_word(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Ident | TokenKind::RawIdent | TokenKind::Literal { .. } | TokenKind::Lifetime { .. }
    )
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn incomplete_outer_parentheses_cannot_be_stripped_as_a_complete_default_expression() {
        for text in ["((value)", "(((value))", "(value", "(value) + other"] {
            let tokens = super::lexemes(text).unwrap();
            let stripped = super::strip_parentheses(&tokens);
            assert_eq!(stripped.first().map(|token| token.start), Some(0), "{text}");
            assert_eq!(stripped.last().map(|token| token.end), Some(text.len()), "{text}");
        }
        let tokens = super::lexemes("((value))").unwrap();
        assert_eq!(
            super::strip_parentheses(&tokens).iter().map(|token| token.text).collect::<Vec<_>>(),
            ["value"]
        );
    }

    fn defaults() -> DefaultPaths {
        DefaultPaths::of(&syn::parse_file("").expect("an empty file parses"))
    }

    #[test]
    fn constructor_noop_proofs_require_matching_types_and_both_generic_arities() {
        let imports = HashMap::default();
        let declarations = crate::ops::collect::Defaults::default();
        let types = Types {
            abstracts: &[],
            defaulted: &[],
            imports: &imports,
            defaults: &declarations,
            aliases: None,
            self_type: None,
            self_associated: None,
        };
        let paths = defaults();
        for (expression, expected_type, expected) in [
            ("((String::new()))", "String", true),
            ("(factory())()", "String", false),
            ("Vec::<u8>::new()", "String", false),
            ("String::default()", "Vec<u8>", false),
            ("String::default()", "String<u8>", false),
            ("Vec::<u8>::default()", "String", false),
            ("String::new()", "String<u8>", false),
            ("String::<u8>::default()", "String", false),
            ("bool::new()", "bool", false),
            ("bool::default()", "u8", false),
            ("u8::default()", "bool", false),
            ("bool::<u8>::default()", "bool", false),
            ("bool::default()", "bool<u8>", false),
            ("bool::default()", "()", false),
            ("Option::default()", "Option", true),
            ("Option::<u8>::default()", "Option", true),
            ("Option::default()", "Option<u8>", true),
            ("Option::<u8,u16>::default()", "Option<u8>", false),
            ("Option::<u8>::default()", "Option<u8,u16>", false),
            ("Option::<u8>::default()", "bool", false),
            ("bool::default()", "Option<u8>", false),
            ("Option::new()", "Option<u8>", false),
            ("String::new::<u8>()", "String", false),
            ("String::<u8>::new()", "String", false),
            ("std::<u8>::string::String::new()", "String", false),
            ("#[cfg(any())] String::new()", "String", false),
            ("String::new(1)", "String", false),
            ("String::other()", "String", false),
            ("<String as Default>::default()", "String", false),
            ("<Owner>::bool::default()", "bool", false),
            ("default()", "String", false),
        ] {
            assert_eq!(
                is_standard_default_constructor(
                    &syn::parse_str(expression).unwrap(),
                    &syn::parse_str(expected_type).unwrap(),
                    &types,
                    &paths
                ),
                expected,
                "{expression} -> {expected_type}"
            );
        }
        for bounded in [false, true] {
            let names = ["std".into(), "Default".into()];
            let shadowed = if bounded {
                Types {
                    defaulted: &names,
                    ..types
                }
            } else {
                Types {
                    abstracts: &names,
                    ..types
                }
            };
            assert!(!is_standard_default_constructor(
                &syn::parse_quote!(std::string::String::new()),
                &syn::parse_quote!(String),
                &shadowed,
                &paths
            ));
        }
        let shadowed_paths = DefaultPaths::of(&syn::parse_file("struct Default;").unwrap());
        assert!(!is_standard_default_constructor(
            &syn::parse_quote!(String::new()),
            &syn::parse_quote!(String),
            &types,
            &shadowed_paths
        ));
    }

    #[test]
    fn constructor_name_shadows_are_independent_from_the_default_trait_shadow() {
        let imports = HashMap::default();
        let declarations = crate::ops::collect::Defaults::default();
        let plain = Types {
            abstracts: &[],
            defaulted: &[],
            imports: &imports,
            defaults: &declarations,
            aliases: None,
            self_type: None,
            self_associated: None,
        };
        for (shadow, text) in [("bool", "bool::default()"), ("std", "std::primitive::bool::default()")] {
            let names = [shadow.into()];
            for bounded in [false, true] {
                let types = if bounded {
                    Types {
                        defaulted: &names,
                        ..plain
                    }
                } else {
                    Types {
                        abstracts: &names,
                        ..plain
                    }
                };
                assert!(
                    !is_standard_default_constructor(&syn::parse_str(text).unwrap(), &syn::parse_quote!(bool), &types, &defaults()),
                    "{text}"
                );
            }
        }
        let aliases = HashMap::from_iter([("bool".into(), None)]);
        let types = Types {
            aliases: Some(&aliases),
            ..plain
        };
        assert!(!is_standard_default_constructor(
            &syn::parse_quote!(bool::default()),
            &syn::parse_quote!(bool),
            &types,
            &defaults()
        ));
        for (expression, expected) in [
            (syn::parse_quote!(String::new()), true),
            (syn::parse_quote!(String::other()), false),
        ] {
            let grouped = Expr::Group(syn::ExprGroup {
                attrs: Vec::new(),
                group_token: syn::token::Group::default(),
                expr: Box::new(expression),
            });
            assert_eq!(
                is_standard_default_constructor(&grouped, &syn::parse_quote!(String), &plain, &defaults()),
                expected
            );
        }
    }

    #[test]
    fn resolved_defaults_reject_attributed_generic_methods_and_preserve_grouped_callees() {
        let imports = HashMap::default();
        let declarations = crate::ops::collect::Defaults::default();
        let names = ["T".into()];
        let types = Types {
            abstracts: &[],
            defaulted: &names,
            imports: &imports,
            defaults: &declarations,
            aliases: None,
            self_type: None,
            self_associated: None,
        };
        for (text, expected) in [
            ("((T::default()))", true),
            ("((T::default))()", true),
            ("(core::default::Default::default)()", true),
            ("#[cfg(any())] T::default()", false),
            ("T::default::<u8>()", false),
            ("<T as Trait>::default()", false),
            ("<Owner>::Default::default()", false),
            ("(factory())()", false),
            ("T::default(1)", false),
            ("T::other()", false),
        ] {
            assert_eq!(
                is_resolved_trait_default(&syn::parse_str(text).unwrap(), &types),
                expected,
                "{text}"
            );
        }
        let grouped = |expr| {
            Expr::Group(syn::ExprGroup {
                attrs: Vec::new(),
                group_token: syn::token::Group::default(),
                expr: Box::new(expr),
            })
        };
        assert!(is_resolved_trait_default(&grouped(syn::parse_quote!(T::default())), &types));
        assert!(!is_resolved_trait_default(&grouped(syn::parse_quote!(T::other())), &types));
        let paths = defaults();
        assert!(is_standard_default_constructor(
            &grouped(syn::parse_quote!(String::new())),
            &syn::parse_quote!(String),
            &types,
            &paths
        ));
        assert!(!is_standard_default_constructor(
            &grouped(syn::parse_quote!(String::other())),
            &syn::parse_quote!(String),
            &types,
            &paths
        ));
    }

    #[test]
    fn constructor_proofs_check_absolute_roots_on_both_sides_of_the_comparison() {
        let imports = HashMap::from_iter([("std".into(), None)]);
        let declarations = crate::ops::collect::Defaults::default();
        let types = Types {
            abstracts: &[],
            defaulted: &[],
            imports: &imports,
            defaults: &declarations,
            aliases: None,
            self_type: None,
            self_associated: None,
        };
        for (expression, expected) in [
            ("::std::primitive::bool::default()", "bool"),
            ("bool::default()", "::std::primitive::bool"),
        ] {
            assert!(
                !is_standard_default_constructor(
                    &syn::parse_str(expression).unwrap(),
                    &syn::parse_str(expected).unwrap(),
                    &types,
                    &defaults()
                ),
                "{expression} -> {expected}"
            );
        }
    }

    #[test]
    fn leaked_default_equivalence_requires_exact_shared_reborrow_and_complete_callees() {
        for (text, expected) in [
            (
                "&*((::std::boxed::Box::leak(Box::new(Default::default()))))",
                Some("Default::default()"),
            ),
            ("&mut Box::leak(Box::new(Default::default()))", None),
            ("-*Box::leak(Box::new(Default::default()))", None),
            ("Box::leak(Box::new((left, right)))", Some("(left, right)")),
            ("::(Box::new(Default::default()))", None),
            ("Box::leak(Box::new(value)", None),
            ("(Box::leak(Box::new(value))", None),
            ("Box::leak(Box::new(value)) + tail", None),
        ] {
            assert_eq!(leaked_value(text), expected, "{text}");
        }
        for original in [
            "&mut Box::leak(Box::new(Default::default()))",
            "-*Box::leak(Box::new(Default::default()))",
        ] {
            assert!(
                !is_same_leak("&*Box::leak(Box::new(Default::default()))", original, &defaults(), &[]),
                "{original}"
            );
        }
    }

    #[test]
    fn local_unit_proofs_respect_module_ancestry_imports_and_opaque_prefixes() {
        let root = vec![(1, 0)];
        let child = vec![(1, 0), (2, 0)];
        let grandchild = vec![(1, 0), (2, 0), (3, 0)];
        let sibling = vec![(1, 0), (9, 0)];
        let empty = HashMap::default();
        let expression: Expr = syn::parse_quote!(local);
        let mut calls = LocalCalls::default();
        calls.function(
            &syn::parse_quote!(
                fn local() {}
            ),
            &root,
        );
        assert!(calls.returns_unit(&expression, &grandchild, &empty));
        calls.module(&child);
        assert!(!calls.returns_unit(&expression, &grandchild, &empty));
        assert!(calls.returns_unit(&expression, &sibling, &empty));
        assert!(calls.returns_unit(&expression, &root, &empty));
        calls.opaque(&root);
        assert!(!calls.scope_is_transparent(&grandchild));
        assert!(calls.scope_is_transparent(&vec![(8, 0), (1, 0)]));
        assert!(!calls.returns_unit(&expression, &grandchild, &empty));
        let mut calls = LocalCalls::default();
        calls.function(
            &syn::parse_quote!(
                fn local() {}
            ),
            &root,
        );
        let imports = HashMap::from_iter([("*".into(), None)]);
        assert!(!calls.returns_unit(&expression, &root, &imports));
        calls.imported("local");
        assert!(!calls.returns_unit(&expression, &root, &empty));
        let mut calls = LocalCalls::default();
        calls.function(
            &syn::parse_quote!(
                #[transform]
                fn local() {}
            ),
            &root,
        );
        assert!(!calls.returns_unit(&expression, &root, &empty));
        assert!(!calls.scope_is_transparent(&grandchild));
        let mut calls = LocalCalls::default();
        calls.function(
            &syn::parse_quote!(
                fn local() {}
            ),
            &root,
        );
        for text in ["::local", "<T>::local", "<T as Trait>::local", "other::local", "1"] {
            assert!(!calls.returns_unit(&syn::parse_str(text).unwrap(), &root, &empty), "{text}");
        }
    }

    #[test]
    fn default_constructor_proofs_reject_shadowing_and_nonconstructor_shapes() {
        let imports = HashMap::default();
        let declarations = crate::ops::collect::Defaults::of(&syn::parse_file("").unwrap());
        let types = Types {
            abstracts: &[],
            defaulted: &[],
            imports: &imports,
            defaults: &declarations,
            aliases: None,
            self_type: None,
            self_associated: None,
        };
        let paths = defaults();
        for (expression, expected_type, expected) in [
            ("(String::new())", "String", true),
            ("String::default()", "String", true),
            ("String::new()", "Vec<u8>", false),
            ("String::new()", "&str", false),
            ("true", "bool", false),
            ("(factory())()", "String", false),
            ("String::new(1)", "String", false),
            ("<String as Default>::default()", "String", false),
            ("String::new::<u8>()", "String", false),
            ("String::other()", "String", false),
            ("default()", "String", false),
            ("bool::default()", "bool", true),
            ("Option::<u8>::default()", "Option<u8>", true),
            ("Vec::<u8>::new()", "Vec<u8>", true),
            ("Option::<u8,u8>::default()", "Option<u8>", false),
        ] {
            assert_eq!(
                is_standard_default_constructor(
                    &syn::parse_str(expression).unwrap(),
                    &syn::parse_str(expected_type).unwrap(),
                    &types,
                    &paths
                ),
                expected,
                "{expression}: {expected_type}"
            );
        }
        let grouped = Expr::Group(syn::ExprGroup {
            attrs: Vec::new(),
            group_token: syn::token::Group::default(),
            expr: Box::new(syn::parse_quote!(String::new())),
        });
        assert!(is_standard_default_constructor(
            &grouped,
            &syn::parse_quote!(String),
            &types,
            &paths
        ));
        let aliases = HashMap::from_iter([("String".to_owned(), None)]);
        for shadow in [
            Types {
                abstracts: &["Default".to_owned()],
                ..types
            },
            Types {
                aliases: Some(&aliases),
                ..types
            },
            Types {
                abstracts: &["String".to_owned()],
                ..types
            },
        ] {
            assert!(!is_standard_default_constructor(
                &syn::parse_quote!(String::new()),
                &syn::parse_quote!(String),
                &shadow,
                &paths
            ));
        }
        let mut call: syn::ExprCall = syn::parse_quote!(String::new());
        let Expr::Path(path) = &mut *call.func else { unreachable!() };
        path.path.segments.clear();
        assert!(!is_standard_default_constructor(
            &Expr::Call(call),
            &syn::parse_quote!(String),
            &types,
            &paths
        ));
    }

    #[test]
    fn resolved_trait_defaults_and_local_calls_require_unambiguous_paths() {
        let imports = HashMap::default();
        let declarations = crate::ops::collect::Defaults::of(&syn::parse_file("").unwrap());
        let types = Types {
            abstracts: &[],
            defaulted: &["T".to_owned()],
            imports: &imports,
            defaults: &declarations,
            aliases: None,
            self_type: None,
            self_associated: None,
        };
        for (text, expected) in [
            ("(T::default())", true),
            ("(T::default)()", true),
            ("T::default(1)", false),
            ("(factory())()", false),
            ("<T as Default>::default()", false),
            ("T::default::<u8>()", false),
            ("T::other()", false),
            ("default()", false),
        ] {
            assert_eq!(
                is_resolved_trait_default(&syn::parse_str(text).unwrap(), &types),
                expected,
                "{text}"
            );
        }
        let grouped = Expr::Group(syn::ExprGroup {
            attrs: Vec::new(),
            group_token: syn::token::Group::default(),
            expr: Box::new(syn::parse_quote!(T::default())),
        });
        assert!(is_resolved_trait_default(&grouped, &types));
        let mut call: syn::ExprCall = syn::parse_quote!(T::default());
        let Expr::Path(path) = &mut *call.func else { unreachable!() };
        path.path.segments.clear();
        assert!(!is_resolved_trait_default(&Expr::Call(call), &types));
        let mut calls = LocalCalls::default();
        calls.function(
            &syn::parse_quote!(
                fn local() {}
            ),
            &Vec::new(),
        );
        assert!(calls.returns_unit(&syn::parse_quote!(local), &Vec::new(), &imports));
        for text in ["1", "::local", "module::local", "<T as Trait>::local", "<T>::local"] {
            assert!(!calls.returns_unit(&syn::parse_str(text).unwrap(), &Vec::new(), &imports), "{text}");
        }
    }

    #[test]
    fn leak_equivalence_requires_both_defaults_and_complete_balanced_calls() {
        let defaults = defaults();
        let defaulted = vec!["T".to_owned()];
        for (left, right, expected) in [
            ("Box::leak(Box::new(T::default()))", "Box::leak(Box::new(Default::default()))", true),
            ("Box::leak(Box::new(1))", "Box::leak(Box::new(Default::default()))", false),
            ("Box::leak(Box::new(T::default()))", "Box::leak(Box::new(1))", false),
            ("Box::leak(Box::new(1))", "Box::leak(Box::new(2))", false),
            ("Box::leak(Box::new((1)))", "Box::leak(Box::new((1)))", true),
        ] {
            assert_eq!(is_same_leak(left, right, &defaults, &defaulted), expected, "{left} / {right}");
        }
        for (text, expected) in [
            ("", None),
            ("&", None),
            ("*", None),
            ("&*", None),
            ("/*", None),
            ("Box::leak(Box::new())", None),
            ("Box::leak(Box::new((value))", None),
            ("Box::leak(Box::new(value", None),
            ("Box::leak(Box::new(value)) extra", None),
            ("Box::leak(Box::new(value))", Some("value")),
        ] {
            assert_eq!(leaked_value(text), expected, "{text}");
        }
        for malformed in ["Box : new", "Box +: new", "Box :+ new", "(Box::new", "Box::new)"] {
            let tokens = lexemes(malformed).unwrap();
            assert_eq!(path_segments(&tokens), None, "{malformed}");
        }
        for text in ["(x", "x)", "((x)) + ((y))"] {
            let tokens = lexemes(text).unwrap();
            assert_eq!(strip_parentheses(&tokens).len(), tokens.len(), "{text}");
        }
    }

    #[test]
    fn layout_and_comments_do_not_change_token_equality() {
        assert!(same_tokens("Some(1)", "Some /* note */ ( 1 )"));
    }

    #[test]
    fn punctuation_jointness_remains_semantic() {
        assert!(!same_tokens("a >= b", "a > = b"));
        assert!(!same_tokens("c\"text\"", "c \"text\""));
    }

    #[test]
    #[cfg_attr(miri, ignore = "10,000 tokenization checks are a native source-map stress regression")]
    fn repeated_noop_checks_do_not_advance_the_proc_macro_source_map() {
        let marker = || {
            "source_map_marker"
                .parse::<proc_macro2::TokenStream>()
                .expect("the marker tokenizes")
                .into_iter()
                .next()
                .expect("the marker has one token")
                .span()
                .byte_range()
                .start
        };
        let first = marker();
        let second = marker();
        let expected_growth = second.saturating_sub(first);
        let defaults = DefaultPaths::of(&syn::parse_file("").expect("an empty file parses"));
        let defaulted = vec!["T".to_owned()];

        for _candidate in 0..10_000 {
            assert!(is_noop(
                "&*Box::leak(Box::new(Default::default()))",
                "{ Box::leak(Box::new(T::default())) }",
                Shape::Block,
                &defaults,
                &defaulted,
            ));
        }

        assert_eq!(marker().saturating_sub(second), expected_growth);
    }

    /// The substring pre-check that skips tokenizing non-leak shapes must not change the answer:
    /// it has to still recognize a genuine match and still reject anything lacking the literal
    /// `leak` text, which is the only case it fast-paths.
    #[test]
    fn the_leak_prefilter_rejects_and_accepts_the_same_pairs_as_full_tokenization() {
        let defaults = defaults();

        assert!(is_same_leak(
            "Box::leak(Box::new(Default::default()))",
            "Box::leak(Box::new(T::default()))",
            &defaults,
            &[String::from("T")],
        ));
        assert!(!is_same_leak("None", "0", &defaults, &[]));
        assert!(!is_same_leak("Box::leak(Box::new(1))", "0", &defaults, &[]));
        assert!(is_same_leak("Box::leak(Box::new(1))", "Box::leak(Box::new(1))", &defaults, &[],));
        assert!(!is_same_leak("Box::leak(Box::new(1))", "Box::leak(Box::new(2))", &defaults, &[],));
    }

    #[test]
    fn literal_validation_accepts_only_complete_rust_literals() {
        assert!(valid_literal(LiteralKind::Int {
            base: rustc_lexer::Base::Decimal,
            empty_int: false,
        }));
        assert!(!valid_literal(LiteralKind::Int {
            base: rustc_lexer::Base::Decimal,
            empty_int: true,
        }));
    }

    #[test]
    fn leaked_values_and_call_arguments_reject_non_plain_calls() {
        let leaked = "&*(Box::leak(Box::new((value))))";

        assert_eq!(leaked_value(leaked), Some("(value)"));
        assert_eq!(leaked_value("Box::leak(Box::new(value))"), Some("value"));
        assert_eq!(leaked_value("&Box::leak(Box::new(value))"), None);
        assert_eq!(leaked_value("*Box::leak(Box::new(value))"), None);
        assert_eq!(leaked_value("Box::leak(Box::new((left, right)))"), Some("(left, right)"));

        let extra = "Box::new(value)()";
        let extra_tokens = lexemes(extra).expect("the extra call tokenizes");
        assert_eq!(call_argument(extra, &extra_tokens, "Box", "new"), None);

        let multiple = "Box::new(value, other)";
        let multiple_tokens = lexemes(multiple).expect("the multi-argument call tokenizes");
        assert_eq!(call_argument(multiple, &multiple_tokens, "Box", "new"), None);

        let empty = "Box::new()";
        let empty_tokens = lexemes(empty).expect("the empty call tokenizes");
        assert_eq!(call_argument(empty, &empty_tokens, "Box", "new"), None);

        let wrong_callee = "Vec::new(value)";
        let wrong_tokens = lexemes(wrong_callee).expect("the wrong callee tokenizes");
        assert_eq!(call_argument(wrong_callee, &wrong_tokens, "Box", "new"), None);

        let unterminated = "Box::new(value";
        let unterminated_tokens = lexemes(unterminated).expect("the unterminated call tokenizes");
        assert_eq!(call_argument(unterminated, &unterminated_tokens, "Box", "new"), None);

        let malformed_path = "Box.new(value)";
        let malformed_tokens = lexemes(malformed_path).expect("the malformed path tokenizes");
        assert!(!path_ends_with(&malformed_tokens[..2], "Box", "new"));

        let absolute = "::alloc::boxed::Box::new";
        let absolute_tokens = lexemes(absolute).expect("the absolute path tokenizes");
        assert!(path_ends_with(&absolute_tokens, "Box", "new"));
        assert!(!path_ends_with(&absolute_tokens, "Vec", "new"));

        let interrupted_leading_colon = ": + Box::new";
        let interrupted_tokens = lexemes(interrupted_leading_colon).expect("the interrupted path tokenizes");
        assert!(!path_ends_with(&interrupted_tokens, "Box", "new"));

        for malformed in ["::", "Box:", "Box:::new", "Box::new::", "Box::0"] {
            let tokens = lexemes(malformed).expect("the malformed path still tokenizes");
            assert!(path_segments(&tokens).is_none(), "{malformed}");
        }

        let partly_parenthesized = "(value) + other";
        let partly_parenthesized_tokens = lexemes(partly_parenthesized).expect("the expression tokenizes");
        assert_eq!(
            strip_parentheses(&partly_parenthesized_tokens).len(),
            partly_parenthesized_tokens.len()
        );

        let two_parenthesized_terms = "(left) + (right)";
        let two_parenthesized_tokens = lexemes(two_parenthesized_terms).expect("the expression tokenizes");
        assert_eq!(strip_parentheses(&two_parenthesized_tokens).len(), two_parenthesized_tokens.len());

        let unterminated_parenthesis = "(value";
        let unterminated_parenthesis_tokens = lexemes(unterminated_parenthesis).expect("the expression tokenizes");
        assert_eq!(
            strip_parentheses(&unterminated_parenthesis_tokens).len(),
            unterminated_parenthesis_tokens.len()
        );
    }

    #[test]
    fn leak_comparison_rejects_text_that_only_mentions_leak() {
        assert!(!is_same_leak("leak + 1", "leak + 1", &defaults(), &[]));
    }

    #[test]
    fn default_call_checks_reject_invalid_shapes() {
        let defaults = defaults();

        assert!(!is_default_call("/*", &defaults, &[]));
        assert!(!is_default_call("Default::default", &defaults, &[]));
        assert!(!is_default_call("Default::default(value)", &defaults, &[]));
        assert!(!is_default_call("1()", &defaults, &[]));
        assert!(!is_default_call("Default::default() + 1", &defaults, &[]));
        assert!(is_default_call("(Default::default())", &defaults, &[]));
    }

    #[test]
    fn noop_block_stripping_applies_only_to_block_shapes() {
        let defaults = defaults();

        assert!(is_noop("true", "{ true }", Shape::Block, &defaults, &[]));
        assert!(is_noop("true", "{ true }", Shape::IterBlock, &defaults, &[]));
        assert!(!is_noop("true", "{ true }", Shape::Expr, &defaults, &[]));
        assert!(!is_noop("true", "{ true }", Shape::Continue, &defaults, &[]));
    }

    #[test]
    fn invalid_tokens_are_rejected_while_literals_remain_valid() {
        assert!(!same_tokens("/*", "/*"));
        assert!(lexemes("/*").is_none());

        let invalid = rustc_lexer::tokenize("/*")
            .next()
            .expect("the unterminated block comment yields one token")
            .kind;
        assert!(!valid(invalid));

        #[rustfmt::skip]
        let literal_kind = rustc_lexer::tokenize("1").find_map(|token| match token.kind { TokenKind::Literal { kind, .. } => Some(kind), _ => None }).expect("an integer tokenizes as a literal");
        let valid_literal_fn: fn(LiteralKind) -> bool = valid_literal;
        assert!(valid_literal_fn(literal_kind));
    }
}
