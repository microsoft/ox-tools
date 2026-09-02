// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The standalone single-file HTML report.
//!
//! One `.html` file with no external dependencies: no CDN, no fonts, no fetch, no network at all.
//! It opens from disk, from a file share, from a CI artifact, and from an air-gapped machine. A
//! report that needs the network to render is a report nobody can attach to a build.
//!
//! The recipe is the one every Stryker implementation uses: inline the viewer bundle and assign
//! the report as a JavaScript property rather than fetching it.
//!
//! The report always embeds the viewer. A report carries the whole mutated source of the code it
//! describes, and loading a remote script would disclose that source and its results to code whose
//! content this repository cannot authenticate.

use std::io;

use camino::Utf8Path;

use crate::Result;
use crate::elements::Report;
use crate::error::error;

/// The vendored viewer, inlined so the report needs nothing at run time.
const VIEWER: &str = include_str!("vendor/mutation-test-elements.js.vendored");

/// The version of the viewer bundle vendored beside this module.
///
/// Read from the file the vendoring step writes, so the two cannot disagree: there is one place
/// that says which upstream release the bytes came from, and it is the same place the bytes are.
#[cfg(test)]
const VIEWER_VERSION: &str = include_str!("vendor/VERSION").trim_ascii();

/// Renders a complete HTML page for a report.
///
/// # Errors
///
/// Returns an error if the report cannot be serialized into the page.
pub fn render(report: &Report) -> Result<String> {
    let mut page = Vec::new();

    // The page is built by the same streaming path that writes it to disk, so the two cannot drift:
    // writing to a `Vec` cannot fail for I/O, and the report serializes infallibly, so the only way
    // this errors is the case handled below.
    stream(report, &mut page).map_err(|cause| error!("could not serialize the report").caused_by(cause))?;

    String::from_utf8(page).map_err(|cause| error!("could not serialize the report").caused_by(cause))
}

/// Writes the self-contained HTML page for `report` to `path`, streamed so neither the report JSON
/// nor the finished page is ever fully resident.
///
/// The bytes are exactly [`render`]'s — this is the same page, produced by the same streaming path — but
/// the prefix, the embedded report and the suffix go straight into the atomic publication's staging
/// file rather than being concatenated into a `String` first.
///
/// # Errors
///
/// Returns an error if the page cannot be serialized or cannot be published to `path`.
pub fn write_page(report: &Report, path: &Utf8Path) -> Result<()> {
    crate::elements::write_streamed(path, |writer| stream(report, writer))
}

/// Streams the self-contained page into `writer`: the prefix, then the report as the embedded
/// script payload, then the suffix.
///
/// The report is written through [`EscapeScript`] so that a `</script>` in a string literal cannot
/// terminate the element carrying it, matching the escaping the whole-string form used. Splitting
/// the page this way is what keeps neither the report JSON nor the page held whole in memory.
fn stream(report: &Report, writer: &mut dyn io::Write) -> io::Result<()> {
    write!(
        writer,
        "<!DOCTYPE html>\n\
         <html lang=\"en\">\n\
         <head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>cargo-gamma mutation report</title>\n\
         <style>{PAGE_STYLE}</style>\n"
    )?;

    write!(writer, "<script>{VIEWER}</script>")?;

    write!(
        writer,
        "\n\
         </head>\n\
         <body>\n\
         <mutation-test-report-app title-postfix=\"cargo-gamma\">\n\
         Your browser does not support custom elements, which this report is built from.\n\
         </mutation-test-report-app>\n\
         <script>\n\
         const app = document.querySelector('mutation-test-report-app');\n\
         {THEME_SCRIPT}\n\
         app.report = "
    )?;

    {
        let mut escaping = EscapeScript { inner: &mut *writer };

        serde_json::to_writer(&mut escaping, report).map_err(io::Error::from)?;
    }

    write!(writer, ";\n</script>\n</body>\n</html>\n")
}

/// The page's own styling, which is only ever about the area the viewer does not paint.
///
/// `color-scheme` is what stops the browser from rendering its own furniture — scrollbars, form
/// controls, the canvas behind the document — in light colors on a dark page.
///
/// The two background rules are fallbacks for the moments the script cannot cover: the media query
/// paints correctly before the viewer has resolved its theme and when scripting never runs at all,
/// and the attribute rule follows the theme the viewer reflects onto itself, including one the
/// reader picked inside the report that disagrees with the system.
const PAGE_STYLE: &str = "\
    :root { color-scheme: light dark; }\
    html, body { margin: 0; padding: 0; }\
    body { background-color: #fff; }\
    @media (prefers-color-scheme: dark) { body { background-color: #18181b; } }\
    body:has(mutation-test-report-app[theme=\"dark\"]) { background-color: #18181b; }\
    body:has(mutation-test-report-app[theme=\"light\"]) { background-color: #fff; }";

/// Keeps the page background in step with the theme the viewer chose.
///
/// The viewer paints its own components but not the page behind them, and it resolves its theme
/// from a saved preference before falling back to the system one — so the page cannot work the
/// answer out for itself, and a CSS media query alone gets it wrong for anyone who overrode the
/// theme inside the report. Listening for the event the viewer already emits is the only way to
/// read the exact color it settled on.
///
/// Registered before the report is assigned, because assigning it is what starts the update cycle
/// that ends in the event.
const THEME_SCRIPT: &str = "\
    app.addEventListener('theme-changed', (event) => {\
    const color = event.detail.themeBackgroundColor;\
    if (color) { document.body.style.backgroundColor = color; }\
    });";

