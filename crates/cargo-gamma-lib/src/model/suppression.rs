// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use serde::{Deserialize, Serialize};

/// Why a mutant was suppressed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suppression {
    /// The channel that suppressed it.
    pub channel: Channel,

    /// The reason given, if any.
    pub reason: Option<String>,

    /// The tag given, if any.
    pub tag: Option<String>,

    /// The line the directive appears on, when it came from source.
    pub line: Option<usize>,
}

/// The channel a suppression arrived through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Channel {
    /// A `// #[gamma::skip(..)]` comment.
    Comment,

    /// A real `#[gamma::skip(..)]` attribute.
    Attribute,

    /// Configuration.
    Config,
}

impl Channel {
    /// Returns the short name used in output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Comment => "comment",
            Self::Attribute => "attribute",
            Self::Config => "config",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_suppression_channel_has_the_short_name_used_in_output() {
        // Suppression reasons are shown in reports, so the channel names must stay stable and
        // human-readable.
        assert_eq!(Channel::Comment.as_str(), "comment");
        assert_eq!(Channel::Attribute.as_str(), "attribute");
        assert_eq!(Channel::Config.as_str(), "config");
    }
}
