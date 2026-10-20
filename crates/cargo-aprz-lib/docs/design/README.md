# cargo-aprz-lib design

This crate implements cargo-aprz's commands, fact collection, metric
extraction, policy-expression evaluation, and reports. It is an implementation
detail of the command-line tool, not a stable library API.

## Native provider acceptance

`tests/crates_provider_integration.rs` and `tests/hosting_provider_integration.rs`
are native acceptance suites, not in-process policy tests. They require Tokio's
socket reactor and loopback listeners; crates-provider cases additionally need
writable, memory-mappable storage for downloaded tables. Each case owns its
Wiremock server and cache directory, uses controlled responses without developer
credentials, and tears both down on completion. No external service is contacted.
The target budget is under one second per case after compilation.

The suggestion replacement regression retains a deliberately ordered candidate
population through the real dump download/table/query boundary. The last-page
link regression exercises real HTTP header handling and asserts exactly one
issue request. These native observations remain separate from pure ranking and
pagination policy checks; moving them into integration targets does not make
their prerequisites disappear.

## Progress styling

Progress phase labels use the command's explicit color policy. Colored labels
are bold cyan; plain labels contain no ANSI styling. Rendering does not make
an independent environment-dependent color decision.

## Console metric wrapping

Console metric values wrap to the detected terminal width (80 columns when
unavailable). The first line includes the metric-name prefix in its width
budget, and continuation lines reserve the same width for indentation.
Words that fit the remaining budget stay together, including at the exact
width boundary.

All width budgets and metric-name padding use terminal display columns, not
UTF-8 byte counts or character counts. Combining characters occupy no extra
column and wide characters occupy two columns.

Individual oversized words are not split. When the prefix occupies the entire
width, the value remains on one line rather than being discarded.
