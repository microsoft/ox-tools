<div align="center">
 <img src="./logo.png" alt="Cargo-Gamma-Process Logo" width="96">

# Cargo-Gamma-Process

[![crates.io](https://img.shields.io/crates/v/cargo-gamma-process.svg)](https://crates.io/crates/cargo-gamma-process)
[![docs.rs](https://docs.rs/cargo-gamma-process/badge.svg)](https://docs.rs/cargo-gamma-process)
[![MSRV](https://img.shields.io/crates/msrv/cargo-gamma-process)](https://crates.io/crates/cargo-gamma-process)
[![CI](https://github.com/microsoft/ox-tools/actions/workflows/main.yml/badge.svg?event=push)](https://github.com/microsoft/ox-tools/actions/workflows/main.yml)
[![Coverage](https://codecov.io/gh/microsoft/ox-tools/graph/badge.svg?token=FCUG0EL5TI)](https://codecov.io/gh/microsoft/ox-tools)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
<a href="../.."><img src="../../logo.svg" alt="This crate was developed as part of the Oxidizer project" width="20"></a>

</div>

This crate is an internal implementation detail of
[`cargo-gamma`][__link0]. It contains cargo-gamma’s bounded
process-tree containment, accounting, observation, and termination lifecycle.

Do not depend on it directly. Its API may change incompatibly without notice; it is published
only so that `cargo-gamma` can be installed through crates.io.

## Process-tree lifecycle

Killing the process a run started is not enough. A test that shells out to a server, a database
or another build leaves those behind when the harness above them is cut off, and they take two
things with them: file locks inside the scratch tree, which turn the next run into a failure
that has nothing to do with any mutant, and inherited pipe handles, which keep whoever is
reading this tool’s output from ever seeing end of file. A run that ends with a hung consumer is
worse than one that ends with a wrong verdict, because nobody can even see the verdict.

Both platforms have a way to say “this process and everything descended from it” — a process
group on Unix and a job object on Windows — but neither is reachable from `std`. The raw calls
live in `cargo-gamma-unsafe`, which exposes safe interfaces; this crate composes them into the
lifecycle used by the rest of the tool.

The same boundary accounts for memory because it is the only place that knows the whole process
tree rather than only its leader. A [`MemoryRequest`][__link1] passed to [`prepare`][__link2] asks for measurement,
a ceiling, or neither; [`ProcessTree::usage`][__link3] answers once the process tree is gone. On Windows
requested accounting requires a dedicated job carrying the limit and accounting; on Linux a
cgroup leaf supplied by `cargo-gamma-unsafe` serves both purposes.

The boundary is in force from the child’s first instruction. On Linux the child moves itself
into the cgroup between fork and exec; on Windows it normally starts suspended, enters the
dedicated job, and only then runs. A child that cannot enter a job already created for it is
rejected because an inherited job cannot be opened later to terminate its descendants. Failure
to create a job is tolerated only when no accounting was requested, degrading termination to
the leader alone. A peak that reached the limit is therefore enforced by the kernel rather than
inferred after the fact.

A terminal delivers `Ctrl-C` to the whole foreground process group, so a child sharing this
process’s group dies with it automatically while a child leading its own group does not. Windows
normally preserves that guarantee through a dedicated job that dies with its last handle. Unix
installs explicit interruption handling through `cargo-gamma-unsafe`.


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-gamma-process">source code</a>.
</sub>

 [__cargo_doc2readme_dependencies_info]: ggGmYW0CYXZlMC43LjJhdIQbFhzZ8rzWNNYbuRaDSGWynFgbH4PMdoT7GNcbVwNPtPjAhvFhYvRhcoQbyT97-uOZjGMbJSTUNtzD3T0bGSLqnro0bgkbPvyEineYDERhZIGDc2NhcmdvLWdhbW1hLXByb2Nlc3NlMC4xLjBzY2FyZ29fZ2FtbWFfcHJvY2Vzcw
 [__link0]: https://crates.io/crates/cargo-gamma
 [__link1]: https://docs.rs/cargo-gamma-process/0.1.0/cargo_gamma_process/?search=MemoryRequest
 [__link2]: https://docs.rs/cargo-gamma-process/0.1.0/cargo_gamma_process/?search=prepare
 [__link3]: https://docs.rs/cargo-gamma-process/0.1.0/cargo_gamma_process/?search=ProcessTree::usage
