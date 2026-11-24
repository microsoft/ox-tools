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

The check models a native, default-feature source installation of
`cargo-spellcheck 0.15.7`. Its bundled `hunspell-sys 0.3.1` compiles C++ and
uses `bindgen 0.61.0` without the runtime feature. Therefore
`clang-sys 1.8.1` must find a **link-time** dynamic libclang, not just any
file whose name resembles libclang.

The caller's `rustc -vV` supplies the native host architecture and environment;
the helper's own compilation target does not select the installation target.
Windows (MSVC and GNU), Linux, and macOS are supported. Cross-target
installation is not supported: a differing `CARGO_BUILD_TARGET` is rejected.

Discovery follows clang-sys's order and candidate policy:

- A set `LIBCLANG_PATH` is authoritative, including an empty value. It accepts
  a matching library file or a directory. No other location is searched when
  this override is set. Windows `lib` directories also search sibling `bin`.
- Otherwise, `llvm-config --prefix` (or the executable named by
  `LLVM_CONFIG_PATH`) supplies `bin`, `lib`, and `lib64` search directories.
  macOS also uses `xcode-select --print-path` and its toolchain library directory.
- `LD_LIBRARY_PATH` entries precede clang-sys's platform directory globs.
  Those include versioned LLVM directories and Visual Studio installations,
  as well as system library directories. Directory glob matching ignores case.
- Linux candidates are `libclang.so` and `libclang-*.so`; suffix-only sonames
  such as `libclang.so.1` are not link-time candidates. `libclang-cpp` is
  excluded. Windows accepts `clang.dll` and `libclang.dll`; macOS accepts
  `libclang.dylib`.
- Candidates are validated before selection. Linux ELF class must match the
  host pointer width; Windows PE class and recognized machine architecture
  must match the host. The highest filename version wins, with earlier
  discovery locations winning ties, as in clang-sys.

On Windows MSVC, the selected library's directory (or the sibling `lib` when
the DLL is in `bin`) must contain `libclang.lib` or `libclang.dll.a`.
The selected DLL directory must also be on `PATH` so the linked bindgen build
script can load it. Path comparison ignores ASCII case, separator direction,
and trailing separators after resolving the DLL directory and PATH entries
against the process working directory. Empty PATH entries mean the working
directory. Path-resolution failures retain their underlying cause.

The selected dynamic library's path is made absolute before opening it, so a
bare filename selects that file rather than invoking the loader's library
search. The library is opened to check loader availability and the
`clang_createIndex` export; this detects missing dependent libraries and
non-libclang lookalikes without requiring libclang when building the helper.
The library is not used to generate bindings.
Loader failures identify both the requested rustc host and the helper process
architecture, including guidance to reinstall the helper if a toolchain change
made them incompatible. This does not change target selection or add a separate
process for the probe.

Compiler discovery delegates to `cc` for C and C++, preserving its `CC`,
`CXX`, flags, wrappers, and Visual Studio environment selection. Help/version
commands check that the selected tools launch successfully. Archiver discovery
also uses `cc`; its executable must launch, but a nonzero help/version exit
is allowed because archiver option conventions differ by platform. No native
objects or source fixtures are compiled by the helper.

System Hunspell and system liblzma are not mandatory: the default installation
bundles Hunspell and can build liblzma's C sources when system discovery fails.
Cargo, rustc, a linker, SDK headers, and the C/C++ standard libraries remain
normal Rust/native development prerequisites. Successful probes do not promise
that every header, symbol, SDK, or final link will work; compiling the actual
dependencies remains authoritative.

Absent search locations are skipped. Optional discovery commands may be
unavailable or fail, as upstream permits; their diagnostics are retained if
no valid libclang is found. Invalid candidates are reported when no valid
candidate remains. Unexpected filesystem access failures are reported with
paths instead of hidden. Errors provide platform-specific installation or
environment guidance and never install system packages.

Upstream contracts:
[cargo-spellcheck](https://docs.rs/crate/cargo-spellcheck/0.15.7/source/Cargo.toml),
[hunspell-sys](https://docs.rs/crate/hunspell-sys/0.3.1/source/build.rs),
[clang-sys discovery](https://github.com/KyleMayes/clang-sys/blob/v1.8.1/build/common.rs),
[clang-sys validation and linking](https://github.com/KyleMayes/clang-sys/blob/v1.8.1/build/dynamic.rs).

## Structure and testing

The package is a publishable binary crate at version `0.1.0`, inheriting the
workspace's edition, Rust version, metadata, and lint policy.
The CLI delegates dictionary generation to the existing library. Prerequisite
policy operates on captured environment values and injected filesystem
operations, allowing platform discovery, failure handling, and command
dispatch to be tested in-process without host dependencies or input fixtures.
