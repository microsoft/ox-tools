// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use syn::parse::Parser as _;
use syn::{Expr, FnArg, Pat, ReturnType, Stmt, Type};

use super::Binding;
use crate::ops::collect::resource_namespaces::{Function, Namespaces, declaration_attributes};
use crate::{HashMap, HashSet};

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod fixture;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests;

/// Certified parameter/result kinds for a deterministic helper, consumed by loop interpretation.
///
/// This records repeatability, not the helper's implementation or a concrete result.
#[derive(Clone)]
pub(in super::super) struct StableCall {
    pub(in super::super) parameters: Vec<Binding>,
    pub(in super::super) output: Binding,
    pub(in super::super) char_option: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Str,
    String,
    Chars,
    Char,
    MaybeChar,
    Unsigned,
    MaybeUnsigned,
    Bool,
    Unit,
}

impl Kind {
    fn binding(self) -> Option<Binding> {
        Some(match self {
            Self::Str => Binding::StrReference,
            Self::String => Binding::String,
            Self::Char => Binding::Char,
            Self::Unsigned => Binding::Integer,
            Self::Bool => Binding::Bool,
            _ => return None,
        })
    }
}

fn ty(ty: &Type) -> Option<Kind> {
    match ty {
        Type::Reference(reference) if reference.mutability.is_none() => {
            matches!(&*reference.elem, Type::Path(path) if path.path.is_ident("str")).then_some(Kind::Str)
        }
        Type::Path(path) if path.qself.is_none() => match path.path.get_ident()?.to_string().as_str() {
            "String" => Some(Kind::String),
            "usize" => Some(Kind::Unsigned),
            "char" => Some(Kind::Char),
            "bool" => Some(Kind::Bool),
            _ => None,
        },
        Type::Tuple(tuple) if tuple.elems.is_empty() => Some(Kind::Unit),
        _ => None,
    }
}

/// Shared namespace evidence, total certification budget, and active call chain for cycle rejection.
struct Analysis<'a> {
    sources: &'a Namespaces,
    remaining: usize,
    active: HashSet<(String, Vec<String>, String)>,
}

/// One helper's typed local environment, return contract, and supported loop-control context.
struct Checker<'a, 'b> {
    analysis: &'a mut Analysis<'b>,
    root: String,
    module: Vec<String>,
    values: HashMap<String, Kind>,
    output: Kind,
    loops: usize,
}

/// Certifies the closed deterministic helper grammar for use as stable symbolic call evidence.
///
/// Unknown types, unresolved effects, recursion, or exhausted work withhold certification.
/// Work and active-call-depth allowances are independent conservative defaults, not measured
/// capacities: one bounds total analysis visits, the other bounds nested helper traversal.
pub(in super::super) fn certify(sources: &Namespaces, function: Function<'_>) -> Option<StableCall> {
    if function.char_constructor {
        return sources.primitive_scope(function.root, &function.module).then(|| StableCall {
            parameters: vec![Binding::Char],
            output: Binding::Opaque,
            char_option: false,
        });
    }
    let mut analysis = Analysis {
        sources,
        remaining: 512,
        active: HashSet::default(),
    };
    let (parameters, output) = analysis.function(function)?;
    Some(StableCall {
        parameters: parameters.into_iter().map(Kind::binding).collect::<Option<_>>()?,
        output: output.binding()?,
        char_option: false,
    })
}

impl Analysis<'_> {
    /// Checks a resolved declaration and its callees under the same work and recursion limits.
    fn function(&mut self, function: Function<'_>) -> Option<(Vec<Kind>, Kind)> {
        if function.char_constructor
            || self.remaining == 0
            || self.active.len() >= 8
            || !self.sources.primitive_scope(function.root, &function.module)
        {
            return None;
        }
        self.remaining -= 1;
        let item = syn::parse_str::<syn::ItemFn>(function.syntax).ok()?;
        if item.sig.asyncness.is_some()
            || !matches!(item.sig.safety, syn::Safety::Default)
            || item.sig.abi.is_some()
            || !item.sig.generics.params.is_empty()
            || !declaration_attributes(&item.attrs)
        {
            return None;
        }
        let identity = (function.root.to_owned(), function.module.clone(), item.sig.ident.to_string());
        if !self.active.insert(identity.clone()) {
            return None;
        }
        let output = match &item.sig.output {
            ReturnType::Default => Kind::Unit,
            ReturnType::Type(_, output) => ty(output)?,
        };
        let mut parameters = Vec::new();
        let mut values = HashMap::default();
        for argument in &item.sig.inputs {
            let FnArg::Typed(argument) = argument else { return None };
            let Pat::Ident(pattern) = &*argument.pat else { return None };
            if pattern.by_ref.is_some() || pattern.subpat.is_some() {
                return None;
            }
            let kind = ty(&argument.ty)?;
            parameters.push(kind);
            let _old = values.insert(pattern.ident.to_string(), kind);
        }
        let mut checker = Checker {
            analysis: self,
            root: function.root.to_owned(),
            module: function.module,
            values,
            output,
            loops: 0,
        };
        let result = checker.block(&item.block)?;
        let _removed = checker.analysis.active.remove(&identity);
        (result == output).then_some((parameters, output))
    }
}

