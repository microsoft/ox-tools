# Native process test fixture

This unpublished, standalone crate is a versionless dev-dependency of the
process and library crates. It keeps helper compilation out of executing test
cases and out of ordinary production builds, without adding a shipped binary
target or requiring a particular test runner.

Its build script compiles `src/helper.rs` with Cargo's host compiler and places
the executable in its owned `OUT_DIR`. Compilation failures fail the test build.
The library exposes only the resulting exact path. Cargo owns rebuild detection
when the source or toolchain changes; tests do not maintain a shared binary cache.

The helper accepts scripted output, exit, readiness, descendant, and memory
operations used by native acceptance tests. It is not a user-facing command.
