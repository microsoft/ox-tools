// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Multiline progress display for planning and testing.
//!
//! [`Dashboard`] owns the visible lifecycle: it starts in planning, transitions to testing, folds
//! events into telemetry, rate-limits redraws, and clears or finalizes the frame before ordinary
//! output resumes. The smaller state types below describe pieces of that lifecycle.

use core::fmt::Write as _;
use core::time::Duration;
use std::collections::VecDeque;
use std::io::Write as _;
use std::time::Instant;

use owo_colors::Style;

use super::Host;
use crate::discover::Plan;
use crate::exec::{SelectionAttempt, SelectionResult, SelectionTier, Session};
use crate::model::{Mutant, Outcome, Summary};
use crate::report::{Styler, encode_controls};

const REDRAW_INTERVAL: Duration = Duration::from_secs(1);
const THROUGHPUT_WINDOW: Duration = Duration::from_mins(5);
const MINIMUM_WIDTH: usize = 40;
const MEDIUM_WIDTH: usize = 80;
const LARGE_WIDTH: usize = 136;
const COLUMN_GAP: &str = "   ";
/// A ten-cell side keeps the large-population waffle at one cell per percentage point.
const WAFFLE_SIDE: usize = 10;
const WAFFLE_CELLS: usize = WAFFLE_SIDE * WAFFLE_SIDE;
const WAFFLE_CELL_WIDTH: usize = 2;
const BEGIN_SYNCHRONIZED_UPDATE: &str = "\x1b[?2026h";
const END_SYNCHRONIZED_UPDATE: &str = "\x1b[?2026l";
const ERASE_TO_END: &str = "\x1b[J";

/// Campaign phase whose metrics and layout the dashboard currently renders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Planning,
    Testing,
}

/// Verdict classification represented by one waffle cell and legend row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaffleState {
    Unviable,
    Pending,
    Killed,
    Survived,
    Timeout,
    OutOfMemory,
    Flaky,
    Uncovered,
}

impl WaffleState {
    const fn label(self) -> &'static str {
        match self {
            Self::Unviable => "Unviable",
            Self::Pending => "Pending",
            Self::Killed => "Killed",
            Self::Survived => "Survived",
            Self::Timeout => "Timed out",
            Self::OutOfMemory => "Out of mem",
            Self::Flaky => "Flaky",
            Self::Uncovered => "Uncovered",
        }
    }

    const fn marker(self) -> &'static str {
        match self {
            Self::Unviable => "UU",
            Self::Pending => "PP",
            Self::Killed => "KK",
            Self::Survived => "SS",
            Self::Timeout => "TT",
            Self::OutOfMemory => "OO",
            Self::Flaky => "FF",
            Self::Uncovered => "CC",
        }
    }

    const fn style(self) -> Style {
        match self {
            Self::Unviable => Style::new().bright_black(),
            Self::Pending => Style::new().cyan(),
            Self::Killed => Style::new().green(),
            Self::Survived => Style::new().red().bold(),
            Self::Timeout => Style::new().yellow(),
            Self::OutOfMemory => Style::new().magenta().bold(),
            Self::Flaky => Style::new().bright_yellow(),
            Self::Uncovered => Style::new().blue(),
        }
    }

    fn cell(self, styler: Styler) -> String {
        if styler.enabled() {
            styler.apply("██", self.style())
        } else {
            self.marker().to_owned()
        }
    }
}

/// Widths of the rows in the last frame, used to erase wrapped terminal rows.
#[derive(Debug, Clone)]
struct DrawnFrame {
    line_widths: Vec<usize>,
}

impl DrawnFrame {
    fn new(lines: &[String]) -> Self {
        Self {
            line_widths: lines.iter().map(|line| visible_width(line)).collect(),
        }
    }

    fn physical_rows(&self, terminal_width: usize) -> usize {
        self.line_widths
            .iter()
            .map(|width| width.saturating_sub(1) / terminal_width + 1)
            .sum()
    }
}

/// Probe attempts and conclusive hits for one test-selection tier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Attempts {
    total: usize,
    hits: usize,
}

impl Attempts {
    fn record(&mut self, attempt: &SelectionAttempt) {
        self.total = self.total.saturating_add(1);
        if attempt.result == SelectionResult::Hit {
            self.hits = self.hits.saturating_add(1);
        }
    }
}

/// Live, multiline testing display selected by `--dashboard`.
pub(super) struct Dashboard {
    enabled: bool,
    active: bool,
    dirty: bool,
    width: usize,
    styler: Styler,
    drawn_frame: Option<DrawnFrame>,
    last_draw: Option<Instant>,
    testing_started: Option<Instant>,
    phase: Phase,
    status: String,
    summary: Summary,
    binaries: usize,
    jobs: usize,
    active_workers: usize,
    completed_runs: usize,
    completions: VecDeque<Instant>,
    all_test_launches: usize,
    specific_test_launches: usize,
    runtime_ms: u64,
    maximum_runtime_ms: u64,
    maximum_memory: Option<u64>,
    saved_ms: u64,
    selection_cost_ms: crate::HashMap<u32, u64>,
    explicit_hints: Attempts,
    inferred_hints: Attempts,
}

impl Dashboard {
    pub(super) fn new(enabled: bool, width: Option<u16>, styler: Styler) -> Self {
        Self {
            enabled,
            active: false,
            dirty: false,
            width: width.map_or(MEDIUM_WIDTH, usize::from).max(MINIMUM_WIDTH),
            styler,
            drawn_frame: None,
            last_draw: None,
            testing_started: None,
            phase: Phase::default(),
            status: String::new(),
            summary: Summary::default(),
            binaries: 0,
            jobs: 0,
            active_workers: 0,
            completed_runs: 0,
            completions: VecDeque::new(),
            all_test_launches: 0,
            specific_test_launches: 0,
            runtime_ms: 0,
            maximum_runtime_ms: 0,
            maximum_memory: None,
            saved_ms: 0,
            selection_cost_ms: crate::HashMap::default(),
            explicit_hints: Attempts::default(),
            inferred_hints: Attempts::default(),
        }
    }

    pub(super) const fn is_active(&self) -> bool {
        self.enabled && self.active
    }

