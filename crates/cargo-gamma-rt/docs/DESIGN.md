# cargo-gamma-rt — Design

> Status: **Implemented**.
> Crate name: `cargo-gamma-rt`.

## Purpose

This crate is the guard runtime injected into crates under mutation test. It
selects one compiled mutation at process start while keeping the inactive path
equivalent to the original program.

## Hard constraints

- Zero ordinary dependencies.
- No features and no build script.
- `no_std` compatibility.
- The library target remains named `gamma_rt`.
- Rustdoc is hidden, and the hand-written README warns downstream users not to
  depend on this implementation crate.

These constraints prevent injection from perturbing dependency resolution,
feature unification, offline builds, or the target crate's standard-library
requirements.

## Selection and census protocol

The runtime captures `GAMMA_ACTIVE` and `GAMMA_CENSUS` during process startup,
then guards use only the captured atomic selection. Linux reads the immutable
environment image retained by `exec`, avoiding races with later environment
mutation. `OVERFLOW` and `SEAL` are public wire-format constants shared with
the census reader so the protocol has one source of truth.

A variable that is absent and one that could not be read are different answers.
Absence selects the baseline, or — for `GAMMA_CENSUS` — an ordinary run; a
failure to open or read the environment image terminates startup with the
runtime's fixed failure marker, so a census the coordinator asked for can never
be silently downgraded into a run that appears to have reached nothing. Signal
delivery interrupts a read without failing it, so reads are retried under a
budget spent across the whole capture rather than per read: an endless stream of
signals is reported as a failure instead of spun on inside a constructor.
Windows preserves the same distinction by clearing last error before each
`GetEnvironmentVariableA` or `GetEnvironmentVariableW` call and classifying a
zero return with `GetLastError`: only `ERROR_ENVVAR_NOT_FOUND` means absence.
Every other API failure emits the same fixed marker and immediately exits with
the same infrastructure-failure status used on Unix.

On non-Linux Unix targets, `getenv` is used under POSIX's precondition that no
native environment mutation occurs concurrently. The capture runs before Rust
`main`, so safe Rust cannot have started such a mutation; a foreign native
constructor that violates the precondition is outside the runtime abstraction.
A second pointer-and-value read detects visibly inconsistent capture results and
turns them into a startup failure. This is an integrity check, not a memory-
safety proof and not proof that forbidden foreign mutation never occurred.

The captured census path lives in a fixed-size buffer the runtime terminates
itself, rather than relying on the environment image to supply a terminator. A
name that fills the buffer leaves census mode selected with no path the runtime
can open, so nothing is written and nothing is sealed and the reader discards
that binary's census exactly as it discards a truncated one.

The vendored standalone crate inherits the workspace edition and minimum Rust
version from the same manifest values used to build this crate.
