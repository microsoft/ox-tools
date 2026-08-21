// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::fmt::{Debug, Formatter};
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use tokio::task::JoinHandle;

use crate::facts::Progress;

type ProgressCallback = Box<dyn Fn() -> (u64, u64, String) + Send + Sync>;

/// Produces the draw target used once the bar becomes visible.
///
/// Production always renders to stderr; tests substitute a hidden target.
type DrawTargetFactory = Box<dyn Fn() -> ProgressDrawTarget + Send + Sync>;
type LineSink = Arc<dyn Fn(&str) + Send + Sync>;

/// Refresh rate for progress updates (10 Hz).
const REFRESH_INTERVAL_MS: u64 = 100;

const DETERMINATE_TEMPLATE: &str = "{prefix:>12.bold.cyan} [{bar:25}] {msg}";
const DETERMINATE_TEMPLATE_NO_COLOR: &str = "{prefix:>12} [{bar:25}] {msg}";
const INDETERMINATE_TEMPLATE: &str = "{prefix:>12.bold.cyan} [{spinner}] {msg}";
const INDETERMINATE_TEMPLATE_NO_COLOR: &str = "{prefix:>12} [{spinner}] {msg}";

struct DelayedProgressState {
    visible_after: Instant,
    visible: AtomicBool,
    is_indeterminate: AtomicBool,
    phase_start_time: Mutex<Instant>,
}

impl Debug for DelayedProgressState {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DelayedProgressState")
            .field("visible_after", &self.visible_after)
            .field("visible", &self.visible)
            .field("is_indeterminate", &self.is_indeterminate)
            .field("phase_start_time", &"<Instant>")
            .finish()
    }
}

/// A progress bar that delays showing itself until a threshold is reached.
#[derive(Clone)]
pub struct ProgressReporter {
    bar: ProgressBar,
    state: Arc<DelayedProgressState>,
    message_callback: Arc<Mutex<ProgressCallback>>,
    refresh_task: Arc<JoinHandle<()>>,
    line_sink: LineSink,
    use_colors: bool,
}

impl ProgressReporter {
    /// Create a new progress reporter.
    ///
    /// The progress bar will only become visible if operations continue beyond the delay threshold.
    /// When `use_colors` is false, progress bar chrome is rendered without ANSI styling.
    #[must_use]
    pub fn new(delay: Duration, use_colors: bool) -> Self {
        Self::with_draw_target(delay, use_colors, Box::new(stderr_draw_target))
    }

    /// Same as [`ProgressReporter::new`], but with a caller-supplied factory for the
    /// draw target that is installed once the bar becomes visible.
    fn with_draw_target(delay: Duration, use_colors: bool, make_draw_target: DrawTargetFactory) -> Self {
        Self::with_draw_target_and_line_sink(delay, use_colors, make_draw_target, Arc::new(|msg| eprintln!("{msg}")))
    }

    fn with_draw_target_and_line_sink(delay: Duration, use_colors: bool, make_draw_target: DrawTargetFactory, line_sink: LineSink) -> Self {
        let bar = ProgressBar::hidden();
        bar.set_draw_target(ProgressDrawTarget::hidden());

        let state = Arc::new(DelayedProgressState {
            visible_after: Instant::now() + delay,
            visible: AtomicBool::new(false),
            is_indeterminate: AtomicBool::new(false),
            phase_start_time: Mutex::new(Instant::now()),
        });

        let message_callback = Arc::new(Mutex::new(Box::new(|| (0u64, 0u64, String::new())) as ProgressCallback));

        Self {
            refresh_task: Arc::new(tokio::spawn(refresh_task(
                bar.clone(),
                Arc::clone(&state),
                Arc::clone(&message_callback),
                make_draw_target,
            ))),
            bar,
            state,
            message_callback,
            line_sink,
            use_colors,
        }
    }
}

impl Progress for ProgressReporter {
    /// Set the prefix label for the progress bar (e.g., "Preparing", "Collecting").
    fn set_phase(&self, phase: &str) {
        self.bar.set_prefix(phase.to_string());
        *self.state.phase_start_time.lock().expect("lock poisoned") = Instant::now();
    }

