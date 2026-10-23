// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(not(feature = "extra"))]
pub fn inactive_dependency_branch() -> i32 {
    11
}

#[cfg(feature = "extra")]
pub fn active_dependency_branch() -> i32 {
    22
}

#[cfg(all(not(feature = "extra"), any(unix, feature = "extra")))]
pub fn nested_inactive_dependency_branch() -> i32 {
    33
}

#[cfg(not(all(not(feature = "extra"), unix)))]
pub fn nested_active_dependency_branch() -> i32 {
    44
}

#[cfg(all(feature = "extra", windows))]
pub fn inactive_platform_control() -> i32 {
    55
}
