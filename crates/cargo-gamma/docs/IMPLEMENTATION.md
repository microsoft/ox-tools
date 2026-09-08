# cargo-gamma — Implementation guide

This guide records executable and end-to-end mechanics behind
[`DESIGN.md`](DESIGN.md).

## Executable boundary

The binary implements the real terminal host and calls
`cargo_gamma_lib::run`. Installed-binary tests launch that executable in both
direct and Cargo subcommand argument shapes, then compare its complete version
output with the package version.

## Scratch layout

The coordinator synchronizes sources, vendors the dependency-free guard
runtime, and places Cargo artifacts and campaign state under the selected cache
base. The default base is selected from a stable physical workspace identity;
published reports remain under the artifact directory rather than reusable
cache state.

## Runtime protocol

The injected runtime uses fixed static buffers and native startup-environment
access because it has no allocator or production dependencies during
construction. Startup acquisition failures use a fixed marker and reserved exit
status so the coordinator cannot mistake an unselected mutant for a baseline
run.

## Test fixtures

Cross-process tests use owned temporary directories and explicit environment
markers. Concurrency tests observe channels, process completion, or ownership
state where those transitions are controllable; watchdogs remain only as
last-resort harness protection outside mutation campaigns.
