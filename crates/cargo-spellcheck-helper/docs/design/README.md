# cargo-spellcheck-helper design

## Purpose

`cargo-spellcheck-helper` replaces the platform-specific scripting needed to
prepare a repository dictionary and check cargo-spellcheck's source-build
prerequisites. It does not implement spell checking or install system packages.
The helper itself builds without libclang or cargo-spellcheck.

## Commands

```text
cargo spellcheck-helper prepare-dictionary --input .spelling --output target/spelling.dic
cargo spellcheck-helper check-source-deps
```

The executable also accepts direct invocation as `cargo-spellcheck-helper`
with the same commands. Success exits with code 0 and writes nothing to stdout.
Invalid arguments exit with code 2. Operational failures exit with code 1 and
an actionable diagnostic on stderr.

### Dictionary preparation

Both paths are explicit and relative to the caller's working directory.
Generation delegates to the `hunspell-dictgen` library: missing input means an
empty dictionary, empty and ASCII-decimal-only lines are removed, remaining
entries are sorted in ordinal order with duplicates retained, and the count
is written before the entries. UTF-8 output uses LF and a final newline.
Publication is atomic and creates the output parent directory as needed.
Other input, decoding, and output failures are not treated as missing input.

### Source-build prerequisites

The check supports Windows, Linux, and macOS. It searches `LIBCLANG_PATH`
first, then these conventional directories in order:

- Windows: `C:\Program Files\LLVM\bin`, then
  `%USERPROFILE%\scoop\apps\llvm\current\bin` when `USERPROFILE` is set.
- macOS: `/usr/local/opt/llvm/lib`, `/opt/homebrew/opt/llvm/lib`.
- Linux: `/usr/lib/x86_64-linux-gnu`, `/usr/lib/aarch64-linux-gnu`,
  `/usr/lib64`.

The expected library filenames are `libclang.dll` on Windows,
`libclang.dylib` on macOS, and `libclang.so` or `libclang.so.1` on Linux.
If Linux's initial search fails, the check scans `/usr/lib`, `/usr/lib64`,
`/usr/lib/x86_64-linux-gnu`, and `/usr/lib/aarch64-linux-gnu` for regular
files named `libclang*.so*`, considering filenames in sorted order.
Symlinks to files are accepted.

An absent candidate is skipped; filesystem failures such as permission denial
are reported with the affected path rather than hidden. The check establishes
file presence, not ABI compatibility or loadability.

On Windows the selected directory must also be an entry in `PATH`.
Comparison ignores ASCII case, separator direction, and trailing separators;
it does not equate aliases or resolve symbolic links. If a library is found
but its directory is absent from `PATH`, the diagnostic identifies the directory
and explains how to set both `LIBCLANG_PATH` and `PATH`.

A missing library produces Linux package-manager, Homebrew, or Windows
LLVM installation guidance appropriate to the current platform. Unsupported
platforms fail explicitly.

## Integration boundary

Just remains responsible for tool-version validation and installation.
Install this helper before using `check-source-deps` for a cargo-spellcheck
source install or prebuilt-install source fallback. A satisfied installed
version or successful prebuilt installation does not need the source probe.

Dictionary preparation precedes the existing cargo-spellcheck command; Just
propagates either command's failure. Anvil recipe adoption, Windows shell
configuration, container recipes, and CI bootstrap migration are separate
changes.

## Structure and testing

The package is a publishable binary crate at version `0.1.0`, inheriting the
workspace's edition, Rust version, metadata, and lint policy.
The CLI delegates dictionary generation to the existing library. Prerequisite
policy operates on captured environment values and injected filesystem
operations, allowing platform discovery, failure handling, and command
dispatch to be tested in-process without host dependencies or input fixtures.
