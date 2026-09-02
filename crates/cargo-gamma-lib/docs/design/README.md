# cargo-gamma-lib — Design

> Status: **Implemented**.
> Crate name: `cargo-gamma-lib`.

This is the crate's top-level design document.

## Purpose

This crate coordinates cargo-gamma campaigns: configuration, discovery,
scratch workspaces, instrumented builds, test selection, process supervision,
verdicts, incremental reuse, reporting, and command dispatch.

## Boundaries

- Rust parsing and instrumentation are delegated to `cargo-gamma-engine`.
- Process-tree mechanics are delegated to `cargo-gamma-process` and
  `cargo-gamma-unsafe`.
- Cargo metadata and nextest inventory commands run through the same contained
  process-output lifecycle as later builds and tests. Their stdout and stderr
  are drained concurrently, and descendants are swept before inherited pipe
  handles are allowed to keep capture open.
- The injected guard protocol is provided by dependency-free
  `cargo-gamma-rt`.
- The `internals` feature exists only for this crate's integration tests and
  is not a supported downstream API.
- The agreement tests enable `cargo-gamma-attrs-impl` through the test-only
  `internals` feature. The implementation crate is an optional normal
  dependency so release tooling publishes it before packaging this crate,
  while ordinary downstream builds do not compile it.
- The crate forbids unsafe code.

## Public contract

The primary public contract is the `cargo gamma` command surface and its
configuration, reports, diagnostics, and exit codes. The Rust API is an
implementation detail used by the thin executable crate. Its rustdoc is hidden,
and its hand-written README warns downstream users not to depend on it.

### Redirected cache security

On Unix, cargo-gamma creates a previously absent redirected cache with
permissions limited to the invoking user, independently of the process umask.
It does not change permissions on a pre-existing directory: the directory and
its physical ancestry must already be owned by the invoking user or root and
must not permit another user to replace entries. Sticky shared ancestors such
as `/tmp` are accepted, but the cache directory itself must not be writable by
group or other.

## Concurrency model checking

The dedicated Loom target verifies race-sensitive reader accounting without a
runner-imposed exploration bound. Each model therefore uses the smallest set of
interchangeable actors that creates contention, avoiding equivalent schedules
that add cost without adding behavior.

Start and finish races are modeled separately. Finish-only models begin from a
valid state already produced by successful starts, including a peak no lower
than the live count. This separation keeps exhaustive exploration tractable
while preserving the production gauge invariants.
