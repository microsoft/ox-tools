# cargo-gamma-lib — Implementation guide

This guide records coordinator mechanics behind the user-visible contracts in
[`design/README.md`](design/README.md).

## Internal facade

Production modules remain private. The `internals` feature mirrors them only
for this crate's integration tests, and the self dev-dependency is the only
workspace consumer that enables it. Helpers used solely within production
modules keep crate or module visibility.

## Cache representation

The default cache name is a truncated BLAKE3 digest of the resolved physical
workspace root. Canonicalizing the existing root makes case and link aliases
converge before lock selection; the ownership marker remains the collision
defense. Explicit cache paths retain their caller-selected identity.

## Process supervision

Platform and process errors remain typed through launch, retry, containment,
and output-reader setup. They become verdict or event text only where the
coordinator constructs user-visible output. Reader threads publish through a
channel so subtree cleanup can precede bounded output draining.

## Deterministic concurrency checks

Loom models cover reader-accounting races with the smallest actor sets that
create contention. Test-only command pauses synchronize on channels rather
than elapsed time. Last-resort watchdogs are disabled under cargo-gamma so
mutation campaign supervision, rather than a test deadline, classifies hangs.