impl Checker<'_, '_> {
    /// Checks supported statements and restores outer bindings after a successful block.
    fn block(&mut self, block: &syn::Block) -> Option<Kind> {
        let saved = self.values.clone();
        let mut result = Kind::Unit;
        for statement in &block.stmts {
            if self.analysis.remaining == 0 {
                return None;
            }
            self.analysis.remaining -= 1;
            result = match statement {
                Stmt::Local(local) if local.attrs.is_empty() => {
                    let init = local.init.as_ref().filter(|init| init.diverge.is_none())?;
                    let kind = self.expr(&init.expr)?;
                    let pattern = match &local.pat {
                        Pat::Type(pattern) if ty(&pattern.ty) == Some(kind) => &*pattern.pat,
                        Pat::Type(_) => return None,
                        pattern => pattern,
                    };
                    let Pat::Ident(pattern) = pattern else { return None };
                    if pattern.by_ref.is_some() || pattern.subpat.is_some() {
                        return None;
                    }
                    let _old = self.values.insert(pattern.ident.to_string(), kind);
                    Kind::Unit
                }
                Stmt::Expr(expression, semi) => {
                    let kind = self.expr(expression)?;
                    if semi.is_some() { Kind::Unit } else { kind }
                }
                _ => return None,
            };
        }
        self.values = saved;
        Some(result)
    }

