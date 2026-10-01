// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! What the file's own declarations say about the types a function mentions.

use std::borrow::Cow;

use syn::{GenericArgument, GenericParam, Generics, PathArguments, ReturnType, Type};

use super::super::defaults::{DefaultPaths, standard_defaulted_parameters};
use super::predicates::payload;
use super::values::{Kind, resolve_type, strip, type_name};
use crate::ops::collect::Defaults;
use crate::{HashMap, HashSet};

#[derive(Clone, PartialEq)]
pub(super) struct Alias {
    pub(super) parameters: Vec<String>,
    pub(super) target: Type,
}

fn substitute_type(ty: &Type, substitutions: &HashMap<String, Type>) -> Type {
    if let Type::Path(path) = ty
        && path.qself.is_none()
        && let Some(ident) = path.path.get_ident()
        && let Some(replacement) = substitutions.get(&ident.to_string())
    {
        return replacement.clone();
    }

    let mut substituted = ty.clone();
    match &mut substituted {
        Type::Array(array) => *array.elem = substitute_type(&array.elem, substitutions),
        Type::Group(group) => *group.elem = substitute_type(&group.elem, substitutions),
        Type::Paren(paren) => *paren.elem = substitute_type(&paren.elem, substitutions),
        Type::Path(path) => {
            for segment in &mut path.path.segments {
                if let PathArguments::AngleBracketed(arguments) = &mut segment.arguments {
                    for argument in &mut arguments.args {
                        if let GenericArgument::Type(inner) = argument {
                            *inner = substitute_type(inner, substitutions);
                        }
                    }
                }
            }
        }
        Type::Ptr(pointer) => *pointer.elem = substitute_type(&pointer.elem, substitutions),
        Type::Reference(reference) => *reference.elem = substitute_type(&reference.elem, substitutions),
        Type::Slice(slice) => *slice.elem = substitute_type(&slice.elem, substitutions),
        Type::Tuple(tuple) => {
            for element in &mut tuple.elems {
                *element = substitute_type(element, substitutions);
            }
        }
        _ => {}
    }
    substituted
}

/// What the value-choosing functions know about the file they are reasoning inside.
///
/// The two facts travel together through the whole recursion and neither is ever used without the
/// other, so they are carried as one thing rather than as a growing parameter list.
pub(super) struct Types<'a> {
    /// Type names in scope that cannot be constructed: type parameters without a `Default` bound,
    /// associated types, and trait objects.
    pub(super) abstracts: &'a [String],

    /// Type parameters carrying an applicable standard `Default` bound.
    pub(super) defaulted: &'a [String],

    /// The module path each imported name came from, so a bare type name can be traced to its crate.
    pub(super) imports: &'a HashMap<String, Option<Vec<String>>>,

    /// What the workspace's own sources say about which of their types implement `Default`.
    pub(super) defaults: &'a Defaults,

    /// Locally visible aliases, used to preserve their declared shape.
    pub(super) aliases: Option<&'a HashMap<String, Option<Alias>>>,

    /// The concrete type named by `Self` inside an `impl` block.
    pub(super) self_type: Option<&'a Type>,

    /// Concrete associated types declared by the enclosing `impl`.
    pub(super) self_associated: Option<&'a HashMap<String, Type>>,
}

