// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The replacement values a function's return type admits.

use compact_str::{CompactString, format_compact};
use syn::{GenericArgument, PathArguments, PathSegment, ReturnType, Type, TypeParamBound};

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

fn some_value((name, text): ReplacementValue) -> ReplacementValue {
    let mutator = if name == "fn_value.default" {
        "fn_value.some_default"
    } else {
        "fn_value.some"
    };
    (mutator, format_compact!("Some({text})"))
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
    let resolved = types.resolve_alias(ty);
    let aliased = !core::ptr::eq(ty, resolved);
    let fmt_result = types.is_fmt_result(resolved);
    let kind = if fmt_result { Kind::Result } else { resolve_type(resolved) };

    // An abstract type contributes nothing rather than a guess. `Default::default()` is what this
    // family reaches for when it cannot name a value, and for a caller's type parameter or a
    // trait's associated type nothing promises there is one to reach for.
    //
    // An `impl Iterator` is the one exception, and only because it is not really a guess: every
    // iterator can be named through `gamma_rt::Either`, whatever concrete type the body chose.
    if kind != Kind::Iterator && is_abstract_type(resolved, types.abstracts) {
        return Vec::new();
    }

    // An error type from another crate contributes nothing for the same reason, but with a stronger
    // warrant: `Default::default()` is not a guess that might be wrong here, it is one that has been
    // measured wrong over and over. This is the largest single cause of mutants that cannot compile.
    if kind == Kind::Unknown && !types.has_default(resolved) {
        return Vec::new();
    }
    if aliased && matches!(kind, Kind::Collection | Kind::Map) && !types.has_default(resolved) {
        return Vec::new();
    }

    if depth == 0 {
        return if types.has_default(resolved) {
            vec![("fn_value.default", "Default::default()".into())]
        } else {
            Vec::new()
        };
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
        | Kind::NonZero) => literal_values(kind, resolved),

        // The empty case is universal; the one-element case needs a value to put in it, which is
        // what the recursion supplies.
        Kind::Option => {
            let mut values = vec![("fn_value.none", "None".into())];
            let inner = inner_values(ty, 0, depth, types);

            if inner.is_empty() {
                if types.payload(ty, 0).is_some_and(|inner| types.has_default(inner)) {
                    values.push(("fn_value.some_default", "Some(Default::default())".into()));
                }
            } else {
                values.extend(inner.into_iter().map(some_value));
            }

            cap(values)
        }

        Kind::Result => {
            if fmt_result {
                return vec![("fn_value.ok_default", "Ok(Default::default())".into())];
            }
            let inner = inner_values(ty, 0, depth, types);
            let mut values = if inner.is_empty() {
                if types.payload(ty, 0).is_some_and(|inner| types.has_default(inner)) {
                    vec![("fn_value.ok_default", "Ok(Default::default())".into())]
                } else {
                    Vec::new()
                }
            } else {
                inner
                    .into_iter()
                    .map(|(name, text)| {
                        let mutator = if name == "fn_value.default" {
                            "fn_value.ok_default"
                        } else {
                            "fn_value.ok"
                        };
                        (mutator, format_compact!("Ok({text})"))
                    })
                    .collect()
            };

            // The error type is only named when the path spells it. `type Result<T> =
            // core::result::Result<T, MyError>` is everywhere in real crates, and there the error
            // is whatever the alias fixed it to — almost never something with a `Default`. Offering
            // `Err(Default::default())` on a guess buys one mutant that usually cannot compile, so
            // it is offered only when the second argument is present and not abstract.
            if types.payload(ty, 1).is_some_and(|inner| types.has_default(inner)) {
                values.push(("fn_value.err_default", "Err(Default::default())".into()));
            }

            cap(values)
        }

        // Every one of these builds from an iterator of its element type, so one construction
        // covers all of them and the element values come from the recursion.
        Kind::Collection => {
            let empty = if aliased || types.defaults.has_default(resolved) {
                CompactString::new("Default::default()")
            } else {
                format_compact!("{}::new()", collection_ctor(resolved))
            };
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
            let empty = if aliased || types.defaults.has_default(resolved) {
                CompactString::new("Default::default()")
            } else {
                format_compact!("{}::new()", collection_ctor(resolved))
            };
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
            if is_unsized_string_wrapper(resolved) {
                return vec![
                    (
                        "fn_value.empty_string",
                        format_compact!("{}::from(\"\")", collection_ctor(resolved)),
                    ),
                    (
                        "fn_value.xyzzy_string",
                        format_compact!("{}::from(\"xyzzy\")", collection_ctor(resolved)),
                    ),
                ];
            }
            let ctor = wrapper_ctor(resolved);

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
            let ctor = collection_ctor(resolved);

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

            if let Some(item) = iterator_item(resolved) {
                values.extend(
                    values_for(item, depth.saturating_sub(1), types)
                        .into_iter()
                        .map(|(_name, text)| ("fn_value.one_element", format_compact!("core::iter::once({text})"))),
                );
            }

            cap(values)
        }
        Kind::Reference => reference_values(resolved, depth, types),

        // Every combination of the elements' values, which is where the product bound earns its
        // keep: three fields with three values each is twenty-seven mutants for one function.
        Kind::Tuple => tuple_values(resolved, depth, types),

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

        'combinations: for existing in &combinations {
            for (_name, text) in &choices {
                if next.len() >= RETURN_WIDTH {
                    break 'combinations;
                }

                let mut combination = existing.clone();

                combination.push(text.clone());
                next.push(combination);
            }
        }

        combinations = next;

        if combinations.is_empty() {
            break;
        }
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
        // slice. This is the narrow standard unsized shape for which leaking the exact boxed
        // referent is a constructor, rather than a generic attempt to invent an arbitrary borrow.
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

