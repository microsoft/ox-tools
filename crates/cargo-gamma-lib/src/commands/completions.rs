// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use clap::CommandFactory as _;

use super::cli::{Cli, CompletionsArgs};
use super::dispatch::EXIT_OK;
use super::host::Host;

/// Writes a shell completion script to standard output.
pub(super) fn completions<H: Host>(host: &mut H, args: &CompletionsArgs) -> i32 {
    let mut command = Cli::command();

    // The generator treats spaces in the name as a subcommand path, so the script is generated for
    // the executable rather than for the `cargo gamma` spelling that goes through cargo's plugin
    // dispatch. Completion still works, because cargo hands the word after `gamma` straight on.
    let name = command.get_name().to_owned();

    clap_complete::generate(args.shell, &mut command, name, &mut host.results());

    EXIT_OK
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use clap_complete::Shell;

    use super::*;
    use crate::testing::Sink;

    #[test]
    fn every_supported_shell_produces_a_script() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::PowerShell, Shell::Elvish] {
            let mut host = Sink::default();
            let code = completions(&mut host, &CompletionsArgs { shell });

            assert_eq!(code, EXIT_OK);
            assert!(!host.out.is_empty(), "{shell} produced nothing");
        }
    }

    #[test]
    fn the_script_names_the_subcommands() {
        let mut host = Sink::default();
        let _code = completions(&mut host, &CompletionsArgs { shell: Shell::Bash });
        let script = String::from_utf8(host.out).expect("the script is not UTF-8");

        assert!(script.contains("merge"), "{script}");
        assert!(script.contains("estimate"), "{script}");
    }

    #[test]
    fn sink_reports_the_shape_the_generator_ignores() {
        let mut host = Sink::default();

        assert_eq!(host.error().write(b"ignored").expect("write"), 7);
        assert!(!host.is_terminal());
        assert_eq!(host.terminal_width(), None);
    }
}