impl Types<'_> {
    fn instantiate_alias(&self, ty: &Type) -> Option<Type> {
        let aliases = self.aliases?;
        let mut resolved = ty.clone();
        let mut visited = HashSet::default();
        let mut changed = false;

        loop {
            let Type::Path(path) = strip(&resolved) else {
                return changed.then_some(resolved);
            };
            if path.qself.is_some() || path.path.segments.len() != 1 {
                return changed.then_some(resolved);
            }

            let name = path.path.segments[0].ident.to_string();
            if !visited.insert(name.clone()) {
                return None;
            }
            let Some(alias) = aliases.get(&name).and_then(Option::as_ref) else {
                return changed.then_some(resolved);
            };
            let arguments = super::values::type_argument_values(&resolved);
            let substitutions = alias
                .parameters
                .iter()
                .zip(arguments)
                .map(|(parameter, argument)| (parameter.clone(), argument.clone()))
                .collect();
            resolved = substitute_type(&alias.target, &substitutions);
            changed = true;
        }
    }

    pub(super) fn resolve_alias<'a>(&'a self, ty: &'a Type) -> &'a Type {
        let Some(aliases) = self.aliases else {
            return ty;
        };
        let mut resolved = ty;
        let mut visited = HashSet::default();

        loop {
            let Type::Path(path) = strip(resolved) else {
                return resolved;
            };
            let Some(name) = (path.path.segments.len() == 1).then(|| path.path.segments[0].ident.to_string()) else {
                return resolved;
            };
            if !visited.insert(name.clone()) {
                return ty;
            }
            let Some(target) = aliases.get(&name).and_then(Option::as_ref) else {
                return resolved;
            };
            resolved = &target.target;
        }
    }

    pub(super) fn payload<'a>(&'a self, ty: &'a Type, index: usize) -> Option<Cow<'a, Type>> {
        if let Some(resolved) = self.instantiate_alias(ty) {
            return payload(&resolved, index).map(|payload| Cow::Owned(payload.clone()));
        }

        let resolved = self.resolve_alias(ty);
        let payload = payload(resolved, index)?;
        Some(Cow::Borrowed(payload))
    }

    /// Returns positive source-visible evidence that a value of `ty` can be defaulted.
    pub(super) fn has_default(&self, ty: &Type) -> bool {
        let ty = self.resolve_alias(ty);
        let concrete = self
            .concrete_self_type(ty)
            .or_else(|| self.concrete_self_associated_type(ty))
            .unwrap_or(ty);

        match resolve_type(concrete) {
            Kind::Unit | Kind::Bool | Kind::Signed | Kind::Unsigned | Kind::Float | Kind::String | Kind::Option => true,
            Kind::Collection => {
                if self
                    .defaults
                    .defines(&type_name(concrete).map(ToString::to_string).unwrap_or_default())
                {
                    return self.local_generic_default(concrete);
                }

                let name = type_name(concrete).map(ToString::to_string).unwrap_or_default();
                if name == "HashSet" {
                    payload(concrete, 1).is_none_or(|hasher| self.has_default(hasher))
                } else {
                    true
                }
            }
            Kind::Map => {
                if self
                    .defaults
                    .defines(&type_name(concrete).map(ToString::to_string).unwrap_or_default())
                {
                    return self.local_generic_default(concrete);
                }
                let name = type_name(concrete).map(ToString::to_string).unwrap_or_default();
                if name == "HashMap" {
                    payload(concrete, 2).is_none_or(|hasher| self.has_default(hasher))
                } else {
                    true
                }
            }

            Kind::Wrapper => payload(concrete, 0).is_some_and(|inner| self.has_default(inner)),
            Kind::Tuple => {
                matches!(strip(concrete), Type::Tuple(tuple) if tuple.elems.iter().all(|element| self.has_default(element)))
            }
            Kind::Unknown => {
                let Type::Path(path) = strip(concrete) else {
                    return false;
                };
                if path.path.is_ident("str") {
                    return false;
                }
                let segments = &path.path.segments;
                let standard_fmt_error = segments.len() == 3
                    && segments
                        .first()
                        .is_some_and(|segment| segment.ident == "std" || segment.ident == "core")
                    && segments.iter().nth(1).is_some_and(|segment| segment.ident == "fmt")
                    && segments.last().is_some_and(|segment| segment.ident == "Error");
                let relative_fmt_error = segments.len() == 2
                    && segments.first().is_some_and(|segment| segment.ident == "fmt")
                    && segments.last().is_some_and(|segment| segment.ident == "Error");
                if standard_fmt_error || relative_fmt_error {
                    return true;
                }
                if path.path.is_ident("Error")
                    && self.imports.get("Error").and_then(Option::as_ref).is_some_and(
                        |prefix| matches!(prefix.as_slice(), [root, module] if matches!(root.as_str(), "std" | "core") && module == "fmt"),
                    )
                {
                    return true;
                }
                if path.path.segments.last().is_some_and(|segment| segment.ident == "RandomState") {
                    return true;
                }
                path.path
                    .get_ident()
                    .is_some_and(|ident| self.defaulted.contains(&ident.to_string()))
                    || self.defaults.has_default(concrete)
            }
            Kind::Result | Kind::StaticStr | Kind::MutStr | Kind::NonZero | Kind::Cow | Kind::Iterator | Kind::Reference => false,
        }
    }

    /// Whether a collection-shaped local type must use the trait constructor.
    ///
    /// Standard collections have stable inherent `new` constructors. A local type may deliberately
    /// reuse one of their names without providing that constructor; positive `Default` evidence is
    /// then the only constructor syntax the source proves.
    pub(super) fn prefers_default_constructor(&self, ty: &Type) -> bool {
        let ty = self.resolve_alias(ty);
        let concrete = self
            .concrete_self_type(ty)
            .or_else(|| self.concrete_self_associated_type(ty))
            .unwrap_or(ty);
        let name = type_name(concrete).map(ToString::to_string).unwrap_or_default();

        self.defaults.defines(&name) && self.local_generic_default(concrete)
    }

    pub(super) fn is_local_type(&self, ty: &Type) -> bool {
        let ty = self.resolve_alias(ty);
        let concrete = self
            .concrete_self_type(ty)
            .or_else(|| self.concrete_self_associated_type(ty))
            .unwrap_or(ty);
        type_name(concrete).is_some_and(|name| self.defaults.defines(&name.to_string()))
    }

    fn local_generic_default(&self, ty: &Type) -> bool {
        self.defaults.declares_default(ty)
            && matches!(
                strip(ty),
                Type::Path(path)
                    if path.path.segments.last().is_some_and(|segment| match &segment.arguments {
                        syn::PathArguments::None => true,
                        syn::PathArguments::AngleBracketed(arguments) => arguments.args.iter().all(|argument| {
                            !matches!(argument, syn::GenericArgument::Type(argument) if !self.has_default(argument))
                        }),
                        syn::PathArguments::Parenthesized(_) => false,
                    })
            )
    }

    /// Returns whether a type has no `Default` to reach for.
    ///
    /// Two independent readings say so, and either is enough: an error type from another crate,
    /// which effectively never has one, or a type this workspace defines and gives none.
    pub(super) fn lacks_default(&self, ty: &Type) -> bool {
        let concrete = self
            .concrete_self_type(ty)
            .or_else(|| self.concrete_self_associated_type(ty))
            .unwrap_or(ty);
        let concrete = self.resolve_alias(concrete);

        matches!(strip(concrete), Type::Path(path) if path.path.is_ident("str"))
            || is_foreign_error(concrete, self.imports)
            || Self::is_standard_time_without_default(concrete, self.imports)
            || self.defaults.lacks_default(concrete)
    }

    fn is_standard_time_without_default(ty: &Type, imports: &HashMap<String, Option<Vec<String>>>) -> bool {
        let Type::Path(path) = strip(ty) else {
            return false;
        };
        let Some(last) = path.path.segments.last() else {
            return false;
        };
        let name = last.ident.to_string();

        if !matches!(name.as_str(), "Instant" | "SystemTime") {
            return false;
        }

        let prefix = if path.path.segments.len() > 1 {
            path.path
                .segments
                .iter()
                .take(path.path.segments.len() - 1)
                .map(|segment| segment.ident.to_string())
                .collect::<Vec<_>>()
        } else {
            let Some(Some(prefix)) = imports.get(&name) else {
                return false;
            };
            prefix.clone()
        };

        matches!(prefix.as_slice(), [root, module] if root == "std" && module == "time")
    }

    /// Returns whether this is the standard library's zero-argument `fmt::Result` alias.
    pub(super) fn is_fmt_result(&self, ty: &Type) -> bool {
        let Type::Path(path) = strip(ty) else {
            return false;
        };
        let segments = path
            .path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>();

        match segments.as_slice() {
            [root, module, result] => matches!(root.as_str(), "std" | "core") && module == "fmt" && result == "Result",
            [module, result] if module == "fmt" && result == "Result" => self
                .imports
                .get(module)
                .and_then(Option::as_ref)
                .is_some_and(|path| matches!(path.as_slice(), [root] if matches!(root.as_str(), "std" | "core"))),
            _ => false,
        }
    }

    pub(super) fn concrete_self_type_or_associated<'a>(&'a self, ty: &Type) -> Option<&'a Type> {
        self.concrete_self_type(ty).or_else(|| self.concrete_self_associated_type(ty))
    }

    fn concrete_self_type<'a>(&'a self, ty: &Type) -> Option<&'a Type> {
        let Type::Path(path) = strip(ty) else {
            return None;
        };

        (path.qself.is_none() && path.path.is_ident("Self"))
            .then_some(self.self_type)
            .flatten()
    }

    fn concrete_self_associated_type<'a>(&'a self, ty: &Type) -> Option<&'a Type> {
        let Type::Path(path) = strip(ty) else {
            return None;
        };
        let mut segments = path.path.segments.iter();
        let (Some(root), Some(name), None) = (segments.next(), segments.next(), segments.next()) else {
            return None;
        };

        (path.qself.is_none() && root.ident == "Self")
            .then(|| self.self_associated?.get(&name.ident.to_string()))
            .flatten()
    }
}