/// Source-independent, promotable values for a reference return.
///
/// Arbitrary references are not constructible from their referent type alone: allocation leaks
/// can change mutability, inference, lifetime bounds, and runtime behavior. The only current
/// reference value is the shared empty slice, whose literal promotion supplies the declared
/// lifetime without allocation. Mutable and other shared references contribute no candidate.
pub(super) fn reference_values(ty: &Type, _depth: usize, _types: &Types<'_>) -> Vec<ReplacementValue> {
    let Some(elem) = reference_elem(ty) else {
        return Vec::new();
    };
    match strip(ty) {
        Type::Reference(reference) if reference.mutability.is_none() && matches!(strip(elem), Type::Slice(_)) => {
            vec![("fn_value.empty_collection", "&[]".into())]
        }
        _ => Vec::new(),
    }
}

fn is_unsized_string_wrapper(ty: &Type) -> bool {
    matches!(resolve_type(ty), Kind::Wrapper)
        && type_argument(ty, 0).is_some_and(|inner| matches!(strip(inner), Type::Path(path) if path.path.is_ident("str")))
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
    types
        .payload(ty, index)
        .map_or_else(Vec::new, |inner| values_for(inner, depth.saturating_sub(1), types))
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
    path_text(ty).unwrap_or_else(|| "Default".to_owned())
}

pub(super) fn type_name(ty: &Type) -> Option<&syn::Ident> {
    let Type::Path(path) = strip(ty) else {
        return None;
    };
    path.path.segments.last().map(|segment| &segment.ident)
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
    path_text(ty).unwrap_or_else(|| "Vec".to_owned())
}

/// The constructor for a smart-pointer type.
pub(super) fn wrapper_ctor(ty: &Type) -> String {
    format!("{}::new", collection_ctor(ty))
}

