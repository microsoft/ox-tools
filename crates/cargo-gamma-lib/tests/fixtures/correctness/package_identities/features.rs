// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub enum MetadataFeatureOption {
    All,
    NoDefault,
    Some(Vec<String>),
}

pub enum Optional {
    Some(u8),
    None,
}
