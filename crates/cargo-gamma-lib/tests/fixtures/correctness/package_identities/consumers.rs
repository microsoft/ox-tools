// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::catalog::{Artifact, Artifact as Renamed};
use super::features::MetadataFeatureOption;

type Alias = Renamed;

pub fn artifact() -> Artifact {
    Artifact
}
pub fn aliased() -> Alias {
    Artifact
}
pub fn qualified() -> super::catalog::Artifact {
    Artifact
}
pub fn artifacts() -> Vec<Artifact> {
    vec![Artifact]
}
pub fn renamed_artifacts() -> Vec<Renamed> {
    vec![Renamed]
}
pub fn qualified_artifacts() -> Vec<super::catalog::Artifact> {
    vec![super::catalog::Artifact]
}
pub fn alternate() -> super::alternate::Artifact {
    super::alternate::Artifact
}
pub fn relocated() -> super::relocated::Artifact {
    super::relocated::Artifact
}
pub fn manual() -> super::alternate::Manual {
    super::alternate::Manual
}
impl Default for super::alternate::Manual {
    fn default() -> Self {
        Self
    }
}
pub fn local_feature() -> MetadataFeatureOption {
    MetadataFeatureOption::Some(Vec::new())
}
pub fn dependency_alias() -> super::dependency::MetadataFeatureOption<Vec<String>> {
    super::dependency::MetadataFeatureOption::Some(Vec::new())
}
pub fn custom_paired(present: bool) -> super::features::Optional {
    use super::features::Optional::{None, Some};
    if present { Some(1) } else { None }
}
#[cfg_attr(any(), derive(Default))]
pub struct Configured;
pub fn configured() -> Configured {
    Configured
}

pub fn compiled_empty() -> Vec<Artifact> {
    Default::default()
}
pub fn compiled_alternate() -> super::alternate::Artifact {
    Default::default()
}
pub fn compiled_alias_none() -> super::dependency::MetadataFeatureOption<Vec<String>> {
    None
}
pub fn compiled_custom_none() -> super::features::Optional {
    use super::features::Optional::None;
    None
}

const _: fn() = || {
    trait AmbiguousIfDefault<A> {
        fn check() {}
    }
    impl<T: ?Sized> AmbiguousIfDefault<()> for T {}
    impl<T: Default> AmbiguousIfDefault<u8> for T {}
    let _ = <Artifact as AmbiguousIfDefault<_>>::check;
};
