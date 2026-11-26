// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Source extents derived from syntax boundaries rather than quoted expression subtrees.

use proc_macro2::Span;
use syn::spanned::Spanned;
use syn::{Expr, PathArguments};

/// Original-source outer bounds for splicing and containment without re-emitting subtrees.
/// Unlike `Spanned::span`, implementations select the node's original boundary tokens.
pub(crate) trait Extent {
    fn extent(&self) -> Span;
}

trait Edges {
    fn edge(&self, first: bool) -> Span;
}

fn token_edge(tokens: &proc_macro2::TokenStream, first: bool) -> Span {
    let mut tokens = tokens.clone().into_iter();
    let token = if first { tokens.next() } else { tokens.last() };
    token.map_or_else(Span::call_site, |token| token.span())
}

impl<T: Edges + Spanned> Extent for T {
    fn extent(&self) -> Span {
        let first = self.edge(true);
        first.join(self.edge(false)).unwrap_or(first)
    }
}

macro_rules! edges {
    ($ty:ty, $node:ident, $first:expr, $last:expr) => {
        impl Edges for $ty {
            fn edge(&self, first: bool) -> Span {
                let $node = self;
                if first {
                    if let Some(attribute) = $node
                        .attrs
                        .iter()
                        .find(|attribute| matches!(attribute.style, syn::AttrStyle::Outer))
                    {
                        return attribute.pound_token.span;
                    }
                    $first
                } else {
                    $last
                }
            }
        }
    };
}

impl Edges for Expr {
    fn edge(&self, first: bool) -> Span {
        macro_rules! dispatch {
            ($($variant:ident),* $(,)?) => {
                match self {
                    $(Self::$variant(node) => node.edge(first),)*
                    Self::Verbatim(tokens) => token_edge(tokens, first),
                    _ => self.span(),
                }
            };
        }
        dispatch!(
            Array, Assign, Async, Await, Binary, Block, Break, Call, Cast, Closure, Const, Continue, Field, ForLoop, Group, If, Index,
            Infer, Let, Lit, Loop, Macro, Match, MethodCall, Paren, Path, Range, RawAddr, Reference, Repeat, Return, Struct, Try, TryBlock,
            Tuple, Unary, Unsafe, While, Yield,
        )
    }
}

