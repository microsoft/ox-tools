# Native process test fixture

`output-helper.rs` is the package's host build script. Cargo runs it without
arguments, and it records its own executable path for the tests. With scripted
arguments the same executable serves as the fixture. This works from a source
archive as well as a checkout, without an installed command, a test-time
compiler, or a separately published helper. Cargo owns its build and rebuilds.

The helper accepts scripted output, exit, readiness, descendant, and memory
operations used by native acceptance tests. It is not a user-facing command.
