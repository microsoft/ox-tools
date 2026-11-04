// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

pub fn resource_path(root: &Utf8Path, path: &Utf8Path) -> Result<Utf8PathBuf> {
    super::placed_under(root, path, "resource evidence")
}

#[must_use]
pub fn checked_survey(root: Utf8PathBuf) -> Survey {
    let path = Utf8PathBuf::from("tests/fixtures/completed-recovery/lib.rs");
    let absolute = root.join(&path);
    Survey {
        target: root.join("target"),
        files: vec![TargetFile {
            path,
            absolute: absolute.clone(),
            package: "alpha".into(),
            source: None,
        }],
        declaration_files: HashMap::default(),
        reach: HashMap::default(),
        selected: vec!["alpha".into()],
        tests: Vec::new(),
        library_tests: Vec::new(),
        compile_fail: Vec::new(),
        by_package: once(("alpha".into(), vec![0])).collect(),
        roots: once(("alpha".into(), vec![absolute])).collect(),
        specs: HashMap::default(),
        cfgs: Cfgs::unconditional(),
        source_dirs: Vec::new(),
        external_inputs: Vec::new(),
        resource_dependencies: Vec::new(),
        untracked_build_script_inputs: false,
        diff: None,
        shard: None,
        name_patterns: None,
        only_mutants: None,
        settled: HashMap::default(),
        exclude_trait_impls: Vec::new(),
        root,
    }
}

pub fn set_diff(survey: &mut Survey, diff: Diff) {
    survey.diff = Some(diff);
}

pub fn set_dependency_roots(survey: &mut Survey, roots: Vec<Utf8PathBuf>, declarations: Vec<Utf8PathBuf>) {
    survey.roots.insert("beta".into(), roots);
    survey.declaration_files.insert("beta".into(), declarations);
}

pub fn link_resource_dependency(survey: &mut Survey, dependency: &Utf8Path) {
    survey.resource_dependencies.push(collect::ResourceDependency {
        crate_root: survey.files[0].absolute.to_string(),
        import_name: "renamed".into(),
        package_id: "beta 1.0.0".into(),
        package_name: "beta".into(),
        version: "1.0.0".into(),
        source: String::new(),
        features: Vec::new(),
        registry_closure: false,
        library_root: Some(dependency.to_string()),
    });
}

pub fn scan_excluding(
    survey: &Survey,
    selection: &Selection,
    ordinal: &mut u32,
    exclusions: &super::super::compiler_exclusions::CompilerExclusions,
) -> Result<Scanned> {
    survey.scan_excluding(None, selection, ordinal, Some(exclusions))
}

pub fn parse_declarations(
    path: &Utf8Path,
    cfg: &CfgSet,
    inspect: impl FnOnce(&Utf8Path, &syn::File, &CfgSet) -> Result<()>,
) -> Result<DeclarationOutcome> {
    super::parse_declarations_with(path, cfg, |path, ast, cfg| {
        inspect(path, ast, cfg)?;
        Ok(Vec::new())
    })
}

pub fn scan_with_inactive_declaration(
    file: &TargetFile,
    outside: &Utf8Path,
    cfg: &CfgSet,
    inspect: impl FnOnce(&[(&Utf8Path, &CfgSet, &str)]) -> Result<()>,
) -> Result<Scan> {
    super::scan_with_declarations(
        &[file],
        &[(outside, cfg, "beta")],
        std::slice::from_ref(&file.absolute),
        &Selection::parse("relational.lt_to_le")?,
        &Cfgs::unconditional(),
        &[],
        &[],
        |inputs| {
            inspect(inputs)?;
            Ok(Declarations {
                declared: vec![(
                    outside.to_owned(),
                    vec![modules::Declaration {
                        target: file.absolute.clone(),
                        excluded: true,
                    }],
                )],
                named: Vec::new(),
                digests: HashMap::default(),
                skipped: Vec::new(),
                defaults: HashMap::default(),
                incomplete_packages: HashSet::default(),
            })
        },
    )
}
