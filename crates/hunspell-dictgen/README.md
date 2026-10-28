<div align="center">
 <img src="./logo.png" alt="Hunspell-Dictgen Logo" width="96">

# Hunspell-Dictgen

[![crates.io](https://img.shields.io/crates/v/hunspell-dictgen.svg)](https://crates.io/crates/hunspell-dictgen)
[![docs.rs](https://docs.rs/hunspell-dictgen/badge.svg)](https://docs.rs/hunspell-dictgen)
[![MSRV](https://img.shields.io/crates/msrv/hunspell-dictgen)](https://crates.io/crates/hunspell-dictgen)
[![CI](https://github.com/microsoft/ox-tools/actions/workflows/anvil-scheduled.yml/badge.svg)](https://github.com/microsoft/ox-tools/actions/workflows/anvil-scheduled.yml)
[![Coverage](https://codecov.io/gh/microsoft/ox-tools/graph/badge.svg?token=FCUG0EL5TI)](https://codecov.io/gh/microsoft/ox-tools)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
<a href="../.."><img src="../../logo.svg" alt="This crate was developed as part of the Oxidizer project" width="20"></a>

</div>

Generate deterministic Hunspell dictionaries from human-maintained word
lists.

`hunspell-dictgen` converts a UTF-8 text file containing one entry per line
into Hunspell’s `.dic` format. It removes empty and ASCII-decimal-only
lines, sorts the remaining entries using Rust string ordering, retains
duplicates, and prefixes the entries with their count.

Missing input is accepted and produces an empty dictionary. This makes an
optional repository word list straightforward to use in build and CI
recipes without hiding other input failures.

## Installation

```text
cargo install hunspell-dictgen
```

## Usage

```text
hunspell-dictgen --input .spelling --output target/spelling.dic
```

The output always uses LF line endings and ends with a newline. Parent
directories are created as needed, and the destination is replaced
atomically from a temporary file in the same directory.


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/hunspell-dictgen">source code</a>.
</sub>