    /// Configure determinate progress reporting with a (total, current, message) callback.
    fn set_determinate(&self, callback: Box<dyn Fn() -> (u64, u64, String) + Send + Sync + 'static>) {
        *self.message_callback.lock().expect("lock poisoned") = callback;
        self.state.is_indeterminate.store(false, Ordering::Relaxed);
        self.bar.disable_steady_tick();
        self.bar.set_length(0);
        self.bar.set_position(0);
        let template = if self.use_colors {
            DETERMINATE_TEMPLATE
        } else {
            DETERMINATE_TEMPLATE_NO_COLOR
        };
        self.bar.set_style(
            ProgressStyle::default_bar()
                .template(template)
                .expect("could not create progress bar style")
                .progress_chars("=> "),
        );
    }

    /// Configure indeterminate progress reporting with a message-only callback.
    fn set_indeterminate(&self, callback: Box<dyn Fn() -> String + Send + Sync + 'static>) {
        *self.message_callback.lock().expect("lock poisoned") = Box::new(move || {
            let message = callback();
            (0, 0, message)
        });
        *self.state.phase_start_time.lock().expect("lock poisoned") = Instant::now();
        self.state.is_indeterminate.store(true, Ordering::Relaxed);
        self.bar.enable_steady_tick(Duration::from_millis(REFRESH_INTERVAL_MS));

        let template = if self.use_colors {
            INDETERMINATE_TEMPLATE
        } else {
            INDETERMINATE_TEMPLATE_NO_COLOR
        };
        self.bar.set_style(
            ProgressStyle::default_spinner()
                .template(template)
                .expect("could not create progress bar style")
                .tick_strings(&[
                    ">                        ", // 1–4 chars padded with spaces to total 25 characters
                    "=>                       ",
                    "==>                      ",
                    "===>                     ",
                    " ===>                    ",
                    "  ===>                   ",
                    "   ===>                  ",
                    "    ===>                 ",
                    "     ===>                ",
                    "      ===>               ",
                    "       ===>              ",
                    "        ===>             ",
                    "         ===>            ",
                    "          ===>           ",
                    "           ===>          ",
                    "            ===>         ",
                    "             ===>        ",
                    "              ===>       ",
                    "               ===>      ",
                    "                ===>     ",
                    "                 ===>    ",
                    "                  ===>   ",
                    "                   ===>  ",
                    "                    ===> ",
                    "                     ===>",
                    "                      ===",
                    "                       ==",
                    "                        =",
                    "                         ",
                    "                        <",
                    "                       <=",
                    "                      <==",
                    "                     <===",
                    "                    <=== ",
                    "                   <===  ",
                    "                  <===   ",
                    "                 <===    ",
                    "                <===     ",
                    "               <===      ",
                    "              <===       ",
                    "             <===        ",
                    "            <===         ",
                    "           <===          ",
                    "          <===           ",
                    "         <===            ",
                    "        <===             ",
                    "       <===              ",
                    "      <===               ",
                    "     <===                ",
                    "    <===                 ",
                    "   <===                  ",
                    "  <===                   ",
                    " <===                    ",
                    "<===                     ",
                    "===                      ",
                    "==                       ",
                    "=                        ",
                    "                         ",
                ]),
        );
    }

    /// Print a message line without disrupting the progress indicator.
    fn println(&self, msg: &str) {
        let line_sink = Arc::clone(&self.line_sink);
        self.bar.suspend(|| line_sink(msg));
    }

    /// Finish and clear the progress indicator.
    fn done(&self) {
        self.refresh_task.abort();
        if self.state.visible.load(Ordering::Relaxed) {
            self.bar.finish_and_clear();
        }
    }

    fn use_colors(&self) -> bool {
        self.use_colors
    }
}

impl Debug for ProgressReporter {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ProgressReporter")
            .field("bar", &self.bar)
            .field("state", &self.state)
            .field("message_callback", &"<callback>")
            .field("refresh_task", &"<task>")
            .field("line_sink", &"<callback>")
            .field("use_colors", &self.use_colors)
            .finish()
    }
}