/// The one type outside this workspace whose name ends in `Error` and that does implement `Default`.
///
/// `core::fmt::Error` is a unit struct standing for "formatting failed", and it derives everything.
/// Every other error type in `std` that was checked does not implement `Default`, so it is cheaper
/// to name the exception than to enumerate the rule.
pub(super) const DEFAULTABLE_ERROR: &str = "fmt";

/// Returns whether a type is an error type from outside this workspace, which will have no `Default`.
///
/// Error types are the largest single source of mutants that cannot compile: `Err(Default::default())`
/// wants an error value, and `std::io::Error`, `anyhow::Error`, `serde_json::Error` and their kind
/// have no `Default` and are not going to acquire one. Withholding the mutant there costs no signal,
/// because there was never a mutant to lose.
///
/// The rule is deliberately confined to types from *other* crates. A workspace error type may well
/// be an enum with a `#[default]` variant, and the collector cannot see the definition to find out,
/// so `crate::`, `self::`, `super::` and any name this file does not import stay optimistic.
pub(super) fn is_foreign_error(ty: &Type, imports: &HashMap<String, Option<Vec<String>>>) -> bool {
    let Type::Path(path) = strip(ty) else {
        return false;
    };

    let Some(last) = path.path.segments.last() else {
        return false;
    };

    if !last.ident.to_string().ends_with("Error") {
        return false;
    }

    // Written out in full, so where it comes from is right there. A single-segment path is a bare
    // name instead, and only the file's imports can say what it was.
    let owned;
    let prefix: &[String] = if path.path.segments.len() > 1 {
        owned = path
            .path
            .segments
            .iter()
            .rev()
            .skip(1)
            .rev()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>();
        &owned
    } else {
        match imports.get(&last.ident.to_string()) {
            // A name two `use` items disagree about is as unknown as one never imported, so it
            // takes the same answer rather than the last writer's.
            Some(Some(path)) => path,
            Some(None) | None => return false,
        }
    };

    let (Some(root), Some(qualifier)) = (prefix.first(), prefix.last()) else {
        return false;
    };

    if matches!(root.as_str(), "crate" | "self" | "super") {
        return false;
    }

    qualifier != DEFAULTABLE_ERROR
}