edges!(syn::ExprArray, n, n.bracket_token.span.open(), n.bracket_token.span.close());
edges!(syn::ExprAssign, n, n.left.edge(true), n.right.edge(false));
edges!(syn::ExprAsync, n, n.async_token.span, n.block.brace_token.span.close());
edges!(syn::ExprAwait, n, n.base.edge(true), n.await_token.span);
edges!(syn::ExprBinary, n, n.left.edge(true), n.right.edge(false));
edges!(
    syn::ExprBlock,
    n,
    n.label
        .as_ref()
        .map_or_else(|| n.block.brace_token.span.open(), |label| label.name.apostrophe),
    n.block.brace_token.span.close()
);
edges!(
    syn::ExprBreak,
    n,
    n.break_token.span,
    n.expr.as_ref().map_or_else(
        || n.label.as_ref().map_or(n.break_token.span, |label| label.ident.span()),
        |value| value.edge(false)
    )
);
edges!(syn::ExprCall, n, n.func.edge(true), n.paren_token.span.close());
edges!(syn::ExprCast, n, n.expr.edge(true), n.ty.edge(false));
edges!(
    syn::ExprClosure,
    n,
    n.lifetimes.as_ref().map_or_else(
        || n.constness.map_or_else(
            || n.asyncness
                .map_or_else(|| n.capture.map_or(n.inputs_begin.span, |token| token.span), |token| token.span),
            |token| token.span
        ),
        |lifetimes| lifetimes.for_token.span
    ),
    n.body.edge(false)
);
edges!(syn::ExprConst, n, n.const_token.span, n.block.brace_token.span.close());
edges!(
    syn::ExprContinue,
    n,
    n.continue_token.span,
    n.label.as_ref().map_or(n.continue_token.span, |label| label.ident.span())
);
edges!(syn::ExprField, n, n.base.edge(true), n.member.span());
edges!(
    syn::ExprForLoop,
    n,
    n.label.as_ref().map_or(n.for_token.span, |label| label.name.apostrophe),
    n.body.brace_token.span.close()
);
edges!(syn::ExprGroup, n, n.group_token.span, n.group_token.span);
edges!(
    syn::ExprIf,
    n,
    n.if_token.span,
    n.else_branch
        .as_ref()
        .map_or_else(|| n.then_branch.brace_token.span.close(), |(_, expression)| expression.edge(false))
);
edges!(syn::ExprIndex, n, n.expr.edge(true), n.bracket_token.span.close());
edges!(syn::ExprInfer, n, n.underscore_token.span, n.underscore_token.span);
edges!(syn::ExprLet, n, n.let_token.span, n.expr.edge(false));
edges!(syn::ExprLit, n, n.lit.span(), n.lit.span());
edges!(
    syn::ExprLoop,
    n,
    n.label.as_ref().map_or(n.loop_token.span, |label| label.name.apostrophe),
    n.body.brace_token.span.close()
);
edges!(syn::ExprMacro, n, n.mac.path.edge(true), n.mac.delimiter.span().close());
edges!(syn::ExprMatch, n, n.match_token.span, n.brace_token.span.close());
edges!(syn::ExprMethodCall, n, n.receiver.edge(true), n.paren_token.span.close());
edges!(syn::ExprParen, n, n.paren_token.span.open(), n.paren_token.span.close());
edges!(
    syn::ExprPath,
    n,
    n.qself.as_ref().map_or_else(|| n.path.edge(true), |qself| qself.lt_token.span),
    n.path.edge(false)
);
edges!(
    syn::ExprRange,
    n,
    n.start.as_ref().map_or_else(|| n.limits.span(), |value| value.edge(true)),
    n.end.as_ref().map_or_else(|| n.limits.span(), |value| value.edge(false))
);
edges!(syn::ExprRawAddr, n, n.and_token.span, n.expr.edge(false));
edges!(syn::ExprReference, n, n.and_token.span, n.expr.edge(false));
edges!(syn::ExprRepeat, n, n.bracket_token.span.open(), n.bracket_token.span.close());
edges!(
    syn::ExprReturn,
    n,
    n.return_token.span,
    n.expr.as_ref().map_or(n.return_token.span, |value| value.edge(false))
);
edges!(
    syn::ExprStruct,
    n,
    n.qself.as_ref().map_or_else(|| n.path.edge(true), |qself| qself.lt_token.span),
    n.brace_token.span.close()
);
edges!(syn::ExprTry, n, n.expr.edge(true), n.question_token.span);
edges!(syn::ExprTryBlock, n, n.try_token.span, n.block.brace_token.span.close());
edges!(syn::ExprTuple, n, n.paren_token.span.open(), n.paren_token.span.close());
edges!(syn::ExprUnary, n, n.op.span(), n.expr.edge(false));
edges!(syn::ExprUnsafe, n, n.unsafe_token.span, n.block.brace_token.span.close());
edges!(
    syn::ExprWhile,
    n,
    n.label.as_ref().map_or(n.while_token.span, |label| label.name.apostrophe),
    n.body.brace_token.span.close()
);
edges!(
    syn::ExprYield,
    n,
    n.yield_token.span,
    n.expr.as_ref().map_or(n.yield_token.span, |value| value.edge(false))
);

