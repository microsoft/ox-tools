// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Working out which module files the selected configuration does not compile as production code.
//!
//! The collector drops a `#[cfg(test)]` item wherever it sees one, which handles the usual
//! `#[cfg(test)] mod tests { … }` written inline. It cannot handle the other spelling:
//!
//! ```ignore
//! #[cfg(test)]
//! #[path = "reader_tests.rs"]
//! mod tests;
//! ```
//!
//! The attribute is on the declaration, and the code is in another file. Files are parsed one at a
//! time and independently, so nothing in `reader_tests.rs` says it is test code — and it was
//! therefore mutated, producing a population of mutants inside assertions, which no test can
//! meaningfully catch.
//!
//! Rather than look at one file, this walks the module tree from the crate root and records how
//! each file is reached. A file reached only through a test-only or inactive configuration
//! declaration cannot contribute production mutants, no matter what it contains.

use camino::{Utf8Path, Utf8PathBuf};
use syn::{Attribute, Item, Meta};

use crate::cfg::{CfgSet, test_gated_for};
use crate::{HashMap, HashSet};

/// One `mod name;` declaration that pulls in another file.
#[derive(Debug, Clone)]
pub(super) struct Declaration {
    /// The file it resolves to, absolute.
    pub(super) target: Utf8PathBuf,

    /// Whether the selected configuration, or an enclosing module, excludes this declaration.
    ///
    /// This includes test scaffolding and a `cfg` condition that does not hold. Both kinds of file
    /// are walked to carry the exclusion to their children, but neither can contribute mutants
    /// unless another live declaration reaches it.
    pub(super) excluded: bool,
}

/// Finds the file declarations a parsed file makes.
///
/// `path` is the file the declarations were written in, absolute, since a `mod` resolves relative
/// to where it appears.
#[must_use]
pub(super) fn declarations(path: &Utf8Path, ast: &syn::File, cfg: &CfgSet) -> Vec<Declaration> {
    let mut found = Vec::new();
    let (Some(directory), Some(beside)) = (owned_directory(path), path.parent()) else {
        return found;
    };

    walk(&ast.items, &directory, beside, cfg, false, &mut found);

    found
}

/// The directory a file's `mod` declarations resolve against.
///
/// `lib.rs`, `main.rs` and `mod.rs` own the directory they sit in; any other file owns a
/// subdirectory named after it. This is the rule the compiler applies, and getting it wrong would
/// silently resolve a declaration to nothing.
fn owned_directory(path: &Utf8Path) -> Option<Utf8PathBuf> {
    let parent = path.parent()?;
    let stem = path.file_stem()?;

    if matches!(stem, "lib" | "main" | "mod") {
        return Some(parent.to_owned());
    }

    Some(parent.join(stem))
}

/// Walks items, following inline modules so that a declaration nested in one resolves correctly.
///
/// `directory` is what a plain `mod name;` resolves against. `base` is what a `#[path = "…"]`
/// resolves against, which is not the same place: at the top level of a file, a `#[path]` is
/// relative to the directory that file sits in, while `mod name;` looks in the directory that file
/// owns. For `de/reader_impl_tests.rs` those are `de/` and `de/reader_impl_tests/` respectively,
/// so reading `#[path = "reader_tests.rs"]` against the wrong one finds nothing at all.
fn walk(items: &[Item], directory: &Utf8Path, base: &Utf8Path, cfg: &CfgSet, inherited_exclusion: bool, found: &mut Vec<Declaration>) {
    for item in items {
        let Item::Mod(module) = item else { continue };
        let excluded = inherited_exclusion || test_gated_for(cfg, &module.attrs) || !cfg.holds_for(&module.attrs);

        // An inline module is a directory rather than a file: `mod outer { mod inner; }` puts
        // `inner` under `outer/`, and its own test gating is inherited by everything below it.
        if let Some((_brace, items)) = module.content.as_ref() {
            let nested =
                path_attribute(&module.attrs).map_or_else(|| directory.join(module.ident.to_string()), |relative| base.join(relative));

            // Inside an inline module both rules point at the same place, so from here down a
            // `#[path]` and a plain declaration resolve against the module's own directory.
            walk(items, &nested, &nested, cfg, excluded, found);
            continue;
        }

        let name = module.ident.to_string();
        let candidates = path_attribute(&module.attrs).map_or_else(
            || vec![directory.join(format!("{name}.rs")), directory.join(&name).join("mod.rs")],
            |relative| vec![base.join(relative)],
        );

        for target in candidates {
            if target.as_std_path().is_file() {
                found.push(Declaration { target, excluded });
                break;
            }
        }
    }
}