    /// Validates expression effects and kinds; unsupported behavior aborts this certification.
    #[expect(
        clippy::too_many_lines,
        reason = "one closed deterministic expression grammar shares its total proof budget"
    )]
    fn expr(&mut self, expression: &Expr) -> Option<Kind> {
        if self.analysis.remaining == 0 {
            return None;
        }
        self.analysis.remaining -= 1;
        match expression {
            Expr::Path(path) if path.attrs.is_empty() && path.qself.is_none() => {
                self.values.get(&path.path.get_ident()?.to_string()).copied()
            }
            Expr::Lit(literal) if literal.attrs.is_empty() => Some(match literal.lit {
                syn::Lit::Str(_) => Kind::Str,
                syn::Lit::Char(_) => Kind::Char,
                syn::Lit::Int(_) => Kind::Unsigned,
                syn::Lit::Bool(_) => Kind::Bool,
                _ => return None,
            }),
            Expr::Tuple(tuple) if tuple.elems.is_empty() && tuple.attrs.is_empty() => Some(Kind::Unit),
            Expr::Paren(paren) if paren.attrs.is_empty() => self.expr(&paren.expr),
            Expr::Block(block) if block.attrs.is_empty() && block.label.is_none() => self.block(&block.block),
            Expr::Binary(binary) if binary.attrs.is_empty() => {
                let left = self.expr(&binary.left)?;
                let right = self.expr(&binary.right)?;
                (left == right && matches!(left, Kind::Char | Kind::Unsigned | Kind::Bool | Kind::MaybeChar)).then_some(Kind::Bool)
            }
            Expr::Call(call) if call.attrs.is_empty() => {
                let Expr::Path(path) = &*call.func else { return None };
                let path = path.path.segments.iter().map(|part| part.ident.to_string()).collect::<Vec<_>>();
                if path.as_slice() == ["String", "with_capacity"] && call.args.len() == 1 && self.expr(&call.args[0])? == Kind::Unsigned {
                    return Some(Kind::String);
                }
                if path.as_slice() == ["Some"] && call.args.len() == 1 && self.expr(&call.args[0])? == Kind::Char {
                    return Some(Kind::MaybeChar);
                }
                let source = self.analysis.sources.function_in(&self.root, &self.module, &path)?;
                let (parameters, output) = self.analysis.function(source)?;
                if parameters.len() != call.args.len() {
                    return None;
                }
                for (argument, expected) in call.args.iter().zip(parameters) {
                    if self.expr(argument)? != expected {
                        return None;
                    }
                }
                Some(output)
            }
            Expr::MethodCall(call) if call.attrs.is_empty() && call.turbofish.is_none() => {
                if !self.analysis.sources.closed_methods(&self.root, &self.module) {
                    return None;
                }
                let receiver = self.expr(&call.receiver)?;
                let arguments = call.args.iter().map(|argument| self.expr(argument)).collect::<Option<Vec<_>>>()?;
                match (receiver, call.method.to_string().as_str(), arguments.as_slice()) {
                    (Kind::Str | Kind::String, "len", []) | (Kind::MaybeUnsigned, "unwrap_or", [Kind::Unsigned]) => Some(Kind::Unsigned),
                    (Kind::Str | Kind::String, "chars", []) | (Kind::Chars, "by_ref", []) => Some(Kind::Chars),
                    (Kind::Chars, "next", []) => Some(Kind::MaybeChar),
                    (Kind::String, "push", [Kind::Char]) => Some(Kind::Unit),
                    (Kind::Str | Kind::String, "width", []) if self.analysis.sources.width_contract(&self.root, &self.module, false) => {
                        Some(Kind::Unsigned)
                    }
                    (Kind::Char, "width", []) if self.analysis.sources.width_contract(&self.root, &self.module, true) => {
                        Some(Kind::MaybeUnsigned)
                    }
                    _ => None,
                }
            }
            Expr::If(condition) if condition.attrs.is_empty() => {
                if self.expr(&condition.cond)? != Kind::Bool {
                    return None;
                }
                let then_kind = self.block(&condition.then_branch)?;
                let else_kind = match &condition.else_branch {
                    Some((_, otherwise)) => self.expr(otherwise)?,
                    None => Kind::Unit,
                };
                (then_kind == else_kind).then_some(then_kind)
            }
            Expr::While(loop_) if loop_.attrs.is_empty() && loop_.label.is_none() => {
                let Expr::Let(condition) = &*loop_.cond else { return None };
                let Expr::MethodCall(next) = &*condition.expr else { return None };
                if next.method != "next"
                    || !matches!(&*next.receiver, Expr::Path(path) if path.path.get_ident()
                    .is_some_and(|name| self.values.get(&name.to_string()) == Some(&Kind::Chars)))
                    || self.expr(&condition.expr)? != Kind::MaybeChar
                {
                    return None;
                }
                let Pat::TupleStruct(pattern) = &*condition.pat else { return None };
                if !pattern.path.is_ident("Some") || pattern.elems.len() != 1 {
                    return None;
                }
                let Pat::Ident(binding) = &pattern.elems[0] else { return None };
                let saved = self.values.clone();
                let _old = self.values.insert(binding.ident.to_string(), Kind::Char);
                self.loops += 1;
                let _result = self.block(&loop_.body)?;
                self.loops -= 1;
                self.values = saved;
                Some(Kind::Unit)
            }
            Expr::ForLoop(loop_) if loop_.attrs.is_empty() && loop_.label.is_none() => {
                if self.expr(&loop_.expr)? != Kind::Chars {
                    return None;
                }
                let Pat::Ident(binding) = &*loop_.pat else { return None };
                let saved = self.values.clone();
                let _old = self.values.insert(binding.ident.to_string(), Kind::Char);
                self.loops += 1;
                let _result = self.block(&loop_.body)?;
                self.loops -= 1;
                self.values = saved;
                Some(Kind::Unit)
            }
            Expr::Continue(continue_) if continue_.attrs.is_empty() && continue_.label.is_none() && self.loops != 0 => Some(Kind::Unit),
            Expr::Break(break_) if break_.attrs.is_empty() && break_.label.is_none() && break_.expr.is_none() && self.loops != 0 => {
                Some(Kind::Unit)
            }
            Expr::Return(return_) if return_.attrs.is_empty() => {
                let result = match &return_.expr {
                    Some(value) => self.expr(value)?,
                    None => Kind::Unit,
                };
                (result == self.output).then_some(Kind::Unit)
            }
            Expr::Macro(mac)
                if mac.attrs.is_empty()
                    && mac.mac.path.is_ident("matches")
                    && self.analysis.sources.standard_macro(&self.root, &self.module, "matches") =>
            {
                let parser = |input: syn::parse::ParseStream<'_>| -> syn::Result<(Expr, Pat)> {
                    let expression = input.parse()?;
                    let _comma = input.parse::<syn::Token![,]>()?;
                    Ok((expression, Pat::parse_multi_with_leading_vert(input)?))
                };
                let (expression, pattern) = parser.parse2(mac.mac.tokens.clone()).ok()?;
                if self.expr(&expression)? != Kind::Char || !character_pattern(&pattern) {
                    return None;
                }
                Some(Kind::Bool)
            }
            _ => None,
        }
    }
}

fn character_pattern(pattern: &Pat) -> bool {
    match pattern {
        Pat::Lit(literal) => matches!(literal.lit, syn::Lit::Char(_)),
        Pat::Range(range) => [&range.start, &range.end]
            .iter()
            .all(|bound| matches!(bound.as_deref(), Some(Expr::Lit(literal)) if matches!(literal.lit, syn::Lit::Char(_)))),
        Pat::Or(or) => or.cases.iter().all(character_pattern),
        _ => false,
    }
}