impl Edges for syn::Path {
    fn edge(&self, first: bool) -> Span {
        if first {
            self.leading_colon.as_ref().map_or_else(
                || self.segments.first().map_or_else(Span::call_site, |segment| segment.ident.span()),
                Spanned::span,
            )
        } else {
            self.segments
                .last()
                .map_or_else(Span::call_site, |segment| match &segment.arguments {
                    PathArguments::None => segment.ident.span(),
                    PathArguments::AngleBracketed(arguments) => arguments.gt_token.span,
                    PathArguments::Parenthesized(arguments) => match &arguments.output {
                        syn::ReturnType::Default => arguments.paren_token.span.close(),
                        syn::ReturnType::Type(_, ty) => ty.edge(false),
                    },
                })
        }
    }
}

impl Edges for syn::Block {
    fn edge(&self, first: bool) -> Span {
        if
        // #[gamma::skip(cond.negate, tag = "equivalent", reason = "Only extent consumes these two edges, and joining same-file brace spans is commutative.")]
        first {
            self.brace_token.span.open()
        } else {
            self.brace_token.span.close()
        }
    }
}

impl Edges for syn::Macro {
    fn edge(&self, first: bool) -> Span {
        if
        // #[gamma::skip(cond.negate, tag = "equivalent", reason = "Only extent consumes these two edges, and joining same-file path and delimiter spans is commutative.")]
        first {
            self.path.edge(true)
        } else {
            self.delimiter.span().close()
        }
    }
}

fn visible(visibility: &syn::Visibility, fallback: impl FnOnce() -> Span) -> Span {
    match visibility {
        syn::Visibility::Public(token) => token.span,
        syn::Visibility::Restricted(restricted) => restricted.pub_token.span,
        syn::Visibility::Inherited => fallback(),
    }
}

macro_rules! visible_edges {
    ($ty:ty, $node:ident, $first:expr, $last:expr) => {
        edges!($ty, $node, visible(&$node.vis, || $first), $last);
    };
}

fn signature_first(signature: &syn::Signature) -> Span {
    signature
        .constness
        .map(|token| token.span)
        .or_else(|| signature.asyncness.map(|token| token.span))
        .or(match &signature.safety {
            // #[gamma::skip(option.some_to_none, tag = "unreachable", reason = "Ordinary parsed function signatures cannot be Safe; foreign signatures never call signature_first.")]
            syn::Safety::Safe(token) => Some(token.span),
            syn::Safety::Unsafe(token) => Some(token.span),
            syn::Safety::Default => None,
        })
        .or_else(|| signature.abi.as_ref().map(|abi| abi.extern_token.span))
        .unwrap_or(signature.fn_token.span)
}

