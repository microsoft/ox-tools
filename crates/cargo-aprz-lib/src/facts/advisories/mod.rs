// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `RustSec`vulnerability advisory database fact provider.

mod advisory_data;
mod provider;

#[cfg(any(test, feature = "internals"))]
pub use advisory_data::AdvisoryCounts;
pub use advisory_data::AdvisoryData;
pub use provider::Provider;
