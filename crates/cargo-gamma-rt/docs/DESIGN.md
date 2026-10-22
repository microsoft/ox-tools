# cargo-gamma-rt — Design

> Status: **Implemented**.
> Crate name: `cargo-gamma-rt`.

## Purpose

This crate is the guard runtime injected into crates under mutation test. It
selects one compiled mutation at process start while keeping the inactive path
equivalent to the original program.

## Hard constraints

- Zero ordinary dependencies.
- No build script and no feature that changes injected runtime behavior. The
  internal `embedding` feature exposes package-local source text only to
  `cargo-gamma-lib`; the vendored crate never enables it.
- `no_std` compatibility.
- The library target remains named `gamma_rt`.
- Rustdoc is hidden, and the README warns downstream users not to
  depend on this implementation crate.

These constraints prevent injection from perturbing dependency resolution,
feature unification, offline builds, or the target crate's standard-library
requirements.

## Mutation helpers

Instrumented source uses same-type helpers to retain the expected-type
constraints of the expression being mutated:

- `default_after<T: Default>` evaluates its argument once, drops that result,
  then constructs its default. Original call side effects still occur.
- `increment` and `decrement` use the primitive `Boundary` implementations to
  add or subtract one without changing the result type. Floating-point
  implementations use `1.0`; integer arithmetic retains its normal overflow
  behavior.
- `panic_on_failure` accepts `Option` and `Result` through `PanicOnFailure`,
  returns a successful container unchanged, and panics on failure without a
  `Debug` bound. Generated source retains the surrounding `?` so its original
  error-conversion and inference constraints remain available.

The helpers require no allocation or standard library of their own; an
operand's destructor or `Default` implementation retains its own behavior.
The source bundle contains `lib.rs`, `either.rs`, `mutation.rs`, and
`runtime.rs`, so embedded and directly compiled runtimes agree.

## Selection and reach observation protocol

The runtime captures `GAMMA_ACTIVE` and `GAMMA_REACH` during process startup,
then guards use only the captured atomic selection. Linux reads the immutable
environment image retained by `exec`, avoiding races with later environment
mutation. `OVERFLOW` and `SEAL` are public wire-format constants shared with
the observation reader so the protocol has one source of truth. `REACH_VAR`
exports the observation environment variable's name.

Only the active mutant's guard can record reach. The runtime retains a single
set-only flag, independent of population size, rather than storing per-site
information. Recording never changes mutant selection; with no active mutant,
every guard remains false and no guard is recorded. At normal exit the optional
observation file receives the active ordinal if reached, then `SEAL`, as
little-endian `u32` records. An unreached active mutant produces only `SEAL`.
Repeated hits produce no additional records, and all valid active ordinals fit
without an observation-specific limit. The runtime does not need to emit
`OVERFLOW`; the constant remains available to the reader to reject invalid
samples.

Recording leases synchronize first-hit publication with sealing: sealing waits
for admitted flag updates to finish and permanently closes recording before
serializing the flag. The observation file is opened only while sealing and uses
append mode so a reused path or a descendant's second sealed sample remains
detectable by the reader. Missing or unsealed files are unusable observations,
never evidence of an unreached mutant. Failure to register the exit handler
does not switch off the selected mutant.

A variable that is absent and one that could not be read are different answers.
Absence selects the baseline, or — for `GAMMA_REACH` — disables observation; a
failure to open or read the environment image terminates startup with the
runtime's fixed failure marker, so an observation the coordinator asked for can
never be silently downgraded into a run that appears to have reached nothing. Signal
delivery interrupts a read without failing it, so reads are retried under a
budget spent across the whole capture rather than per read: an endless stream of
signals is reported as a failure instead of spun on inside a native constructor,
a loader or C-runtime startup hook that runs before `main`.
Windows preserves the same distinction by clearing last error before each
`GetEnvironmentVariableA` or `GetEnvironmentVariableW` call and classifying a
zero return with `GetLastError`: only `ERROR_ENVVAR_NOT_FOUND` means absence.
Every other API failure emits the same fixed marker and immediately exits with
the same infrastructure-failure status used on Unix.

On hosted Unix and Windows targets, the cached selection starts in a transient
uninstalled state until this crate's native constructor publishes the captured
environment. If instrumented code in an earlier native constructor reaches a
guard during that window, the runtime writes a fixed diagnostic and terminates
through `_exit` or `ExitProcess` with the infrastructure-failure status.
Termination does not unwind, so constructor code cannot catch the failure,
continue startup, and let the runtime overwrite the evidence with an ordinary
selection. The diagnostic marker is part of the parent/runtime protocol so the
coordinator excludes this infrastructure failure from mutation scoring.
Targets without a supported constructor mechanism, and Miri
executions that cannot run one, use the permanent unmutated fallback instead.

On non-Linux Unix targets, `getenv` is used under POSIX's precondition that no
native environment mutation occurs concurrently. The capture runs before Rust
`main`, so safe Rust cannot have started such a mutation; a foreign native
constructor that violates the precondition is outside the runtime abstraction.
A second pointer-and-value read detects visibly inconsistent capture results and
turns them into a startup failure. This is an integrity check, not a memory-
safety proof and not proof that forbidden foreign mutation never occurred.

The captured observation path lives in a fixed-size buffer the runtime terminates
itself, rather than relying on the environment image to supply a terminator. A
path that fills the buffer leaves mutant selection unchanged, with no path the
runtime can open. Nothing is written or sealed, and the reader discards that
binary's observation exactly as it discards a truncated one.

The package owns the source bundle, edition, and minimum Rust version used to
write the vendored standalone crate. This keeps the coordinator's published
package self-contained while ensuring the copied runtime is the exact source
compiled as `cargo-gamma-rt`. The edition is deliberately explicit because
Cargo exposes no `CARGO_PKG_EDITION` compile-time variable, and a workspace
edition bump must not silently reinterpret source injected into another
repository.

## Runtime test oracles and isolation

| Family in `runtime.rs::tests` | Break rejected by the oracle | Isolation |
| --- | --- | --- |
| Startup selection and cached guards | Wrong ordinal, reserved ordinal activated, environment reread, or changed post-startup selection | Child launch environments select cases; native environment mutation happens only in constructors that exit before libtest starts threads |
| Allocation measurements | Inactive guard allocation or a disabled allocation/reallocation counter | Per-thread counters have a positive alloc/realloc control and a handshaken allocating sibling |
| Bounded copies and environment scanning | Missing terminator, prefix accepted as a full value, cross-chunk key/value corruption, failure mistaken for absence, or unbounded EINTR retry | Local buffers and finite scripted reads; call counts and copied bytes accompany classified outcomes |
| Native environment outcomes and exit registration | Empty/absent/error conflated or failed reach registration changing mutant selection | Pure injected results and callbacks; no shared environment writes |
| Recording/sealing | Seal overtakes an admitted writer, recording reopens, short write produces a seal, or an unreached observation loses its seal | Local recorder states plus isolated observation children; assertions check writer completion at seal time, not only after joins; Loom checks the same completion point |
| Startup failure diagnostics | Failure unwinds, continues, uses the wrong status, or loses its stderr marker | Isolated pre-install and overlapped-stderr children; state markers, reserved status, and diagnostic bytes are checked |

The native `getenv` and post-install environment-mutation helpers exit from
single-threaded constructors with distinct test-only statuses (73 and 72).
Parents require those exact statuses: a filtered libtest process with zero
matching tests cannot masquerade as a completed constructor check. Observation
helpers likewise require their exact test name in the harness output, in addition
to success and the expected wire records.