visible_edges!(
    syn::ItemFn,
    n,
    n.modifiers.defaultness.map_or_else(|| signature_first(&n.sig), |token| token.span),
    n.block.brace_token.span.close()
);
visible_edges!(
    syn::ImplItemFn,
    n,
    n.modifiers.defaultness.map_or_else(|| signature_first(&n.sig), |token| token.span),
    n.block.brace_token.span.close()
);
edges!(
    syn::TraitItemFn,
    n,
    signature_first(&n.sig),
    n.default.as_ref().map_or_else(
        || n.semi_token.map_or_else(Span::call_site, |token| token.span),
        |block| block.brace_token.span.close()
    )
);
visible_edges!(syn::ItemConst, n, n.const_token.span, n.semi_token.span);
visible_edges!(syn::ItemEnum, n, n.enum_token.span, n.brace_token.span.close());
visible_edges!(syn::ItemExternCrate, n, n.extern_token.span, n.semi_token.span);
edges!(
    syn::ItemForeignMod,
    n,
    n.unsafety.map_or(n.abi.extern_token.span, |token| token.span),
    n.brace_token.span.close()
);
edges!(
    syn::ItemImpl,
    n,
    n.modifiers
        .defaultness
        .map_or_else(|| n.unsafety.map_or(n.impl_token.span, |token| token.span), |token| token.span),
    n.brace_token.span.close()
);
edges!(
    syn::ItemMacro,
    n,
    n.mac.path.edge(true),
    n.semi_token.map_or_else(|| n.mac.delimiter.span().close(), |token| token.span)
);
visible_edges!(
    syn::ItemMod,
    n,
    n.unsafety.map_or(n.mod_token.span, |token| token.span),
    n.content.as_ref().map_or_else(
        || n.semi.map_or_else(|| n.ident.span(), |token| token.span),
        |(brace, _)| brace.span.close()
    )
);
visible_edges!(syn::ItemStatic, n, n.static_token.span, n.semi_token.span);
visible_edges!(
    syn::ItemStruct,
    n,
    n.struct_token.span,
    n.semi_token.map_or_else(
        || match &n.fields {
            syn::Fields::Named(fields) => fields.brace_token.span.close(),
            _ => n.ident.span(),
        },
        |token| token.span
    )
);
visible_edges!(
    syn::ItemTrait,
    n,
    n.unsafety.map_or_else(
        || n.modifiers.auto_token.map_or(n.trait_token.span, |token| token.span),
        |token| token.span
    ),
    n.brace_token.span.close()
);
visible_edges!(syn::ItemTraitAlias, n, n.trait_token.span, n.semi_token.span);
visible_edges!(syn::ItemType, n, n.type_token.span, n.semi_token.span);
visible_edges!(syn::ItemUnion, n, n.union_token.span, n.fields.brace_token.span.close());
visible_edges!(syn::ItemUse, n, n.use_token.span, n.semi_token.span);
edges!(syn::Local, n, n.let_token.span, n.semi_token.span);
edges!(
    syn::StmtMacro,
    n,
    n.mac.path.edge(true),
    n.semi_token.map_or_else(|| n.mac.delimiter.span().close(), |token| token.span)
);
edges!(syn::FieldValue, n, n.member.span(), n.expr.edge(false));
edges!(
    syn::Arm,
    n,
    n.pat.edge(true),
    n.comma.map_or_else(|| n.body.edge(false), |token| token.span)
);
visible_edges!(
    syn::Field,
    n,
    n.ident.as_ref().map_or_else(|| n.ty.edge(true), syn::Ident::span),
    n.ty.edge(false)
);

impl Edges for syn::Attribute {
    fn edge(&self, first: bool) -> Span {
        if
        // #[gamma::skip(cond.negate, tag = "equivalent", reason = "Only extent consumes these two edges, and joining same-file pound and bracket spans is commutative.")]
        first {
            self.pound_token.span
        } else {
            self.bracket_token.span.close()
        }
    }
}

impl Edges for syn::Type {
    // #[gamma::skip(match_arm.never_matches, tag = "performance-only", reason = "Each arm falls back to the same whole syntax span; consumers join its edges or an enclosing boundary, preserving the final extent.")]
    fn edge(&self, first: bool) -> Span {
        match self {
            Self::Array(n) => n.edge(first),
            Self::FnPtr(n) => n.edge(first),
            Self::Group(n) => n.edge(first),
            Self::ImplTrait(n) => n.edge(first),
            Self::Infer(n) => n.edge(first),
            Self::Macro(n) => n.edge(first),
            Self::Never(n) => n.edge(first),
            Self::Paren(n) => n.edge(first),
            Self::Path(n) => n.edge(first),
            Self::Ptr(n) => n.edge(first),
            Self::Reference(n) => n.edge(first),
            Self::Slice(n) => n.edge(first),
            Self::TraitObject(n) => n.edge(first),
            Self::Tuple(n) => n.edge(first),
            Self::Verbatim(tokens) => token_edge(tokens, first),
            _ => self.span(),
        }
    }
}

fn bound_edge(bounds: &syn::punctuated::Punctuated<syn::TypeParamBound, syn::Token![+]>, first: bool) -> Span {
    if first {
        bounds.first().map_or_else(Span::call_site, |bound| bound.edge(true))
    } else {
        bounds.pairs().next_back().map_or_else(Span::call_site, |pair| match pair {
            syn::punctuated::Pair::Punctuated(_, token) => token.span,
            syn::punctuated::Pair::End(bound) => bound.edge(false),
        })
    }
}

