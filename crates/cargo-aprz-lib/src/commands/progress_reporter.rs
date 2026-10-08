// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::fmt::{Debug, Formatter};
use core::num::NonZeroU8;
#[cfg(test)]
use core::sync::atomic::AtomicUsize;
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

/// Refresh rate for progress updates.
const REFRESHES_PER_SECOND: NonZeroU8 = NonZeroU8::new(10).expect("the progress refresh rate is non-zero");

fn refresh_interval() -> Duration {
    Duration::from_millis(1_000 / u64::from(REFRESHES_PER_SECOND.get()))
}

const DETERMINATE_TEMPLATE: &str = "{prefix:>12.bold.cyan} [{bar:25}] {msg}";
const DETERMINATE_TEMPLATE_NO_COLOR: &str = "{prefix:>12} [{bar:25}] {msg}";
const INDETERMINATE_TEMPLATE: &str = "{prefix:>12.bold.cyan} [{spinner}] {msg}";
const INDETERMINATE_TEMPLATE_NO_COLOR: &str = "{prefix:>12} [{spinner}] {msg}";

const fn determinate_template(use_colors: bool) -> &'static str {
    if use_colors {
        DETERMINATE_TEMPLATE
    } else {
        DETERMINATE_TEMPLATE_NO_COLOR
    }
}

const fn indeterminate_template(use_colors: bool) -> &'static str {
    if use_colors {
        INDETERMINATE_TEMPLATE
    } else {
        INDETERMINATE_TEMPLATE_NO_COLOR
    }
}

struct DelayedProgressState {
    visible_after: Instant,
    visible: AtomicBool,
    is_indeterminate: AtomicBool,
    phase_start_time: Mutex<Instant>,
    #[cfg(test)]
    refreshes: AtomicUsize,
}

impl Debug for DelayedProgressState {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        let mut debug = f.debug_struct("DelayedProgressState");
        debug
            .field("visible_after", &self.visible_after)
            .field("visible", &self.visible)
            .field("is_indeterminate", &self.is_indeterminate)
            .field("phase_start_time", &"<Instant>");
        #[cfg(test)]
        debug.field("refreshes", &self.refreshes);
        debug.finish()
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

