// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use strum::{Display, EnumIter};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumIter, Display)]
pub enum MetricCategory {
    Metadata,
    Stability,
    Usage,
    Community,
    Activity,
    Documentation,
    Trustworthiness,
    Codebase,
    Advisories,
}

impl MetricCategory {
    #[must_use]
    pub const fn as_uppercase_str(self) -> &'static str {
        match self {
            Self::Metadata => "METADATA",
            Self::Stability => "STABILITY",
            Self::Usage => "USAGE",
            Self::Community => "COMMUNITY",
            Self::Activity => "ACTIVITY",
            Self::Documentation => "DOCUMENTATION",
            Self::Trustworthiness => "TRUSTWORTHINESS",
            Self::Codebase => "CODEBASE",
            Self::Advisories => "ADVISORIES",
        }
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;

    #[test]
    fn uppercase_names_match_category_variants() {
        assert_eq!(MetricCategory::Metadata.as_uppercase_str(), "METADATA");
        assert_eq!(MetricCategory::Stability.as_uppercase_str(), "STABILITY");
        assert_eq!(MetricCategory::Usage.as_uppercase_str(), "USAGE");
        assert_eq!(MetricCategory::Community.as_uppercase_str(), "COMMUNITY");
        assert_eq!(MetricCategory::Activity.as_uppercase_str(), "ACTIVITY");
        assert_eq!(MetricCategory::Documentation.as_uppercase_str(), "DOCUMENTATION");
        assert_eq!(MetricCategory::Trustworthiness.as_uppercase_str(), "TRUSTWORTHINESS");
        assert_eq!(MetricCategory::Codebase.as_uppercase_str(), "CODEBASE");
        assert_eq!(MetricCategory::Advisories.as_uppercase_str(), "ADVISORIES");
    }
}
