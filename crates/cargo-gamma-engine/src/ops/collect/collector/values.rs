// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The replacement values a function's return type admits.

use compact_str::{CompactString, format_compact};
use syn::{GenericArgument, PathArguments, PathSegment, ReturnType, Type, TypeParamBound};

use super::predicates::payload;
use super::types::{Types, is_abstract_type};

/// How deep the recursion through nested return types is allowed to go.
///
/// A tuple of options of results nests as far as the author cared to write, and each level
/// multiplies the number of values below it. Three levels reaches `Result<Option<bool>, E>`, which
/// is the shape this exists for, and stops well before a type whose values would dominate the
/// population of the whole file.
pub(super) const RETURN_DEPTH: usize = 3;

/// The most replacement values any single return type may contribute.
///
/// The bound is on the product, not on any one level, because it is the product that decides how
/// many mutants a function costs. A tuple of four booleans is sixteen combinations, and every one
/// of them is a separate build round's worth of test time.
pub(super) const RETURN_WIDTH: usize = 8;

type ReplacementValue = (&'static str, CompactString);

fn some_value((_name, text): ReplacementValue) -> ReplacementValue {
    ("fn_value.some", format_compact!("Some({text})"))
}

/// The replacement values worth trying for a function's return type.
///
/// Each entry is a mutator name and the text of a value of that type. The list is generated rather
/// than looked up so that nested types compose: a `Result<Option<bool>, E>` is a `Result` whose
/// success values are the `Option` values, which are in turn the `bool` values.
///
/// The type is read syntactically, so an alias, a generic parameter or an associated type falls
/// through to `Default::default()`, which may not compile — an acceptable trade, since a bad guess
/// costs one rollback round rather than losing the mutant entirely.
pub(super) fn return_values(output: &ReturnType, types: &Types<'_>) -> Vec<ReplacementValue> {
    let ReturnType::Type(_arrow, ty) = output else {
        return vec![("fn_value.unit", "()".into())];
    };

    values_for(ty, RETURN_DEPTH, types)
}

/// The replacement values for one type, recursing through its parameters.
///
/// `depth` bounds the recursion; at zero the type contributes `Default::default()` rather than
/// nothing, because a value that type-checks is still worth trying even when its shape is unknown.
#[expect(clippy::too_many_lines, reason = "the match exhaustively maps every return-type kind")]
pub(super) fn values_for(ty: &Type, depth: usize, types: &Types<'_>) -> Vec<ReplacementValue> {
    let kind = if types.is_fmt_result(ty) { Kind::Result } else { resolve_type(ty) };

    // An abstract type contributes nothing rather than a guess. `Default::default()` is what this
    // family reaches for when it cannot name a value, and for a caller's type parameter or a
    // trait's associated type nothing promises there is one to reach for.
    //
    // An `impl Iterator` is the one exception, and only because it is not really a guess: every
    // iterator can be named through `gamma_rt::Either`, whatever concrete type the body chose.
    if kind != Kind::Iterator && is_abstract_type(ty, types.abstracts) {
        return Vec::new();
    }

    // An error type from another crate contributes nothing for the same reason, but with a stronger
    // warrant: `Default::default()` is not a guess that might be wrong here, it is one that has been
    // measured wrong over and over. This is the largest single cause of mutants that cannot compile.
    if kind == Kind::Unknown && types.lacks_default(ty) {
        return Vec::new();
    }

    if depth == 0 {
        return vec![("fn_value.default", "Default::default()".into())];
    }

    match kind {
        // The kinds whose values are written out rather than built from another type's.
        kind @ (Kind::Unit
        | Kind::Bool
        | Kind::Signed
        | Kind::Unsigned
        | Kind::Float
        | Kind::StaticStr
        | Kind::MutStr
        | Kind::String
        | Kind::NonZero) => literal_values(kind, ty),

        // The empty case is universal; the one-element case needs a value to put in it, which is
        // what the recursion supplies.
        Kind::Option => {
            let mut values = vec![("fn_value.none", "None".into())];
            let inner = inner_values(ty, 0, depth, types);

            if inner.is_empty() {
                // Nothing is known about the payload, either because it is a type this file cannot
                // resolve or because it is a reference. `Default::default()` is the one expression
                // that stands for a value of any type it fits, so it is what is left to try. It
                // will not always compile, and that is accepted: the alternative is to emit only
                // `None` and never ask whether the present case is tested at all.
                if !payload(ty, 0).is_some_and(|inner| is_abstract_type(inner, types.abstracts) || types.lacks_default(inner)) {
                    values.push(("fn_value.some_default", "Some(Default::default())".into()));
                }
            } else {
                values.extend(inner.into_iter().map(some_value));
            }

            cap(values)
        }

        Kind::Result => {
            let inner = inner_values(ty, 0, depth, types);
            let mut values = if inner.is_empty() {
                if payload(ty, 0).is_some_and(|inner| is_abstract_type(inner, types.abstracts) || types.lacks_default(inner)) {
                    Vec::new()
                } else {
                    vec![("fn_value.ok_default", "Ok(Default::default())".into())]
                }
            } else {
                inner
                    .into_iter()
                    .map(|(_name, text)| ("fn_value.ok", format_compact!("Ok({text})")))
                    .collect()
            };

            // The error type is only named when the path spells it. `type Result<T> =
            // core::result::Result<T, MyError>` is everywhere in real crates, and there the error
            // is whatever the alias fixed it to — almost never something with a `Default`. Offering
            // `Err(Default::default())` on a guess buys one mutant that usually cannot compile, so
            // it is offered only when the second argument is present and not abstract.
            if payload(ty, 1).is_some_and(|inner| !is_abstract_type(inner, types.abstracts) && !types.lacks_default(inner)) {
                values.push(("fn_value.err_default", "Err(Default::default())".into()));
            }

            cap(values)
        }

        // Every one of these builds from an iterator of its element type, so one construction
        // covers all of them and the element values come from the recursion.
        Kind::Collection => {
            let empty = format_compact!("{}::new()", collection_ctor(ty));
            let mut values = vec![("fn_value.empty_collection", empty)];

            values.extend(
                inner_values(ty, 0, depth, types)
                    .into_iter()
                    .map(|(_name, text)| ("fn_value.one_element", format_compact!("core::iter::once({text}).collect()"))),
            );

            cap(values)
        }

        // A map's element is a pair, so its one-element form needs both parameters rather than the
        // first alone.
        Kind::Map => {
            let empty = format_compact!("{}::new()", collection_ctor(ty));
            let mut values = vec![("fn_value.empty_collection", empty)];

            let keys = inner_values(ty, 0, depth, types);
            let vals = inner_values(ty, 1, depth, types);

            if let (Some((_kn, key)), Some((_vn, value))) = (keys.first(), vals.first()) {
                values.push((
                    "fn_value.one_element",
                    format_compact!("core::iter::once(({key}, {value})).collect()"),
                ));
            }

            cap(values)
        }

        // A smart pointer is transparent to the caller's reasoning, so the values worth trying are
        // its contents wrapped back up.
        Kind::Wrapper => {
            let ctor = wrapper_ctor(ty);

            cap(inner_values(ty, 0, depth, types)
                .into_iter()
                .map(|(name, text)| (name, format_compact!("{ctor}({text})")))
                .collect())
        }

        // `Cow` is a wrapper whose constructor is a variant rather than a function, and `Owned`
        // is the variant that does not borrow from anything in scope.
        // The written path is reused rather than `std::borrow::Cow`, which would name a different
        // type from the one the function returns whenever the author meant somebody else's.
        Kind::Cow => {
            let ctor = collection_ctor(ty);

            cap(inner_values(ty, 0, depth, types)
                .into_iter()
                .map(|(name, text)| (name, format_compact!("{ctor}::Owned({text})")))
                .collect())
        }

        // An `impl Iterator` return is one concrete type chosen by the body, so `Empty<T>`,
        // `Once<T>` and whatever the author wrote are three types that cannot be arms of one `if`
        // on their own. `Shape::IterBlock` wraps each arm so that they can be, which is why values
        // are offered here rather than withheld.
        //
        // `empty()` needs no item type at all, since the wrapper infers it from the other arm.
        // `once(v)` needs a value, so it is offered only when the signature wrote `Item = T` and
        // `T` is a type this tool can name a value of.
        Kind::Iterator => {
            let mut values = vec![("fn_value.empty_collection", "core::iter::empty()".into())];

            if let Some(item) = iterator_item(ty) {
                values.extend(
                    values_for(item, depth.saturating_sub(1), types)
                        .into_iter()
                        .map(|(_name, text)| ("fn_value.one_element", format_compact!("core::iter::once({text})"))),
                );
            }

            cap(values)
        }
        Kind::Reference => reference_values(ty, depth, types),

        // Every combination of the elements' values, which is where the product bound earns its
        // keep: three fields with three values each is twenty-seven mutants for one function.
        Kind::Tuple => tuple_values(ty, depth, types),

        Kind::Unknown => vec![("fn_value.default", "Default::default()".into())],
    }
}

/// Every combination of a tuple's element values, which is where the width bound earns its keep:
/// three fields with three values each is twenty-seven mutants for a single function, and the
/// user has to read every one of them.
pub(super) fn tuple_values(ty: &Type, depth: usize, types: &Types<'_>) -> Vec<ReplacementValue> {
    let Type::Tuple(tuple) = strip(ty) else {
        return vec![("fn_value.default", "Default::default()".into())];
    };

    let mut combinations: Vec<Vec<CompactString>> = vec![Vec::new()];

    for element in &tuple.elems {
        let choices = values_for(element, depth.saturating_sub(1), types);
        let mut next = Vec::new();

        for existing in &combinations {
            for (_name, text) in &choices {
                if next.len() >= RETURN_WIDTH {
                    break;
                }

                let mut combination = existing.clone();

                combination.push(text.clone());
                next.push(combination);
            }
        }

        combinations = next;
    }

    combinations
        .into_iter()
        .map(|parts| {
            let text = if parts.len() == 1 {
                format_compact!("({},)", parts[0])
            } else {
                format_compact!("({})", parts.join(", "))
            };

            ("fn_value.tuple", text)
        })
        .collect()
}

/// The replacement values for a type that has a fixed list of them.
///
/// These are the kinds whose values can be written down directly, as opposed to the containers and
/// wrappers whose values are built by recursing into a parameter. Kinds outside that group
/// contribute nothing here, because they are handled by the caller.
pub(super) fn literal_values(kind: Kind, ty: &Type) -> Vec<ReplacementValue> {
    match kind {
        Kind::Unit => vec![("fn_value.unit", "()".into())],

        Kind::Bool => vec![("fn_value.bool_true", "true".into()), ("fn_value.bool_false", "false".into())],

        Kind::Signed => vec![
            ("fn_value.zero", "0".into()),
            ("fn_value.one", "1".into()),
            ("fn_value.minus_one", "-1".into()),
        ],

        Kind::Unsigned => vec![("fn_value.zero", "0".into()), ("fn_value.one", "1".into())],

        Kind::Float => vec![
            ("fn_value.zero", "0.0".into()),
            ("fn_value.one", "1.0".into()),
            ("fn_value.minus_one", "-1.0".into()),
        ],

        Kind::StaticStr => vec![
            ("fn_value.empty_string", "\"\"".into()),
            ("fn_value.xyzzy_string", "\"xyzzy\"".into()),
        ],

        // A literal will not do here — it is `&'static str`, and the signature asked for a mutable
        // slice. Leaking a boxed `str` yields the `&'static mut str` that will actually type-check,
        // using the same `Box::leak` idiom the reference values are built with.
        Kind::MutStr => vec![
            ("fn_value.empty_string", "Box::leak(String::new().into_boxed_str())".into()),
            (
                "fn_value.xyzzy_string",
                "Box::leak(String::from(\"xyzzy\").into_boxed_str())".into(),
            ),
        ],

        Kind::String => vec![
            ("fn_value.empty_string", "String::new()".into()),
            ("fn_value.xyzzy_string", "\"xyzzy\".to_owned()".into()),
        ],

        // A `NonZero` cannot hold the zero every other numeric type offers, so the interesting
        // values are the smallest it can hold and one that is merely different.
        Kind::NonZero => vec![
            ("fn_value.one", format_compact!("{}::new(1).unwrap()", type_text(ty))),
            ("fn_value.two", format_compact!("{}::new(2).unwrap()", type_text(ty))),
        ],

        _ => Vec::new(),
    }
}

/// The values for a reference return, produced by leaking a box.
///
/// A reference has to point at something that outlives the call, and the obvious spellings do not:
/// `&Default::default()` borrows a temporary that dies at the end of the expression, so the mutant
/// fails to compile rather than answering anything. `Box::leak` is what makes the family reach
/// these returns at all — it yields a `&'static mut T`, which coerces to a reference of any shorter
/// lifetime and to a shared one, so `&T`, `&'a T` and `&mut T` are all served by the same text.
///
/// This matters more than it sounds. A getter handing back `&String` or `&[T]` behind a reference
/// is one of the commonest shapes in Rust, and until this existed every one of them was passed over
/// in silence — not reported as unmutatable, simply absent, so a suite that never checked what a
/// getter returned still scored perfectly.
///
/// The values are the element type's own, so `&Vec<T>` offers an empty vector and a one-element
/// one exactly as `Vec<T>` would. The leak is deliberate and its cost is bounded by the mutant's
/// own lifetime: the process running the tests exits shortly afterwards. A mutant leaking on a hot
/// path can exhaust the memory limit and be reported as `OUTOFMEM`, which is a kill — the mutant
/// did change observable behaviour — though not the one the tests were asked about. That is the
/// boundary of the policy: the leak's own allocation is allowed to decide a verdict, but only ever
/// as a documented kill, never as a survivor a suite could be blamed for.
pub(super) fn reference_values(ty: &Type, depth: usize, types: &Types<'_>) -> Vec<ReplacementValue> {
    let Some(elem) = reference_elem(ty) else {
        return Vec::new();
    };

    // `Box::leak` hands back `&mut T`. Where the signature asked for `&T` that is usually invisible,
    // because a return position reborrows it silently — but not always. As an `impl Iterator`'s
    // item it is the thing the item type is *inferred from*, so `&mut String` is inferred where
    // `&String` was promised and the mutant is withdrawn as unviable. Reborrowing here says what
    // was meant in every position instead of relying on one of them being forgiving.
    let shared = matches!(strip(ty), Type::Reference(reference) if reference.mutability.is_none());
    let prefix = if shared { "&*" } else { "" };

    cap(values_for(elem, depth.saturating_sub(1), types)
        .into_iter()
        .map(|(name, text)| (name, format_compact!("{prefix}Box::leak(Box::new({text}))")))
        .collect())
}

/// The type a reference points at, seeing through parentheses and invisible grouping.
pub(super) fn reference_elem(ty: &Type) -> Option<&Type> {
    match ty {
        Type::Reference(reference) => Some(&reference.elem),
        Type::Paren(paren) => reference_elem(&paren.elem),
        _ => None,
    }
}

/// Truncates a value list to the width bound.
pub(super) fn cap(mut values: Vec<ReplacementValue>) -> Vec<ReplacementValue> {
    values.truncate(RETURN_WIDTH);
    values
}

/// The values of a generic type's `index`th type parameter.
///
/// Lifetime and const parameters are skipped, so `Cow<'a, str>` finds `str` at index zero the way
/// `Option<T>` finds `T`.
pub(super) fn inner_values(ty: &Type, index: usize, depth: usize, types: &Types<'_>) -> Vec<ReplacementValue> {
    type_argument(ty, index).map_or_else(
        || vec![("fn_value.default", "Default::default()".into())],
        |inner| values_for(inner, depth.saturating_sub(1), types),
    )
}

/// The `Item` type an `impl Iterator` signature binds, when it wrote one.
///
/// A bare `impl Iterator`, or one whose item is itself opaque, gives nothing to build a value
/// from. That costs only the one-element mutant: the empty one needs no item type, because the
/// wrapper infers it from the arm holding the original.
pub(super) fn iterator_item(ty: &Type) -> Option<&Type> {
    let Type::ImplTrait(imp) = strip(ty) else {
        return None;
    };

    imp.bounds.iter().find_map(|bound| {
        let TypeParamBound::Trait(tr) = bound else {
            return None;
        };

        let PathArguments::AngleBracketed(args) = &tr.path.segments.last()?.arguments else {
            return None;
        };

        args.args.iter().find_map(|arg| match arg {
            GenericArgument::AssocType(assoc) if assoc.ident == "Item" => Some(&assoc.ty),
            _ => None,
        })
    })
}

/// The `index`th type argument of a path type, ignoring lifetimes and const generics.
pub(super) fn type_argument(ty: &Type, index: usize) -> Option<&Type> {
    let Type::Path(path) = strip(ty) else {
        return None;
    };

    let PathArguments::AngleBracketed(args) = &path.path.segments.last()?.arguments else {
        return None;
    };

    args.args
        .iter()
        .filter_map(|arg| match arg {
            GenericArgument::Type(inner) => Some(inner),
            _ => None,
        })
        .nth(index)
}

/// The path text of a type, so that an associated function can be called on it.
pub(super) fn type_text(ty: &Type) -> String {
    match strip(ty) {
        Type::Path(path) => path
            .path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>()
            .join("::"),
        _ => "Default".to_owned(),
    }
}

/// The number of type arguments a path segment carries.
///
/// Lifetimes and const arguments are not counted, because they say nothing about which type is
/// being named: `Cow<'a, str>` names one type, not two.
pub(super) fn type_arguments(segment: &PathSegment) -> usize {
    let PathArguments::AngleBracketed(args) = &segment.arguments else {
        return 0;
    };

    args.args.iter().filter(|arg| matches!(arg, GenericArgument::Type(_))).count()
}

/// The constructor path for a collection type, keeping any qualification the author wrote.
pub(super) fn collection_ctor(ty: &Type) -> String {
    let Type::Path(path) = strip(ty) else {
        return "Vec".to_owned();
    };

    path.path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

/// The constructor for a smart-pointer type.
pub(super) fn wrapper_ctor(ty: &Type) -> String {
    format!("{}::new", collection_ctor(ty))
}

/// Sees through parentheses to the type underneath.
pub(super) fn strip(ty: &Type) -> &Type {
    match ty {
        Type::Paren(paren) => strip(&paren.elem),
        other => other,
    }
}

/// The coarse classification of a return type that decides which values are worth trying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Unit,
    Bool,
    Signed,
    Unsigned,
    Float,
    StaticStr,
    /// A mutable string slice, whose values cannot be string literals.
    MutStr,
    String,
    NonZero,
    Option,
    Result,
    /// Anything built from an iterator of a single element type: `Vec`, `VecDeque`, sets, heaps.
    Collection,
    /// Anything built from an iterator of key-value pairs.
    Map,
    /// A smart pointer constructed by `new`: `Box`, `Rc`, `Arc`.
    Wrapper,
    Cow,
    Iterator,
    Tuple,
    Reference,
    Unknown,
}

/// Classifies a return type syntactically.
///
/// Only the last segment of a path is compared, because a standard type may be written bare, fully
/// qualified, or re-exported, and there is no name resolution here to tell those apart. That alone
/// would take any type whose final segment reads `Vec` for the standard one, so the count of type
/// arguments is checked as well: a `Vec` that takes none is somebody else's `Vec`, and treating it
/// as the standard one produces a mutant that cannot compile. A name that carries the wrong number
/// of type arguments is therefore classified as unknown, which still offers `Default::default()`
/// and so keeps a guess without pretending to know the shape.
///
/// A local type that shadows a standard name *and* matches its arity — a bare `struct String` —
/// remains indistinguishable and will yield a mutant that does not compile. That mutant is
/// classified as unviable and reported as such, which is the accepted cost of not resolving names.
pub(super) fn resolve_type(ty: &Type) -> Kind {
    match ty {
        Type::Tuple(tuple) if tuple.elems.is_empty() => Kind::Unit,

        Type::Reference(reference) => match &*reference.elem {
            // Mutability is load-bearing, not decoration: `StaticStr`'s values are string literals,
            // which are `&'static str` and cannot be returned where `&mut str` was promised. A
            // mutable one therefore gets its own kind rather than being folded in here.
            Type::Path(path) if path.path.is_ident("str") => {
                if reference.mutability.is_some() {
                    Kind::MutStr
                } else {
                    Kind::StaticStr
                }
            }

            // `&[T]` has a `Default`, unlike references in general, so it keeps the mutant that
            // depends on one.
            Type::Slice(_) => Kind::Unknown,

            _ => Kind::Reference,
        },

        Type::Tuple(_) => Kind::Tuple,

        Type::Paren(paren) => resolve_type(&paren.elem),

        // The traits whose `impl Trait` returns this tool can synthesize a value for. All four are
        // satisfied by `gamma_rt::Either` whenever both of its sides satisfy them, and both
        // `core::iter::empty()` and `core::iter::once(v)` satisfy all four.
        //
        // Any other `impl Trait` has no expression this tool can name that is guaranteed to
        // satisfy it.
        Type::ImplTrait(imp) => {
            let iterator = imp.bounds.iter().any(|bound| {
                matches!(bound, TypeParamBound::Trait(tr)
                if tr.path.segments.last().is_some_and(|segment| {
                    matches!(segment.ident.to_string().as_str(),
                        "Iterator" | "DoubleEndedIterator" | "ExactSizeIterator" | "FusedIterator")
                }))
            });

            if iterator { Kind::Iterator } else { Kind::Unknown }
        }

        Type::Path(path) => {
            let Some(segment) = path.path.segments.last() else {
                return Kind::Unknown;
            };

            let name = segment.ident.to_string();
            let arity = type_arguments(segment);

            if name.starts_with("NonZero") && name != "NonZero" {
                return if arity == 0 { Kind::NonZero } else { Kind::Unknown };
            }

            match (name.as_str(), arity) {
                ("bool", 0) => Kind::Bool,
                ("i8" | "i16" | "i32" | "i64" | "i128" | "isize", 0) => Kind::Signed,
                ("u8" | "u16" | "u32" | "u64" | "u128" | "usize", 0) => Kind::Unsigned,
                ("f32" | "f64", 0) => Kind::Float,
                ("String", 0) => Kind::String,
                ("Option", 1..) => Kind::Option,
                ("Result", 1..) => Kind::Result,
                ("Vec" | "VecDeque" | "HashSet" | "BTreeSet" | "BinaryHeap" | "LinkedList", 1..) => Kind::Collection,
                ("HashMap" | "BTreeMap", 2..) => Kind::Map,
                ("Box" | "Rc" | "Arc", 1..) => Kind::Wrapper,
                ("Cow", 1..) => Kind::Cow,
                _ => Kind::Unknown,
            }
        }

        _ => Kind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use syn::punctuated::Punctuated;
    use syn::{Path, TypeImplTrait, TypePath, parse_quote};

    use super::*;
    use crate::HashMap;
    use crate::ops::collect::Defaults;

    fn empty_path_type() -> Type {
        Type::Path(TypePath {
            qself: None,
            path: Path {
                leading_colon: None,
                segments: Punctuated::<syn::PathSegment, syn::token::PathSep>::new(),
            },
        })
    }

    fn test_types<'a>(abstracts: &'a [String], imports: &'a HashMap<String, Option<Vec<String>>>, defaults: &'a Defaults) -> Types<'a> {
        Types {
            abstracts,
            imports,
            defaults,
            self_type: None,
            self_associated: None,
        }
    }

    #[test]
    fn unknown_undefaultable_types_and_non_tuples_contribute_fallbacks() {
        let abstracts = Vec::new();
        let imports = HashMap::default();
        let defaults = Defaults::default();
        let types = test_types(&abstracts, &imports, &defaults);
        let foreign_error: Type = parse_quote!(std::io::Error);
        let plain: Type = parse_quote!(String);

        assert!(values_for(&foreign_error, RETURN_DEPTH, &types).is_empty());
        assert_eq!(
            tuple_values(&plain, RETURN_DEPTH, &types),
            vec![("fn_value.default", "Default::default()".into())]
        );
        assert!(literal_values(Kind::Option, &parse_quote!(Option<bool>)).is_empty());
    }

    #[test]
    fn reference_and_type_argument_helpers_reject_non_matching_syntax() {
        let abstracts = Vec::new();
        let imports = HashMap::default();
        let defaults = Defaults::default();
        let types = test_types(&abstracts, &imports, &defaults);
        let plain: Type = parse_quote!(String);
        let bare_collection: Type = parse_quote!(Vec);
        let tuple: Type = parse_quote!((bool,));
        let reference: Type = parse_quote!(&str);

        assert!(reference_values(&plain, RETURN_DEPTH, &types).is_empty());
        assert_eq!(reference_elem(&plain), None);
        assert_eq!(type_argument(&reference, 0), None);
        assert_eq!(type_argument(&bare_collection, 0), None);
        assert_eq!(
            inner_values(&bare_collection, 0, RETURN_DEPTH, &types),
            vec![("fn_value.default", "Default::default()".into())]
        );
        assert_eq!(type_text(&tuple), "Default");
        assert_eq!(collection_ctor(&tuple), "Vec");
    }

    #[test]
    fn iterator_item_requires_an_impl_trait_with_item_binding() {
        let plain: Type = parse_quote!(Vec<u8>);
        let imp = Type::ImplTrait(TypeImplTrait {
            impl_token: syn::token::Impl::default(),
            bounds: Punctuated::from_iter([
                TypeParamBound::Lifetime(parse_quote!('static)),
                TypeParamBound::Trait(parse_quote!(Iterator<Output = u8>)),
            ]),
        });

        assert_eq!(iterator_item(&plain), None);
        assert_eq!(iterator_item(&imp), None);
    }

    #[test]
    fn resolve_type_treats_an_empty_path_as_unknown() {
        assert_eq!(resolve_type(&empty_path_type()), Kind::Unknown);
    }
}