/// Returns whether a signature's `Result` fixes its error type to one with no `Default`.
///
/// `Ok(v)` becoming `Err(Default::default())` needs a value of the error type, and the call site
/// does not name it — the signature does. When the error type is written out, it is read straight
/// off the return type. When the signature uses a crate-wide `type Result<T>` alias instead, which
/// is close to universal in real Rust, the alias is resolved through the workspace index.
pub(super) fn returns_undefaultable_error(output: &ReturnType, types: &Types<'_>) -> bool {
    let ReturnType::Type(_arrow, ty) = output else {
        return false;
    };

    let Type::Path(path) = strip(ty) else {
        return false;
    };
    if resolve_type(ty) != Kind::Result {
        return false;
    }

    if let Some(error) = payload(ty, 1) {
        return types.lacks_default(error);
    }

    // No second argument, so the error type is whatever an alias fixed it to. The alias is named by
    // the return type's last segment, and the index answers for the name it resolved to.
    let alias = path
        .path
        .segments
        .last()
        .expect("a type resolved as Result must have a final path segment");

    types
        .defaults
        .aliased_error(&alias.ident.to_string())
        .is_some_and(|error| types.defaults.lacks_error_default(error))
}

/// Returns whether a type is one no concrete `Default` can be assumed for.
///
/// `Default::default()` is the fallback for an unknown concrete type, but a caller's type
/// parameter, an associated type projected from one, and a trait object or `impl Trait` name no
/// constructible type unless the signature supplies a `Default` bound.
///
/// `Self::Value` is deliberately excluded. Inside an `impl` block it resolves to the type chosen
/// by that block and may implement `Default`.
pub(super) fn is_abstract_type(ty: &Type, abstracts: &[String]) -> bool {
    match ty {
        Type::TraitObject(_) | Type::ImplTrait(_) => true,

        Type::Paren(paren) => is_abstract_type(&paren.elem, abstracts),

        Type::Path(path) if path.qself.is_none() => {
            let Some(last) = path.path.segments.last() else {
                return false;
            };

            if path.path.segments.len() > 1 {
                let root = &path.path.segments[0].ident;
                return root == "Self" || abstracts.contains(&root.to_string());
            }

            // `Box<dyn Reader>` is exactly as unconstructable as the `dyn Reader` inside it.
            if last.ident == "Box" {
                return payload(ty, 0).is_some_and(|inner| is_abstract_type(inner, abstracts));
            }

            abstracts.contains(&last.ident.to_string())
        }

        _ => false,
    }
}

