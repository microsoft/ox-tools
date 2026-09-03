// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The live progress display, rendered as a `cargo`-style bar.

use core::time::Duration;
use std::io::Write;
use std::time::Instant;

use super::Styler;
use super::text::{VERB_WIDTH, continuation, quantity};
use crate::advise::human;
use crate::commands::Host;
use crate::model::Outcome;
use crate::report::{encode_controls, encode_preserving_color};

/// Shortest interval between redraws.
///
/// Redrawing on every event makes a fast run spend real time on terminal writes, and produces a
/// flicker nobody can read anyway.
const REDRAW_INTERVAL: Duration = Duration::from_millis(100);

/// Enough completed work to keep one unusually fast launch from becoming the first ETA.
const MIN_ETA_SAMPLES: usize = 3;

/// Width of the gauge itself, in columns.
///
/// The gauge is a fixed size rather than whatever the caption leaves over. Sizing it from the
/// remainder makes it collapse to nothing the moment the caption grows, and makes it twitch by a
/// column every time a counter gains a digit.
const BAR_WIDTH: usize = 25;

/// The live progress display.
///
/// Every subject that reaches this type is control-character encoded on the way in, because the
/// display writes real escape sequences of its own and a terminal cannot distinguish sequences in
/// a file name from ours. Encoding on entry rather than at each write is what makes the guarantee hold for
/// the subject a phase holds open across a build and rewrites later: it is stored encoded, so it
/// cannot be reintroduced raw by whichever path finally paints it. Labels are exempt because this
/// tool composes them itself; a tool's relayed output is encoded by the policy that keeps colour.
#[derive(Debug)]
pub struct Progress {
    enabled: bool,

    /// Whether [`begin`](Self::begin) has written a line that [`end`](Self::end) has not closed.
    open: bool,

    /// The active label, completed label, and subject of the phase [`begin`](Self::begin) opened,
    /// so that [`end`](Self::end) can write the completed line and [`abandon`](Self::abandon) can
    /// restore the active line when something interrupted it.
    pending: Option<(String, String, String)>,

    /// The borrowed line currently on screen, so an unchanged one is not repainted.
    shown: Option<String>,
    styler: Styler,
    width: usize,
    last_draw: Option<Instant>,
    dirty: bool,
    total: usize,
    done: usize,
    survived: usize,
    timeouts: usize,
    out_of_memory: usize,
    started: Option<Instant>,
}

impl Progress {
    /// Creates a display.
    ///
    /// `enabled` is the already-resolved decision, so this type never has to know what a terminal
    /// is; `width` is the terminal width if there is one.
    #[must_use]
    pub fn new(enabled: bool, styler: Styler, width: Option<u16>) -> Self {
        Self {
            enabled,
            open: false,
            pending: None,
            shown: None,
            styler,
            width: width.map_or(80, |value| usize::from(value).max(20)),
            last_draw: None,
            dirty: false,
            total: 0,
            done: 0,
            survived: 0,
            timeouts: 0,
            out_of_memory: 0,
            started: None,
        }
    }

    /// Sets the number of mutants that are about to be tested, and starts the clock the time
    /// estimate is derived from.
    pub fn set_total(&mut self, total: usize) {
        self.total = total;
        self.started = Some(Instant::now());
        self.dirty = true;
    }

    /// Records one evaluated mutant.
    pub const fn record(&mut self, outcome: Outcome) {
        self.done += 1;

        match outcome {
            Outcome::Survived => self.survived += 1,
            Outcome::Timeout => self.timeouts += 1,
            Outcome::OutOfMemory => self.out_of_memory += 1,
            _ => {}
        }

        self.dirty = true;
    }

    /// Returns the completed fraction, clamped to `0.0..=1.0`.
    #[must_use]
    pub fn fraction(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }

        #[expect(clippy::cast_precision_loss, reason = "a mutant count far exceeds any plausible workspace")]
        let fraction = self.done as f64 / self.total as f64;

