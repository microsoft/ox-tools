// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// The risk level assigned to a crate after policy evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    Low,
    Medium,
    High,
}

impl core::fmt::Display for Risk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Low => write!(f, "LOW RISK"),
            Self::Medium => write!(f, "MEDIUM RISK"),
            Self::High => write!(f, "HIGH RISK"),
        }
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;

    #[test]
    fn displays_each_risk_level() {
        assert_eq!(Risk::Low.to_string(), "LOW RISK");
        assert_eq!(Risk::Medium.to_string(), "MEDIUM RISK");
        assert_eq!(Risk::High.to_string(), "HIGH RISK");
    }
}