/// The written path of a type, without its generic arguments.
fn path_text(ty: &Type) -> Option<String> {
    let Type::Path(path) = strip(ty) else {
        return None;
    };

    Some(
        path.path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>()
            .join("::"),
    )
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
            attrs: Vec::new(),
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
            defaulted: &[],
            imports,
            defaults,
            aliases: None,
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
    fn aliases_and_abstract_payloads_require_positive_default_evidence() {
        let abstracts = vec![String::from("T")];
        let defaulted = vec![String::from("T")];
        let imports = HashMap::default();
        let defaults = Defaults::default();
        let mut aliases = HashMap::default();
        let _old = aliases.insert("Set".to_owned(), Some(parse_quote!(HashSet<u8, NoDefault>)));
        let types = Types {
            abstracts: &abstracts,
            defaulted: &defaulted,
            imports: &imports,
            defaults: &defaults,
            aliases: Some(&aliases),
            self_type: None,
            self_associated: None,
        };

        assert!(values_for(&parse_quote!(Set), RETURN_DEPTH, &types).is_empty());
        assert!(values_for(&parse_quote!(&str), 0, &types).is_empty());
        assert_eq!(
            texts(values_for(&parse_quote!(Option<T>), RETURN_DEPTH, &types)),
            ["fn_value.none:None", "fn_value.some_default:Some(Default::default())"]
        );
        assert_eq!(
            texts(values_for(&parse_quote!(Result<T, NoDefault>), RETURN_DEPTH, &types)),
            ["fn_value.ok_default:Ok(Default::default())"]
        );
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
        assert_eq!(type_argument(&parse_quote!(Cow<'static, str>), 0), Some(&parse_quote!(str)));
        assert_eq!(inner_values(&bare_collection, 0, RETURN_DEPTH, &types), Vec::new());
        assert_eq!(type_text(&tuple), "Default");
        assert_eq!(type_name(&plain).map(ToString::to_string).as_deref(), Some("String"));
        assert_eq!(type_name(&tuple), None);
        assert_eq!(collection_ctor(&tuple), "Vec");
    }

    #[test]
    fn iterator_item_requires_an_impl_trait_with_item_binding() {
        let plain: Type = parse_quote!(Vec<u8>);
        let imp = Type::ImplTrait(TypeImplTrait {
            attrs: Vec::new(),
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
    fn any_supported_bound_makes_an_impl_trait_an_iterator() {
        let ty: Type = parse_quote!(impl Send + Iterator<Item = u8>);

        assert_eq!(resolve_type(&ty), Kind::Iterator);
    }

    #[test]
    fn resolve_type_treats_an_empty_path_as_unknown() {
        assert_eq!(resolve_type(&empty_path_type()), Kind::Unknown);
    }

    fn texts(values: Vec<ReplacementValue>) -> Vec<String> {
        values.into_iter().map(|(name, text)| format!("{name}:{text}")).collect()
    }

    #[test]
    fn nested_value_generation_has_a_deterministic_oracle() {
        let abstracts = Vec::new();
        let imports = HashMap::default();
        let defaults = Defaults::default();
        let types = test_types(&abstracts, &imports, &defaults);

        assert_eq!(texts(return_values(&ReturnType::Default, &types)), ["fn_value.unit:()"]);
        assert_eq!(
            texts(values_for(&parse_quote!(bool), 0, &types)),
            ["fn_value.default:Default::default()"]
        );
        assert_eq!(
            texts(values_for(&parse_quote!(Option<bool>), RETURN_DEPTH, &types)),
            ["fn_value.none:None", "fn_value.some:Some(true)", "fn_value.some:Some(false)"]
        );
        assert_eq!(
            texts(values_for(&parse_quote!(Result<Option<bool>, String>), RETURN_DEPTH, &types)),
            [
                "fn_value.ok:Ok(None)",
                "fn_value.ok:Ok(Some(true))",
                "fn_value.ok:Ok(Some(false))",
                "fn_value.err_default:Err(Default::default())",
            ]
        );
        assert_eq!(
            texts(values_for(&parse_quote!(Vec<bool>), RETURN_DEPTH, &types)),
            [
                "fn_value.empty_collection:Vec::new()",
                "fn_value.one_element:core::iter::once(true).collect()",
                "fn_value.one_element:core::iter::once(false).collect()",
            ]
        );
        assert_eq!(
            texts(values_for(&parse_quote!(HashMap<bool, u8>), RETURN_DEPTH, &types)),
            [
                "fn_value.empty_collection:HashMap::new()",
                "fn_value.one_element:core::iter::once((true, 0)).collect()",
            ]
        );
        assert_eq!(
            texts(values_for(&parse_quote!(Box<bool>), RETURN_DEPTH, &types)),
            ["fn_value.bool_true:Box::new(true)", "fn_value.bool_false:Box::new(false)"]
        );
        assert_eq!(
            texts(values_for(
                &parse_quote!(impl DoubleEndedIterator<Item = bool>),
                RETURN_DEPTH,
                &types
            )),
            [
                "fn_value.empty_collection:core::iter::empty()",
                "fn_value.one_element:core::iter::once(true)",
                "fn_value.one_element:core::iter::once(false)",
            ]
        );
        assert_eq!(texts(values_for(&parse_quote!(&bool), RETURN_DEPTH, &types)), Vec::<String>::new());
        assert_eq!(
            texts(values_for(&parse_quote!(&mut bool), RETURN_DEPTH, &types)),
            Vec::<String>::new()
        );
    }

    #[test]
    fn tuple_width_and_type_helpers_have_exact_boundaries() {
        let abstracts = Vec::new();
        let imports = HashMap::default();
        let defaults = Defaults::default();
        let types = test_types(&abstracts, &imports, &defaults);
        let tuple: Type = parse_quote!((bool, bool, bool, bool));
        let values = tuple_values(&tuple, RETURN_DEPTH, &types);

        assert_eq!(values.len(), RETURN_WIDTH);
        assert_eq!(values.first().map(|(_, text)| text.as_str()), Some("(true, true, true, true)"));
        assert_eq!(values.last().map(|(_, text)| text.as_str()), Some("(true, false, false, false)"));
        assert_eq!(cap(vec![("x", "x".into()); RETURN_WIDTH + 1]).len(), RETURN_WIDTH);

        let qualified: Type = parse_quote!(std::collections::VecDeque<bool>);
        assert_eq!(type_text(&qualified), "std::collections::VecDeque");
        assert_eq!(collection_ctor(&qualified), "std::collections::VecDeque");
        assert_eq!(wrapper_ctor(&parse_quote!(std::sync::Arc<bool>)), "std::sync::Arc::new");
        let cow: PathSegment = parse_quote!(Cow<'static, str>);
        let array: PathSegment = parse_quote!(Array<u8, 4>);
        assert_eq!(type_arguments(&cow), 1);
        assert_eq!(type_arguments(&array), 1);
    }

    #[test]
    fn tuple_elements_receive_one_less_level_of_return_depth() {
        let abstracts = Vec::new();
        let imports = HashMap::default();
        let defaults = Defaults::default();
        let types = test_types(&abstracts, &imports, &defaults);
        let tuple: Type = parse_quote!((Option<Option<bool>>,));

        assert_eq!(
            texts(tuple_values(&tuple, 2, &types)),
            ["fn_value.tuple:(None,)", "fn_value.tuple:(Some(Default::default()),)"]
        );
    }

    #[test]
    fn an_option_with_an_unconstructable_reference_payload_offers_only_none() {
        let abstracts = vec![String::from("T")];
        let imports = HashMap::default();
        let defaults = Defaults::default();
        let types = test_types(&abstracts, &imports, &defaults);

        assert_eq!(
            texts(values_for(&parse_quote!(Option<&T>), RETURN_DEPTH, &types)),
            ["fn_value.none:None"]
        );
    }

    #[test]
    fn type_classification_checks_names_arity_and_iterator_bounds() {
        let cases = [
            (parse_quote!(()), Kind::Unit),
            (parse_quote!(&str), Kind::StaticStr),
            (parse_quote!(&mut str), Kind::MutStr),
            (parse_quote!(NonZeroU8), Kind::NonZero),
            (parse_quote!(NonZero<u8>), Kind::Unknown),
            (parse_quote!(Vec<bool>), Kind::Collection),
            (parse_quote!(Vec), Kind::Unknown),
            (parse_quote!(HashMap<bool, u8>), Kind::Map),
            (parse_quote!(HashMap<bool>), Kind::Unknown),
            (parse_quote!(impl FusedIterator<Item = bool>), Kind::Iterator),
            (parse_quote!(impl Display), Kind::Unknown),
        ];

        for (ty, expected) in cases {
            assert_eq!(resolve_type(&ty), expected, "{ty:?}");
        }
    }
}
