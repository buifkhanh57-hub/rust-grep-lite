//! Wall-clock timing for the search pipeline.
//!
//! `main.rs` measures two phases — the parallel directory walk and the
//! content search — with a [`Timer`]; `--stats` prints the resulting
//! [`Timeline`] next to the counters from [`crate::stats`]. The timer is a
//! plain value type: no threads, no shared state. Worker threads never touch
//! it, so the coordinating thread is the single writer and needs no
//! synchronization.
//!
//! Durations are *recorded*, not averaged; each phase appears at most once
//! in the timeline and phases keep the order in which they ran.

use std::fmt;
use std::time::{Duration, Instant};

use crate::stats::format_duration;

/// One completed, named phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phase {
    /// Phase label, e.g. `"walk"`.
    pub label: String,
    /// Wall duration of the phase.
    pub duration: Duration,
}

/// Sequential stopwatch for named phases.
///
/// Phases are expected to run one after another (`begin` → `end` → `begin`
/// → ...). Forgetting an `end` is tolerated: the next `begin` (or `finish`)
/// closes the still-open phase automatically, so a small bookkeeping
/// mistake can shorten a phase but never corrupts the report.
#[derive(Debug)]
pub struct Timer {
    started: Instant,
    phases: Vec<Phase>,
    open: Option<(&'static str, Instant)>,
}

impl Timer {
    /// Start a new stopwatch with no recorded phases.
    pub fn start() -> Timer {
        Timer {
            started: Instant::now(),
            phases: Vec::new(),
            open: None,
        }
    }

    /// Begin timing `label`. A phase left open by a missing [`Timer::end`]
    /// call is closed first.
    pub fn begin(&mut self, label: &'static str) {
        self.close_open();
        self.open = Some((label, Instant::now()));
    }

    /// Close the open phase, if any. Calling `end` twice is harmless.
    pub fn end(&mut self) {
        self.close_open();
    }

    fn close_open(&mut self) {
        if let Some((label, started)) = self.open.take() {
            self.phases.push(Phase {
                label: label.to_string(),
                duration: started.elapsed(),
            });
        }
    }

    /// Record a measured duration directly.
    ///
    /// Useful for callers that time a phase elsewhere and only want it to
    /// appear in the report, and for building sample output in tests.
    pub fn record(&mut self, label: &str, duration: Duration) {
        self.phases.push(Phase {
            label: label.to_string(),
            duration,
        });
    }

    /// Total wall time since [`Timer::start`], open phase included.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Sum of all recorded phase durations (saturating).
    pub fn phases_total(&self) -> Duration {
        self.phases
            .iter()
            .fold(Duration::ZERO, |total, phase| {
                total.saturating_add(phase.duration)
            })
    }

    /// Close any open phase and freeze the run into a [`Timeline`].
    pub fn finish(mut self) -> Timeline {
        self.close_open();
        Timeline {
            phases: self.phases,
            total: self.started.elapsed(),
        }
    }
}

/// Immutable report of one timed run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timeline {
    /// Completed phases in recording order.
    pub phases: Vec<Phase>,
    /// Total wall time of the run (at least the sum of the phases).
    pub total: Duration,
}

impl Timeline {
    /// Build a timeline from `(label, micros)` pairs — handy for tests and
    /// for rendering sample output deterministically.
    pub fn from_micros(pairs: &[(&str, u64)]) -> Timeline {
        let phases: Vec<Phase> = pairs
            .iter()
            .map(|(label, micros)| Phase {
                label: (*label).to_string(),
                duration: Duration::from_micros(*micros),
            })
            .collect();
        let total_micros: u64 = pairs.iter().map(|(_, micros)| micros).sum();
        Timeline {
            phases,
            total: Duration::from_micros(total_micros),
        }
    }

    /// Duration recorded for `label`, if the phase ran.
    pub fn phase(&self, label: &str) -> Option<Duration> {
        self.phases
            .iter()
            .find(|phase| phase.label == label)
            .map(|phase| phase.duration)
    }
}

impl fmt::Display for Timeline {
    /// Render an aligned phase table:
    ///
    /// ```text
    /// walk     1.2 ms
    /// search   8.9 ms
    /// total   10.1 ms
    /// ```
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut width = "total".len();
        for phase in &self.phases {
            width = width.max(phase.label.len());
        }
        for phase in &self.phases {
            writeln!(
                f,
                "{:<width$}  {}",
                phase.label,
                format_duration(phase.duration),
                width = width
            )?;
        }
        writeln!(
            f,
            "{:<width$}  {}",
            "total",
            format_duration(self.total),
            width = width
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn begin_end_records_the_phase() {
        let mut timer = Timer::start();
        timer.begin("walk");
        thread::sleep(Duration::from_millis(2));
        timer.end();

        let timeline = timer.finish();
        assert_eq!(timeline.phases.len(), 1);
        let walk = timeline.phase("walk").expect("walk recorded");
        assert!(walk >= Duration::from_millis(2), "elapsed: {:?}", walk);
        assert!(timeline.total >= walk);
        assert_eq!(timeline.phase("search"), None);
    }

    #[test]
    fn begin_twice_closes_the_open_phase() {
        let mut timer = Timer::start();
        timer.begin("walk");
        timer.begin("search");
        timer.end();

        let timeline = timer.finish();
        assert_eq!(timeline.phases.len(), 2);
        assert_eq!(timeline.phases[0].label, "walk");
        assert_eq!(timeline.phases[1].label, "search");
    }

    #[test]
    fn finish_closes_a_forgotten_phase() {
        let mut timer = Timer::start();
        timer.begin("walk");
        thread::sleep(Duration::from_millis(1));
        // No end() call on purpose.
        let timeline = timer.finish();
        assert!(timeline.phase("walk").is_some());
        assert_eq!(timeline.phases.len(), 1);
    }

    #[test]
    fn record_and_phases_total() {
        let mut timer = Timer::start();
        timer.record("walk", Duration::from_millis(2));
        timer.record("search", Duration::from_millis(5));
        assert_eq!(timer.phases_total(), Duration::from_millis(7));
        // elapsed() only requires the clock to have moved forward.
        assert!(timer.elapsed() >= Duration::ZERO);
    }

    #[test]
    fn end_without_begin_is_harmless() {
        let mut timer = Timer::start();
        timer.end();
        assert_eq!(timer.finish().phases.len(), 0);
    }

    #[test]
    fn timeline_builds_from_micro_pairs() {
        let timeline = Timeline::from_micros(&[("walk", 1_200), ("search", 8_900)]);
        assert_eq!(timeline.phase("walk"), Some(Duration::from_micros(1_200)));
        assert_eq!(timeline.phase("search"), Some(Duration::from_micros(8_900)));
        assert_eq!(timeline.phase("render"), None);
        assert_eq!(timeline.total, Duration::from_micros(10_100));
    }

    #[test]
    fn display_renders_an_aligned_table() {
        let timeline = Timeline::from_micros(&[("walk", 1_200), ("search", 8_900)]);
        let text = timeline.to_string();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("walk"), "got: {}", lines[0]);
        assert!(lines[0].contains("1.2 ms"));
        assert!(lines[1].starts_with("search"));
        assert!(lines[1].contains("8.9 ms"));
        assert!(lines[2].starts_with("total"));
        assert!(lines[2].contains("10.1 ms"));
    }
}