impl Edges for syn::TypeParamBound {
    // #[gamma::skip(match_arm.never_matches, tag = "performance-only", reason = "Each arm falls back to the same whole syntax span; consumers join its edges or an enclosing boundary, preserving the final extent.")]
    fn edge(&self, first: bool) -> Span {
        match self {
            Self::Trait(bound) => {
                if let Some(paren) = &bound.paren_token {
                    return if first { paren.span.open() } else { paren.span.close() };
                }
                if first {
                    bound.lifetimes.as_ref().map_or_else(
                        || bound.maybe.map_or_else(|| bound.path.edge(true), |token| token.span),
                        |lifetimes| lifetimes.for_token.span,
                    )
                } else {
                    bound.path.edge(false)
                }
            }
            Self::Lifetime(lifetime) => {
                if first {
                    lifetime.apostrophe
                } else {
                    lifetime.ident.span()
                }
            }
            Self::PreciseCapture(capture) => {
                if
                // #[gamma::skip(cond.always_false, tag = "unreachable", reason = "Parsed precise captures occur only in impl Trait, which requests their last edge; trait objects reject precise captures.")]
                first {
                    capture.use_token.span
                } else {
                    capture.gt_token.span
                }
            }
            Self::Verbatim(tokens) => token_edge(tokens, first),
            _ => self.span(),
        }
    }
}

edges!(syn::TypeArray, n, n.bracket_token.span.open(), n.bracket_token.span.close());
edges!(
    syn::TypeFnPtr,
    n,
    n.lifetimes.as_ref().map_or_else(
        || n.unsafety.map_or_else(
            || n.abi.as_ref().map_or(n.fn_token.span, |abi| abi.extern_token.span),
            |token| token.span
        ),
        |lifetimes| lifetimes.for_token.span
    ),
    match &n.output {
        syn::ReturnType::Default => n.paren_token.span.close(),
        syn::ReturnType::Type(_, ty) => ty.edge(false),
    }
);
edges!(syn::TypeGroup, n, n.group_token.span, n.group_token.span);
edges!(syn::TypeImplTrait, n, n.impl_token.span, bound_edge(&n.bounds, false));
edges!(syn::TypeInfer, n, n.underscore_token.span, n.underscore_token.span);
edges!(syn::TypeMacro, n, n.mac.path.edge(true), n.mac.delimiter.span().close());
edges!(syn::TypeNever, n, n.bang_token.span, n.bang_token.span);
edges!(syn::TypeParen, n, n.paren_token.span.open(), n.paren_token.span.close());
edges!(
    syn::TypePath,
    n,
    n.qself.as_ref().map_or_else(|| n.path.edge(true), |qself| qself.lt_token.span),
    n.path.edge(false)
);
edges!(syn::TypePtr, n, n.star_token.span, n.elem.edge(false));
edges!(syn::TypeReference, n, n.and_token.span, n.elem.edge(false));
edges!(syn::TypeSlice, n, n.bracket_token.span.open(), n.bracket_token.span.close());
edges!(
    syn::TypeTraitObject,
    n,
    n.dyn_token.map_or_else(|| bound_edge(&n.bounds, true), |token| token.span),
    bound_edge(&n.bounds, false)
);
edges!(syn::TypeTuple, n, n.paren_token.span.open(), n.paren_token.span.close());