/// Reads the path a `#[path = "…"]` attribute points at.
fn path_attribute(attrs: &[Attribute]) -> Option<String> {
    attrs.iter().find_map(|attr| {
        let Meta::NameValue(pair) = &attr.meta else { return None };

        if !pair.path.is_ident("path") {
            return None;
        }

        let syn::Expr::Lit(literal) = &pair.value else { return None };
        let syn::Lit::Str(text) = &literal.lit else { return None };

        Some(text.value())
    })
}

/// Works out which files have no live module path under the selected configuration.
///
/// `roots` are the crate roots — a package's lib and bin entry points — which are never test code.
/// A file reachable from one of them without passing through an excluded declaration is real code,
/// whatever else also points at it; files reached only through excluded declarations are absent
/// from the selected build or exist only for tests.
///
/// A file nothing points at is left alone rather than assumed to be either. It may be pulled in by
/// `include!`, or by a `mod` behind a `cfg` this cannot evaluate, and dropping mutants on a guess
/// would quietly shrink the population.
#[must_use]
pub(super) fn excluded_files(roots: &[Utf8PathBuf], declared: &[(Utf8PathBuf, Vec<Declaration>)]) -> HashSet<Utf8PathBuf> {
    let edges: HashMap<&Utf8Path, &[Declaration]> = declared.iter().map(|(from, list)| (from.as_path(), list.as_slice())).collect();

    let mut live: HashSet<Utf8PathBuf> = HashSet::default();
    let mut queue: Vec<&Utf8Path> = roots.iter().map(Utf8PathBuf::as_path).collect();

    while let Some(file) = queue.pop() {
        if !live.insert(file.to_owned()) {
            continue;
        }

        for declaration in edges.get(file).copied().unwrap_or(&[]) {
            if !declaration.excluded {
                queue.push(declaration.target.as_path());
            }
        }
    }

    // Everything below an excluded module is absent too, so the exclusion follows every edge from
    // there rather than stopping at the file the declaration named.
    let mut excluded: HashSet<Utf8PathBuf> = HashSet::default();
    let mut queue: Vec<&Utf8Path> = declared
        .iter()
        .flat_map(|(_from, list)| list)
        .filter(|declaration| declaration.excluded)
        .map(|declaration| declaration.target.as_path())
        .collect();

    while let Some(file) = queue.pop() {
        if live.contains(file) || !excluded.insert(file.to_owned()) {
            continue;
        }

        for declaration in edges.get(file).copied().unwrap_or(&[]) {
            queue.push(declaration.target.as_path());
        }
    }

    excluded
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;

    fn parse(text: &str) -> syn::File {
        syn::parse_file(text).unwrap()
    }

    fn test_gated(attrs: &[Attribute]) -> bool {
        test_gated_for(&CfgSet::default(), attrs)
    }

    #[test]
    fn a_file_stem_that_is_not_a_module_root_owns_a_subdirectory() {
        assert_eq!(owned_directory(Utf8Path::new("/a/src/lib.rs")), Some(Utf8PathBuf::from("/a/src")));
        assert_eq!(owned_directory(Utf8Path::new("/a/src/mod.rs")), Some(Utf8PathBuf::from("/a/src")));
        assert_eq!(owned_directory(Utf8Path::new("/a/src/de.rs")), Some(Utf8PathBuf::from("/a/src/de")));
    }

    #[test]
    fn a_cfg_test_declaration_is_recognised() {
        // The `fn` is here so that the filter below has a non-module item to reject, which is the
        // shape every real source file has.
        let ast = parse("fn not_a_module() {}\n#[cfg(test)]\nmod tests;\nmod real;");
        let gated: Vec<bool> = ast
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Mod(module) => Some(test_gated(&module.attrs)),
                _ => None,
            })
            .collect();

        assert_eq!(gated, vec![true, false]);
    }

    #[test]
    fn a_compound_gate_is_read_all_the_way_down() {
        // The parser this replaced looked one level deep, so `all(test, unix)` read as a plain
        // `unix` gate and the module below it was surveyed as production code.
        let ast = parse(concat!(
            "#[cfg(all(test, unix))]\nmod a;\n",
            "#[cfg(not(feature = \"x\"))]\n#[cfg(test)]\nmod b;\n",
            "#[cfg(all(unix, all(test, feature = \"x\")))]\nmod c;\n",
        ));
        let gated: Vec<bool> = ast
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Mod(module) => Some(test_gated(&module.attrs)),
                _ => None,
            })
            .collect();

        assert_eq!(gated, vec![true, true, true]);
    }

    #[test]
    fn a_gate_a_production_build_can_also_satisfy_is_not_test_only() {
        // `any(test, …)` holds whenever the other arm does, so the module is compiled into the
        // library the run measures. Treating it as test code would drop every mutant in it.
        let ast = parse(concat!(
            "#[cfg(any(test, feature = \"runtime\"))]\nmod a;\n",
            "#[cfg(not(test))]\nmod b;\n",
            "#[cfg(any(all(test, unix), windows))]\nmod c;\n",
        ));
        let gated: Vec<bool> = ast
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Mod(module) => Some(test_gated(&module.attrs)),
                _ => None,
            })
            .collect();

        assert_eq!(gated, vec![false, false, false]);
    }

    #[test]
    fn a_path_attribute_is_read() {
        let ast = parse("#[path = \"reader_tests.rs\"]\nmod tests;");
        let Item::Mod(module) = &ast.items[0] else {
            panic!("expected a module")
        };

        assert_eq!(path_attribute(&module.attrs), Some("reader_tests.rs".to_owned()));
    }

    #[test]
    fn a_path_attribute_resolves_beside_the_file_that_wrote_it() {
        // The rule that this got wrong first time. `mod name;` looks in the directory the file
        // owns, but `#[path]` looks in the directory the file sits in, and the two differ for
        // every file that is not `lib.rs`, `main.rs` or `mod.rs`.
        let directory = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).unwrap();
        let declaring = root.join("de").join("reader_impl_tests.rs");
        let target = root.join("de").join("reader_tests.rs");

        std::fs::create_dir_all(root.join("de").as_std_path()).unwrap();
        std::fs::write(declaring.as_std_path(), "").unwrap();
        std::fs::write(target.as_std_path(), "").unwrap();

        let ast = parse("#[cfg(test)]\n#[path = \"reader_tests.rs\"]\nmod tests;");
        let found = declarations(&declaring, &ast, &CfgSet::unconditional());

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].target, target);
        assert!(found[0].excluded);
    }

    #[test]
    fn an_active_cfg_attr_marks_an_external_module_as_test_only() {
        let directory = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).unwrap();
        let declaring = root.join("lib.rs");
        let target = root.join("tests.rs");

        std::fs::write(target.as_std_path(), "").unwrap();

        let ast = parse("#[cfg_attr(unix, cfg(test))]\nmod tests;");
        let active = declarations(&declaring, &ast, &CfgSet::parse("unix\n"));
        let inactive = declarations(&declaring, &ast, &CfgSet::parse("windows\n"));

        assert_eq!(active.len(), 1, "{active:?}");
        assert_eq!(active[0].target, target);
        assert!(active[0].excluded);
        assert_eq!(inactive.len(), 1, "{inactive:?}");
        assert!(!inactive[0].excluded);
    }

    #[test]
    fn an_active_cfg_attr_excludes_an_external_module() {
        let directory = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).unwrap();
        let declaring = root.join("lib.rs");
        let target = root.join("platform.rs");

        std::fs::write(target.as_std_path(), "").unwrap();

        let ast = parse("#[cfg_attr(unix, cfg(windows))]\nmod platform;");
        let active = declarations(&declaring, &ast, &CfgSet::parse("unix\n"));
        let inactive = declarations(&declaring, &ast, &CfgSet::parse("windows\n"));
        let declared = vec![(declaring.clone(), active.clone())];

        assert_eq!(active.len(), 1, "{active:?}");
        assert!(active[0].excluded);
        assert_eq!(inactive.len(), 1, "{inactive:?}");
        assert!(!inactive[0].excluded);
        assert!(excluded_files(&[declaring], &declared).contains(&target));
    }

    #[test]
    fn a_plain_declaration_resolves_in_the_directory_the_file_owns() {
        let directory = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).unwrap();
        let declaring = root.join("de.rs");
        let target = root.join("de").join("raw.rs");

        std::fs::create_dir_all(root.join("de").as_std_path()).unwrap();
        std::fs::write(target.as_std_path(), "").unwrap();

        let found = declarations(&declaring, &parse("mod raw;"), &CfgSet::unconditional());

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].target, target);
        assert!(!found[0].excluded);
    }

    #[test]
    fn a_file_reached_only_through_a_test_module_is_excluded() {
        let root = Utf8PathBuf::from("/w/src/lib.rs");
        let helper = Utf8PathBuf::from("/w/src/helper.rs");
        let tests = Utf8PathBuf::from("/w/src/reader_tests.rs");
        let declared = vec![(
            root.clone(),
            vec![
                Declaration {
                    target: helper.clone(),
                    excluded: false,
                },
                Declaration {
                    target: tests.clone(),
                    excluded: true,
                },
            ],
        )];

        let excluded = excluded_files(&[root], &declared);

        assert!(excluded.contains(&tests));
        assert!(!excluded.contains(&helper));
    }

    #[test]
    fn a_file_a_test_module_shares_with_real_code_is_kept() {
        // Reached both ways, so it is real code that tests happen to also pull in. Dropping it
        // would silently remove mutants from code the crate actually ships.
        let root = Utf8PathBuf::from("/w/src/lib.rs");
        let shared = Utf8PathBuf::from("/w/src/shared.rs");
        let declared = vec![(
            root.clone(),
            vec![
                Declaration {
                    target: shared.clone(),
                    excluded: true,
                },
                Declaration {
                    target: shared,
                    excluded: false,
                },
            ],
        )];

        assert!(excluded_files(&[root], &declared).is_empty());
    }

    #[test]
    fn a_file_nothing_declares_is_left_alone() {
        let root = Utf8PathBuf::from("/w/src/lib.rs");

        assert!(excluded_files(&[root], &[]).is_empty());
    }

    #[test]
    fn a_module_below_a_test_module_is_excluded_too() {
        let root = Utf8PathBuf::from("/w/src/lib.rs");
        let outer = Utf8PathBuf::from("/w/src/outer.rs");
        let inner = Utf8PathBuf::from("/w/src/outer/inner.rs");
        let declared = vec![
            (
                root.clone(),
                vec![Declaration {
                    target: outer.clone(),
                    excluded: true,
                }],
            ),
            (
                outer.clone(),
                vec![Declaration {
                    target: inner.clone(),
                    excluded: false,
                }],
            ),
        ];

        let excluded = excluded_files(&[root], &declared);

        // Both: `inner` is only ever reached by walking through `outer`, which exists for tests,
        // so it is test code as surely as its parent is.
        assert!(excluded.contains(&outer));
        assert!(excluded.contains(&inner));
    }

    /// A file two live modules both declare is walked once, and stays production code.
    #[test]
    fn a_file_declared_by_two_live_modules_is_walked_once() {
        // `#[path]` lets two modules name the same file. The live walk has to notice it has been
        // there before, or a diamond in the module graph becomes an exponential re-walk.
        let root = Utf8PathBuf::from("/w/src/lib.rs");
        let left = Utf8PathBuf::from("/w/src/left.rs");
        let right = Utf8PathBuf::from("/w/src/right.rs");
        let shared = Utf8PathBuf::from("/w/src/shared.rs");
        let declared = vec![
            (
                root.clone(),
                vec![
                    Declaration {
                        target: left.clone(),
                        excluded: false,
                    },
                    Declaration {
                        target: right.clone(),
                        excluded: false,
                    },
                ],
            ),
            (
                left,
                vec![Declaration {
                    target: shared.clone(),
                    excluded: false,
                }],
            ),
            (
                right,
                vec![Declaration {
                    target: shared.clone(),
                    excluded: false,
                }],
            ),
        ];

        let excluded = excluded_files(&[root], &declared);

        assert!(!excluded.contains(&shared), "{excluded:?}");
    }

    #[test]
    fn a_cycle_between_two_files_terminates() {
        // `#[path]` makes a declaration cycle expressible, and the walk has to notice it has been
        // somewhere before rather than following the edge round for ever.
        let root = Utf8PathBuf::from("/w/src/lib.rs");
        let first = Utf8PathBuf::from("/w/src/first.rs");
        let second = Utf8PathBuf::from("/w/src/second.rs");
        let declared = vec![
            (
                root.clone(),
                vec![Declaration {
                    target: first.clone(),
                    excluded: true,
                }],
            ),
            (
                first.clone(),
                vec![Declaration {
                    target: second.clone(),
                    excluded: false,
                }],
            ),
            (
                second.clone(),
                vec![Declaration {
                    target: first.clone(),
                    excluded: false,
                }],
            ),
        ];

        let excluded = excluded_files(&[root], &declared);

        assert!(excluded.contains(&first));
        assert!(excluded.contains(&second));
    }

    #[test]
    fn a_path_with_no_directory_declares_nothing() {
        // A bare file name has no parent directory to resolve a `mod` against. Returning nothing is
        // the honest answer; guessing the current directory would attribute declarations to
        // whatever happened to be beside the process.
        assert!(declarations(Utf8Path::new(""), &parse("mod raw;"), &CfgSet::unconditional()).is_empty());
    }

    #[test]
    fn a_declaration_pointing_at_no_file_is_dropped() {
        // A `mod` behind a `cfg` this cannot evaluate, or one generated by a build script, names a
        // file that is not on disk. It has to be skipped rather than recorded as a source file the
        // run would then fail to open.
        let directory = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).unwrap();

        assert!(declarations(&root.join("lib.rs"), &parse("mod nowhere;"), &CfgSet::unconditional()).is_empty());
    }

    #[test]
    fn an_attribute_that_is_neither_cfg_nor_path_is_ignored() {
        // Every item carries attributes this does not care about. Reading one as a `#[path]` would
        // resolve a module to a doc string.
        let ast = parse("#[doc = \"a module\"]\n#[derive(Debug)]\nmod plain;");
        let Item::Mod(module) = &ast.items[0] else {
            panic!("expected a module")
        };

        assert_eq!(path_attribute(&module.attrs), None);
        assert!(!test_gated(&module.attrs));
    }

    #[test]
    fn a_path_attribute_on_an_inline_module_redirects_everything_below_it() {
        // `#[path] mod outer { mod inner; }` puts `inner` under the redirected directory, not under
        // one named after `outer`. Resolving it against the module's own name would find nothing.
        let directory = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).unwrap();
        let target = root.join("custom").join("inner.rs");

        std::fs::create_dir_all(root.join("custom").as_std_path()).unwrap();
        std::fs::write(target.as_std_path(), "").unwrap();

        let source = "#[path = \"custom\"]\nmod outer { mod inner; }";
        let found = declarations(&root.join("lib.rs"), &parse(source), &CfgSet::unconditional());

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].target, target);
    }
}
