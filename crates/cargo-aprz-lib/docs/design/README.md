# cargo-aprz-lib design

This crate implements cargo-aprz's commands, fact collection, metric
extraction, policy-expression evaluation, and reports. It is an implementation
detail of the command-line tool, not a stable library API.

## Policy string sizes

For string metrics in custom policies, both `size(crate.description)` and
`crate.description.size()` count Unicode scalar values, not UTF-8 bytes, UTF-16
code units, or grapheme clusters. An empty string has size 0, and `"rust"` has size 4.
U+00E9 and the supplementary-plane character U+1F980 each have size 1. In contrast,
`e` followed by the combining acute accent U+0301 has size 2, even when displayed
as one character.

For list metrics, `size(crate.owners)` and `crate.owners.size()` count elements.
The built-in policy's `size(crate.owners) >= 2` check is therefore unaffected by
string-size semantics.

## Native provider acceptance

User owners are identified by the crates.io `username` exported in `users.csv`,
not necessarily their GitHub login. Older dumps with `gh_login` instead of
`username` remain accepted. Missing identity columns are errors, not anonymous
owners. When both columns exist, `username` takes precedence. The binary cache
stores the same ID, identity, and display-name layout
for both CSV schemas.

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

## HTTP and advisory acquisition

HTTP requests use the public `fetch` crate's Tokio transport with native TLS.
Coverage, documentation, and hosting providers can be constructed and cloned
without a Tokio runtime. They create their shared transport on the first async
request inside Tokio, using the same request resilience configuration.
Connections are direct; environment HTTP proxy settings are not consumed.
Windows and macOS use their platform TLS implementations; Linux builds require
supported, patched OpenSSL 3 development packages, dynamically linked. The
Fetch request pipeline configures native retry and timeout middleware:
three retries for transport errors,
5xx, 429, and 403 with `Retry-After`. Downloads retry the complete transfer on
body failures through a separate whole-transfer Seatbelt wrapper. Transport errors
and unsuccessful HTTP responses that exhaust request resilience are returned
without restarting documentation or crates.io dump downloads.
Request timeouts apply to each attempt at each redirect hop. Responses remain streamed for large database and documentation
downloads.
Small response bodies (coverage badges, hosting API JSON, and documentation error
details) have a separate one-minute read deadline after headers arrive. Body
timeouts do not restart request retries. Large downloads retain their longer
transfer-specific deadlines.

GET requests follow at most ten HTTP(S) redirects, including relative locations.
Authorization is sensitive and is permanently removed when a redirect changes
origin. The user agent remains `cargo-aprz`.

Advisories use an explicit gzip-compressed tar archive endpoint, defaulting to
`https://github.com/RustSec/advisory-db/archive/refs/heads/main.tar.gz`.
`--advisory-archive-url` and `APRZ_ADVISORY_ARCHIVE_URL` replace the former
repository-address override; there is no legacy alias or repository-to-archive
URL inference. Local HTTP mirrors and arbitrary archive URLs are supported.
The archive contains a `crates/` directory either directly or beneath one
top-level directory, as in GitHub source archives.

Extraction accepts only regular files and directories, refuses paths outside
the destination and filesystem links, and imposes entry-count and uncompressed
size limits. All gzip members are decoded and validated under one uncompressed
size budget. A staged database must parse successfully before it is published.
Each transfer attempt uses its own temporary file so a timed-out write cannot
corrupt a retry. The advisory body-transfer timeout starts after response headers
arrive; it does not interrupt request retries or `Retry-After` waits.
Advisory HTTP failures that exhaust request resilience and
compressed-size violations do not trigger whole-transfer retries. Failed
downloads or extraction never replace the last usable database or mark the
refresh successful. Cache identity uses a BLAKE3 fingerprint of the complete
archive URL, including credentials and query parameters, without storing the URL
itself. Changing mirrors or signed URLs does not reuse another endpoint's
freshness marker. Successful publication also
removes the legacy Git checkout; failed refreshes leave it intact. RustSec is
used only for parsing and queries, with its Git features disabled; advisory
collection requires neither a Git executable nor RustSec's HTTP transports.

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
