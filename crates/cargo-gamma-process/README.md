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

Containment itself is not conditional on that request. A Unix process group is escapable — a
descendant that calls `setsid` leaves it, and every later signal to the group misses it — so
every Windows launch enters a job and every Linux launch attempts to enter a cgroup leaf whether
or not anything is being measured. An unmetered Linux launch may use best-effort process-group
containment only when the host has no supported cgroup facility. Sealed containment means a
boundary descendants cannot leave: [`containment`][__link4] reports when the host cannot provide one
before repository-controlled code executes, and [`ProcessTree::sealed`][__link5] reports the resulting
state for one launch.

The boundary is in force from the child’s first instruction. On Linux the child moves itself
into the cgroup between fork and exec; on Windows it normally starts suspended, enters the
dedicated job, and only then runs. A child that cannot enter a job already created for it is
rejected because an inherited job cannot be opened later to terminate its descendants. A peak
that reached the limit is therefore enforced by the kernel rather than inferred after the fact.

Because the Linux boundary is installed as a pre-exec step naming one particular leaf, and a
[`Command`][__link6] accumulates every such step it is given, a command can only
be prepared once. That is stated in the types rather than checked: [`prepare`][__link7] consumes the
command and returns a [`PreparedCommand`][__link8], whose consuming spawn advances to
[`SpawnedCommand`][__link9]. A failed spawn returns [`SpawnFailure`][__link10] with the preparation intact. The
caller classifies its underlying operating-system error, retries transient resource-related
spawn failures after [`PreparedCommand::backoff`][__link11], and propagates permanent failures. A
successful spawn can only be surrendered to [`ProcessTree::adopt`][__link12], so one preparation cannot
leave an earlier child outside containment and launch another; dropping the post-spawn state
before adoption terminates and reaps that child.

[`output`][__link13] is the contained counterpart of
[`Command::output`][__link14]. It drains stdout and stderr concurrently,
then sweeps descendants before waiting for inherited pipe handles to close.

A terminal delivers `Ctrl-C` to the whole foreground process group, so a child sharing this
process’s group dies with it automatically while a child leading its own group does not. Windows
normally preserves that guarantee through a dedicated job that dies with its last handle. Unix
installs explicit interruption handling through `cargo-gamma-unsafe`.


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-gamma-process">source code</a>.
</sub>

 [__cargo_doc2readme_dependencies_info]: ggGmYW0CYXZlMC43LjNhdIQbFhzZ8rzWNNYbuRaDSGWynFgbH4PMdoT7GNcbVwNPtPjAhvFhYvRhcoQbzJIscAd99o8bNjOMWNtQqzob7B-tkBhXsbcbeRzSwG3PJgJhZIGDc2NhcmdvLWdhbW1hLXByb2Nlc3NlMC4xLjBzY2FyZ29fZ2FtbWFfcHJvY2Vzcw
 [__link0]: https://crates.io/crates/cargo-gamma
 [__link1]: https://docs.rs/cargo-gamma-process/0.1.0/cargo_gamma_process/?search=MemoryRequest
 [__link10]: https://docs.rs/cargo-gamma-process/0.1.0/cargo_gamma_process/?search=SpawnFailure
 [__link11]: https://docs.rs/cargo-gamma-process/0.1.0/cargo_gamma_process/?search=PreparedCommand::backoff
 [__link12]: https://docs.rs/cargo-gamma-process/0.1.0/cargo_gamma_process/?search=ProcessTree::adopt
 [__link13]: https://docs.rs/cargo-gamma-process/0.1.0/cargo_gamma_process/?search=output
 [__link14]: https://doc.rust-lang.org/stable/std/?search=process::Command::output
 [__link2]: https://docs.rs/cargo-gamma-process/0.1.0/cargo_gamma_process/?search=prepare
 [__link3]: https://docs.rs/cargo-gamma-process/0.1.0/cargo_gamma_process/?search=ProcessTree::usage
 [__link4]: https://docs.rs/cargo-gamma-process/0.1.0/cargo_gamma_process/?search=containment
 [__link5]: https://docs.rs/cargo-gamma-process/0.1.0/cargo_gamma_process/?search=ProcessTree::sealed
 [__link6]: https://doc.rust-lang.org/stable/std/?search=process::Command
 [__link7]: https://docs.rs/cargo-gamma-process/0.1.0/cargo_gamma_process/?search=prepare
 [__link8]: https://docs.rs/cargo-gamma-process/0.1.0/cargo_gamma_process/?search=PreparedCommand
 [__link9]: https://docs.rs/cargo-gamma-process/0.1.0/cargo_gamma_process/?search=SpawnedCommand
