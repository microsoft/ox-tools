// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `RustSec`vulnerability advisory database fact provider.

mod advisory_data;
#[cfg(not(feature = "internals"))]
mod archive;
#[cfg(feature = "internals")]
pub mod archive;
mod provider;

#[cfg(any(test, feature = "internals"))]
pub use advisory_data::AdvisoryCounts;
pub use advisory_data::AdvisoryData;
pub use provider::Provider;

#[cfg(feature = "internals")]
pub fn configure_request_resilience(builder: fetch::HttpClientBuilder) -> fetch::HttpClientBuilder {
    crate::facts::resilient_http::configure_request_resilience(builder)
}
