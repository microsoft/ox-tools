// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(feature = "dep")]
pub fn implicit_local_feature_is_released() {}

#[cfg(not(feature = "dep"))]
pub fn impossible_without_required_dependency() {}