/// The draw target used in production once the bar becomes visible.
// Not covered: installing this target renders to the real stderr, which tests must not do.
#[cfg_attr(coverage_nightly, coverage(off))]
fn stderr_draw_target() -> ProgressDrawTarget {
    ProgressDrawTarget::stderr_with_hz(10)
}

/// Background refresh task that periodically updates the progress bar.
async fn refresh_task(
    bar: ProgressBar,
    state: Arc<DelayedProgressState>,
    callback: Arc<Mutex<ProgressCallback>>,
    make_draw_target: DrawTargetFactory,
) {
    let mut interval = tokio::time::interval(Duration::from_millis(REFRESH_INTERVAL_MS));
    #[expect(clippy::infinite_loop, reason = "task runs until aborted")]
    loop {
        let _ = interval.tick().await;

        if !state.visible.load(Ordering::Relaxed) && Instant::now() >= state.visible_after {
            state.visible.store(true, Ordering::Relaxed);
            bar.set_draw_target(make_draw_target());
        }

        if state.visible.load(Ordering::Relaxed) {
            let (length, position, mut message) = {
                let callback_guard = callback.lock().expect("lock poisoned");
                callback_guard()
            };

            // In indeterminate mode, prepend elapsed seconds to the message
            if state.is_indeterminate.load(Ordering::Relaxed) {
                let elapsed_secs = {
                    let start_time = state.phase_start_time.lock().expect("lock poisoned");
                    start_time.elapsed().as_secs()
                };
                message = format!("{elapsed_secs}s: {message}");
            }

            if length > 0 {
                bar.set_length(length);
                bar.set_position(position);
            }
            bar.set_message(message);
        }
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use core::sync::atomic::Ordering;
    use core::time::Duration;
    use std::sync::{Arc, Mutex};

    use indicatif::ProgressDrawTarget;

    use super::{DrawTargetFactory, ProgressReporter};
    use crate::facts::Progress;

    /// A reporter whose visible draw target is hidden, so nothing ever reaches stderr.
    fn hidden_reporter(delay: Duration, use_colors: bool) -> ProgressReporter {
        let factory: DrawTargetFactory = Box::new(ProgressDrawTarget::hidden);
        ProgressReporter::with_draw_target(delay, use_colors, factory)
    }

    /// Poll until `predicate` holds or the deadline passes, so tests don't depend on exact timing.
    ///
    /// Coverage is off because the post-loop timeout result is only reached when the machine
    /// stalls for five seconds, which no passing test run does.
    #[cfg_attr(coverage_nightly, coverage(off))]
    async fn wait_until(reporter: &ProgressReporter, predicate: impl Fn(&ProgressReporter) -> bool) -> bool {
        for _ in 0..200_u32 {
            if predicate(reporter) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        predicate(reporter)
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "requires tokio timers and threads")]
    async fn determinate_progress_becomes_visible_and_tracks_callback() {
        let reporter = hidden_reporter(Duration::from_millis(1), true);
        reporter.set_phase("Collecting");
        reporter.set_determinate(Box::new(|| (10, 4, "4/10 crates".to_owned())));

        assert!(wait_until(&reporter, |r| r.bar.message() == "4/10 crates").await);
        assert_eq!("Collecting", reporter.bar.prefix());
        assert_eq!(Some(10), reporter.bar.length());
        assert_eq!(4, reporter.bar.position());
        assert!(reporter.state.visible.load(Ordering::Relaxed));
        assert!(!reporter.state.is_indeterminate.load(Ordering::Relaxed));
        assert!(reporter.use_colors());

        reporter.done();
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "requires tokio timers and threads")]
    async fn determinate_progress_with_zero_length_leaves_bar_untouched() {
        let reporter = hidden_reporter(Duration::from_millis(1), false);
        reporter.set_determinate(Box::new(|| (0, 7, "warming up".to_owned())));

        assert!(wait_until(&reporter, |r| r.bar.message() == "warming up").await);
        assert_eq!(Some(0), reporter.bar.length());
        assert_eq!(0, reporter.bar.position());
        assert!(!reporter.use_colors());

        reporter.done();
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "requires tokio timers and threads")]
    async fn indeterminate_progress_prefixes_elapsed_seconds() {
        let reporter = hidden_reporter(Duration::from_millis(1), true);
        reporter.set_phase("Fetching");
        reporter.set_indeterminate(Box::new(|| "downloading".to_owned()));

        assert!(wait_until(&reporter, |r| r.bar.message().ends_with(": downloading")).await);
        let message = reporter.bar.message();
        let (elapsed, rest) = message.split_once(": ").expect("waited for a message containing a ': ' separator");
        assert_eq!("downloading", rest);
        assert!(elapsed.ends_with('s'), "expected an elapsed-seconds prefix, got {message}");
        assert!(
            elapsed.trim_end_matches('s').parse::<u64>().is_ok(),
            "unexpected prefix in {message}"
        );
        assert!(reporter.state.is_indeterminate.load(Ordering::Relaxed));

        reporter.done();
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "requires tokio timers and threads")]
    async fn indeterminate_progress_without_colors_uses_plain_template() {
        let reporter = hidden_reporter(Duration::from_millis(1), false);
        reporter.set_indeterminate(Box::new(|| "scanning".to_owned()));

        assert!(wait_until(&reporter, |r| r.bar.message().ends_with(": scanning")).await);

        reporter.done();
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "requires tokio timers and threads")]
    async fn bar_stays_hidden_until_the_delay_elapses() {
        let reporter = hidden_reporter(Duration::from_hours(1), true);
        reporter.set_phase("Preparing");
        reporter.set_determinate(Box::new(|| (5, 1, "1/5".to_owned())));

        tokio::time::sleep(Duration::from_millis(350)).await;

        assert!(!reporter.state.visible.load(Ordering::Relaxed));
        assert_eq!("", reporter.bar.message());

        // `done` on an invisible bar must not touch the bar.
        reporter.done();
        assert!(!reporter.state.visible.load(Ordering::Relaxed));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "requires tokio timers and threads")]
    async fn println_and_debug_work_while_hidden() {
        let reporter = hidden_reporter(Duration::from_hours(1), false);
        reporter.println("a message");

        let debug = format!("{reporter:?}");
        assert!(debug.contains("ProgressReporter"), "unexpected debug output: {debug}");
        assert!(debug.contains("DelayedProgressState"), "unexpected debug output: {debug}");
        assert!(debug.contains("line_sink"), "unexpected debug output: {debug}");

        let cloned = reporter.clone();
        cloned.done();
        reporter.done();
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "requires tokio timers and threads")]
    async fn println_emits_each_line_through_the_configured_sink() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink_lines = Arc::clone(&lines);
        let reporter = ProgressReporter::with_draw_target_and_line_sink(
            Duration::from_hours(1),
            false,
            Box::new(ProgressDrawTarget::hidden),
            Arc::new(move |line| {
                sink_lines
                    .lock()
                    .expect("no test panics while holding the line sink lock")
                    .push(line.to_owned());
            }),
        );

        reporter.println("first");
        reporter.println("second");

        assert_eq!(
            *lines.lock().expect("no test panics while holding the line sink lock"),
            vec!["first".to_owned(), "second".to_owned()]
        );
        reporter.done();
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "requires tokio timers and threads")]
    async fn done_aborts_the_refresh_task() {
        let reporter = hidden_reporter(Duration::from_hours(1), true);
        assert!(!reporter.refresh_task.is_finished());

        reporter.done();

        assert!(wait_until(&reporter, |r| r.refresh_task.is_finished()).await);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "requires tokio timers and threads")]
    async fn public_constructor_stays_hidden_for_a_long_delay() {
        let reporter = ProgressReporter::new(Duration::from_hours(1), true);
        reporter.set_phase("Preparing");
        reporter.done();

        assert!(!reporter.state.visible.load(Ordering::Relaxed));
    }
}