        fraction.clamp(0.0, 1.0)
    }

    /// Estimates the time left, by extrapolating from the rate achieved so far.
    ///
    /// Extrapolation beats the up-front projection here because it needs no model: it absorbs the
    /// job count, the machine's actual throughput, and the share of mutants that hang, all of which
    /// the projection can only guess at. It is worthless until a few mutants have finished, so it
    /// is reported as absent rather than as a wild number.
    fn remaining(&self) -> Option<Duration> {
        let started = self.started?;

        if self.done < MIN_ETA_SAMPLES || self.done >= self.total {
            return None;
        }

        #[expect(clippy::cast_precision_loss, reason = "a mutant count far exceeds any plausible workspace")]
        let (done, left) = (self.done as f64, (self.total - self.done) as f64);

        Duration::try_from_secs_f64(started.elapsed().as_secs_f64() / done * left).ok()
    }

    /// Writes a completed status line, above the progress bar.
    ///
    /// Status lines belong to the progress display and are suppressed with it. They also duplicate
    /// what the summary reports, so emitting them when progress is off would put every survivor on
    /// screen twice.
    pub fn status<H: Host>(&mut self, host: &mut H, verb: &str, subject: &str) {
        let label = self.styler.verb(verb);

        self.line(host, &label, &encode_controls(subject));
    }

    /// Opens a status line and leaves it open, so that what the phase found can be added to it once
    /// it is known.
    ///
    /// A phase that names what it is about to do and then goes quiet for a minute is easier to sit
    /// through than one that says nothing until it is done — but the counts it reports do not exist
    /// until then, and putting them on a second line makes the sequence twice as long as the work
    /// it describes.
    pub fn begin<H: Host>(&mut self, host: &mut H, active: &str, completed: &str, subject: &str) {
        if !self.enabled {
            return;
        }

        self.clear(host);

        let active = self.styler.verb(active);
        let completed = self.styler.verb(completed);
        let subject = encode_controls(subject).into_owned();
        paint(host, &format!("{active} {subject}"));

        self.open = true;
        self.dirty = true;
        self.pending = Some((active, completed, subject));
    }

    /// Closes the line [`begin`](Self::begin) opened.
    ///
    /// A phase that had to print something mid-flight — a build's progress bar, a compiler error —
    /// no longer has its opening line to append to, so the whole line is written again with the
    /// ending attached. Putting only the ending on a line of its own reads as a fragment: the
    /// count means nothing without the name of what was counted, and the name is by then several
    /// screens up.
    pub fn end<H: Host>(&mut self, host: &mut H, subject: &str) {
        self.close(host, subject, true);
    }

    /// Closes the line [`begin`](Self::begin) opened, replacing its in-progress subject with the
    /// completed result.
    pub fn complete<H: Host>(&mut self, host: &mut H, subject: &str) {
        self.close(host, subject, false);
    }

    /// Closes an open phase, either extending or replacing its in-progress subject.
    fn close<H: Host>(&mut self, host: &mut H, subject: &str, extend: bool) {
        if !self.enabled {
            return;
        }

        let encoded = encode_controls(subject);
        let subject = encoded.as_ref();
        let pending = self.pending.take();

        if !self.open {
            match pending {
                Some((_, completed, opening)) => {
                    let completed_subject = if extend {
                        format!("{opening}{subject}")
                    } else {
                        subject.to_owned()
                    };

                    self.line(host, &completed, &completed_subject);
                }
                None => self.line(host, &continuation(), subject.trim_start().trim_start_matches(',').trim_start()),
            }

            return;
        }

        let Some((_, completed, opening)) = pending else {
            paint(host, &format!("{subject}\n"));
            self.open = false;
            self.dirty = true;
            return;
        };

        let completed_subject = if extend {
            format!("{opening}{subject}")
        } else {
            subject.to_owned()
        };

        paint(host, &format!("\r\x1b[2K{completed} {completed_subject}\n"));

        self.open = false;
        self.dirty = true;
    }

    /// Ends an open phase line that will never get the ending it was waiting for.
    ///
    /// A phase names what it is about to do and holds the line open until it can say what it
    /// found. When it fails instead, nothing closes the line, and whatever is printed next — an
    /// error, most of the time — is run onto the end of it, so the failure reads as part of the
    /// sentence that was describing the work.
    pub fn abandon<H: Host>(&mut self, host: &mut H) {
        if !self.open {
            // A borrowed line took the opening off the screen so that it could be written again
            // whole. The ending never came, so put it back: an error with no phase above it does
            // not say what was being attempted.
            if let Some((active, _, subject)) = self.pending.take() {
                self.clear(host);

                paint(host, &format!("{active} {subject}\n"));

                self.dirty = true;
            }

            return;
        }

        paint(host, "\n");

        self.open = false;
        self.dirty = true;
    }

    /// Marks an open phase line as retracted, keeping what it said for [`end`](Self::end).
    ///
    /// A build's progress bar needs the line the phase is sitting on. Closing that line with a
    /// newline would commit it, and the ending would then have to be written as a second copy of
    /// the whole line — the phase would appear twice, once without its ending and once with.
    /// Erasing it instead leaves one line, written when there is something complete to say.
    fn retract(&mut self) {
        if !self.open {
            return;
        }

        self.open = false;
        self.shown = None;
    }

    /// Restores the active phase line after a borrowed build-progress row is released.
    pub fn restore<H: Host>(&mut self, host: &mut H) {
        if !self.enabled || self.open {
            return;
        }

        let Some((active, _, subject)) = self.pending.as_ref() else {
            self.clear(host);

            return;
        };

        // Replacing the borrowed row and restoring the phase is one write. An erase followed by a
        // second write leaves a visible blank frame on the Windows console.
        paint(host, &format!("\r\x1b[2K{active} {subject}"));

        self.last_draw = None;
        self.shown = None;
        self.open = true;
        self.dirty = true;
    }

    /// Draws a completed/total bar for the phase currently in progress.
    pub fn phase_progress<H: Host>(&mut self, host: &mut H, completed: usize, total: usize, unit: &str) {
        if total == 0 {
            return;
        }

        let Some((active, _, _subject)) = self.pending.as_ref() else {
            return;
        };

        let filled = completed.saturating_mul(BAR_WIDTH).checked_div(total).unwrap_or(0).min(BAR_WIDTH);
        let mut bar = String::with_capacity(BAR_WIDTH);

        if filled > 0 {
            for _ in 0..filled - 1 {
                bar.push('=');
            }

            bar.push(if filled == BAR_WIDTH { '=' } else { '>' });
        }

        for _ in filled..BAR_WIDTH {
            bar.push(' ');
        }

        let line = format!("{active} [{bar}] {completed}/{total} {}", encode_controls(unit));

        self.draw_borrowed(host, &line);
    }

    /// Writes a status line under a caller-supplied label, which must already be styled and
    /// aligned. Used where the label is not a verb, so that one thing is not given two names.
    pub fn labelled<H: Host>(&mut self, host: &mut H, label: &str, subject: &str) {
        self.line(host, label, &encode_controls(subject));
    }

    /// Writes a status line under a caller-supplied label, whether or not the display is on.
    ///
    /// Everything else here is suppressed with the display, because the display is off when output
    /// is piped and progress chatter in a log file is noise. Output a user asked for by name is not
    /// chatter: `--show-build` exists to be read in CI, which is exactly the environment the
    /// display's terminal heuristic turns itself off in, so routing it through the display would
    /// let a heuristic overrule an explicit request.
    pub fn insist<H: Host>(&mut self, host: &mut H, label: &str, subject: &str) {
        self.insist_encoded(host, label, &encode_controls(subject));
    }

    /// Relays one line of another tool's output, keeping its color and bold diagnostic styling.
    ///
    /// Separate from [`insist`](Self::insist) because the two carry different text under different
    /// rules. This tool's own sentences never contain an escape, so an escape in one is an
    /// injection; a compiler's output legitimately arrives colored and bold, and reprinting it
    /// with the styling spelled out as `\e[1;31m` would make `--show-build` unreadable. Other SGR
    /// effects and terminal controls are encoded, and a trusted reset contains the accepted style
    /// to this line.
    pub(crate) fn relay<H: Host>(&mut self, host: &mut H, label: &str, line: &str) {
        self.insist_encoded(host, label, &encode_preserving_color(line));
    }

    /// Writes an already-encoded line under `label`, with or without the display.
    fn insist_encoded<H: Host>(&mut self, host: &mut H, label: &str, subject: &str) {
        if self.enabled {
            self.line(host, label, subject);

            return;
        }

        paint(host, &format!("{label} {subject}\n"));
    }

    /// Writes one line above the bar.
    ///
    /// The subject must already be encoded: every caller reaching here has applied one of the two
    /// policies, and encoding again would turn a relayed `\e[31m` into `\\e[31m`.
    fn line<H: Host>(&mut self, host: &mut H, label: &str, subject: &str) {
        if !self.enabled {
            return;
        }

        // A phase holds its line open until it can say what it found, so anything printed
        // meanwhile would otherwise be run onto the end of that unfinished sentence.
        self.abandon(host);
        self.clear(host);

        paint(host, &format!("{label} {subject}\n"));

        self.dirty = true;
    }

    /// Whether anything is actually drawn.
    ///
    /// A caller that has to avoid saying the same thing twice needs to know whether this said it
    /// the first time.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Redraws the progress bar if enough time has passed and something changed.
    pub fn tick<H: Host>(&mut self, host: &mut H) {
        if !self.enabled || !self.dirty {
            return;
        }

        let now = Instant::now();

        if self.last_draw.is_some_and(|last| now.duration_since(last) < REDRAW_INTERVAL) {
            return;
        }

        self.last_draw = Some(now);
        self.dirty = false;
        self.shown = None;

        let line = self.render();
        paint(host, &format!("\r\x1b[2K{line}"));
    }

    /// Erases the progress bar.
    pub fn clear<H: Host>(&mut self, host: &mut H) {
        if !self.enabled || self.last_draw.is_none() {
            return;
        }

        self.last_draw = None;
        self.shown = None;

        paint(host, "\r\x1b[2K");
    }

    /// Draws a line another program owns where the bar goes, throttled and erasable like the bar.
    ///
    /// Used for cargo's own progress bar during a build. Cargo already knows the unit graph and so
    /// knows the denominator, which nothing on this side can obtain on stable; rendering its line
    /// rather than a reconstruction also means the build reads exactly as it does under plain
    /// `cargo`, which is the point of the display.
    ///
    /// The line an open phase left unterminated is taken off the screen first, because the erase
    /// sequence this writes would otherwise wipe it. It is written again, whole, when the phase can
    /// say what it found.
    ///
    /// Cargo redraws its bar far more often than the redraw interval, and the reader splits on
    /// carriage returns, so most calls arrive inside the interval with nothing new to say. Those are
    /// dropped before anything is written, and an unchanged line is dropped too even once the
    /// interval has passed: erasing and rewriting identical text is invisible on a console that
    /// coalesces writes within a frame and a visible flash on one that paints each write as it
    /// arrives, which is the difference between how this looked on Unix and on Windows.
    pub fn borrowed<H: Host>(&mut self, host: &mut H, line: &str) {
        if !self.enabled {
            return;
        }

        // The throttle is checked before encoding as well as after, because cargo redraws its bar
        // far more often than the interval and most calls have nothing to say: encoding a line that
        // is about to be dropped is work nobody sees.
        if self
            .last_draw
            .is_some_and(|last| Instant::now().duration_since(last) < REDRAW_INTERVAL)
        {
            return;
        }

        self.draw_borrowed(host, &encode_preserving_color(line));
    }

    /// Draws an already-encoded borrowed line.
    ///
    /// Split from [`borrowed`](Self::borrowed) so the phase bar this type composes itself — whose
    /// only untrusted part, the unit, is encoded where it enters — is not encoded a second time
    /// and stripped of the styling its own label carries.
    fn draw_borrowed<H: Host>(&mut self, host: &mut H, line: &str) {
        if !self.enabled {
            return;
        }

        let now = Instant::now();

        if self.last_draw.is_some_and(|last| now.duration_since(last) < REDRAW_INTERVAL) {
            return;
        }

        let line = truncate(line, self.width);

        if self.shown.as_deref() == Some(line.as_str()) && !self.open {
            return;
        }

        // State first, then one atomic replacement of the open phase with Cargo's bar. Retracting
        // with a separate erase write is the blank frame that made this transition flicker.
        self.retract();

        self.last_draw = Some(now);

        paint(host, &format!("\r\x1b[2K{line}"));

        self.shown = Some(line);
    }

    /// Erases the progress bar for good.
    pub fn finish<H: Host>(&mut self, host: &mut H) {
        self.clear(host);
        self.dirty = false;
    }

    /// Renders the bar as a string.
    ///
    /// The arrowhead is counted as part of the filled run, exactly as cargo does it, so the bar
    /// does not gain a column when it starts and lose one when it completes.
    ///
    /// On a narrow terminal the caption is shortened rather than cut. Truncation takes the columns
    /// from the right, which is where the time remaining lives, so cutting a terminal a few columns
    /// short there would lose the most volatile part of the line and leave a dangling ellipsis.
    /// Dropping the running verdict counts instead keeps a line that is still worth reading: how far
    /// along the run is, and how much longer it has. The counts are recoverable from the survivors
    /// already printed above, and from the summary at the end.
    #[must_use]
    pub fn render(&self) -> String {
        let estimate = self
            .remaining()
            .map_or_else(String::new, |remaining| format!(", ETA ~{}", human(remaining)));

        #[expect(clippy::cast_precision_loss, reason = "the operand is a bar width")]
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the value is bounded by the bar width"
        )]
        let filled = (self.fraction() * BAR_WIDTH as f64) as usize;

        let filled = filled.min(BAR_WIDTH);
        let mut bar = String::with_capacity(BAR_WIDTH);

        if filled > 0 {
            for _ in 0..filled - 1 {
                bar.push('=');
            }

            bar.push(if filled == BAR_WIDTH { '=' } else { '>' });
        }

        for _ in filled..BAR_WIDTH {
            bar.push(' ');
        }

        let room = self.width.saturating_sub(VERB_WIDTH + 1);
        let mut findings = Vec::with_capacity(3);

        if self.survived > 0 {
            findings.push(format!("{} survived", self.survived));
        }

        if self.timeouts > 0 {
            findings.push(quantity(self.timeouts, "timeout"));
        }

        if self.out_of_memory > 0 {
            findings.push(format!("{} out of memory", self.out_of_memory));
        }

        let verdicts = if findings.is_empty() {
            String::new()
        } else {
            format!(" ({})", findings.join(", "))
        };
        let counted = format!("[{bar}] {}/{} mutants evaluated", self.done, self.total);
        let full = format!("{counted}{verdicts}{estimate}");

        let body = if full.chars().count() <= room {
            full
        } else {
            // Everything after the bar is optional, in the order it is least useful.
            let shorter = format!("{counted}{estimate}");

            if shorter.chars().count() <= room {
                shorter
            } else {
                truncate(&shorter, room)
            }
        };

        // The verb is styled after truncating, because the escape sequences that style it are not
        // columns and would otherwise be counted as though they were.
        format!("{} {body}", self.styler.verb("Testing"))
    }
}

