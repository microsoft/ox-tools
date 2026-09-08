// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use clap::ValueEnum;

/// When to colorize output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum When {
    /// Colorize when the stream is a terminal.
    #[default]
    Auto,

    /// Never colorize.
    Never,

    /// Always colorize.
    Always,
}

impl When {
    /// Resolves the setting against whether the stream is actually a terminal.
    #[must_use]
    pub const fn resolve(self, is_terminal: bool) -> bool {
        match self {
            Self::Auto => is_terminal,
            Self::Never => false,
            Self::Always => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_auto_follows_the_terminal() {
        assert!(When::Auto.resolve(true));
        assert!(!When::Auto.resolve(false));
        assert!(!When::Never.resolve(true));
        assert!(When::Always.resolve(false));
    }
}