impl Edges for syn::Pat {
    // #[gamma::skip(match_arm.never_matches, tag = "performance-only", reason = "Each arm falls back to the same whole syntax span; consumers join its edges or an enclosing boundary, preserving the final extent.")]
    fn edge(&self, first: bool) -> Span {
        match self {
            Self::Const(n) => n.edge(first),
            Self::Guard(n) => n.edge(first),
            Self::Ident(n) => n.edge(first),
            Self::Lit(n) => n.edge(first),
            Self::Macro(n) => n.edge(first),
            Self::Or(n) => n.edge(first),
            Self::Paren(n) => n.edge(first),
            Self::Path(n) => n.edge(first),
            Self::Range(n) => n.edge(first),
            Self::Reference(n) => n.edge(first),
            Self::Rest(n) => n.edge(first),
            Self::Slice(n) => n.edge(first),
            Self::Struct(n) => n.edge(first),
            Self::Tuple(n) => n.edge(first),
            Self::TupleStruct(n) => n.edge(first),
            Self::Type(n) => n.edge(first),
            Self::Wild(n) => n.edge(first),
            Self::Verbatim(tokens) => token_edge(tokens, first),
            _ => self.span(),
        }
    }
}

edges!(syn::PatGuard, n, n.pat.edge(true), n.guard.edge(false));
edges!(
    syn::PatIdent,
    n,
    n.by_ref.map_or_else(
        || n.mutability.map_or_else(|| n.ident.span(), |token| token.span),
        |token| token.span
    ),
    n.subpat.as_ref().map_or_else(|| n.ident.span(), |(_, pattern)| pattern.edge(false))
);
edges!(
    syn::PatOr,
    n,
    n.leading_vert.map_or_else(
        || n.cases.first().map_or_else(Span::call_site, |pattern| pattern.edge(true)),
        |token| token.span
    ),
    n.cases.last().map_or_else(Span::call_site, |pattern| pattern.edge(false))
);
edges!(syn::PatParen, n, n.paren_token.span.open(), n.paren_token.span.close());
edges!(syn::PatReference, n, n.and_token.span, n.pat.edge(false));
edges!(syn::PatRest, n, n.dot2_token.span(), n.dot2_token.span());
edges!(syn::PatSlice, n, n.bracket_token.span.open(), n.bracket_token.span.close());
edges!(
    syn::PatStruct,
    n,
    n.qself.as_ref().map_or_else(|| n.path.edge(true), |qself| qself.lt_token.span),
    n.brace_token.span.close()
);
edges!(syn::PatTuple, n, n.paren_token.span.open(), n.paren_token.span.close());
edges!(
    syn::PatTupleStruct,
    n,
    n.qself.as_ref().map_or_else(|| n.path.edge(true), |qself| qself.lt_token.span),
    n.paren_token.span.close()
);
edges!(syn::PatType, n, n.pat.edge(true), n.ty.edge(false));
edges!(syn::PatWild, n, n.underscore_token.span, n.underscore_token.span);

impl Edges for syn::Item {
    // #[gamma::skip(match_arm.never_matches, tag = "performance-only", reason = "Each arm falls back to the same whole syntax span; consumers join its edges or an enclosing boundary, preserving the final extent.")]
    fn edge(&self, first: bool) -> Span {
        match self {
            Self::Const(n) => n.edge(first),
            Self::Enum(n) => n.edge(first),
            Self::ExternCrate(n) => n.edge(first),
            Self::Fn(n) => n.edge(first),
            Self::ForeignMod(n) => n.edge(first),
            Self::Impl(n) => n.edge(first),
            Self::Macro(n) => n.edge(first),
            Self::Mod(n) => n.edge(first),
            Self::Static(n) => n.edge(first),
            Self::Struct(n) => n.edge(first),
            Self::Trait(n) => n.edge(first),
            Self::TraitAlias(n) => n.edge(first),
            Self::Type(n) => n.edge(first),
            Self::Union(n) => n.edge(first),
            Self::Use(n) => n.edge(first),
            Self::Verbatim(tokens) => token_edge(tokens, first),
            _ => self.span(),
        }
    }
}