/// Shortens text to a column count, measuring what a terminal would show rather than what the
/// string contains.
///
/// The distinction matters because the only text passed through here that gamma did not write is
/// cargo's progress bar, which arrives styled. Counting its escape sequences as columns would
/// truncate a line that fits, and taking characters by count could cut an escape in half and leave
/// the terminal reading the rest of the line as a command.
fn truncate(text: &str, width: usize) -> String {
    if visible_width(text) <= width {
        return text.to_owned();
    }

    let keep = width.saturating_sub(3);
    let mut kept = String::with_capacity(text.len());
    let mut shown = 0;
    let mut styled = false;
    let mut characters = text.chars();

    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            styled = true;
            kept.push(character);

            if let Some(next) = characters.next() {
                kept.push(next);

                if next == '[' {
                    for byte in characters.by_ref() {
                        kept.push(byte);

                        if matches!(byte, '\u{40}'..='\u{7e}') {
                            break;
                        }
                    }
                }
            }

            continue;
        }

        if shown == keep {
            break;
        }

        kept.push(character);
        shown += 1;
    }

    kept.push_str("...");

    // The escapes kept above are unterminated once the text carrying them is cut, so the styling
    // would otherwise run on into whatever is printed next.
    if styled {
        kept.push_str("\u{1b}[0m");
    }

    kept
}

