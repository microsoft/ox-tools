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

    #[cfg(test)]
    pub(super) fn testing_status_for_test(&self) -> &str {
        &self.status
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
            let label = self.styler.verb("Planned");
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
        // #[gamma::skip(call_result.default, call.replace_with_default, stmt.delete_call, reason = "both panels have exactly ten content rows plus two borders, so equalization is a no-op; call_result.default also retains the original unit call")]
        equalize_panels(&mut left, &mut right);
        // #[gamma::skip(iter.max_to_min, literal.int_increment, reason = "panel produces nonempty rectangular rows, so every row has the same width and unwrap_or's fallback is unreachable")]
        let left_width = left.iter().map(|line| visible_width(line)).max().unwrap_or(0);
        let rows = left.len();
        let mut body = Vec::with_capacity(rows.saturating_add(10));
        body.push(self.header());
        body.push(String::new());
        // #[gamma::skip(iter.max_to_min, literal.int_increment, reason = "panel produces nonempty rectangular rows, so every row has the same width and unwrap_or's fallback is unreachable")]
        let right_width = right.iter().map(|line| visible_width(line)).max().unwrap_or(0);
        let top_width = left_width.saturating_add(3).saturating_add(right_width);
        let top_indent = " ".repeat(self.width.saturating_sub(top_width) / 2);

        for row in 0..rows {
            // #[gamma::skip(literal.str_to_xyzzy, reason = "row is below left.len(), so get(row) is Some and the fallback string is unreachable")]
            let lhs = left.get(row).map_or("", String::as_str);
            // #[gamma::skip(literal.str_to_xyzzy, reason = "equalize_panels makes right.len() equal left.len(), so get(row) is Some throughout the row loop")]
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

    #[test]
    fn survivor_wide_dashboard_rows_keep_exact_three_column_gaps_and_centering_for_both_width_parities() {
        let now = Instant::now();
        for width in [160, 161] {
            let dashboard = populated(width);
            let left = dashboard.mutants_panel();
            let right = dashboard.test_execution_panel_at(now);
            assert_eq!(left.len(), right.len());
            let columns = left[0].chars().count() + 3 + right[0].chars().count();
            let indent = " ".repeat((usize::from(width) - columns) / 2);
            let rendered = dashboard.render_at(now);
            for (row, (left, right)) in left.iter().zip(&right).enumerate() {
                assert_eq!(rendered[row + 2], format!("{indent}{left}   {right}"), "width {width}, row {row}");
            }
        }
    }

    #[test]
    fn survivor_exceptional_dashboard_output_requires_a_complete_redraw_and_keeps_hint_table_order() {
        let mut dashboard = populated(160);
        let now = Instant::now();
        let mut host = Sink::default().terminal(160);
        dashboard.dirty = true;
        dashboard.tick_at(&mut host, now);
        assert!(!dashboard.dirty);
        dashboard.line(&mut host, "compiler", "native\nline");
        assert!(
            dashboard.dirty,
            "an exceptional line invalidates the frame even when no counters changed"
        );
        assert!(dashboard.drawn_frame.is_none());
        assert!(host.err().contains("compiler native\\nline"));
        let before_redraw = host.err();
        dashboard.tick_at(&mut host, now + REDRAW_INTERVAL);
        assert!(
            host.err().len() > before_redraw.len(),
            "the cleared dashboard must actually be repainted"
        );
        assert!(dashboard.drawn_frame.is_some());
        assert!(!dashboard.dirty);
        let table = dashboard.hints_panel();
        assert!(
            table[1].contains("Type") && table[1].contains("Count") && table[1].contains("Hit rate"),
            "{table:?}"
        );
        assert!(table[2].contains(&"─".repeat(48)), "{table:?}");
        assert!(table[3].contains("Explicit hints") && table[3].contains("87.5%"), "{table:?}");
        assert!(table[4].contains("Inferred hints") && table[4].contains("50.0%"), "{table:?}");
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
    fn survivor_formatting_boundaries_have_exact_human_readable_values() {
        for (input, expected) in [
            (0, "0ms"),
            (999, "999ms"),
            (1_000, "1.0s"),
            (59_999, "60.0s"),
            (60_000, "1m00s"),
            (61_999, "1m01s"),
            (3_599_999, "59m59s"),
            (3_600_000, "1h00m"),
            (7_321_999, "2h02m"),
        ] {
            assert_eq!(duration(input), expected, "{input}");
        }
        for (value, total, expected) in [(0, 0, "0.0%"), (0, 3, "0.0%"), (1, 1, "100.0%"), (1, 3, "33.3%"), (3, 3, "100.0%")] {
            assert_eq!(percent(value, total), expected);
        }
        for (value, expected) in [
            (0, "0"),
            (1, "1"),
            (999, "999"),
            (1_000, "1,000"),
            (10_001, "10,001"),
            (1_234_567, "1,234,567"),
        ] {
            assert_eq!(number(value), expected);
        }
        for (cells, expected) in [(0, 1), (1, 1), (2, 2), (4, 2), (5, 3), (9, 3), (10, 4), (100, 10)] {
            assert_eq!(square_columns(cells), expected);
        }
    }

    #[test]
    fn survivor_panels_preserve_borders_padding_centering_and_insert_before_the_bottom() {
        let styler = Styler::new(false);
        assert_eq!(panel("X", &[], styler), ["╭─ X ╮", "╰────╯"]);
        let short = panel("X", &["ab".to_owned(), "c".to_owned()], styler);
        assert_eq!(short, ["╭─ X ╮", "│ ab │", "│ c  │", "╰────╯"]);
        let tall = panel("Y", &["1234".to_owned(), "q".to_owned(), "r".to_owned()], styler);
        assert_eq!(centered(short.clone(), 11), ["  ╭─ X ╮", "  │ ab │", "  │ c  │", "  ╰────╯"]);
        assert_eq!(centered(short.clone(), 3), short);
        let mut left = short.clone();
        let mut right = tall.clone();
        equalize_panels(&mut left, &mut right);
        assert_eq!(left, ["╭─ X ╮", "│ ab │", "│ c  │", "│    │", "╰────╯"]);
        assert_eq!(right, tall);
        let mut left = tall.clone();
        let mut right = short;
        equalize_panels(&mut left, &mut right);
        assert_eq!(left, tall);
        assert_eq!(right, ["╭─ X ╮", "│ ab │", "│ c  │", "│    │", "╰────╯"]);
        let colored = panel("X", &["ab".to_owned(), "c".to_owned()], Styler::new(true));
        assert!(colored[0].contains('\u{1b}'));
        assert_eq!(
            colored.iter().map(|line| crate::report::unstyled(line)).collect::<Vec<_>>(),
            ["╭─ X ╮", "│ ab │", "│ c  │", "╰────╯"]
        );
    }

    #[test]
    fn survivor_waffle_population_matrix_retains_classification_and_blank_rows() {
        for (killed, survived, columns, rows) in [(0, 0, 1, 0), (1, 0, 1, 1), (2, 1, 2, 2), (9, 0, 3, 3), (99, 1, 10, 10)] {
            let mut dashboard = Dashboard::new(true, Some(160), Styler::new(false));
            dashboard.summary.killed = killed;
            dashboard.summary.survived = survived;
            let (cells, actual_columns, actual_rows) = dashboard.waffle();
            assert_eq!((actual_columns, actual_rows), (columns, rows));
            assert_eq!(cells.len(), (killed + survived) as usize);
            assert_eq!(cells.iter().filter(|&&state| state == WaffleState::Killed).count(), killed as usize);
            assert_eq!(
                cells.iter().filter(|&&state| state == WaffleState::Survived).count(),
                survived as usize
            );
            assert_eq!(
                cells,
                core::iter::repeat_n(WaffleState::Killed, killed as usize)
                    .chain(core::iter::repeat_n(WaffleState::Survived, survived as usize))
                    .collect::<Vec<_>>()
            );
            let rendered = dashboard.mutants_panel();
            assert_eq!(rendered.len(), 12);
            let chart: Vec<_> = rendered[1..11]
                .iter()
                .map(|line| line.chars().skip(2).take(20).collect::<String>())
                .collect();
            assert_eq!(chart.iter().map(|line| line.matches("KK").count()).sum::<usize>(), killed as usize);
            assert_eq!(
                chart.iter().map(|line| line.matches("SS").count()).sum::<usize>(),
                survived as usize
            );
            for line in &chart[rows..] {
                assert_eq!(line, &" ".repeat(20));
            }
            assert_eq!(rendered.iter().filter(|line| line.contains("Mutation score")).count(), 1);
            assert!(rendered[10].contains("Mutation score"));
        }
        let classes = [
            (WaffleState::Killed, 101),
            (WaffleState::Survived, 202),
            (WaffleState::Timeout, 303),
        ];
        assert_eq!(apportioned(&classes, 100, 606), [17, 33, 50]);
        assert_eq!(apportioned(&classes, 606, 606), [101, 202, 303]);
        assert_eq!(
            apportioned(
                &[
                    (WaffleState::Killed, 303),
                    (WaffleState::Survived, 202),
                    (WaffleState::Timeout, 101)
                ],
                100,
                606
            ),
            [50, 33, 17]
        );
        let mut capped = Dashboard::new(true, None, Styler::new(false));
        capped.summary.killed = 101;
        let (cells, columns, rows) = capped.waffle();
        assert_eq!((cells.len(), columns, rows), (100, 10, 10));
    }

    #[test]
    fn survivor_throughput_includes_the_five_minute_boundary_and_excludes_older_completions() {
        let started = Instant::now();
        let now = started + Duration::from_mins(10);
        let mut dashboard = Dashboard::new(true, None, Styler::new(false));
        assert!(dashboard.recent_throughput_at(now).abs() < f64::EPSILON);
        dashboard.testing_started = Some(started);
        dashboard.completions = [
            started + Duration::from_secs(299),
            started + Duration::from_mins(5),
            started + Duration::from_secs(599),
            now,
        ]
        .into();
        assert!((dashboard.recent_throughput_at(now) - 0.6).abs() < f64::EPSILON);
        dashboard.testing_started = Some(now);
        assert!(dashboard.recent_throughput_at(now).abs() < f64::EPSILON);
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
        let planning = rendered.find("     Planned 7/7 mutants planned\n").expect("durable planning line");
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

    #[test]
    fn constructor_retains_color_policy_width_boundaries_and_zero_telemetry() {
        for enabled in [false, true] {
            for colored in [false, true] {
                for (width, expected) in [
                    (None, 80),
                    (Some(0), 40),
                    (Some(39), 40),
                    (Some(40), 40),
                    (Some(79), 79),
                    (Some(80), 80),
                    (Some(135), 135),
                    (Some(136), 136),
                ] {
                    let dashboard = Dashboard::new(enabled, width, Styler::new(colored));
                    assert_eq!(dashboard.enabled(), enabled);
                    assert!(!dashboard.is_active());
                    assert!(!dashboard.active);
                    assert!(!dashboard.dirty);
                    assert_eq!(dashboard.styler.enabled(), colored);
                    assert_eq!(dashboard.width, expected);
                    assert_eq!(dashboard.phase, Phase::Planning);
                    assert_eq!(dashboard.summary, Summary::default());
                    assert!(dashboard.status.is_empty());
                    assert!(dashboard.drawn_frame.is_none());
                    assert!(dashboard.last_draw.is_none());
                    assert!(dashboard.testing_started.is_none());
                    assert_eq!(
                        (
                            dashboard.binaries,
                            dashboard.jobs,
                            dashboard.active_workers,
                            dashboard.completed_runs
                        ),
                        (0, 0, 0, 0)
                    );
                    assert_eq!(
                        (dashboard.all_test_launches, dashboard.specific_test_launches, dashboard.launches()),
                        (0, 0, 0)
                    );
                    assert_eq!((dashboard.runtime_ms, dashboard.maximum_runtime_ms, dashboard.saved_ms), (0, 0, 0));
                    assert_eq!(dashboard.maximum_memory, None);
                    assert_eq!(dashboard.explicit_hints, Attempts::default());
                    assert_eq!(dashboard.inferred_hints, Attempts::default());
                    assert!(dashboard.selection_cost_ms.is_empty());
                    assert!(dashboard.completions.is_empty());
                }
            }
        }
    }

    #[test]
    fn display_helpers_cover_empty_nonunit_and_wrapping_boundaries() {
        for (value, expected) in [
            (0, "0"),
            (1, "1"),
            (12, "12"),
            (999, "999"),
            (1_000, "1,000"),
            (12_345, "12,345"),
            (123_456_789, "123,456,789"),
        ] {
            assert_eq!(number(value), expected);
        }
        for (total, count, expected) in [(0, 0, 0), (19, 0, 0), (0, 5, 0), (19, 1, 19), (19, 4, 4), (u64::MAX, 1, u64::MAX)] {
            assert_eq!(average(total, count), expected);
        }
        for (numerator, denominator, expected) in [(0, 0, 0.0), (8, 0, 0.0), (0, 8, 0.0), (3, 2, 1.5), (1, 4, 0.25)] {
            assert!((ratio(numerator, denominator) - expected).abs() < f64::EPSILON);
        }
        assert_eq!(metric("Binaries", 12_345), format!("Binaries{}12,345", " ".repeat(34)));
        assert_eq!(metric_text(&"L".repeat(47), "V"), format!("{}   V", "L".repeat(47)));
        for (state, marker, label) in [
            (WaffleState::Unviable, "UU", "Unviable"),
            (WaffleState::Pending, "PP", "Pending"),
            (WaffleState::Killed, "KK", "Killed"),
            (WaffleState::Survived, "SS", "Survived"),
            (WaffleState::Timeout, "TT", "Timed out"),
            (WaffleState::OutOfMemory, "OO", "Out of mem"),
            (WaffleState::Flaky, "FF", "Flaky"),
            (WaffleState::Uncovered, "CC", "Uncovered"),
        ] {
            assert_eq!(state.cell(Styler::new(false)), marker);
            assert_eq!(state.label(), label);
            let colored = state.cell(Styler::new(true));
            assert!(colored.contains('\x1b'));
            assert_eq!(crate::report::unstyled(&colored), "██");
        }
        for (lines, width, expected) in [
            (vec![], 40, 0),
            (vec![String::new()], 40, 1),
            (vec!["x".repeat(40)], 40, 1),
            (vec!["x".repeat(41)], 40, 2),
            (vec!["x".repeat(80)], 40, 2),
            (vec!["x".repeat(81), String::new(), "界".repeat(21)], 40, 6),
            (vec!["\x1b[31m界\x1b[0m".to_owned(), "abc".to_owned()], 2, 3),
        ] {
            assert_eq!(DrawnFrame::new(&lines).physical_rows(width), expected);
        }
    }

    fn measurement_fixture(outcomes: &[Outcome]) -> (Plan, Session) {
        let plan = Plan {
            root: camino::Utf8PathBuf::new(),
            files: Vec::new(),
            mutants: outcomes
                .iter()
                .map(|outcome| ci_fixture::mutant("src/lib.rs", 1, "x", *outcome))
                .collect(),
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            digests: crate::HashMap::default(),
            skipped: Vec::new(),
            reach: crate::HashMap::default(),
            specs: crate::HashMap::default(),
        };
        let session = Session {
            baseline: Duration::ZERO,
            baseline_wall: Duration::ZERO,
            tests: None,
            quiet: Duration::ZERO,
            stall: None,
            build: Duration::ZERO,
            peak: Some(123_456),
            metered: false,
            unbounded: None,
            withdrawn: 0,
            census: Vec::new(),
            rounds: 1,
            rounds_taken: Vec::new(),
            binaries: vec![crate::testing::test_binary("first"), crate::testing::test_binary("second")],
            scratch: camino::Utf8PathBuf::new(),
            filtered: 0,
            widened: false,
            ordering: crate::exec::OrderingHints::default(),
            phases: crate::exec::Phases::default(),
        };
        (plan, session)
    }

    #[test]
    fn measurement_and_planning_replace_previous_state_without_touching_disabled_displays() {
        for enabled in [false, true] {
            for pending in [false, true] {
                let (plan, session) = measurement_fixture(if pending {
                    &[Outcome::Pending, Outcome::Pending, Outcome::Survived]
                } else {
                    &[Outcome::Killed]
                });
                let mut dashboard = populated(100);
                dashboard.enabled = enabled;
                dashboard.active = false;
                dashboard.phase = Phase::Testing;
                dashboard.status = "previous".to_owned();
                dashboard.dirty = false;
                let previous_summary = dashboard.summary;
                dashboard.measured(&plan, &session);
                assert_eq!(
                    dashboard.summary,
                    if enabled { Summary::of(&plan.mutants) } else { previous_summary }
                );
                assert_eq!(dashboard.binaries, if enabled { 2 } else { 12 });
                assert_eq!(
                    dashboard.maximum_memory,
                    if enabled { Some(123_456) } else { Some(16 * 1024 * 1024) }
                );
                assert_eq!(dashboard.dirty, enabled);
                assert_eq!(dashboard.phase, Phase::Testing);
                assert!(!dashboard.active);
                let mut host = Sink::default();
                dashboard.start_planning(&mut host, &plan, 5, 7);
                assert_eq!(dashboard.active, enabled);
                assert_eq!(dashboard.phase, if enabled { Phase::Planning } else { Phase::Testing });
                assert_eq!(dashboard.binaries, if enabled { 5 } else { 12 });
                assert_eq!(dashboard.jobs, if enabled { 7 } else { 4 });
                assert_eq!(
                    dashboard.status,
                    if enabled {
                        if pending {
                            "0/2 mutants planned"
                        } else {
                            "Preparing mutation sweep"
                        }
                    } else {
                        "previous"
                    }
                );
                assert_eq!(host.err.is_empty(), !enabled);
                assert_eq!(dashboard.drawn_frame.is_some(), enabled);
                assert!(!dashboard.dirty);
            }
        }
    }

    #[test]
    fn testing_transition_commits_planning_only_when_it_was_active_and_forces_a_new_frame() {
        let now = Instant::now();
        for active in [false, true] {
            for phase in [Phase::Planning, Phase::Testing] {
                let mut dashboard = populated(100);
                dashboard.active = active;
                dashboard.phase = phase;
                dashboard.status = "previous".to_owned();
                dashboard.last_draw = Some(now);
                dashboard.dirty = false;
                let mut host = Sink::default();
                dashboard.start_testing_at(&mut host, 9, "new".to_owned(), now);
                assert!(dashboard.active);
                assert_eq!(dashboard.phase, Phase::Testing);
                assert_eq!(dashboard.testing_started, Some(now));
                assert_eq!(dashboard.jobs, 9);
                assert_eq!(dashboard.status, "new");
                let transitioned = active && phase == Phase::Planning;
                assert_eq!(host.err().contains("     Planned previous\n\r"), transitioned);
                assert_eq!(dashboard.drawn_frame.is_some(), transitioned);
                assert_eq!(dashboard.dirty, !transitioned);
                assert_eq!(dashboard.last_draw, Some(now));
            }
        }
        let mut dashboard = Dashboard::new(true, None, Styler::new(false));
        dashboard.start_testing(&mut Sink::default(), 13, "immediate".to_owned());
        assert_eq!(dashboard.jobs, 13);
        assert!(dashboard.testing_started.is_some());
        assert_eq!(dashboard.status, "immediate");
    }

    #[test]
    fn selection_telemetry_tables_cover_every_tier_result_chain_and_disabled_case() {
        for enabled in [false, true] {
            for tier in [
                SelectionTier::Exact,
                SelectionTier::Item,
                SelectionTier::Reach,
                SelectionTier::File,
                SelectionTier::Census,
                SelectionTier::Selected,
                SelectionTier::Whole,
                SelectionTier::HintedFallback,
            ] {
                for result in [SelectionResult::Hit, SelectionResult::Miss, SelectionResult::Inconclusive] {
                    for all_tests in [false, true] {
                        let mut dashboard = Dashboard::new(enabled, None, Styler::new(false));
                        dashboard.selection_cost_ms.insert(1, 13);
                        dashboard.record_selection(&selection(tier, result, all_tests, 17, 100));
                        let hit = result == SelectionResult::Hit;
                        let inferred = matches!(tier, SelectionTier::Item | SelectionTier::Reach | SelectionTier::File);
                        assert_eq!(
                            dashboard.explicit_hints,
                            Attempts {
                                total: usize::from(enabled && tier == SelectionTier::Exact),
                                hits: usize::from(enabled && tier == SelectionTier::Exact && hit),
                            }
                        );
                        assert_eq!(
                            dashboard.inferred_hints,
                            Attempts {
                                total: usize::from(enabled && inferred),
                                hits: usize::from(enabled && inferred && hit),
                            }
                        );
                        assert_eq!(dashboard.all_test_launches, usize::from(enabled && all_tests));
                        assert_eq!(dashboard.specific_test_launches, usize::from(enabled && !all_tests));
                        assert_eq!(dashboard.runtime_ms, if enabled { 17 } else { 0 });
                        assert_eq!(dashboard.maximum_runtime_ms, if enabled { 17 } else { 0 });
                        assert_eq!(
                            dashboard.saved_ms,
                            if enabled && hit && (inferred || tier == SelectionTier::Exact) {
                                70
                            } else {
                                0
                            }
                        );
                        assert_eq!(
                            dashboard.selection_cost_ms.get(&1).copied(),
                            if !enabled {
                                Some(13)
                            } else if hit || tier == SelectionTier::Whole {
                                None
                            } else {
                                Some(30)
                            }
                        );
                        assert_eq!(dashboard.dirty, enabled);
                    }
                }
            }
        }
        let mut dashboard = Dashboard::new(true, None, Styler::new(false));
        dashboard.selection_cost_ms.insert(1, 200);
        dashboard.record_selection(&selection(SelectionTier::Exact, SelectionResult::Hit, false, 17, 100));
        assert_eq!(dashboard.saved_ms, 0);
        dashboard.record_selection(&selection(SelectionTier::Exact, SelectionResult::Hit, false, 5, 100));
        assert_eq!(dashboard.saved_ms, 95);
        assert_eq!(dashboard.maximum_runtime_ms, 17);
    }

    #[test]
    fn recording_verdicts_and_worker_updates_are_exact_from_nonzero_state() {
        let now = Instant::now();
        for enabled in [false, true] {
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
                let mut dashboard = populated(100);
                dashboard.enabled = enabled;
                dashboard.dirty = false;
                let mut expected = dashboard.summary;
                if enabled {
                    expected.pending -= 1;
                    match outcome {
                        Outcome::Killed => expected.killed += 1,
                        Outcome::Survived => expected.survived += 1,
                        Outcome::Timeout => expected.timeout += 1,
                        Outcome::OutOfMemory => expected.out_of_memory += 1,
                        Outcome::Flaky => expected.flaky += 1,
                        Outcome::CompileError => expected.unviable += 1,
                        Outcome::Ignored => expected.ignored += 1,
                        Outcome::NoCoverage => expected.uncovered += 1,
                        Outcome::NotBuilt => expected.not_built += 1,
                        Outcome::Pending => expected.pending += 1,
                    }
                }
                dashboard.record_at(&ci_fixture::mutant("src/lib.rs", 1, "x", outcome), now);
                assert_eq!(dashboard.summary, expected);
                assert_eq!(dashboard.active_workers, if enabled { 2 } else { 3 });
                assert_eq!(dashboard.completed_runs, if enabled { 81 } else { 80 });
                assert_eq!(
                    dashboard.completions.iter().copied().collect::<Vec<_>>(),
                    if enabled { vec![now] } else { vec![] }
                );
                assert_eq!(dashboard.dirty, enabled);
                dashboard.dirty = false;
                let previous_workers = dashboard.active_workers;
                dashboard.mutant_started();
                assert_eq!(dashboard.active_workers, previous_workers + usize::from(enabled));
                assert_eq!(dashboard.dirty, enabled);
                dashboard.dirty = false;
                dashboard.testing_status("new status".to_owned());
                assert_eq!(dashboard.status, if enabled { "new status" } else { "" });
                assert_eq!(dashboard.dirty, enabled);
            }
        }
    }

    #[test]
    fn active_status_updates_encode_controls_and_zero_total_does_not_dirty_or_redraw() {
        for enabled in [false, true] {
            for active in [false, true] {
                let mut dashboard = Dashboard::new(enabled, Some(100), Styler::new(false));
                dashboard.active = active;
                let mut host = Sink::default();
                dashboard.status(&mut host, "\nBuild", "\x1b[31m\r");
                let updating = enabled && active;
                assert_eq!(dashboard.status, if updating { r"\nBuild \e[31m\r" } else { "" });
                assert_eq!(host.err.is_empty(), !updating);
                assert!(!dashboard.dirty);
                dashboard.last_draw = None;
                host.err.clear();
                dashboard.phase_progress(&mut host, 1, 1, "\nunit");
                assert_eq!(dashboard.status, if updating { r"1/1 \nunit" } else { "" });
                assert_eq!(host.err.is_empty(), !updating);
                let before = host.err.len();
                dashboard.phase_progress(&mut host, 5, 0, "ignored");
                assert_eq!(host.err.len(), before);
                assert!(!dashboard.dirty);
                assert_eq!(dashboard.status, if updating { r"1/1 \nunit" } else { "" });
            }
        }
        let mut dashboard = populated(100);
        let now = Instant::now();
        dashboard.last_draw = Some(now + Duration::from_secs(10));
        let mut host = Sink::default();
        dashboard.status(&mut host, "  Build", "subject  ");
        assert_eq!(dashboard.status, "Build subject");
        assert!(dashboard.dirty);
        assert!(host.err.is_empty());
        dashboard.phase_progress(&mut host, 2, 3, "items");
        assert_eq!(dashboard.status, "2/3 items");
        assert!(dashboard.dirty);
    }

    #[test]
    fn frame_lifecycle_is_exact_for_blank_wrapped_dirty_disabled_and_resized_displays() {
        let now = Instant::now();
        for active in [false, true] {
            for dirty in [false, true] {
                let mut dashboard = Dashboard::new(true, Some(40), Styler::new(false));
                dashboard.active = active;
                dashboard.dirty = dirty;
                let mut host = Sink::default();
                dashboard.tick_at(&mut host, now);
                assert_eq!(host.err.is_empty(), !(active && dirty));
                assert_eq!(dashboard.dirty, dirty && !active);
                assert_eq!(dashboard.last_draw, (active && dirty).then_some(now));
                if active && dirty {
                    assert_eq!(
                        host.err(),
                        format!(
                            "{BEGIN_SYNCHRONIZED_UPDATE}\r{}{END_SYNCHRONIZED_UPDATE}",
                            dashboard.render_small().join("\n")
                        )
                    );
                    assert_eq!(dashboard.drawn_frame.as_ref().map(|frame| frame.line_widths.len()), Some(5));
                }
            }
        }
        let mut dashboard = Dashboard::new(true, Some(80), Styler::new(false));
        assert_eq!(dashboard.begin_frame(), format!("{BEGIN_SYNCHRONIZED_UPDATE}\r"));
        for (widths, expected) in [
            (vec![1], format!("{BEGIN_SYNCHRONIZED_UPDATE}\r{ERASE_TO_END}")),
            (vec![80, 80], format!("{BEGIN_SYNCHRONIZED_UPDATE}\r\x1b[1A{ERASE_TO_END}")),
            (vec![81], format!("{BEGIN_SYNCHRONIZED_UPDATE}\r\x1b[1A{ERASE_TO_END}")),
        ] {
            dashboard.drawn_frame = Some(DrawnFrame { line_widths: widths });
            assert_eq!(dashboard.begin_frame(), expected);
        }
        dashboard.active = true;
        dashboard.dirty = false;
        dashboard.drawn_frame = Some(DrawnFrame { line_widths: vec![81] });
        let mut host = Sink::default().terminal(40);
        dashboard.line(&mut host, "LABEL", "\n");
        assert_eq!(dashboard.width, 40);
        assert_eq!(
            host.err(),
            format!("{BEGIN_SYNCHRONIZED_UPDATE}\r\x1b[2A{ERASE_TO_END}LABEL \\n\n\r{END_SYNCHRONIZED_UPDATE}")
        );
        assert!(dashboard.drawn_frame.is_none());
        assert!(dashboard.dirty);
        dashboard.drawn_frame = Some(DrawnFrame { line_widths: vec![81] });
        host.resize_terminal(80);
        host.err.clear();
        dashboard.finish(&mut host);
        assert_eq!(dashboard.width, 80);
        assert_eq!(
            host.err(),
            format!("{BEGIN_SYNCHRONIZED_UPDATE}\r\x1b[1A{ERASE_TO_END}{END_SYNCHRONIZED_UPDATE}")
        );
        assert!(!dashboard.active);
        assert!(!dashboard.dirty);
        assert!(dashboard.drawn_frame.is_none());
        let before = host.err.len();
        dashboard.finish(&mut host);
        assert_eq!(host.err.len(), before);
        for (width, expected) in [(0, 40), (39, 40), (40, 40), (41, 41)] {
            host.resize_terminal(width);
            dashboard.dirty = false;
            let changed = dashboard.width != expected;
            dashboard.resize(&host);
            assert_eq!(dashboard.width, expected);
            assert_eq!(dashboard.dirty, changed);
        }
    }

    #[test]
    fn layout_boundaries_preserve_complete_rows_metrics_spacing_and_order() {
        let now = Instant::now();
        for width in [79, 80, 135, 136, 160] {
            let mut dashboard = populated(width);
            dashboard.phase = Phase::Testing;
            dashboard.status = "running".to_owned();
            assert_eq!(dashboard.launches(), 80);
            let lines = dashboard.render_at(now);
            assert_eq!(lines[0], "running");
            if width < 80 {
                assert_eq!(
                    lines,
                    [
                        "running".to_owned(),
                        "─".repeat(usize::from(width)),
                        "Mutants   K 70 · S 4 · T 2 · OOM 1 · F 1 · U 2 · P 7".to_owned(),
                        "Hints     explicit 35/40 · inferred 10/20".to_owned(),
                        "Process   12 bin · 80 runs · 3/4 busy".to_owned(),
                    ]
                );
            } else if width < 136 {
                assert_eq!(lines.len(), 6);
                assert_eq!(lines[1], "─".repeat(usize::from(width)));
                assert_eq!(
                    lines[2],
                    Dashboard::fit(
                        "Mutants   killed 70 · survived 4 · timeout 2 · OOM 1 · flaky 1 · uncovered 2 · pending 7",
                        usize::from(width)
                    )
                );
                assert_eq!(lines[3], "Hints     explicit 40 (87.5%) · inferred 20 (50.0%)");
                assert_eq!(lines[4], "Process   12 binaries · 80 launches · 150ms average · 3/4 busy");
                assert_eq!(lines[5], "Progress  80/87 (92.0%)");
            } else {
                assert_eq!(lines.len(), 21);
                assert_eq!(lines[1], "");
                assert_eq!(lines[14], "");
                let indent = (usize::from(width) - 108) / 2;
                for row in &lines[2..14] {
                    assert_eq!(visible_width(row), indent + 108, "{row}");
                    assert!(row.starts_with(&" ".repeat(indent)));
                    assert_eq!(row.chars().nth(indent + 53), Some(' '));
                    assert_eq!(
                        row.chars().nth(indent + 56),
                        Some(if row == &lines[2] {
                            '╭'
                        } else if row == &lines[13] {
                            '╰'
                        } else {
                            '│'
                        })
                    );
                }
                assert!(lines[2].contains("MUTANTS"));
                assert!(lines[2].contains("TEST EXECUTION"));
                assert!(lines[15].starts_with(&format!("{}╭─ HINTS", " ".repeat((usize::from(width) - 52) / 2))));
            }
        }
        let dashboard = populated(160);
        let expected_metrics = [
            ("Test binaries", "12"),
            ("Workers busy", "3 / 4"),
            ("Recent throughput", "0.0 mutants/min"),
            ("All-tests selections", "50"),
            ("Specific-test selections", "30"),
            ("Average selection runtime", "150ms"),
            ("Maximum selection runtime", "2.0s"),
            ("Average selections/completed mutant", "1.00"),
            ("Baseline peak memory", "16.0 MB"),
            ("Estimated time saved by hints", "1h00m"),
        ];
        let execution = dashboard.test_execution_panel_at(now);
        for (row, (label, value)) in execution[1..11].iter().zip(expected_metrics) {
            assert_eq!(row, &format!("│ {label}{}{value} │", " ".repeat(48 - label.len() - value.len())));
        }
        let mut dashboard = Dashboard::new(true, None, Styler::new(false));
        dashboard.maximum_memory = None;
        assert!(dashboard.test_execution_panel_at(now)[9].contains("not measured"));
        dashboard.summary.pending = 1;
        assert_eq!(dashboard.render_medium()[5], "Progress  0/1 (0.0%)");
    }
}