impl Edges for syn::Stmt {
    fn edge(&self, first: bool) -> Span {
        match self {
            Self::Local(n) => n.edge(first),
            Self::Item(n) => n.edge(first),
            Self::Expr(n, semi) => {
                if
                // #[gamma::skip(cond.negate, tag = "equivalent", reason = "Only extent consumes these two statement edges, and their same-file span union is commutative.")]
                first {
                    n.edge(true)
                } else {
                    semi.map_or_else(|| n.edge(false), |token| token.span)
                }
            }
            Self::Macro(n) => n.edge(first),
        }
    }
}

impl Edges for syn::File {
    fn edge(&self, first: bool) -> Span {
        if
        // #[gamma::skip(cond.negate, tag = "equivalent", reason = "Only extent consumes these two file edges, and their same-file span union is commutative; an empty file has equal call-site edges.")]
        first {
            self.attrs.first().map_or_else(
                || self.items.first().map_or_else(Span::call_site, |item| item.edge(true)),
                |attribute| attribute.pound_token.span,
            )
        } else {
            self.items.last().map_or_else(
                || {
                    self.attrs
                        .last()
                        .map_or_else(Span::call_site, |attribute| attribute.bracket_token.span.close())
                },
                |item| item.edge(false),
            )
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use syn::parse::Parser as _;
    use syn::visit::{self, Visit};

    use super::*;

    #[test]
    fn type_and_pattern_boundaries_preserve_complete_token_ranges() {
        for source in ["_", "!", "(u8)", "types!()"] {
            let ty = syn::parse_str::<syn::Type>(source).unwrap();
            assert_eq!(ty.extent().byte_range(), 0..source.len(), "{source}");
        }
        for source in ["crate::VALUE", "1..=3", "&value", ".."] {
            let pattern = syn::Pat::parse_multi.parse_str(source).unwrap();
            assert_eq!(pattern.extent().byte_range(), 0..source.len(), "{source}");
        }
    }

    #[test]
    fn trait_object_extents_keep_first_bound_modifiers_and_last_bound_delimiters() {
        for source in [
            "some::Trait + Send",
            "for<'a> some::Trait<'a> + Send",
            "'a + some::Trait",
            "(some::Trait) + Send",
            "dyn Send + (some::Trait)",
            "dyn some::Trait<u8>",
            "dyn Fn() -> Option<u8>",
        ] {
            let ty = syn::parse_str::<syn::Type>(source).unwrap();
            assert_eq!(ty.extent().byte_range(), 0..source.len(), "{source}");
        }
    }

    #[test]
    fn verbatim_boundaries_preserve_enclosing_type_pattern_and_file_extents() {
        for source in ["*const dyn* some::Trait", "impl [const] some::Trait", "impl const some::Trait"] {
            let ty = syn::parse_str::<syn::Type>(source).unwrap();
            assert_eq!(ty.extent().byte_range(), 0..source.len(), "{source}");
        }
        let source = "box first | second";
        let pattern = syn::Pat::parse_multi.parse_str(source).unwrap();
        assert_eq!(pattern.extent().byte_range(), 0..source.len());
        let source = "macro m($arg:expr) { $arg }\nfn f() {}";
        let file = syn::parse_file(source).unwrap();
        assert!(matches!(file.items[0], syn::Item::Verbatim(_)));
        assert_eq!(file.extent().byte_range(), 0..source.len());
    }

    #[test]
    fn unsafe_signatures_and_multiple_inner_attributes_keep_the_first_source_token() {
        let source = "unsafe extern \"C\" fn f() {}";
        let item = syn::parse_str::<syn::ItemFn>(source).unwrap();
        assert_eq!(item.extent().byte_range(), 0..source.len());
        let source = "#![allow(unused)]\n#![allow(dead_code)]\nfn f() {}";
        let file = syn::parse_file(source).unwrap();
        assert_eq!(file.extent().byte_range(), 0..source.len());
    }

    #[test]
    fn spine_extents_match_quoted_extents_across_nested_and_loop_dense_syntax() {
        struct Check;
        impl<'ast> Visit<'ast> for Check {
            fn visit_expr(&mut self, i: &'ast Expr) {
                assert_eq!(i.extent().byte_range(), i.span().byte_range(), "{i:?}");
                visit::visit_expr(self, i);
            }

            fn visit_macro(&mut self, i: &'ast syn::Macro) {
                assert_eq!(i.extent().byte_range(), i.span().byte_range(), "{i:?}");
                visit::visit_macro(self, i);
            }

            fn visit_type(&mut self, i: &'ast syn::Type) {
                assert_eq!(i.extent().byte_range(), i.span().byte_range(), "{i:?}");
                visit::visit_type(self, i);
            }

            fn visit_pat(&mut self, i: &'ast syn::Pat) {
                assert_eq!(i.extent().byte_range(), i.span().byte_range(), "{i:?}");
                visit::visit_pat(self, i);
            }

            fn visit_item(&mut self, i: &'ast syn::Item) {
                assert_eq!(i.extent().byte_range(), i.span().byte_range(), "{i:?}");
                visit::visit_item(self, i);
            }

            fn visit_stmt(&mut self, i: &'ast syn::Stmt) {
                assert_eq!(i.extent().byte_range(), i.span().byte_range(), "{i:?}");
                visit::visit_stmt(self, i);
            }

            fn visit_attribute(&mut self, i: &'ast syn::Attribute) {
                assert_eq!(i.extent().byte_range(), i.span().byte_range(), "{i:?}");
                visit::visit_attribute(self, i);
            }

            fn visit_arm(&mut self, i: &'ast syn::Arm) {
                assert_eq!(i.extent().byte_range(), i.span().byte_range(), "{i:?}");
                visit::visit_arm(self, i);
            }

            fn visit_field(&mut self, i: &'ast syn::Field) {
                assert_eq!(i.extent().byte_range(), i.span().byte_range(), "{i:?}");
                visit::visit_field(self, i);
            }
        }
        let binaries = format!("fn f(a: i32) -> i32 {{ {} }}", vec!["a"; 80].join(" + "));
        let nested = format!("fn f(a: i32) -> i32 {{ {}a{} }}", "(".repeat(32), ")".repeat(32));
        let loops = format!(
            "fn f(a: i32) {{ {} }}",
            "while a > 0 { for i in 0..a { if i > 0 { continue; } } }".repeat(32)
        );
        for source in [
            &binaries,
            &nested,
            &loops,
            include_str!("../ops/collect/collector/fixtures/expanding_aliases.rs"),
        ] {
            Check.visit_file(&syn::parse_file(source).unwrap());
        }
        for source in [
            "#[allow(unused)] a + b",
            "'label: loop { break 'label 1 }",
            "async move { value.await }",
            "move |x| x + 1",
            "return Some(7)",
            "&raw const value",
            "<Vec<u8> as Trait>::value()",
            "Foo::<{ 3 }>::BAR",
            "value.method::<u8>(1)",
            "if flag { 1 } else { 2 }",
            "match value { 1 => 2, _ => 3 }",
            "S { value: 1, ..other }",
            "vec![1, 2]",
            "some::macro_name!(a + b)",
            "some::macro_name! { a + b }",
        ] {
            Check.visit_expr(&syn::parse_str::<Expr>(source).unwrap());
        }
        for source in [
            "type T = &'static mut [Option<fn(u8) -> &'static str>];",
            "type T = unsafe extern \"C\" fn(u8, ...) -> *const [u8; 1 + 2];",
            "type T = dyn for<'a> Trait<'a> + Send + 'static;",
            "fn f() -> impl Iterator<Item = u8> + use<> { 0..7 }",
            "/// doc\nstruct S { /// doc\n pub value: [u8; 1 + 2] }",
            "fn f(value: Option<[u8; 2]>) { match value { Some([first, ..]) if first > 0 => (), _ => () } }",
        ] {
            Check.visit_file(&syn::parse_file(source).unwrap());
        }
    }
}
