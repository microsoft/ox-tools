# cargo-aprz-lib design

This crate implements cargo-aprz's commands, fact collection, metric
extraction, policy-expression evaluation, and reports. It is an implementation
detail of the command-line tool, not a stable library API.

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