/// Writes one complete terminal update in a single call.
///
/// Every redraw here goes through this rather than through `write!`, and the reason is a flicker
/// that only appeared on Windows. The diagnostic stream is unbuffered, and `write!` with a format
/// string issues one write per literal and per interpolated argument — so
/// `write!(stream, "\r\x1b[2K{line}")` was two of them: the erase arrived first, leaving the row
/// blank, and the text arrived second. A terminal that repaints asynchronously never displays that
/// gap, which is why it looked correct on Unix, but the Windows console paints each write as it
/// arrives and so drew the blank row and then the text on every redraw. At ten redraws a second
/// that is a continuous flicker.
///
/// Composing the update first and handing it over whole makes it atomic as far as any console is
/// concerned. It costs one short-lived allocation against a hundred-millisecond interval.
fn paint<H: Host>(host: &mut H, update: &str) {
    let mut stream = host.error();

    let _ = stream.write_all(update.as_bytes());
    let _ = stream.flush();
}

/// The number of columns text occupies, ignoring the escape sequences that occupy none.
fn visible_width(text: &str) -> usize {
    crate::report::unstyled(text).chars().count()
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;
    use crate::testing::Sink;

    /// A stream that records how many separate writes reached it, not just the bytes.
    ///
    /// The flicker this guards against is invisible in the concatenated bytes: erase-then-text and
    /// erase-plus-text produce identical output and a completely different display on a console that
    /// paints each write as it arrives. Only the call count tells them apart.
    #[derive(Default)]
    struct Counted {
        bytes: Vec<u8>,
        writes: usize,
    }

    impl Write for &mut Counted {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            self.bytes.extend_from_slice(buf);

            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A host whose diagnostic stream counts writes.
    #[derive(Default)]
    struct CountingHost {
        err: Counted,
        out: Vec<u8>,
    }

    impl Host for CountingHost {
        fn output(&mut self) -> impl Write {
            &mut self.out
        }

        fn error(&mut self) -> impl Write {
            &mut self.err
        }

        fn is_terminal(&self) -> bool {
            true
        }

        fn terminal_width(&self) -> Option<u16> {
            Some(80)
        }
    }

    /// Renders a bar over a population of a hundred mutants, `done` of which have been evaluated.
    fn bar(done: usize, width: u16) -> String {
        let mut progress = Progress::new(true, Styler::new(false), Some(width));

        progress.set_total(100);

        for _ in 0..done {
            progress.record(Outcome::Killed);
        }

        progress.render()
    }

    #[test]
    fn an_empty_bar_has_no_arrowhead() {
        let rendered = bar(0, 80);

        assert!(rendered.contains("[    "), "{rendered}");
        assert!(!rendered.contains('>'), "{rendered}");
    }

    #[test]
    fn a_partial_bar_ends_in_an_arrowhead() {
        let rendered = bar(50, 80);

        assert!(rendered.contains("=>"), "{rendered}");
    }

    #[test]
    fn a_full_bar_has_no_arrowhead() {
        let rendered = bar(100, 80);

        assert!(!rendered.contains('>'), "{rendered}");
        assert!(rendered.contains("==="), "{rendered}");
    }

    /// The bracketed bar on its own, without the caption that follows it.
    fn gauge(rendered: &str) -> String {
        rendered
            .split_once('[')
            .and_then(|(_, tail)| tail.split_once(']'))
            .map(|(bar, _)| bar.to_owned())
            .expect("the bar is bracketed")
    }

    #[test]
    fn the_arrowhead_is_counted_inside_the_filled_run() {
        // Otherwise the bar would visibly widen at the start of the run and narrow at the end.
        let empty = gauge(&bar(0, 80));
        let half = gauge(&bar(50, 80));
        let full = gauge(&bar(100, 80));

        assert_eq!(empty.chars().count(), half.chars().count());
        assert_eq!(half.chars().count(), full.chars().count());
    }

    #[test]
    fn a_narrow_terminal_drops_the_verdict_counts_rather_than_cutting_the_time_remaining() {
        // Truncation takes columns from the right, which is where the time remaining lives, so a
        // narrow terminal would otherwise lose the most useful part of the line to an ellipsis.
        let render = |width| {
            let mut progress = Progress::new(true, Styler::new(false), Some(width));

            progress.set_total(100);

            for _ in 0..49 {
                progress.record(Outcome::Killed);
            }

            progress.record(Outcome::Survived);
            progress.render()
        };
        let wide = render(140);
        let narrow = render(80);

        assert!(wide.contains("survived"), "{wide}");
        assert!(!narrow.contains("survived"), "{narrow}");
        assert!(narrow.contains("ETA"), "{narrow}");
        assert!(!narrow.contains('…'), "{narrow}");
    }

    #[test]
    fn the_time_remaining_is_marked_approximate_rather_than_spelled_out() {
        let rendered = bar(50, 140);

        assert!(rendered.contains('~'), "{rendered}");
        assert!(!rendered.contains("estimating"), "{rendered}");
    }

    #[test]
    fn the_bar_never_exceeds_the_terminal_width() {
        for width in [20_u16, 40, 80, 200] {
            let rendered = bar(50, width);

            assert!(rendered.chars().count() <= usize::from(width));
        }
    }

    #[test]
    fn a_narrow_terminal_still_renders_something() {
        let rendered = bar(50, 20);

        assert!(!rendered.is_empty());
    }

    #[test]
    fn the_fraction_is_clamped() {
        let mut progress = Progress::new(true, Styler::new(false), Some(80));

        progress.set_total(2);

        for _ in 0..10 {
            progress.record(Outcome::Killed);
        }

        assert!((progress.fraction() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn no_total_means_no_progress() {
        let progress = Progress::new(true, Styler::new(false), Some(80));

        assert!(progress.fraction().abs() < f64::EPSILON);
    }

    #[test]
    fn the_caption_counts_evaluated_mutants_and_what_they_found() {
        let mut progress = Progress::new(true, Styler::new(false), Some(200));

        progress.set_total(10);
        progress.record(Outcome::Killed);
        progress.record(Outcome::Survived);
        progress.record(Outcome::Timeout);
        progress.record(Outcome::Timeout);
        progress.record(Outcome::OutOfMemory);

        let rendered = progress.render();

        assert!(
            rendered.contains("5/10 mutants evaluated (1 survived, 2 timeouts, 1 out of memory)"),
            "{rendered}"
        );
    }

    #[test]
    fn zero_verdict_counts_are_omitted() {
        let mut progress = Progress::new(true, Styler::new(false), Some(200));

        progress.set_total(10);
        progress.record(Outcome::Timeout);

        let rendered = progress.render();

        assert!(rendered.contains("(1 timeout)"), "{rendered}");
        assert!(!rendered.contains("survived"), "{rendered}");
        assert!(!rendered.contains("out of memory"), "{rendered}");
    }

    #[test]
    fn an_out_of_memory_verdict_is_counted() {
        let mut progress = Progress::new(true, Styler::new(false), Some(200));

        progress.set_total(10);
        progress.record(Outcome::OutOfMemory);

        assert!(progress.render().contains("(1 out of memory)"), "{}", progress.render());
    }

    #[test]
    fn a_clean_run_has_no_verdict_section() {
        let mut progress = Progress::new(true, Styler::new(false), Some(200));

        progress.set_total(10);
        progress.record(Outcome::Killed);

        let rendered = progress.render();

        assert!(!rendered.contains('('), "{rendered}");
    }

    #[test]
    fn the_gauge_keeps_its_width_however_long_the_caption_grows() {
        // Sizing the gauge from what the caption leaves over collapsed it to nothing.
        let mut progress = Progress::new(true, Styler::new(false), Some(200));

        progress.set_total(1_000_000);

        let empty = progress.render();

        for _ in 0..10 {
            progress.record(Outcome::Survived);
        }

        let busy = progress.render();

        assert!(empty.contains(&format!("[{}]", " ".repeat(BAR_WIDTH))), "{empty}");
        assert!(busy.contains(&format!("[{}]", " ".repeat(BAR_WIDTH))), "{busy}");
    }

    #[test]
    fn a_time_estimate_appears_once_there_is_something_to_extrapolate_from() {
        let mut progress = Progress::new(true, Styler::new(false), Some(200));

        progress.set_total(10);

        assert!(!progress.render().contains("ETA"), "{}", progress.render());

        for _ in 0..MIN_ETA_SAMPLES {
            progress.record(Outcome::Killed);
        }

        assert!(progress.render().contains("ETA"), "{}", progress.render());
    }

    #[test]
    fn a_finished_run_has_no_time_left_to_report() {
        let mut progress = Progress::new(true, Styler::new(false), Some(200));

        progress.set_total(1);
        progress.record(Outcome::Killed);

        assert!(!progress.render().contains("ETA"), "{}", progress.render());
    }

    #[test]
    fn truncation_appends_an_ellipsis() {
        assert_eq!(truncate("abcdefghij", 6), "abc...");
        assert_eq!(truncate("abc", 6), "abc");
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        assert_eq!(truncate("ééééé", 5).chars().count(), 5);
    }

    #[test]
    fn styling_costs_no_columns_and_a_cut_line_stops_styling() {
        let styled = "\u{1b}[1;36mBuilding\u{1b}[0m";

        // Eight visible columns, so it fits a width of eight and survives untouched.
        assert_eq!(truncate(styled, 8), styled);

        let cut = truncate(&format!("{styled} [==>  ]"), 10);

        assert_eq!(crate::report::unstyled(&cut), "Buildin...");
        assert!(cut.ends_with("\u{1b}[0m"), "{cut:?}");
    }

    fn written(steps: impl FnOnce(&mut Progress, &mut Sink)) -> String {
        let mut host = Sink::default();
        let mut progress = Progress::new(true, Styler::new(false), Some(80));

        steps(&mut progress, &mut host);

        host.err()
    }

    /// Puts the last redraw far enough in the past that the next call is not throttled.
    ///
    /// `None` is the same answer for the purpose: it means nothing has been drawn yet, which is also
    /// not throttled.
    fn expire(progress: &mut Progress) {
        progress.last_draw = None;
    }

    /// Replays the erase sequences the display writes, so a test can assert on what is left on
    /// screen rather than on every byte that passed through it.
    fn visible(stream: &str) -> String {
        stream
            .split('\n')
            .map(|line| line.rsplit("\r\u{1b}[2K").next().unwrap_or(line).to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The bar is drawn where the phase line is still open, and the erase sequence it uses would
    /// take that line with it.
    #[test]
    fn a_borrowed_line_takes_the_phase_line_off_the_screen_rather_than_committing_it() {
        let screen = visible(&written(|progress, host| {
            progress.begin(host, "Mutating", "Mutated", "the workspace");
            progress.borrowed(host, "Building [==>  ] 2/9: syn");
        }));

        assert_eq!(
            screen, "Building [==>  ] 2/9: syn",
            "the opening was committed and will be written again"
        );
    }

    #[test]
    fn phase_progress_counts_completed_units_on_the_active_phase_line() {
        let screen = visible(&written(|progress, host| {
            progress.begin(host, "Optimizing", "Optimized", "4 test binaries");
            progress.phase_progress(host, 0, 4, "test binaries");
            expire(progress);
            progress.phase_progress(host, 2, 4, "test binaries");
        }));

        assert_eq!(screen, format!("  Optimizing [{}>             ] 2/4 test binaries", "=".repeat(11)));
    }

    #[test]
    fn completed_phase_progress_fills_the_gauge() {
        let screen = visible(&written(|progress, host| {
            progress.begin(host, "Optimizing", "Optimized", "1 test binary");
            progress.phase_progress(host, 1, 1, "test binary");
        }));

        assert_eq!(screen, format!("  Optimizing [{}] 1/1 test binary", "=".repeat(BAR_WIDTH)));
    }

    #[test]
    fn baseline_phase_progress_contains_only_the_binary_count() {
        let screen = visible(&written(|progress, host| {
            progress.begin(host, "Baselining", "Baseline", "building the test binaries and running the suite");
            progress.phase_progress(host, 2, 4, "test binaries");
        }));

        assert!(screen.contains("2/4 test binaries"), "{screen}");
        assert!(!screen.contains("elapsed"), "{screen}");
        assert!(!screen.contains("ETA"), "{screen}");
    }

    /// Having said what it was about to do, a phase must still be able to say what it found, even
    /// though something printed meanwhile took the line it was holding. The whole line is written
    /// again rather than just the ending: a bare count is a fragment, and what it counts is by then
    /// several screens up.
    #[test]
    fn a_phase_interrupted_by_a_build_repeats_its_opening_with_the_result_attached() {
        let screen = visible(&written(|progress, host| {
            progress.begin(host, "Mutating", "Mutated", "the workspace");
            progress.borrowed(host, "Building [==>  ] 2/9: syn");
            progress.end(host, ", 14 viable mutants");
        }));

        // Once, not once bare and once complete.
        assert_eq!(screen, "     Mutated the workspace, 14 viable mutants\n");
    }

    /// A phase whose build failed never reaches its ending, so the opening the bar took away has to
    /// come back — an error with no phase above it does not say what was being attempted.
    #[test]
    fn a_phase_interrupted_by_a_build_that_then_failed_gets_its_opening_back() {
        let screen = visible(&written(|progress, host| {
            progress.begin(host, "Mutating", "Mutated", "the workspace");
            progress.borrowed(host, "Building [==>  ] 2/9: syn");
            progress.abandon(host);
        }));

        assert_eq!(screen, "    Mutating the workspace\n");
    }

    /// An ending with no opening to attach it to still has to appear, in the continuation column.
    #[test]
    fn an_ending_with_no_phase_behind_it_is_written_under_the_status_column() {
        let output = written(|progress, host| {
            progress.end(host, ", 14 viable mutants");
        });

        assert!(output.ends_with("14 viable mutants\n"), "{output:?}");
        assert!(
            !output.contains(", 14 viable"),
            "the comma joined a sentence that is not there: {output:?}"
        );
    }

    /// A call that arrives inside the redraw interval must leave the screen alone.
    ///
    /// Cargo redraws its bar far more often than the interval and the reader splits on carriage
    /// returns, so most calls are throttled ones. Erasing before the interval was consulted blanked
    /// the line on every one of them and left it blank until the next call that drew, which is what
    /// a flicker is: the bar was absent for most of the build rather than present and stale.
    #[test]
    fn a_throttled_borrowed_line_leaves_the_previous_one_on_screen() {
        let output = written(|progress, host| {
            progress.borrowed(host, "Building [==>  ] 2/9: syn");
            progress.borrowed(host, "Building [===> ] 3/9: serde");
            progress.borrowed(host, "Building [====>] 4/9: clap");
        });

        assert_eq!(
            visible(&output),
            "Building [==>  ] 2/9: syn",
            "a throttled call blanked the line instead of leaving the bar up: {output:?}"
        );
        assert!(
            !output.ends_with("\r\u{1b}[2K"),
            "the display was left erased rather than showing a bar: {output:?}"
        );
    }

    /// Past the interval, an unchanged line is not erased and rewritten.
    ///
    /// Repainting identical text is invisible on a terminal that coalesces writes within a frame and
    /// a visible flash on one that paints each write as it arrives, which is why this was reported
    /// on Windows and not elsewhere.
    #[test]
    fn an_unchanged_borrowed_line_is_not_repainted() {
        let output = written(|progress, host| {
            progress.borrowed(host, "Building [==>  ] 2/9: syn");

            expire(progress);

            progress.borrowed(host, "Building [==>  ] 2/9: syn");
        });

        assert_eq!(output.matches("Building").count(), 1, "the same line was painted twice: {output:?}");
    }

    /// A changed line past the interval must still be drawn, or the bar would freeze.
    #[test]
    fn a_changed_borrowed_line_is_drawn_once_the_interval_has_passed() {
        let output = written(|progress, host| {
            progress.borrowed(host, "Building [==>  ] 2/9: syn");

            expire(progress);

            progress.borrowed(host, "Building [===> ] 3/9: serde");
        });

        assert_eq!(visible(&output), "Building [===> ] 3/9: serde", "{output:?}");
    }

    /// Nothing may be drawn where there is no display to draw on.
    #[test]
    fn a_borrowed_line_is_not_drawn_when_the_display_is_off() {
        let mut host = Sink::default();
        let mut progress = Progress::new(false, Styler::new(false), Some(80));

        progress.borrowed(&mut host, "Building [==>  ] 2/9: syn");

        assert_eq!(host.err(), "");
    }

    /// One redraw must reach the terminal as one write.
    ///
    /// This is the flicker reported on Windows. `write!` with a format string issues a write per
    /// piece, so the erase arrived on its own and blanked the row before the text followed. A
    /// console that repaints asynchronously hides that; the Windows console paints each write, so it
    /// showed a blank row on every redraw of a bar that redraws ten times a second.
    #[test]
    fn a_redraw_reaches_the_terminal_as_a_single_write() {
        let mut host = CountingHost::default();
        let mut progress = Progress::new(true, Styler::new(false), Some(80));

        progress.borrowed(&mut host, "Building [==>  ] 2/9: syn");

        assert_eq!(
            host.err.writes, 1,
            "the erase and the text reached the console separately, which is the flicker"
        );
        assert!(
            String::from_utf8_lossy(&host.err.bytes).ends_with("Building [==>  ] 2/9: syn"),
            "the update did not carry its text"
        );
    }

    /// Replacing an active phase with Cargo's bar must not erase in one write and draw in another.
    #[test]
    fn the_first_build_bar_replaces_the_phase_in_one_write() {
        let mut host = CountingHost::default();
        let mut progress = Progress::new(true, Styler::new(false), Some(80));

        progress.begin(&mut host, "Mutating", "Mutated", "the workspace");
        host.err.writes = 0;
        host.err.bytes.clear();

        progress.borrowed(&mut host, "Building [==>  ] 2/9: syn");

        assert_eq!(host.err.writes, 1, "the phase was erased separately from drawing the build bar");
        assert!(
            String::from_utf8_lossy(&host.err.bytes).ends_with("Building [==>  ] 2/9: syn"),
            "the replacement did not carry the build bar"
        );
    }

    /// Releasing Cargo's row restores the active phase instead of leaving a blank line.
    #[test]
    fn finishing_a_build_restores_the_active_phase() {
        let screen = visible(&written(|progress, host| {
            progress.begin(host, "Baselining", "Baseline", "running the suite");
            progress.borrowed(host, "Building [==>  ] 2/9: syn");
            progress.restore(host);
        }));

        assert_eq!(screen, "  Baselining running the suite");
    }

    /// The mutant bar redraws from the same routine and must be equally atomic.
    #[test]
    fn the_mutant_bar_also_redraws_in_a_single_write() {
        let mut host = CountingHost::default();
        let mut progress = Progress::new(true, Styler::new(false), Some(80));

        progress.set_total(10);
        progress.record(Outcome::Killed);
        progress.tick(&mut host);

        assert_eq!(host.err.writes, 1, "the bar was painted in pieces");
    }

    /// A line the user asked for by name is not progress chatter, so the heuristic that silences
    /// the display when output is piped must not silence it too.
    #[test]
    fn an_insisted_line_is_written_whether_the_display_is_on_or_off() {
        let mut host = Sink::default();
        let mut progress = Progress::new(false, Styler::new(false), None);

        progress.insist(&mut host, "  Compiling", "serde v1.0.229");

        assert!(host.err().contains("serde v1.0.229"), "{}", host.err());

        let shown = written(|progress, host| progress.insist(host, "  Compiling", "serde v1.0.229"));

        assert!(shown.contains("serde v1.0.229"), "{shown}");
    }

    /// A phase holds its line open until it can say what it found, so a line printed meanwhile
    /// would otherwise be run onto the end of that unfinished sentence.
    #[test]
    fn a_line_printed_mid_phase_does_not_land_on_the_phase_line() {
        let output = written(|progress, host| {
            progress.begin(host, "Baselining", "Baseline", "building the test binaries");
            progress.labelled(host, "  Compiling", "serde v1.0.229");
        });

        assert!(output.contains("Baselining building the test binaries\n"), "{output:?}");
    }

    /// Progress is chatter, so none of it may land on the stream carrying the results.
    #[test]
    fn progress_writes_to_the_diagnostic_stream_and_never_to_the_result_stream() {
        let mut host = Sink::default();
        let mut progress = Progress::new(true, Styler::new(false), Some(80));

        progress.begin(&mut host, "Baselining", "Baseline", "building the test binaries");
        progress.end(&mut host, ", done");

        assert!(host.out().is_empty(), "{}", host.out());
        assert!(host.err().contains("Baseline"), "{}", host.err());
    }

    /// A completed phase may report its result without repeating how the work was described while
    /// it was underway.
    #[test]
    fn a_completed_result_can_replace_the_in_progress_subject() {
        let screen = visible(&written(|progress, host| {
            progress.begin(host, "Baselining", "Baseline", "building the test binaries and running the suite");
            progress.complete(host, "42 tests ran in 1.2s");
        }));

        assert_eq!(screen, "    Baseline 42 tests ran in 1.2s\n");
    }

    #[test]
    fn an_abandoned_phase_line_is_closed_so_what_follows_starts_on_its_own_line() {
        let text = written(|progress, host| {
            progress.begin(host, "Baselining", "Baseline", "building the test binaries");
            progress.abandon(host);
        });

        assert!(text.ends_with('\n'), "{text:?}");
    }

    #[test]
    fn abandoning_a_line_that_was_already_closed_writes_nothing_extra() {
        let closed = written(|progress, host| {
            progress.begin(host, "Baselining", "Baseline", "building");
            progress.end(host, ", done");
        });

        let abandoned = written(|progress, host| {
            progress.begin(host, "Baselining", "Baseline", "building");
            progress.end(host, ", done");
            progress.abandon(host);
        });

        assert_eq!(closed, abandoned);
    }

    #[test]
    fn abandoning_without_a_phase_at_all_writes_nothing() {
        assert!(written(Progress::abandon).is_empty());
    }

    #[test]
    fn a_disabled_display_writes_nothing() {
        let mut host = Sink::default();
        let mut progress = Progress::new(false, Styler::new(false), None);

        progress.set_total(10);
        progress.record(Outcome::Killed);
        progress.tick(&mut host);
        progress.finish(&mut host);

        assert!(host.err().is_empty(), "{}", host.err());
        assert!(!host.is_terminal());
        assert_eq!(host.terminal_width(), None);
    }

    /// Text a repository controls reaches the display beside escape sequences the display wrote,
    /// and a terminal obeys both alike.
    ///
    /// One crafted path, source fragment, or test name would otherwise erase the survivors printed
    /// above it, redraw them as killed, or hang a hyperlink of its author's choosing on a line the
    /// reader believes this tool wrote. Every subject-bearing entry point is checked, because the
    /// guarantee is worth nothing if one of them is left raw.
    #[test]
    fn every_subject_entry_point_encodes_terminal_control_sequences() {
        /// One named entry point and the call that drives it with a hostile subject.
        type EntryPoint = (&'static str, fn(&mut Progress, &mut Sink));

        const HOSTILE: &str = "src/\r\u{1b}[2K\u{9b}31mforged\u{1b}]8;;https://evil.test\u{7}link\n.rs";

        let entries: Vec<EntryPoint> = vec![
            ("status", |progress, host| progress.status(host, "Testing", HOSTILE)),
            ("begin", |progress, host| progress.begin(host, "Testing", "Tested", HOSTILE)),
            ("end", |progress, host| {
                progress.begin(host, "Testing", "Tested", "one file");
                progress.end(host, HOSTILE);
            }),
            ("complete", |progress, host| {
                progress.begin(host, "Testing", "Tested", "one file");
                progress.complete(host, HOSTILE);
            }),
            ("labelled", |progress, host| progress.labelled(host, "  SURVIVED", HOSTILE)),
            ("insist", |progress, host| progress.insist(host, "   warning", HOSTILE)),
            ("relay", |progress, host| progress.relay(host, "          ", HOSTILE)),
            ("borrowed", |progress, host| {
                expire(progress);
                progress.borrowed(host, HOSTILE);
            }),
            ("phase_progress", |progress, host| {
                progress.begin(host, "Testing", "Tested", "one file");
                expire(progress);
                progress.phase_progress(host, 1, 2, HOSTILE);
            }),
        ];

        for (name, steps) in entries {
            let text = written(steps);
            // The display's own erase sequence introduces every redraw, so what is checked is that
            // nothing else in the line can move a cursor, erase a row, or address the terminal.
            let subject = text.replace("\r\u{1b}[2K", "");

            assert!(!subject.contains('\r'), "{name} relayed a carriage return: {subject:?}");
            assert!(!subject.contains("\u{1b}["), "{name} relayed a control sequence: {subject:?}");
            assert!(
                !subject.contains("\u{1b}]"),
                "{name} relayed an operating-system command: {subject:?}"
            );
            assert!(
                !subject.contains('\u{9b}'),
                "{name} relayed a C1 control sequence introducer: {subject:?}"
            );
            assert!(!subject.contains('\u{7}'), "{name} relayed a bell: {subject:?}");
            assert!(
                subject.matches('\n').count() <= 1,
                "{name} let a subject forge a second line: {subject:?}"
            );
            assert!(subject.contains("\\r\\e[2K"), "{name} did not show what it refused: {subject:?}");
        }
    }

    /// A phase whose subject was captured while it was open is still encoded when it is rewritten.
    ///
    /// The open line is stored and repainted later — by `restore` after a build borrowed the row,
    /// and by `abandon` when the phase never finished — so a policy applied only at the first paint
    /// would leave the stored copy raw and reintroduce it at the second.
    #[test]
    fn a_held_open_subject_is_encoded_in_every_later_repaint() {
        let restored = written(|progress, host| {
            progress.begin(host, "Building", "Built", "pkg\r\u{1b}[2Kforged");
            expire(progress);
            progress.borrowed(host, "cargo bar");
            progress.restore(host);
        });

        let abandoned = written(|progress, host| {
            progress.begin(host, "Building", "Built", "pkg\r\u{1b}[2Kforged");
            expire(progress);
            progress.borrowed(host, "cargo bar");
            progress.abandon(host);
        });

        for text in [restored, abandoned] {
            let subject = text.replace("\r\u{1b}[2K", "");

            assert!(!subject.contains('\r'), "{subject:?}");
            assert!(subject.contains("pkg\\r\\e[2Kforged"), "{subject:?}");
        }
    }

    /// Relayed build output keeps safe diagnostic styling and contains it to the relayed line.
    #[test]
    fn relayed_tool_output_keeps_safe_styling_and_loses_everything_else() {
        let text = written(|progress, host| {
            progress.relay(host, "          ", "\u{1b}[1;31merror\u{1b}[0m: \u{1b}[2Kwiped");
        });

        assert!(text.contains("\u{1b}[1;31merror\u{1b}[0m"), "{text:?}");
        assert!(text.contains("\\e[2Kwiped\u{1b}[0m\n"), "{text:?}");
    }

    /// This tool's own status lines are unaffected by the policy that protects them.
    #[test]
    fn ordinary_subjects_are_rendered_exactly_as_before() {
        let screen = visible(&written(|progress, host| {
            progress.status(host, "Testing", "42 mutants in src/lib.rs");
        }));

        assert_eq!(screen, "     Testing 42 mutants in src/lib.rs\n");
    }
}
