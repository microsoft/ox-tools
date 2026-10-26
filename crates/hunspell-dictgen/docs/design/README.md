# Hunspell Dictgen design

## Problem

Repositories often keep spelling exceptions in a human-maintained text file,
while Hunspell requires a `.dic` file whose first line is an entry count.
Ad-hoc shell pipelines make the conversion platform-dependent and can leave a
partially written dictionary when they fail.

`hunspell-dictgen` provides one small, reusable executable for deterministic,
cross-platform conversion.

## User experience

The command requires explicit input and output paths:

```text
hunspell-dictgen --input .spelling --output target/spelling.dic
```

Both arguments are filesystem paths. The command writes no dictionary content
to standard output. Success exits with code 0; argument, input, decoding, or
output failures exit nonzero with an error that names the affected path and
operation.

## Input contract

The input is UTF-8 text with one candidate entry per line.

- A missing input path is a successful empty word list. This preserves the
  optional-word-list contract used by repository spell-check recipes without
  treating any other read failure as absence.
- Lines use Rust's cross-platform text-line semantics: LF and CRLF terminators
  are removed.
- Empty lines are removed.
- Lines containing only ASCII decimal digits (`0` through `9`) are removed.
- Every other line is retained exactly after line-ending removal. In
  particular, surrounding whitespace, whitespace-only lines, mixed
  alphanumeric values, and non-ASCII decimal characters are meaningful.
- Retained entries are sorted with Rust `str` ordering, an ordinal ordering
  over their Unicode scalar values.
- Duplicate entries are retained. The source pipeline used `Sort-Object`
  without `-Unique`, and Hunspell dictionaries permit repeated entries.

The transformation is exposed independently from filesystem operations so its
complete filtering, ordering, counting, and formatting contract can be tested
with in-memory strings.

## Output contract

The generated dictionary is:

1. the retained-entry count in ASCII decimal followed by LF;
2. each sorted entry followed by LF.

Even an empty dictionary is therefore the two bytes `0\n`. Output always has a
final newline and never depends on the host platform's newline convention.

The output parent directory is created when necessary. The complete dictionary
is written and flushed to a temporary file in the destination directory, then
atomically published over the requested path. Staging beside the destination
keeps the rename on one filesystem. A failure before publication leaves the
previous destination untouched, and temporary files are cleaned up by RAII.

## Errors

Library operations return typed `ohno` errors. Diagnostics distinguish input
read and UTF-8 failures from output-directory creation, temporary-file
creation, writing, flushing, and atomic publication failures. Each diagnostic
includes the relevant input or output path and preserves its underlying cause.

## Packaging and compatibility

The package and executable are both named `hunspell-dictgen`. The crate uses
the workspace Rust version and dependency policy and is publishable on
crates.io. Its initial version is `0.1.0`.

## Out of scope

- Hunspell affix generation or validation
- Deduplication or spelling normalization
- Trimming or case folding
- Recursive input discovery
- In-place editing of the human-maintained source