/// Names every type parameter a generics list declares without a `Default` bound.
///
/// A parameter written `T: Default` is excluded, because there the promise this is looking for was
/// made explicitly and the mutant it would otherwise withhold compiles.
pub(super) fn undefaulted_parameters(generics: &Generics, defaults: &DefaultPaths) -> Vec<String> {
    let defaulted = standard_defaulted_parameters(generics, defaults);

    generics
        .params
        .iter()
        .filter_map(|param| match param {
            GenericParam::Type(ty) => (!defaulted.iter().any(|name| name == &ty.ident.to_string())).then(|| ty.ident.to_string()),

            _ => None,
        })
        .collect()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use syn::punctuated::Punctuated;
    use syn::{Path, PathSegment, TypePath, parse_quote};

    use super::*;

    fn defaults(source: &str) -> Defaults {
        Defaults::of(&syn::parse_file(source).expect("test source should parse"))
    }

    fn empty_path_type() -> Type {
        Type::Path(TypePath {
            attrs: Vec::new(),
            qself: None,
            path: Path {
                leading_colon: None,
                segments: Punctuated::<PathSegment, syn::token::PathSep>::new(),
            },
        })
    }

    #[test]
    fn foreign_error_detection_rejects_empty_paths_and_empty_import_prefixes() {
        let mut imports = HashMap::default();
        let imported_error: Type = parse_quote!(TheirError);

        let _old = imports.insert("TheirError".to_owned(), Some(Vec::new()));

        assert!(!is_foreign_error(&empty_path_type(), &HashMap::default()));
        assert!(!is_foreign_error(&imported_error, &imports));
        assert!(is_foreign_error(&parse_quote!(std::num::ParseIntError), &HashMap::default()));
    }

    #[test]
    fn standard_time_detection_handles_unknown_and_imported_names() {
        let defaults = Defaults::default();
        let abstracts = Vec::new();
        let defaulted = Vec::new();
        let aliases = HashMap::default();
        let mut imports = HashMap::default();
        let _old = imports.insert("Instant".to_owned(), Some(vec!["std".to_owned(), "time".to_owned()]));
        let types = Types {
            abstracts: &abstracts,
            defaulted: &defaulted,
            imports: &imports,
            defaults: &defaults,
            aliases: Some(&aliases),
            self_type: None,
            self_associated: None,
        };

        assert!(!types.lacks_default(&empty_path_type()));
        assert!(types.lacks_default(&parse_quote!(Instant)));
        assert!(types.lacks_default(&parse_quote!(std::time::SystemTime)));
        assert!(!types.lacks_default(&parse_quote!(SystemTime)));
        assert!(!types.lacks_default(&parse_quote!(&'static str)));
    }

    #[test]
    fn positive_default_evidence_covers_containers_wrappers_and_tuples() {
        let defaults = Defaults::default();
        let abstracts = Vec::new();
        let defaulted = Vec::new();
        let aliases = HashMap::default();
        let imports = HashMap::default();
        let types = Types {
            abstracts: &abstracts,
            defaulted: &defaulted,
            imports: &imports,
            defaults: &defaults,
            aliases: Some(&aliases),
            self_type: None,
            self_associated: None,
        };

        for ty in [
            parse_quote!(std::collections::BTreeMap<u8, u8>),
            parse_quote!(Box<bool>),
            parse_quote!((bool, String)),
        ] {
            assert!(types.has_default(&ty), "{ty:?}");
        }
    }

    #[test]
    fn cyclic_file_wide_aliases_stop_at_the_original_type() {
        let defaults = Defaults::default();
        let abstracts = Vec::new();
        let defaulted = Vec::new();
        let imports = HashMap::default();
        let aliases = HashMap::from_iter([
            (
                "A".to_owned(),
                Some(Alias {
                    parameters: Vec::new(),
                    target: parse_quote!(B),
                }),
            ),
            (
                "B".to_owned(),
                Some(Alias {
                    parameters: Vec::new(),
                    target: parse_quote!(A),
                }),
            ),
        ]);
        let types = Types {
            abstracts: &abstracts,
            defaulted: &defaulted,
            imports: &imports,
            defaults: &defaults,
            aliases: Some(&aliases),
            self_type: None,
            self_associated: None,
        };
        let original: Type = parse_quote!(A);

        assert_eq!(types.resolve_alias(&original), &original);
    }

    #[test]
    fn chained_generic_aliases_substitute_each_declared_parameter_role() {
        let defaults = Defaults::default();
        let abstracts = Vec::new();
        let defaulted = Vec::new();
        let imports = HashMap::default();
        let aliases = HashMap::from_iter([
            (
                "Outer".to_owned(),
                Some(Alias {
                    parameters: vec!["T".to_owned()],
                    target: parse_quote!(Inner<Option<T>>),
                }),
            ),
            (
                "Inner".to_owned(),
                Some(Alias {
                    parameters: vec!["U".to_owned()],
                    target: parse_quote!(Result<U, Error>),
                }),
            ),
        ]);
        let types = Types {
            abstracts: &abstracts,
            defaulted: &defaulted,
            imports: &imports,
            defaults: &defaults,
            aliases: Some(&aliases),
            self_type: None,
            self_associated: None,
        };

        assert_eq!(
            types.payload(&parse_quote!(Outer<bool>), 0).as_deref(),
            Some(&parse_quote!(Option<bool>))
        );
        assert_eq!(types.payload(&parse_quote!(Outer<bool>), 1).as_deref(), Some(&parse_quote!(Error)));
    }

    #[test]
    fn generic_substitution_descends_through_every_supported_type_container() {
        let substitutions = HashMap::from_iter([("T".to_owned(), parse_quote!(bool))]);

        for (input, expected) in [
            (parse_quote!([T; 1]), parse_quote!([bool; 1])),
            (parse_quote!((T)), parse_quote!((bool))),
            (parse_quote!(*const T), parse_quote!(*const bool)),
            (parse_quote!(&T), parse_quote!(&bool)),
            (parse_quote!([T]), parse_quote!([bool])),
            (parse_quote!((T, Option<T>)), parse_quote!((bool, Option<bool>))),
        ] {
            assert_eq!(substitute_type(&input, &substitutions), expected);
        }
    }

    #[test]
    fn result_aliases_and_direct_errors_can_both_be_screened() {
        let defaults = defaults(
            "
            struct Error;
            type Result<T> = core::result::Result<T, Error>;
            ",
        );
        let abstracts = Vec::new();
        let defaulted = Vec::new();
        let aliases = HashMap::default();
        let imports = HashMap::default();
        let types = Types {
            abstracts: &abstracts,
            defaulted: &defaulted,
            imports: &imports,
            defaults: &defaults,
            aliases: Some(&aliases),
            self_type: None,
            self_associated: None,
        };
        let alias_output: ReturnType = parse_quote!(-> Result<bool>);
        let direct: ReturnType = parse_quote!(-> Result<bool, std::io::Error>);

        assert!(returns_undefaultable_error(&alias_output, &types));
        assert!(returns_undefaultable_error(&direct, &types));
    }

    #[test]
    fn abstract_type_detection_handles_empty_paths_and_boxed_trait_objects() {
        assert!(!is_abstract_type(&empty_path_type(), &[]));
        assert!(is_abstract_type(&parse_quote!(Box<dyn core::fmt::Debug>), &[]));
        assert!(!is_abstract_type(
            &parse_quote!(<Concrete as Abstract>::Item),
            &[String::from("Abstract")]
        ));
    }
}