/// A writer that escapes the two angle brackets so a payload cannot terminate the script element
/// that carries it, applied to the report as it streams into the page.
///
/// An HTML parser looks for the literal characters `</script` inside a script element and stops
/// there, without any knowledge of JavaScript. `serde_json` does not escape `<`, so a crate with
/// `"</script>"` in a string literal would otherwise cut its own report in half — and the tail of
/// the document would be reinterpreted as markup.
///
/// In JSON the only place `<` or `>` can appear is inside a string literal, so rewriting them to
/// their `\u` escapes is safe everywhere in the document and needs no parsing to do correctly.
/// Both are ASCII, so they never occur as a continuation byte of a multi-byte character, which is
/// why rewriting the byte stream is equivalent to rewriting the characters and never corrupts one.
struct EscapeScript<W> {
    inner: W,
}

impl<W: io::Write> io::Write for EscapeScript<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut start = 0;

        for (index, &byte) in buf.iter().enumerate() {
            let escape: &[u8] = match byte {
                b'<' => b"\\u003c",
                b'>' => b"\\u003e",
                _ => continue,
            };

            self.inner.write_all(&buf[start..index])?;
            self.inner.write_all(escape)?;
            start = index + 1;
        }

        self.inner.write_all(&buf[start..])?;

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::fixtures;

    fn report() -> Report {
        fixtures::report()
    }

    #[test]
    fn the_inline_page_carries_the_whole_viewer() {
        let page = render(&report()).expect("renders");

        assert!(page.contains("<mutation-test-report-app"), "the custom element is missing");
        assert!(page.len() > VIEWER.len(), "the viewer was not inlined");
        assert!(!page.contains("cdn.jsdelivr.net"), "the offline report must not reference a CDN");
    }

    /// No rendered page may load code from anywhere but itself.
    ///
    /// This is the whole security property of the report format, so it is asserted over the
    /// executable markup rather than over an option. URL strings embedded in the vendored
    /// JavaScript are inert and do not imply a network load.
    #[test]
    fn the_page_loads_no_code_from_anywhere_else() {
        let page = render(&report()).expect("renders");
        let head = page.split("app.report =").next().expect("the page has a prefix");

        assert!(!head.contains("<script src"), "the viewer must not be fetched");
        assert!(!head.contains("<link "), "the page must not fetch a stylesheet");
    }

    /// The vendored bundle names the upstream release it came from.
    ///
    /// The version is read from the file the vendoring step writes rather than restated in code, so
    /// the marker and bundle are maintained together. It preserves the complete upstream semantic
    /// version, including pre-release or build metadata.
    #[test]
    fn the_vendored_viewer_records_its_upstream_version() {
        assert!(!VIEWER_VERSION.is_empty(), "the vendored viewer has no recorded version");
        cargo_metadata::semver::Version::parse(VIEWER_VERSION)
            .unwrap_or_else(|cause| panic!("`{VIEWER_VERSION}` is not a semantic version: {cause}"));
    }

    #[test]
    fn the_page_follows_the_theme_the_viewer_settled_on() {
        let page = render(&report()).expect("renders");

        assert!(page.contains("color-scheme: light dark"), "{page}");
        assert!(page.contains("prefers-color-scheme: dark"), "{page}");
        assert!(page.contains("theme-changed"), "{page}");
    }

    #[test]
    fn the_theme_listener_is_registered_before_the_report_starts_the_update_cycle() {
        // Assigning the report is what makes the viewer resolve its theme and emit the event, so a
        // listener added afterwards is racing the very update it exists to hear about.
        let page = render(&report()).expect("renders");
        let listener = page.find("addEventListener").expect("the listener is present");
        let assignment = page.find("app.report =").expect("the report is assigned");

        assert!(listener < assignment, "{page}");
    }

    #[test]
    fn a_closing_script_tag_in_the_source_cannot_break_out() {
        // The payload is assigned to a property, so source text is JSON-escaped rather than
        // reproduced into the markup. Getting this wrong turns any crate containing the sequence
        // in a string literal into a broken report.
        let mut subject = report();

        let _ = subject.files.insert(
            "src/lib.rs".to_owned(),
            crate::elements::FileResult {
                source: "let s = \"</script><script>alert(1)</script>\";".to_owned(),
                language: "rust".to_owned(),
                mutants: Vec::new(),
            },
        );

        let page = render(&subject).expect("renders");

        assert!(!page.contains("</script><script>alert(1)"), "{page}");
        assert!(page.contains("\\u003c/script"), "{page}");
    }

    #[test]
    fn escaping_leaves_the_document_valid_json() {
        // The escapes have to be readable back as the original text, or the viewer would render
        // mangled source.
        use std::io::Write as _;

        let mut escaped = Vec::new();
        EscapeScript { inner: &mut escaped }
            .write_all(b"{\"a\":\"x < y > z\"}")
            .expect("writing to a vec cannot fail");
        let escaped = String::from_utf8(escaped).expect("ascii escapes stay valid utf-8");
        let parsed: Value = serde_json::from_str(&escaped).expect("still valid JSON");

        assert_eq!(parsed["a"], "x < y > z");
        assert!(!escaped.contains('<'));
        assert!(!escaped.contains('>'));
    }

    /// The streamed page is byte-for-byte the string form, and it is published atomically — so
    /// writing the report to disk by streaming cannot change the page a browser opens.
    #[test]
    fn the_streamed_page_matches_the_string_form() {
        let report = report();
        let directory = crate::testing::workdir("html-stream-page-");
        let root = camino::Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the scratch path is UTF-8");
        let path = root.join("report.html");

        write_page(&report, &path).expect("streams the page");

        let expected = render(&report).expect("renders");
        assert_eq!(std::fs::read_to_string(path.as_std_path()).expect("published bytes"), expected);
    }
}