        let state = Arc::new(DelayedProgressState {
            visible_after: Instant::now() + delay,
            visible: AtomicBool::new(false),
            is_indeterminate: AtomicBool::new(false),
            phase_start_time: Mutex::new(Instant::now()),
            #[cfg(test)]
            refreshes: AtomicUsize::new(0),
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
        self.bar.set_length(0);
        self.bar.set_position(0);
        // #[gamma::skip(bool_expr.negate, tag = "trivial", reason = "indicatif strips ANSI styling from injected test terminals; the selected colored and plain templates are asserted directly")]
        self.bar.set_style(determinate_style(self.use_colors));
    }

    /// Configure indeterminate progress reporting with a message-only callback.
    fn set_indeterminate(&self, callback: Box<dyn Fn() -> String + Send + Sync + 'static>) {
        *self.message_callback.lock().expect("lock poisoned") = Box::new(move || {
            let message = callback();
            (0, 0, message)
        });
        *self.state.phase_start_time.lock().expect("lock poisoned") = Instant::now();
        self.state.is_indeterminate.store(true, Ordering::Relaxed);

        // #[gamma::skip(bool_expr.negate, tag = "trivial", reason = "indicatif strips ANSI styling from injected test terminals; the selected colored and plain templates are asserted directly")]
        self.bar.set_style(indeterminate_style(self.use_colors));
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

fn determinate_style(use_colors: bool) -> ProgressStyle {
    // #[gamma::skip(parameter.default_shadow, bool_expr.negate, tag = "trivial", reason = "this thin indicatif adapter has no observable color difference on injected test terminals; determinate_template tests the complete selection truth table")]
    ProgressStyle::default_bar()
        .template(determinate_template(use_colors))
        .expect("could not create progress bar style")
        .progress_chars("=> ")
}

fn indeterminate_style(use_colors: bool) -> ProgressStyle {
    // #[gamma::skip(parameter.default_shadow, bool_expr.negate, tag = "trivial", reason = "this thin indicatif adapter has no observable color difference on injected test terminals; indeterminate_template tests the complete selection truth table")]
    ProgressStyle::default_spinner()
        .template(indeterminate_template(use_colors))
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
        ])
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
    // #[gamma::skip(call.replace_with_default, call_result.default, tag = "trivial", reason = "constructing the production draw target writes to real stderr; refresh frequency is tested independently and tests inject non-stderr targets")]
    ProgressDrawTarget::stderr_with_hz(refreshes_per_second())
}

const fn refreshes_per_second() -> u8 {
    REFRESHES_PER_SECOND.get()
}

/// Background refresh task that periodically updates the progress bar.
async fn refresh_task(
    bar: ProgressBar,
    state: Arc<DelayedProgressState>,
    callback: Arc<Mutex<ProgressCallback>>,
    make_draw_target: DrawTargetFactory,
) {
    let mut interval = tokio::time::interval(refresh_interval());
    #[expect(clippy::infinite_loop, reason = "task runs until aborted")]
    loop {
        let _ = interval.tick().await;

        // #[gamma::skip(relational.le_to_lt, tag = "trivial", reason = "equality between two independently sampled monotonic instants is not controllable or observably distinct from the next refresh tick")]
        if !state.visible.load(Ordering::Relaxed) && state.visible_after <= Instant::now() {
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
                bar.tick();
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
            #[cfg(test)]
            state.refreshes.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod miri_tests {
    use core::time::Duration;

    use super::{
        DETERMINATE_TEMPLATE, DETERMINATE_TEMPLATE_NO_COLOR, INDETERMINATE_TEMPLATE, INDETERMINATE_TEMPLATE_NO_COLOR, determinate_template,
        indeterminate_template, refresh_interval, refreshes_per_second,
    };

    #[test]
    fn refresh_interval_is_one_tenth_of_a_second() {
        assert_eq!(refresh_interval(), Duration::from_millis(100));
        assert_eq!(refreshes_per_second(), 10);
    }

    #[test]
    fn templates_follow_the_color_setting() {
        assert_eq!(determinate_template(true), DETERMINATE_TEMPLATE);
        assert_eq!(determinate_template(false), DETERMINATE_TEMPLATE_NO_COLOR);
        assert_eq!(indeterminate_template(true), INDETERMINATE_TEMPLATE);
        assert_eq!(indeterminate_template(false), INDETERMINATE_TEMPLATE_NO_COLOR);
    }
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use core::sync::atomic::Ordering;
    use core::time::Duration;
    use std::fmt::Debug;
    use std::io;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use indicatif::{ProgressDrawTarget, TermLike};
    use tokio::time::sleep;

    use super::{DrawTargetFactory, ProgressReporter, refresh_interval};
    use crate::facts::Progress;

    /// In-memory terminal adapter that captures the progress bar's current frame without touching stderr.
    ///
    /// Clearing a line clears the captured frame, matching how the tests observe redraws.
    #[derive(Clone, Debug, Default)]
    struct CaptureTerm {
        contents: Arc<Mutex<String>>,
    }

    impl CaptureTerm {
        fn contents(&self) -> String {
            self.contents.lock().expect("capture lock is not poisoned").clone()
        }
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    impl TermLike for CaptureTerm {
        fn width(&self) -> u16 {
            80
        }

        fn move_cursor_up(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }

        fn move_cursor_down(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }

        fn move_cursor_right(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }

        fn move_cursor_left(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }

        fn write_line(&self, s: &str) -> io::Result<()> {
            let mut contents = self.contents.lock().expect("capture lock is not poisoned");
            contents.push_str(s);
            contents.push('\n');
            Ok(())
        }

        fn write_str(&self, s: &str) -> io::Result<()> {
            self.contents.lock().expect("capture lock is not poisoned").push_str(s);
            Ok(())
        }

        fn clear_line(&self) -> io::Result<()> {
            self.contents.lock().expect("capture lock is not poisoned").clear();
            Ok(())
        }

        fn flush(&self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A reporter whose visible draw target is hidden, so nothing ever reaches stderr.
    fn hidden_reporter(delay: Duration, use_colors: bool) -> ProgressReporter {
        let factory: DrawTargetFactory = Box::new(ProgressDrawTarget::hidden);
        ProgressReporter::with_draw_target(delay, use_colors, factory)
    }

    #[tokio::test]
    async fn reporter_starts_hidden() {
        let reporter = hidden_reporter(Duration::from_hours(1), false);
        assert!(reporter.bar.is_hidden());
        assert!(!reporter.state.visible.load(Ordering::Relaxed));
        reporter.done();
    }

    #[tokio::test]
    async fn callbacks_start_at_zero_and_indeterminate_callbacks_keep_zero_counts() {
        let reporter = hidden_reporter(Duration::from_hours(1), false);
        assert_eq!(
            reporter.message_callback.lock().expect("lock is not poisoned")(),
            (0, 0, String::new())
        );

        reporter.set_indeterminate(Box::new(|| "working".to_owned()));
        assert_eq!(
            reporter.message_callback.lock().expect("lock is not poisoned")(),
            (0, 0, "working".to_owned())
        );
        reporter.done();
    }

    #[tokio::test]
    async fn phase_changes_reset_elapsed_time() {
        let reporter = hidden_reporter(Duration::from_hours(1), false);
        *reporter.state.phase_start_time.lock().expect("lock is not poisoned") = Instant::now()
            .checked_sub(Duration::from_secs(10))
            .expect("the current instant is more than ten seconds after the monotonic clock epoch");
        reporter.set_phase("new phase");
        assert!(reporter.state.phase_start_time.lock().expect("lock is not poisoned").elapsed() < Duration::from_secs(1));

        *reporter.state.phase_start_time.lock().expect("lock is not poisoned") = Instant::now()
            .checked_sub(Duration::from_secs(10))
            .expect("the current instant is more than ten seconds after the monotonic clock epoch");
        reporter.set_indeterminate(Box::new(|| "working".to_owned()));
        assert!(reporter.state.phase_start_time.lock().expect("lock is not poisoned").elapsed() < Duration::from_secs(1));
        reporter.done();
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
            sleep(Duration::from_millis(25)).await;
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
    async fn configured_styles_render_the_expected_chrome() {
        let determinate_term = CaptureTerm::default();
        let determinate = ProgressReporter::with_draw_target(
            Duration::ZERO,
            false,
            Box::new({
                let determinate_term = determinate_term.clone();
                move || ProgressDrawTarget::term_like(Box::new(determinate_term.clone()))
            }),
        );
        determinate.set_phase("Collecting");
        determinate.set_determinate(Box::new(|| (10, 4, "crates".to_owned())));
        assert!(wait_until(&determinate, |_| determinate_term.contents().contains("crates")).await);
        let rendered = determinate_term.contents();
        assert!(rendered.contains("Collecting ["), "{rendered:?}");
        assert!(rendered.contains("=>"), "{rendered:?}");
        determinate.done();

        let indeterminate_term = CaptureTerm::default();
        let indeterminate = ProgressReporter::with_draw_target(
            Duration::ZERO,
            false,
            Box::new({
                let indeterminate_term = indeterminate_term.clone();
                move || ProgressDrawTarget::term_like(Box::new(indeterminate_term.clone()))
            }),
        );
        indeterminate.set_phase("Fetching");
        indeterminate.set_indeterminate(Box::new(|| "data".to_owned()));
        assert!(wait_until(&indeterminate, |_| indeterminate_term.contents().contains("data")).await);
        let rendered = indeterminate_term.contents();
        assert!(rendered.contains("Fetching ["), "{rendered:?}");
        indeterminate.done();
    }

    #[tokio::test]
    async fn refresh_loop_advances_only_indeterminate_animation() {
        let term = CaptureTerm::default();
        let reporter = ProgressReporter::with_draw_target(
            Duration::ZERO,
            false,
            Box::new({
                let term = term.clone();
                move || ProgressDrawTarget::term_like(Box::new(term.clone()))
            }),
        );
        reporter.set_indeterminate(Box::new(|| "working".to_owned()));
        assert!(wait_until(&reporter, |_| term.contents().contains("working")).await);
        let first_frame = term.contents();
        assert!(
            wait_until(&reporter, |_| term.contents() != first_frame).await,
            "the indeterminate spinner did not advance"
        );

        reporter.set_determinate(Box::new(|| (10, 4, "steady".to_owned())));
        assert!(wait_until(&reporter, |_| term.contents().contains("steady")).await);
        let determinate_frame = term.contents();
        let completed = reporter.state.refreshes.load(Ordering::Relaxed);
        assert!(
            wait_until(&reporter, |r| r.state.refreshes.load(Ordering::Relaxed) >= completed + 2).await,
            "two determinate refreshes did not complete"
        );
        assert_eq!(term.contents(), determinate_frame, "determinate progress kept animating");
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
    async fn determinate_transition_resets_mode_and_position() {
        let reporter = hidden_reporter(Duration::from_hours(1), false);
        reporter.set_indeterminate(Box::new(|| "working".to_owned()));
        reporter.bar.set_position(7);

        reporter.set_determinate(Box::new(|| (1, 1, "done".to_owned())));

        assert!(!reporter.state.is_indeterminate.load(Ordering::Relaxed));
        assert_eq!(reporter.bar.position(), 0);
        reporter.done();
    }

    #[tokio::test]
    async fn one_item_progress_updates_length_and_position() {
        let reporter = hidden_reporter(Duration::ZERO, false);
        reporter.set_determinate(Box::new(|| (1, 1, "done".to_owned())));

        assert!(wait_until(&reporter, |item| item.bar.position() == 1).await);
        assert_eq!(reporter.bar.length(), Some(1));
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

        sleep(refresh_interval() * 3 + Duration::from_millis(50)).await;

        assert!(!reporter.state.visible.load(Ordering::Relaxed));
        assert_eq!("", reporter.bar.message());

        // `done` on an invisible bar must not touch the bar.
        reporter.done();
        assert!(!reporter.state.visible.load(Ordering::Relaxed));
        assert!(!reporter.bar.is_finished(), "an invisible bar is aborted without being finished");
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
        let reporter = hidden_reporter(Duration::ZERO, true);
        reporter.set_determinate(Box::new(|| (1, 1, "done".to_owned())));
        assert!(wait_until(&reporter, |r| r.state.visible.load(Ordering::Relaxed)).await);
        assert!(!reporter.refresh_task.is_finished());

        reporter.done();

        assert!(wait_until(&reporter, |r| r.refresh_task.is_finished()).await);
        assert!(reporter.bar.is_finished(), "a visible bar is finished and cleared");
    }

    #[tokio::test]
    async fn draw_target_is_installed_only_once_after_becoming_visible() {
        let installations = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&installations);
        let reporter = ProgressReporter::with_draw_target(
            Duration::ZERO,
            false,
            Box::new(move || {
                observed.fetch_add(1, Ordering::Relaxed);
                ProgressDrawTarget::hidden()
            }),
        );
        reporter.set_determinate(Box::new(|| (1, 1, "done".to_owned())));
        assert!(wait_until(&reporter, |_| installations.load(Ordering::Relaxed) == 1).await);
        let completed = reporter.state.refreshes.load(Ordering::Relaxed);
        assert!(
            wait_until(&reporter, |r| r.state.refreshes.load(Ordering::Relaxed) >= completed + 2).await,
            "two refreshes did not complete after installing the draw target"
        );
        assert_eq!(installations.load(Ordering::Relaxed), 1);
        reporter.done();
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "requires tokio timers and threads")]
    async fn public_constructor_stays_hidden_for_a_long_delay() {
        let reporter = ProgressReporter::new(Duration::from_hours(1), true);
        assert!(reporter.use_colors());
        reporter.set_phase("Preparing");
        reporter.done();

        assert!(!reporter.state.visible.load(Ordering::Relaxed));

        let reporter = ProgressReporter::new(Duration::from_hours(1), false);
        assert!(!reporter.use_colors());
        reporter.done();
    }
}