    pub(super) const fn enabled(&self) -> bool {
        self.enabled
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    pub(super) fn measured(&mut self, plan: &Plan, session: &Session) {
        if !self.enabled {
            return;
        }

        self.summary = Summary::of(&plan.mutants);
        self.binaries = session.binaries.len();
        self.maximum_memory = session.peak;
        self.dirty = true;
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    pub(super) fn start_planning<H: Host>(&mut self, host: &mut H, plan: &Plan, binaries: usize, jobs: usize) {
        if !self.enabled {
            return;
        }

        self.summary = Summary::of(&plan.mutants);
        self.binaries = binaries;
        self.active = true;
        self.phase = Phase::Planning;
        let pending = self.summary.pending;
        self.status = if pending == 0 {
            "Preparing mutation sweep".to_owned()
        } else {
            format!("0/{pending} mutants planned")
        };
        self.jobs = jobs;
        self.dirty = true;
        self.tick(host);
    }

    pub(super) fn start_testing<H: Host>(&mut self, host: &mut H, jobs: usize, status: String) {
        self.start_testing_at(host, jobs, status, Instant::now());
    }

    fn start_testing_at<H: Host>(&mut self, host: &mut H, jobs: usize, status: String, now: Instant) {
        if !self.enabled {
            return;
        }

        if self.active && self.phase == Phase::Planning {
            let label = self.styler.verb("Planning");
            let detail = self.status.clone();
            self.line(host, &label, &detail);
            self.last_draw = None;
        }

        self.active = true;
        self.phase = Phase::Testing;
        self.status = status;
        self.testing_started = Some(now);
        self.jobs = jobs;
        self.dirty = true;
        self.tick_at(host, now);
    }

    pub(super) fn status<H: Host>(&mut self, host: &mut H, verb: &str, detail: &str) {
        if !self.is_active() {
            return;
        }

        format!("{} {}", encode_controls(verb), encode_controls(detail))
            .trim()
            .clone_into(&mut self.status);
        self.dirty = true;
        self.tick(host);
    }

    pub(super) fn phase_progress<H: Host>(&mut self, host: &mut H, completed: usize, total: usize, unit: &str) {
        if !self.is_active() || total == 0 {
            return;
        }

        self.status = format!("{completed}/{total} {}", encode_controls(unit));
        self.dirty = true;
        self.tick(host);
    }

    pub(super) fn record_selection(&mut self, attempt: &SelectionAttempt) {
        if !self.enabled {
            return;
        }

        // Exact is the explicit persisted-hint row; item, reach, and file are inferred hints.
        // Census, selected, whole, and hinted fallback remain ordinary selection telemetry.
        // A successful explicit or inferred probe avoids the remainder of its fallback estimate.
        match attempt.tier {
            SelectionTier::Exact => self.explicit_hints.record(attempt),
            SelectionTier::Item | SelectionTier::Reach | SelectionTier::File => self.inferred_hints.record(attempt),
            SelectionTier::Census | SelectionTier::Selected | SelectionTier::Whole | SelectionTier::HintedFallback => {}
        }
        if attempt.all_tests {
            self.all_test_launches = self.all_test_launches.saturating_add(1);
        } else {
            self.specific_test_launches = self.specific_test_launches.saturating_add(1);
        }
        let chain_cost = self
            .selection_cost_ms
            .entry(attempt.ordinal)
            .and_modify(|elapsed| *elapsed = elapsed.saturating_add(attempt.elapsed_ms))
            .or_insert(attempt.elapsed_ms);
        if attempt.result == SelectionResult::Hit {
            if matches!(
                attempt.tier,
                SelectionTier::Exact | SelectionTier::Item | SelectionTier::Reach | SelectionTier::File
            ) {
                self.saved_ms = self.saved_ms.saturating_add(attempt.fallback_ms.saturating_sub(*chain_cost));
            }
            self.selection_cost_ms.remove(&attempt.ordinal);
        } else if attempt.tier == SelectionTier::Whole {
            self.selection_cost_ms.remove(&attempt.ordinal);
        }
        self.runtime_ms = self.runtime_ms.saturating_add(attempt.elapsed_ms);
        self.maximum_runtime_ms = self.maximum_runtime_ms.max(attempt.elapsed_ms);
        self.dirty = true;
    }

    pub(super) fn mutant_started(&mut self) {
        if self.enabled {
            self.active_workers = self.active_workers.saturating_add(1);
            self.dirty = true;
        }
    }

    pub(super) fn testing_status(&mut self, status: String) {
        if self.enabled {
            self.status = status;
            self.dirty = true;
        }
    }

    pub(super) fn record(&mut self, mutant: &Mutant) {
        self.record_at(mutant, Instant::now());
    }

    fn record_at(&mut self, mutant: &Mutant, now: Instant) {
        if !self.enabled {
            return;
        }

        self.summary.pending = self.summary.pending.saturating_sub(1);
        match mutant.outcome {
            Outcome::Killed => self.summary.killed = self.summary.killed.saturating_add(1),
            Outcome::Survived => self.summary.survived = self.summary.survived.saturating_add(1),
            Outcome::Timeout => self.summary.timeout = self.summary.timeout.saturating_add(1),
            Outcome::OutOfMemory => self.summary.out_of_memory = self.summary.out_of_memory.saturating_add(1),
            Outcome::Flaky => self.summary.flaky = self.summary.flaky.saturating_add(1),
            Outcome::CompileError => self.summary.unviable = self.summary.unviable.saturating_add(1),
            Outcome::Ignored => self.summary.ignored = self.summary.ignored.saturating_add(1),
            Outcome::NoCoverage => self.summary.uncovered = self.summary.uncovered.saturating_add(1),
            Outcome::NotBuilt => self.summary.not_built = self.summary.not_built.saturating_add(1),
            Outcome::Pending => self.summary.pending = self.summary.pending.saturating_add(1),
        }
        self.active_workers = self.active_workers.saturating_sub(1);
        self.completed_runs = self.completed_runs.saturating_add(1);
        self.completions.push_back(now);
        while self
            .completions
            .front()
            .is_some_and(|completed| now.duration_since(*completed) > THROUGHPUT_WINDOW)
        {
            let _expired = self.completions.pop_front();
        }
        self.dirty = true;
    }

    pub(super) fn line<H: Host>(&mut self, host: &mut H, label: &str, detail: &str) {
        if !self.is_active() {
            return;
        }

        self.resize(host);
        let mut frame = self.begin_frame();
        let _ = writeln!(frame, "{label} {}", encode_controls(detail));
        frame.push('\r');
        frame.push_str(END_SYNCHRONIZED_UPDATE);

        let mut stream = host.error();
        let _ = stream.write_all(frame.as_bytes());
        let _ = stream.flush();
        self.drawn_frame = None;
        self.dirty = true;
    }

    pub(super) fn tick<H: Host>(&mut self, host: &mut H) {
        self.tick_at(host, Instant::now());
    }

    fn tick_at<H: Host>(&mut self, host: &mut H, now: Instant) {
        if !self.is_active() {
            return;
        }

        self.resize(host);
        if !self.dirty {
            return;
        }

        if self.last_draw.is_some_and(|last| now.duration_since(last) < REDRAW_INTERVAL) {
            return;
        }

        let lines = self.render_at(now);
        let mut frame = self.begin_frame();
        frame.push_str(&lines.join("\n"));
        frame.push_str(END_SYNCHRONIZED_UPDATE);

        let mut stream = host.error();
        let _ = stream.write_all(frame.as_bytes());
        let _ = stream.flush();

        self.drawn_frame = Some(DrawnFrame::new(&lines));
        self.last_draw = Some(now);
        self.dirty = false;
    }

    pub(super) fn finish<H: Host>(&mut self, host: &mut H) {
        self.clear(host);
        self.active = false;
        self.dirty = false;
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn clear<H: Host>(&mut self, host: &mut H) {
        self.resize(host);
        if self.drawn_frame.is_none() {
            return;
        }

        let mut frame = self.begin_frame();
        frame.push_str(END_SYNCHRONIZED_UPDATE);
        let mut stream = host.error();
        let _ = stream.write_all(frame.as_bytes());
        let _ = stream.flush();
        self.drawn_frame = None;
    }

    fn begin_frame(&self) -> String {
        let mut frame = String::from(BEGIN_SYNCHRONIZED_UPDATE);
        frame.push('\r');
        if let Some(drawn) = &self.drawn_frame {
            let rows = drawn.physical_rows(self.width);
            if rows > 1 {
                let _ = write!(frame, "\x1b[{}A", rows - 1);
            }
            frame.push_str(ERASE_TO_END);
        }
        frame
    }

    fn resize<H: Host>(&mut self, host: &H) {
        if let Some(width) = host.terminal_width().map(usize::from).map(|width| width.max(MINIMUM_WIDTH))
            && width != self.width
        {
            self.width = width;
            self.dirty = true;
        }
    }

    #[cfg(test)]
    fn render(&self) -> Vec<String> {
        self.render_at(Instant::now())
    }

    fn render_at(&self, now: Instant) -> Vec<String> {
        if self.width < MEDIUM_WIDTH {
            return self.render_small();
        }
        if self.width < LARGE_WIDTH {
            return self.render_medium();
        }

        let mut left = self.mutants_panel();
        let mut right = self.test_execution_panel_at(now);
        equalize_panels(&mut left, &mut right);
        let left_width = left.iter().map(|line| visible_width(line)).max().unwrap_or(0);
        let rows = left.len();
        let mut body = Vec::with_capacity(rows.saturating_add(10));
        body.push(self.header());
        body.push(String::new());
        let right_width = right.iter().map(|line| visible_width(line)).max().unwrap_or(0);
        let top_width = left_width.saturating_add(3).saturating_add(right_width);
        let top_indent = " ".repeat(self.width.saturating_sub(top_width) / 2);

        for row in 0..rows {
            let lhs = left.get(row).map_or("", String::as_str);
            let rhs = right.get(row).map_or("", String::as_str);
            let gap = left_width.saturating_sub(visible_width(lhs)).saturating_add(3);
            body.push(format!("{top_indent}{lhs}{}{rhs}", " ".repeat(gap)));
        }

        body.push(String::new());
        body.extend(centered(self.hints_panel(), self.width));
        body
    }

    fn render_small(&self) -> Vec<String> {
        let launches = self.launches();
        vec![
            self.header(),
            self.styler.apply(&"─".repeat(self.width), Style::new().cyan()),
            self.labelled(
                "Mutants",
                &format!(
                    "K {} · S {} · T {} · OOM {} · F {} · U {} · P {}",
                    self.summary.killed,
                    self.summary.survived,
                    self.summary.timeout,
                    self.summary.out_of_memory,
                    self.summary.flaky,
                    self.summary.uncovered,
                    self.summary.pending
                ),
            ),
            self.labelled(
                "Hints",
                &format!(
                    "explicit {}/{} · inferred {}/{}",
                    self.explicit_hints.hits, self.explicit_hints.total, self.inferred_hints.hits, self.inferred_hints.total,
                ),
            ),
            self.labelled(
                "Process",
                &format!(
                    "{} bin · {launches} runs · {}/{} busy",
                    self.binaries, self.active_workers, self.jobs,
                ),
            ),
        ]
    }

    fn render_medium(&self) -> Vec<String> {
        let viable = self.viable();
        let completed = viable.saturating_sub(self.summary.pending as usize);
        vec![
            self.header(),
            self.styler.apply(&"─".repeat(self.width), Style::new().cyan()),
            self.labelled(
                "Mutants",
                &format!(
                    "killed {} · survived {} · timeout {} · OOM {} · flaky {} · uncovered {} · pending {}",
                    self.summary.killed,
                    self.summary.survived,
                    self.summary.timeout,
                    self.summary.out_of_memory,
                    self.summary.flaky,
                    self.summary.uncovered,
                    self.summary.pending
                ),
            ),
            self.labelled(
                "Hints",
                &format!(
                    "explicit {} ({}) · inferred {} ({})",
                    self.explicit_hints.total,
                    percent(self.explicit_hints.hits, self.explicit_hints.total),
                    self.inferred_hints.total,
                    percent(self.inferred_hints.hits, self.inferred_hints.total),
                ),
            ),
            self.labelled(
                "Process",
                &format!(
                    "{} binaries · {} launches · {} average · {}/{} busy",
                    self.binaries,
                    self.launches(),
                    duration(average(self.runtime_ms, self.launches())),
                    self.active_workers,
                    self.jobs,
                ),
            ),
            self.labelled("Progress", &format!("{completed}/{viable} ({})", percent(completed, viable))),
        ]
    }

    fn header(&self) -> String {
        if self.phase == Phase::Planning {
            let detail_width = self
                .width
                .saturating_sub(visible_width(&crate::report::continuation()).saturating_add(1));
            return format!("{} {}", self.styler.verb("Planning"), Self::fit(&self.status, detail_width));
        }

        self.status.clone()
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn labelled(&self, label: &str, detail: &str) -> String {
        let text = format!("{label:<9} {detail}");
        let plain = Self::fit(&text, self.width);
        let Some(rest) = plain.strip_prefix(label) else {
            return plain;
        };
        format!("{}{rest}", self.styler.apply(label, Style::new().bold().cyan()))
    }

    fn fit(line: &str, width: usize) -> String {
        crate::report::fit(line, width)
    }

    fn mutants_panel(&self) -> Vec<String> {
        let (cells, columns, rows) = self.waffle();
        let classifications = self.classifications();
        let mut content = Vec::with_capacity(WAFFLE_SIDE);

        for row in 0..WAFFLE_SIDE {
            let chart = if row < rows {
                let start = row * columns;
                let end = (start + columns).min(cells.len());
                let mut chart = cells[start..end].iter().map(|state| state.cell(self.styler)).collect::<String>();
                chart.push_str(&" ".repeat(WAFFLE_SIDE.saturating_sub(end - start) * WAFFLE_CELL_WIDTH));
                chart
            } else {
                " ".repeat(WAFFLE_SIDE * WAFFLE_CELL_WIDTH)
            };

            let legend = if let Some((state, count)) = classifications.get(row).copied() {
                format!("{} {:<12}{COLUMN_GAP}{:>7}", state.cell(self.styler), state.label(), count)
            } else if row == WAFFLE_SIDE - 1 {
                format!("{:<15}{COLUMN_GAP}{:>7.1}%", "Mutation score", self.summary.score())
            } else {
                String::new()
            };
            content.push(format!("{chart}   {legend}"));
        }

        panel("MUTANTS", &content, self.styler)
    }

    fn hints_panel(&self) -> Vec<String> {
        let content = vec![
            format!("{:<20}{COLUMN_GAP}{:>10}{COLUMN_GAP}{:>12}", "Type", "Count", "Hit rate"),
            self.styler.apply(&"─".repeat(48), Style::new().cyan()),
            format!(
                "{:<20}{COLUMN_GAP}{:>10}{COLUMN_GAP}{:>12}",
                "Explicit hints",
                self.explicit_hints.total,
                percent(self.explicit_hints.hits, self.explicit_hints.total)
            ),
            format!(
                "{:<20}{COLUMN_GAP}{:>10}{COLUMN_GAP}{:>12}",
                "Inferred hints",
                self.inferred_hints.total,
                percent(self.inferred_hints.hits, self.inferred_hints.total)
            ),
        ];
        panel("HINTS", &content, self.styler)
    }

    fn test_execution_panel_at(&self, now: Instant) -> Vec<String> {
        let launches = self.launches();
        let memory = self.maximum_memory.map_or_else(|| "not measured".to_owned(), crate::report::bytes);
        let content = vec![
            metric("Test binaries", self.binaries),
            metric_text("Workers busy", &format!("{} / {}", self.active_workers, self.jobs)),
            metric_text("Recent throughput", &format!("{:.1} mutants/min", self.recent_throughput_at(now))),
            metric("All-tests selections", self.all_test_launches),
            metric("Specific-test selections", self.specific_test_launches),
            metric_text("Average selection runtime", &duration(average(self.runtime_ms, launches))),
            metric_text("Maximum selection runtime", &duration(self.maximum_runtime_ms)),
            metric_text(
                "Average selections/completed mutant",
                &format!("{:.2}", ratio(launches, self.completed_runs)),
            ),
            metric_text("Baseline peak memory", &memory),
            metric_text("Estimated time saved by hints", &duration(self.saved_ms)),
        ];
        panel("TEST EXECUTION", &content, self.styler)
    }

    fn classifications(&self) -> [(WaffleState, usize); 8] {
        [
            (WaffleState::Unviable, self.summary.unviable as usize),
            (WaffleState::Pending, self.summary.pending as usize),
            (WaffleState::Killed, self.summary.killed as usize),
            (WaffleState::Survived, self.summary.survived as usize),
            (WaffleState::Timeout, self.summary.timeout as usize),
            (WaffleState::OutOfMemory, self.summary.out_of_memory as usize),
            (WaffleState::Flaky, self.summary.flaky as usize),
            (WaffleState::Uncovered, self.summary.uncovered as usize),
        ]
    }

    fn waffle(&self) -> (Vec<WaffleState>, usize, usize) {
        let classifications = self.classifications();
        let total = classifications.iter().map(|(_, count)| count).sum::<usize>();
        if total == 0 {
            return (Vec::new(), 1, 0);
        }

        let cell_count = total.min(WAFFLE_CELLS);
        let allocations = apportioned(&classifications, cell_count, total);
        let cells = classifications
            .iter()
            .zip(allocations)
            .flat_map(|((state, _), count)| core::iter::repeat_n(*state, count))
            .collect::<Vec<_>>();
        let columns = square_columns(cell_count);
        let rows = cell_count.div_ceil(columns);
        (cells, columns, rows)
    }

    fn launches(&self) -> usize {
        self.all_test_launches.saturating_add(self.specific_test_launches)
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    #[cfg(test)]
    fn recent_throughput(&self) -> f64 {
        self.recent_throughput_at(Instant::now())
    }

    fn recent_throughput_at(&self, now: Instant) -> f64 {
        let Some(started) = self.testing_started else {
            return 0.0;
        };
        let elapsed = now.duration_since(started).min(THROUGHPUT_WINDOW);
        if elapsed.is_zero() {
            return 0.0;
        }
        let completed = self
            .completions
            .iter()
            .filter(|completed| now.duration_since(**completed) <= THROUGHPUT_WINDOW)
            .count();
        #[expect(clippy::cast_precision_loss, reason = "dashboard rates are approximate human-readable telemetry")]
        let completed = completed as f64;
        completed * 60.0 / elapsed.as_secs_f64()
    }

    fn viable(&self) -> usize {
        [
            self.summary.killed,
            self.summary.survived,
            self.summary.timeout,
            self.summary.out_of_memory,
            self.summary.flaky,
            self.summary.uncovered,
            self.summary.pending,
        ]
        .into_iter()
        .map(|count| count as usize)
        .sum()
    }
}

fn average(total: u64, count: usize) -> u64 {
    u64::try_from(count)
        .ok()
        .filter(|count| *count > 0)
        .map_or(0, |count| total / count)
}

fn percent(value: usize, total: usize) -> String {
    if total == 0 {
        return "0.0%".to_owned();
    }

    #[expect(clippy::cast_precision_loss, reason = "campaign counts are far below f64's exact integer range")]
    let percentage = value as f64 * 100.0 / total as f64;
    format!("{percentage:.1}%")
}

fn duration(milliseconds: u64) -> String {
    if milliseconds < 1_000 {
        return format!("{milliseconds}ms");
    }
    if milliseconds < 60_000 {
        let milliseconds = u32::try_from(milliseconds).expect("guarded above to be less than 60,000");
        return format!("{:.1}s", f64::from(milliseconds) / 1_000.0);
    }

    let seconds = milliseconds / 1_000;
    if seconds >= 60 * 60 {
        return format!("{}h{:02}m", seconds / (60 * 60), seconds / 60 % 60);
    }
    format!("{}m{:02}s", seconds / 60, seconds % 60)
}

fn visible_width(text: &str) -> usize {
    crate::report::unstyled_width(text)
}

fn square_columns(cells: usize) -> usize {
    let mut columns = 1usize;
    while columns.saturating_mul(columns) < cells {
        // #[gamma::skip(all, reason = "removing the positive increment makes the dashboard calculation non-terminating for every population larger than one")]
        columns = columns.saturating_add(1);
    }
    columns.min(WAFFLE_SIDE)
}

/// Uses proportional largest remainders to fill every cell without changing the total.
///
/// Equal remainders retain classification order, which keeps the display stable and gives the
/// dashboard's established verdict order the tie break.
fn apportioned(classifications: &[(WaffleState, usize)], cells: usize, total: usize) -> Vec<usize> {
    if total <= cells {
        return classifications.iter().map(|(_, count)| *count).collect();
    }

    let mut allocation = classifications
        .iter()
        .map(|(_, count)| count.saturating_mul(cells) / total)
        .collect::<Vec<_>>();
    let assigned = allocation.iter().sum::<usize>();
    let mut order = classifications
        .iter()
        .enumerate()
        .map(|(index, (_, count))| (count.saturating_mul(cells) % total, index))
        .collect::<Vec<_>>();
    order.sort_by(|left, right| right.cmp(left));
    for (_, index) in order.into_iter().take(cells.saturating_sub(assigned)) {
        allocation[index] = allocation[index].saturating_add(1);
    }
    allocation
}

fn panel(title: &str, content: &[String], styler: Styler) -> Vec<String> {
    let border = Style::new().cyan();
    let title_width = visible_width(title);
    let width = content
        .iter()
        .map(|line| visible_width(line).saturating_add(2))
        .max()
        .unwrap_or(0)
        .max(title_width.saturating_add(3));
    let mut lines = Vec::with_capacity(content.len().saturating_add(2));
    lines.push(styler.apply(
        &format!("╭─ {title} {}╮", "─".repeat(width.saturating_sub(title_width + 3))),
        border.bold(),
    ));
    lines.extend(content.iter().map(|line| {
        let shown = visible_width(line);
        format!(
            "{} {line}{} {}",
            styler.apply("│", border),
            " ".repeat(width.saturating_sub(shown + 2)),
            styler.apply("│", border)
        )
    }));
    lines.push(styler.apply(&format!("╰{}╯", "─".repeat(width)), border));
    lines
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn equalize_panels(left: &mut Vec<String>, right: &mut Vec<String>) {
    let target = left.len().max(right.len());
    while left.len() < target {
        let width = visible_width(&left[0]);
        left.insert(left.len() - 1, format!("│{}│", " ".repeat(width.saturating_sub(2))));
    }
    while right.len() < target {
        let width = visible_width(&right[0]);
        right.insert(right.len() - 1, format!("│{}│", " ".repeat(width.saturating_sub(2))));
    }
}

fn centered(lines: Vec<String>, width: usize) -> Vec<String> {
    let panel_width = lines.first().map_or(0, |line| visible_width(line));
    let indent = width.saturating_sub(panel_width) / 2;
    let prefix = " ".repeat(indent);
    lines.into_iter().map(|line| format!("{prefix}{line}")).collect()
}

fn metric(label: &str, value: usize) -> String {
    metric_text(label, &number(value))
}

fn metric_text(label: &str, value: &str) -> String {
    const WIDTH: usize = 48;
    let spacing = WIDTH
        .saturating_sub(visible_width(label).saturating_add(visible_width(value)))
        .max(COLUMN_GAP.len());
    format!("{label}{}{value}", " ".repeat(spacing))
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn number(value: usize) -> String {
    let digits = value.to_string();
    let mut rendered = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            rendered.push(',');
        }
        rendered.push(character);
    }
    rendered
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        return 0.0;
    }
    #[expect(clippy::cast_precision_loss, reason = "dashboard ratios are approximate human-readable telemetry")]
    let numerator = numerator as f64;
    #[expect(clippy::cast_precision_loss, reason = "dashboard ratios are approximate human-readable telemetry")]
    let denominator = denominator as f64;
    numerator / denominator
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::testing::{Sink, ci_fixture};

    fn populated(width: u16) -> Dashboard {
        let mut dashboard = Dashboard::new(true, Some(width), Styler::new(false));
        dashboard.active = true;
        dashboard.binaries = 12;
        dashboard.jobs = 4;
        dashboard.active_workers = 3;
        dashboard.completed_runs = 80;
        dashboard.all_test_launches = 50;
        dashboard.specific_test_launches = 30;
        dashboard.runtime_ms = 12_000;
        dashboard.maximum_runtime_ms = 2_000;
        dashboard.maximum_memory = Some(16 * 1024 * 1024);
        dashboard.saved_ms = 60 * 60 * 1_000;
        dashboard.summary = Summary {
            killed: 70,
            survived: 4,
            timeout: 2,
            out_of_memory: 1,
            flaky: 1,
            unviable: 10,
            ignored: 2,
            uncovered: 2,
            not_built: 1,
            pending: 7,
        };
        dashboard.explicit_hints = Attempts { total: 40, hits: 35 };
        dashboard.inferred_hints = Attempts { total: 20, hits: 10 };
        dashboard
    }

    fn selection(tier: SelectionTier, result: SelectionResult, all_tests: bool, elapsed_ms: u64, fallback_ms: u64) -> SelectionAttempt {
        SelectionAttempt {
            ordinal: 1,
            tier,
            package: "subject".to_owned(),
            target: "lib".to_owned(),
            test: None,
            rank: 1,
            result,
            all_tests,
            elapsed_ms,
            fallback_ms,
        }
    }

    #[test]
    fn a_wide_dashboard_balances_mutants_and_execution_with_centered_hints_below() {
        let dashboard = populated(160);
        let rendered = dashboard.render().join("\n");

        assert!(rendered.contains("Baseline peak memory"), "{rendered}");
        assert!(rendered.contains("Planning"), "{rendered}");
        assert!(rendered.contains("MUTANTS"), "{rendered}");
        assert!(rendered.contains("TEST EXECUTION"), "{rendered}");
        assert!(rendered.contains("HINTS"), "{rendered}");
        assert!(rendered.contains('╭'), "{rendered}");

        let mutants = rendered.find("MUTANTS").expect("mutants table");
        let execution = rendered.find("TEST EXECUTION").expect("test execution table");
        let hints = rendered.find("HINTS").expect("hints table");
        assert!(mutants < hints);
        assert!(execution < hints);

        let hints_line = rendered.lines().find(|line| line.contains("╭─ HINTS")).expect("hints heading");
        assert!(hints_line.starts_with(' '), "{hints_line:?}");
    }

    #[test]
    fn a_ready_sweep_transitions_the_dashboard_from_planning_to_testing() {
        let mut dashboard = populated(160);
        dashboard.status = "1/7 mutants planned".to_owned();
        assert_eq!(dashboard.header(), "    Planning 1/7 mutants planned");

        dashboard.phase = Phase::Testing;
        dashboard.status = "     Testing [=========>               ] 80/87 mutants evaluated".to_owned();

        let header = dashboard.header();
        assert!(header.contains("Testing"), "{header}");
        assert!(header.contains("80/87"), "{header}");
    }

    #[test]
    fn starting_testing_commits_the_completed_planning_line_above_the_dashboard() {
        let mut dashboard = populated(160);
        let mut host = Sink::default().terminal(160);
        dashboard.status = "7/7 mutants planned".to_owned();
        dashboard.tick(&mut host);
        let transition = host.err.len();

        dashboard.start_testing(
            &mut host,
            4,
            "     Testing [>                        ] 0/7 mutants evaluated".to_owned(),
        );

        let rendered = str::from_utf8(&host.err[transition..]).expect("dashboard output is UTF-8");
        let planning = rendered.find("    Planning 7/7 mutants planned\n").expect("durable planning line");
        let testing = rendered.find("     Testing [>").expect("testing dashboard");
        assert!(planning < testing, "{rendered:?}");
    }

    #[test]
    fn planning_progress_uses_the_same_cargo_style_as_other_phase_lines() {
        let mut dashboard = populated(160);
        let mut host = Sink::default().terminal(160);

        dashboard.phase_progress(&mut host, 3, 7, "mutants planned");

        assert_eq!(dashboard.header(), "    Planning 3/7 mutants planned");
        assert!(!dashboard.header().contains("cargo-gamma"));
        assert!(!dashboard.header().contains("jobs"));
        assert!(!dashboard.header().contains("elapsed"));
    }

    #[test]
    fn planning_status_is_truncated_before_it_can_wrap_the_terminal() {
        let mut dashboard = populated(40);
        dashboard.status = "building a reachability index with an unexpectedly long package name".to_owned();

        let header = dashboard.header();

        assert_eq!(visible_width(&header), 40, "{header}");
        assert!(header.ends_with("..."), "{header}");
    }

    #[test]
    fn dashboard_width_uses_terminal_columns_for_wide_and_combining_text() {
        assert_eq!(visible_width("界"), 2);
        assert_eq!(visible_width("e\u{301}"), 1);
        assert_eq!(Dashboard::fit("界界界", 5), "界...");
        assert_eq!(Dashboard::fit("e\u{301}fg", 3), "e\u{301}fg");
        assert_eq!(Dashboard::fit("👩‍💻abcd", 5), "👩‍💻...");
        assert_eq!(Dashboard::fit("abc", 2), "..");
    }

    #[test]
    fn a_medium_dashboard_keeps_each_table_as_a_compact_summary() {
        let rendered = populated(100).render().join("\n");

        assert!(rendered.contains("Mutants   killed 70"), "{rendered}");
        assert!(rendered.contains("Hints     explicit 40 (87.5%)"), "{rendered}");
        assert!(rendered.contains("Process   12 binaries"), "{rendered}");
    }

    #[test]
    fn a_small_dashboard_uses_abbreviated_rows_that_fit_the_terminal() {
        let rendered = populated(60).render();

        assert!(rendered.iter().any(|line| line.starts_with("Mutants   K 70")), "{rendered:?}");
        assert!(
            rendered.iter().any(|line| line.starts_with("Hints     explicit 35/40")),
            "{rendered:?}"
        );
        assert!(rendered.iter().any(|line| line.starts_with("Process   12 bin")), "{rendered:?}");
        assert!(rendered.iter().all(|line| visible_width(line) <= 60), "{rendered:?}");
    }

    #[test]
    fn dashboard_layout_tracks_terminal_resizes() {
        let mut dashboard = populated(160);
        dashboard.dirty = true;
        let mut host = Sink::default().terminal(160);
        dashboard.tick(&mut host);

        let large_rows = dashboard
            .drawn_frame
            .as_ref()
            .expect("the large dashboard was drawn")
            .physical_rows(100);
        host.resize_terminal(100);
        dashboard.last_draw = None;
        let medium_start = host.err.len();
        dashboard.tick(&mut host);
        let medium = str::from_utf8(&host.err[medium_start..]).expect("dashboard output is UTF-8");
        assert!(medium.contains(&format!("\r\x1b[{}A{ERASE_TO_END}", large_rows - 1)), "{medium:?}");
        assert!(medium.contains("Mutants   killed 70"), "{medium:?}");

        let medium_rows = dashboard
            .drawn_frame
            .as_ref()
            .expect("the medium dashboard was drawn")
            .physical_rows(60);
        host.resize_terminal(60);
        dashboard.last_draw = None;
        let small_start = host.err.len();
        dashboard.tick(&mut host);
        let small = str::from_utf8(&host.err[small_start..]).expect("dashboard output is UTF-8");
        assert!(small.contains(&format!("\r\x1b[{}A{ERASE_TO_END}", medium_rows - 1)), "{small:?}");
        assert!(small.contains("Mutants   K 70"), "{small:?}");
    }

    #[test]
    fn dashboard_colors_follow_the_resolved_color_policy() {
        let plain = populated(160).render().join("\n");
        let mut colorful = populated(160);
        colorful.styler = Styler::new(true);
        let colorful = colorful.render().join("\n");

        assert!(!plain.contains('\x1b'), "{plain:?}");
        assert!(colorful.contains('\x1b'), "{colorful:?}");
        assert!(crate::report::unstyled(&colorful).contains("Survived"));
    }

    #[test]
    fn dashboard_redraws_at_the_exact_interval_boundary() {
        let mut dashboard = populated(160);
        dashboard.dirty = true;
        let mut host = Sink::default().terminal(160);
        let started = Instant::now();

        dashboard.tick_at(&mut host, started);
        let first_draw = host.err.len();
        dashboard.dirty = true;
        let before_boundary = (started + REDRAW_INTERVAL)
            .checked_sub(Duration::from_nanos(1))
            .expect("one nanosecond is shorter than the redraw interval");
        dashboard.tick_at(&mut host, before_boundary);

        assert!(first_draw > 0);
        assert_eq!(host.err.len(), first_draw, "drawing before the boundary must be deferred");

        dashboard.tick_at(&mut host, started + REDRAW_INTERVAL);

        assert!(host.err.len() > first_draw, "drawing at the boundary must repaint");
    }

    #[test]
    fn redraw_replaces_the_dashboard_without_a_global_saved_cursor() {
        let mut dashboard = populated(160);
        dashboard.dirty = true;
        let mut host = Sink::default().terminal(160);
        dashboard.tick(&mut host);

        let rows = dashboard
            .drawn_frame
            .as_ref()
            .expect("the first dashboard was drawn")
            .physical_rows(160);
        let first_draw = host.err.len();
        dashboard.last_draw = None;
        dashboard.dirty = true;
        dashboard.tick(&mut host);

        let repaint = str::from_utf8(&host.err[first_draw..]).expect("dashboard output is UTF-8");
        assert!(repaint.starts_with(BEGIN_SYNCHRONIZED_UPDATE), "{repaint:?}");
        assert!(repaint.ends_with(END_SYNCHRONIZED_UPDATE), "{repaint:?}");
        assert!(repaint.contains(&format!("\r\x1b[{}A{ERASE_TO_END}", rows - 1)), "{repaint:?}");
        assert!(!repaint.contains("\x1b7"), "{repaint:?}");
        assert!(!repaint.contains("\x1b8"), "{repaint:?}");
        assert!(repaint.contains("    Planning"), "{repaint:?}");
    }

    #[test]
    fn exceptional_lines_encode_repository_control_sequences() {
        let mut dashboard = populated(160);
        let mut host = Sink::default().terminal(160);

        dashboard.line(&mut host, "SURVIVED", "src/\r\u{1b}[2Kforged\n.rs");

        let output = String::from_utf8(host.err).expect("dashboard output is UTF-8");
        assert!(output.contains(r"src/\r\e[2Kforged\n.rs"), "{output:?}");
    }

    #[test]
    fn exceptional_lines_retract_the_dashboard_before_durable_output() {
        let mut dashboard = populated(160);
        dashboard.dirty = true;
        let mut host = Sink::default().terminal(160);
        dashboard.tick(&mut host);

        let rows = dashboard.drawn_frame.as_ref().expect("the dashboard was drawn").physical_rows(160);
        let line_start = host.err.len();
        dashboard.line(&mut host, "SURVIVED", "src/lib.rs:10");
        let line = str::from_utf8(&host.err[line_start..]).expect("dashboard output is UTF-8");

        assert!(line.contains(&format!("\r\x1b[{}A{ERASE_TO_END}", rows - 1)), "{line:?}");
        assert!(line.contains("SURVIVED src/lib.rs:10\n\r"), "{line:?}");
        assert!(dashboard.drawn_frame.is_none());
    }

    #[test]
    fn panel_borders_and_rows_have_the_same_width() {
        let rendered = panel("X", &["alpha".to_owned(), "longest".to_owned()], Styler::new(false));
        let width = visible_width(&rendered[0]);

        assert!(rendered.iter().all(|line| visible_width(line) == width), "{rendered:#?}");
        assert_eq!(width, "longest".len() + 4, "{rendered:#?}");
    }

    #[test]
    fn table_columns_use_the_shared_gutter() {
        assert_eq!(metric_text("Label", "7"), format!("{:<35}{COLUMN_GAP}{:>10}", "Label", "7"));

        let hints = populated(160).hints_panel().join("\n");
        assert!(hints.contains(&format!("Type{:<16}{COLUMN_GAP}", "")), "{hints}");
    }

    #[test]
    fn long_execution_metrics_stay_inside_the_panel() {
        let mut dashboard = populated(160);
        let started = Instant::now();
        let now = started + Duration::from_mins(1);
        dashboard.testing_started = Some(started);
        dashboard.completions = (0..11).map(|_| now).collect();

        let rendered = dashboard.test_execution_panel_at(now);
        let width = visible_width(&rendered[0]);
        let throughput = rendered
            .iter()
            .find(|line| line.contains("Recent throughput"))
            .expect("throughput row");

        assert!(throughput.contains("11.0 mutants/min"), "{throughput}");
        assert!(throughput.ends_with("11.0 mutants/min │"), "{throughput}");
        assert_eq!(visible_width(throughput), width, "{rendered:#?}");
    }

    #[test]
    fn recording_a_completion_prunes_entries_outside_the_throughput_window() {
        let mut dashboard = Dashboard::new(true, Some(160), Styler::new(false));
        let started = Instant::now();
        let boundary = started + Duration::from_secs(1);
        let now = boundary + THROUGHPUT_WINDOW;
        dashboard.completions.extend([started, boundary]);
        let mut mutant = crate::fixtures::mutant();
        mutant.outcome = Outcome::Killed;

        dashboard.record_at(&mutant, now);

        assert_eq!(dashboard.completions.into_iter().collect::<Vec<_>>(), [boundary, now]);
    }

    #[test]
    fn recent_throughput_includes_completions_at_the_window_boundary() {
        let started = Instant::now();
        let boundary = started + Duration::from_nanos(1);
        let now = boundary + THROUGHPUT_WINDOW;
        let mut dashboard = Dashboard::new(true, Some(160), Styler::new(false));
        dashboard.testing_started = Some(started);
        dashboard.completions.extend([started, boundary, now]);

        let throughput = dashboard.recent_throughput_at(now);

        assert!((throughput - 0.4).abs() < f64::EPSILON, "{throughput}");
    }

    #[test]
    fn large_populations_are_apportioned_to_exactly_one_hundred_cells() {
        let classifications = [
            (WaffleState::Unviable, 86),
            (WaffleState::Pending, 3_116),
            (WaffleState::Killed, 2_092),
            (WaffleState::Survived, 61),
            (WaffleState::Timeout, 18),
            (WaffleState::OutOfMemory, 4),
            (WaffleState::Flaky, 9),
            (WaffleState::Uncovered, 0),
        ];

        let allocation = apportioned(&classifications, 100, 5_386);

        assert_eq!(allocation.iter().sum::<usize>(), 100);
        assert_eq!(allocation, [2, 58, 39, 1, 0, 0, 0, 0]);
    }

    #[test]
    fn small_populations_use_one_cell_per_mutant() {
        let classifications = [(WaffleState::Killed, 6), (WaffleState::Pending, 4)];

        assert_eq!(apportioned(&classifications, 10, 10), [6, 4]);
        assert_eq!(square_columns(10), 4);
    }

    #[test]
    fn selection_telemetry_keeps_only_the_agreed_hint_and_process_metrics() {
        let mut dashboard = Dashboard::new(true, Some(160), Styler::new(false));
        dashboard.record_selection(&selection(SelectionTier::Exact, SelectionResult::Hit, false, 100, 1_000));
        dashboard.record_selection(&selection(SelectionTier::Item, SelectionResult::Miss, true, 300, 2_000));
        dashboard.record_selection(&selection(SelectionTier::Census, SelectionResult::Hit, false, 500, 3_000));
        dashboard.record_selection(&selection(SelectionTier::Whole, SelectionResult::Miss, true, 700, 4_000));

        assert_eq!(dashboard.explicit_hints, Attempts { total: 1, hits: 1 });
        assert_eq!(dashboard.inferred_hints, Attempts { total: 1, hits: 0 });
        assert_eq!(dashboard.specific_test_launches, 2);
        assert_eq!(dashboard.all_test_launches, 2);
        assert_eq!(dashboard.runtime_ms, 1_600);
        assert_eq!(dashboard.maximum_runtime_ms, 700);
        assert_eq!(dashboard.saved_ms, 900);
    }

    #[test]
    fn hint_savings_include_failed_attempts_before_the_hit() {
        let mut dashboard = Dashboard::new(true, Some(160), Styler::new(false));
        let mut miss = selection(SelectionTier::Exact, SelectionResult::Miss, false, 300, 2_000);
        miss.ordinal = 7;
        let mut hit = selection(SelectionTier::Item, SelectionResult::Hit, false, 500, 2_000);
        hit.ordinal = 7;

        dashboard.record_selection(&miss);
        dashboard.record_selection(&hit);

        assert_eq!(dashboard.saved_ms, 1_200);
        assert!(!dashboard.selection_cost_ms.contains_key(&7));
    }

    #[test]
    fn disabled_dashboard_updates_are_inert() {
        let mut dashboard = Dashboard::new(false, None, Styler::new(false));
        let mut host = Sink::default();

        dashboard.start_testing(&mut host, 4, "testing".to_owned());
        dashboard.status(&mut host, "Testing", "one");
        dashboard.phase_progress(&mut host, 1, 2, "mutants planned");
        dashboard.record_selection(&selection(SelectionTier::Exact, SelectionResult::Hit, false, 1, 2));
        dashboard.mutant_started();
        dashboard.testing_status("changed".to_owned());
        dashboard.record(&ci_fixture::mutant("src/lib.rs", 1, "x", Outcome::Killed));
        dashboard.line(&mut host, "Killed", "src/lib.rs");
        dashboard.tick(&mut host);

        assert!(!dashboard.is_active());
        assert!(dashboard.status.is_empty());
        assert!(host.err.is_empty());
    }

    #[test]
    fn recording_each_outcome_updates_the_matching_counter() {
        let mut dashboard = Dashboard::new(true, None, Styler::new(false));
        dashboard.summary.pending = 9;
        for _ in 0..9 {
            dashboard.mutant_started();
        }
        dashboard.testing_status("running".to_owned());
        for outcome in [
            Outcome::Killed,
            Outcome::Survived,
            Outcome::Timeout,
            Outcome::OutOfMemory,
            Outcome::Flaky,
            Outcome::CompileError,
            Outcome::Ignored,
            Outcome::NoCoverage,
            Outcome::NotBuilt,
            Outcome::Pending,
        ] {
            dashboard.record(&ci_fixture::mutant("src/lib.rs", 1, "x", outcome));
        }

        assert_eq!(dashboard.summary.killed, 1);
        assert_eq!(dashboard.summary.survived, 1);
        assert_eq!(dashboard.summary.timeout, 1);
        assert_eq!(dashboard.summary.out_of_memory, 1);
        assert_eq!(dashboard.summary.flaky, 1);
        assert_eq!(dashboard.summary.unviable, 1);
        assert_eq!(dashboard.summary.ignored, 1);
        assert_eq!(dashboard.summary.uncovered, 1);
        assert_eq!(dashboard.summary.not_built, 1);
        assert_eq!(dashboard.summary.pending, 1);
        assert_eq!(dashboard.completed_runs, 10);
        assert_eq!(dashboard.active_workers, 0);
        assert_eq!(dashboard.status, "running");
    }

    #[test]
    fn dashboard_empty_and_boundary_helpers_are_stable() {
        let mut dashboard = Dashboard::new(true, None, Styler::new(false));
        assert_eq!(dashboard.waffle(), (Vec::new(), 1, 0));
        assert!(dashboard.recent_throughput().abs() < f64::EPSILON);
        assert_eq!(duration(60_000), "1m00s");

        let mut left = vec!["a".to_owned()];
        let mut right = vec!["b".to_owned(), "c".to_owned()];
        equalize_panels(&mut left, &mut right);
        assert_eq!(left.len(), right.len());
        equalize_panels(&mut right, &mut left);
        equalize_panels(&mut left, &mut right);

        dashboard.active = true;
        dashboard.drawn_frame = Some(DrawnFrame::new(&["drawn".to_owned()]));
        let mut host = Sink::default();
        dashboard.finish(&mut host);
        dashboard.finish(&mut host);
        assert!(!dashboard.active);
        assert!(dashboard.drawn_frame.is_none());
    }
}
