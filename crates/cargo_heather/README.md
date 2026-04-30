<div align="center">
 <img src="./logo.png" alt="Cargo Heather Logo" width="96">

# Cargo Heather

[![crate.io](https://img.shields.io/crates/v/cargo_heather.svg)](https://crates.io/crates/cargo_heather)
[![docs.rs](https://docs.rs/cargo_heather/badge.svg)](https://docs.rs/cargo_heather)
[![MSRV](https://img.shields.io/crates/msrv/cargo_heather)](https://crates.io/crates/cargo_heather)
[![CI](https://github.com/microsoft/ox-tools/actions/workflows/main.yml/badge.svg?event=push)](https://github.com/microsoft/ox-tools/actions/workflows/main.yml)
[![Coverage](https://codecov.io/gh/microsoft/ox-tools/graph/badge.svg?token=FCUG0EL5TI)](https://codecov.io/gh/microsoft/ox-tools)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
<a href="../.."><img src="../../logo.svg" alt="This crate was developed as part of the Oxidizer project" width="20"></a>

</div>

## cargo-heather

Library for validating and rewriting license headers in Rust (`.rs`) and
TOML (`.toml`) source files. The accompanying `cargo-heather` binary uses
this library to discover files on disk and apply the rewrites.

### Public API

The library is intentionally minimal: a pair of stream-based functions
that operate on any [`std::io::Read`][__link0] / [`std::io::Write`][__link1].

* [`check`][__link2] reads content and reports whether the expected header is
  present, missing, or mismatched.
* [`fix`][__link3] reads content and writes the fixed-up content.

Callers are responsible for opening files, deciding which paths to
process, and writing results back to disk.

```rust
use cargo_heather::{check, fix, CheckResult, FileKind};

let input = b"fn main() {}\n";
let header = "Licensed under the MIT License.";

// Check whether the header is present.
let result = check(&input[..], header, FileKind::Rust).unwrap();
assert_eq!(result, CheckResult::Missing);

// Produce a fixed copy.
let mut output: Vec<u8> = Vec::new();
fix(&input[..], &mut output, header, FileKind::Rust).unwrap();
assert!(output.starts_with(b"// Licensed under the MIT License.\n"));
```

### Supported file kinds

* [`FileKind::Rust`][__link4] — regular Rust source (`//` comments).
* [`FileKind::Toml`][__link5] — TOML files (`#` comments).
* [`FileKind::CargoScript`][__link6] — Rust script with shebang + `---`
  frontmatter; the header lives inside the frontmatter using `#`.

Use [`FileKind::detect`][__link7] (or [`is_cargo_script`][__link8]) to classify a file
from its path and content before calling [`check`][__link9] / [`fix`][__link10].

### License header lookup

The [`license`][__link11] module maps SPDX identifiers to canonical short header
strings; this is what the binary uses when no custom header is supplied.


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo_heather">source code</a>.
</sub>

 [__cargo_doc2readme_dependencies_info]: ggGmYW0CYXZlMC43LjJhdIQbYLuo4OFUWT8bvMCT2d1BCU8bCvLHCBSvMr0bKR38GpAvnJ5hYvRhcoQbIO-yhm33vhYbzeULwGvlFEwbtCQvvMS9iNAbir3G8ex7VL9hZIGCbWNhcmdvX2hlYXRoZXJlMC4xLjA
 [__link0]: https://doc.rust-lang.org/stable/std/?search=io::Read
 [__link1]: https://doc.rust-lang.org/stable/std/?search=io::Write
 [__link10]: https://docs.rs/cargo_heather/0.1.0/cargo_heather/?search=fix
 [__link11]: https://docs.rs/cargo_heather/0.1.0/cargo_heather/license/index.html
 [__link2]: https://docs.rs/cargo_heather/0.1.0/cargo_heather/?search=check
 [__link3]: https://docs.rs/cargo_heather/0.1.0/cargo_heather/?search=fix
 [__link4]: https://docs.rs/cargo_heather/0.1.0/cargo_heather/?search=FileKind::Rust
 [__link5]: https://docs.rs/cargo_heather/0.1.0/cargo_heather/?search=FileKind::Toml
 [__link6]: https://docs.rs/cargo_heather/0.1.0/cargo_heather/?search=FileKind::CargoScript
 [__link7]: https://docs.rs/cargo_heather/0.1.0/cargo_heather/?search=FileKind::detect
 [__link8]: https://docs.rs/cargo_heather/0.1.0/cargo_heather/?search=is_cargo_script
 [__link9]: https://docs.rs/cargo_heather/0.1.0/cargo_heather/?search=check
