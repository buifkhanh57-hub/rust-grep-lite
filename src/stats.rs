//! Counters and timings for the search pipeline.
//!
//! Every statistic searchlight reports is an [`AtomicU64`] so worker threads
//! update shared state without locks; the main thread snapshots the counters
//! once the pipeline finishes. Durations are recorded per phase (walk and
//! search) by the coordinating thread, which avoids any shared-clock tricks.
//!
//! The `--stats` table is rendered from a [`StatsSnapshot`], an immutable
//! copy, so the numbers printed can never change while they are formatted.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Shared, lock-free counters for one search run.
#[derive(Debug, Default)]
pub struct Stats {
    /// Candidate files discovered (after glob/size/ignore filtering).
    pub files_found: AtomicU64,
    /// Text files actually scanned by the search layer.
    pub files_searched: AtomicU64,
    /// Files skipped because they looked binary or were not valid UTF-8.
    pub files_binary: AtomicU64,
    /// Files containing at least one matching line.
    pub files_matched: AtomicU64,
    /// Logical lines examined (matching or not).
    pub lines_scanned: AtomicU64,
    /// Total UTF-8 bytes of scanned content.
    pub bytes_scanned: AtomicU64,
    /// Matching lines reported across all files.
    pub match_lines: AtomicU64,
    /// Per-file failures (unreadable paths and similar).
    pub errors: AtomicU64,
    /// Wall time spent walking the directory tree (microseconds).
    walk_micros: AtomicU64,
    /// Wall time spent searching file contents (microseconds).
    search_micros: AtomicU64,
}

impl Stats {
    /// All counters zeroed.
    pub fn new() -> Stats {
        Stats::default()
    }

    /// Add `delta` to a counter (small helper keeping call sites terse).
    pub fn bump(&self, counter: &AtomicU64, delta: u64) {
        counter.fetch_add(delta, Ordering::Relaxed);
    }

    /// Record the elapsed directory-walk time.
    pub fn record_walk_time(&self, elapsed: Duration) {
        self.walk_micros
            .store(elapsed.as_micros() as u64, Ordering::Relaxed);
    }

    /// Record the elapsed content-search time.
    pub fn record_search_time(&self, elapsed: Duration) {
        self.search_micros
            .store(elapsed.as_micros() as u64, Ordering::Relaxed);
    }

    /// Total matching lines reported so far.
    pub fn total_match_lines(&self) -> u64 {
        self.match_lines.load(Ordering::Relaxed)
    }

    /// `true` when any per-file error was recorded.
    pub fn any_errors(&self) -> bool {
        self.errors.load(Ordering::Relaxed) > 0
    }

    /// Immutable copy of every counter plus both phase durations.
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            files_found: self.files_found.load(Ordering::Relaxed),
            files_searched: self.files_searched.load(Ordering::Relaxed),
            files_binary: self.files_binary.load(Ordering::Relaxed),
            files_matched: self.files_matched.load(Ordering::Relaxed),
            lines_scanned: self.lines_scanned.load(Ordering::Relaxed),
            bytes_scanned: self.bytes_scanned.load(Ordering::Relaxed),
            match_lines: self.match_lines.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            walk: Duration::from_micros(self.walk_micros.load(Ordering::Relaxed)),
            search: Duration::from_micros(self.search_micros.load(Ordering::Relaxed)),
        }
    }
}

/// An immutable view of [`Stats`] at one instant, used for rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsSnapshot {
    /// Candidate files discovered by the walker.
    pub files_found: u64,
    /// Text files scanned by the search layer.
    pub files_searched: u64,
    /// Files skipped as binary / non-UTF-8.
    pub files_binary: u64,
    /// Files with at least one match.
    pub files_matched: u64,
    /// Lines examined in total.
    pub lines_scanned: u64,
    /// UTF-8 bytes examined in total.
    pub bytes_scanned: u64,
    /// Matching lines reported.
    pub match_lines: u64,
    /// Per-file failures.
    pub errors: u64,
    /// Directory-walk duration.
    pub walk: Duration,
    /// Content-search duration.
    pub search: Duration,
}

impl StatsSnapshot {
    /// Content throughput in bytes per second (0 when no time was recorded).
    pub fn bytes_per_second(&self) -> u64 {
        let secs = self.search.as_secs_f64();
        if secs <= 0.0 {
            return 0;
        }
        (self.bytes_scanned as f64 / secs) as u64
    }
}

impl fmt::Display for StatsSnapshot {
    /// Render the aligned `--stats` table. Values are human formatted (see
    /// [`format_bytes`] and [`format_duration`]) and right-aligned to a
    /// fixed label column so the block reads like classic `time` output.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rows: [(&str, String); 10] = [
            ("files found", separate_thousands(self.files_found)),
            ("files searched", separate_thousands(self.files_searched)),
            ("files with matches", separate_thousands(self.files_matched)),
            ("binary files skipped", separate_thousands(self.files_binary)),
            ("matching lines", separate_thousands(self.match_lines)),
            ("lines scanned", separate_thousands(self.lines_scanned)),
            ("bytes scanned", format_bytes(self.bytes_scanned)),
            ("walk time", format_duration(self.walk)),
            ("search time", format_duration(self.search)),
            (
                "search throughput",
                format!("{} /s", format_bytes(self.bytes_per_second())),
            ),
        ];
        writeln!(f, "searchlight statistics")?;
        for (label, value) in &rows {
            writeln!(f, "  {:<20} {}", label, value)?;
        }
        if self.errors > 0 {
            writeln!(
                f,
                "  {:<20} {}",
                "i/o errors",
                separate_thousands(self.errors)
            )?;
        }
        Ok(())
    }
}

/// Group digits with `,` every three positions: `1234567` → `1,234,567`.
pub fn separate_thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    // Size of the leading group: 1..=3 digits so every later group has 3.
    let first_group = (digits.len() - 1) % 3 + 1;
    out.push_str(&digits[..first_group]);
    let mut index = first_group;
    while index < digits.len() {
        out.push(',');
        out.push_str(&digits[index..index + 3]);
        index += 3;
    }
    out
}

/// Human-readable byte count using binary units.
///
/// 0–1023 bytes are shown verbatim with a `B` suffix; larger values use
/// KiB/MiB/GiB/TiB with one decimal digit.
pub fn format_bytes(bytes: u64) -> String {
    const UNIT: f64 = 1024.0;
    if bytes < 1024 {
        return format!("{} B", bytes);
    }
    let value = bytes as f64;
    let kib = value / UNIT;
    if kib < UNIT {
        return format!("{:.1} KiB", kib);
    }
    let mib = kib / UNIT;
    if mib < UNIT {
        return format!("{:.1} MiB", mib);
    }
    let gib = mib / UNIT;
    if gib < UNIT {
        return format!("{:.1} GiB", gib);
    }
    format!("{:.1} TiB", gib / UNIT)
}

/// Human-readable duration: microseconds below 1 ms, milliseconds below
/// 1 s, seconds otherwise (two decimals).
pub fn format_duration(duration: Duration) -> String {
    let micros = duration.as_micros();
    if micros < 1_000 {
        format!("{} µs", micros)
    } else if micros < 1_000_000 {
        format!("{:.1} ms", micros as f64 / 1_000.0)
    } else {
        format!("{:.2} s", duration.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_increment_across_threads() {
        let stats = Stats::new();
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let handle = std::thread::spawn(|| {
                    for _ in 0..250 {
                        STATS_FOR_THREADS.lines_scanned.fetch_add(1, Ordering::Relaxed);
                    }
                });
                handle
            })
            .collect();
        for handle in handles {
            handle.join().expect("worker must not panic");
        }
        assert_eq!(STATS_FOR_THREADS.lines_scanned.load(Ordering::Relaxed), 1_000);
        assert!(!STATS_FOR_THREADS.any_errors());
        STATS_FOR_THREADS.lines_scanned.store(0, Ordering::Relaxed);
    }

    /// Shared fixture for the thread test above; reset by that test.
    static STATS_FOR_THREADS: Stats = Stats {
        files_found: AtomicU64::new(0),
        files_searched: AtomicU64::new(0),
        files_binary: AtomicU64::new(0),
        files_matched: AtomicU64::new(0),
        lines_scanned: AtomicU64::new(0),
        bytes_scanned: AtomicU64::new(0),
        match_lines: AtomicU64::new(0),
        errors: AtomicU64::new(0),
        walk_micros: AtomicU64::new(0),
        search_micros: AtomicU64::new(0),
    };

    #[test]
    fn bump_helper_adds_deltas() {
        let stats = Stats::new();
        stats.bump(&stats.files_found, 5);
        stats.bump(&stats.files_found, 2);
        assert_eq!(stats.files_found.load(Ordering::Relaxed), 7);
        assert!(!stats.any_errors());
        stats.bump(&stats.errors, 1);
        assert!(stats.any_errors());
        assert_eq!(stats.total_match_lines(), 0);
        stats.bump(&stats.match_lines, 12);
        assert_eq!(stats.total_match_lines(), 12);
    }

    #[test]
    fn snapshot_copies_every_field() {
        let stats = Stats::new();
        stats.bump(&stats.files_found, 10);
        stats.bump(&stats.files_searched, 9);
        stats.bump(&stats.files_binary, 1);
        stats.bump(&stats.files_matched, 4);
        stats.bump(&stats.match_lines, 40);
        stats.record_walk_time(Duration::from_millis(2));
        stats.record_search_time(Duration::from_millis(7));

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.files_found, 10);
        assert_eq!(snapshot.files_searched, 9);
        assert_eq!(snapshot.files_binary, 1);
        assert_eq!(snapshot.files_matched, 4);
        assert_eq!(snapshot.match_lines, 40);
        assert_eq!(snapshot.walk, Duration::from_millis(2));
        assert_eq!(snapshot.search, Duration::from_millis(7));
        assert_eq!(snapshot.errors, 0);
    }

    #[test]
    fn thousands_separator_groups_digits() {
        assert_eq!(separate_thousands(0), "0");
        assert_eq!(separate_thousands(7), "7");
        assert_eq!(separate_thousands(999), "999");
        assert_eq!(separate_thousands(1_000), "1,000");
        assert_eq!(separate_thousands(12_345), "12,345");
        assert_eq!(separate_thousands(1_234_567), "1,234,567");
        assert_eq!(separate_thousands(1_000_000_000), "1,000,000,000");
    }

    #[test]
    fn byte_formatting_uses_binary_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1), "1 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
        assert_eq!(format_bytes(2 * 1024_u64.pow(4)), "2.0 TiB");
    }

    #[test]
    fn duration_formatting_scales_units() {
        assert_eq!(format_duration(Duration::from_micros(1)), "1 µs");
        assert_eq!(format_duration(Duration::from_micros(999)), "999 µs");
        assert_eq!(format_duration(Duration::from_millis(1)), "1.0 ms");
        assert_eq!(format_duration(Duration::from_millis(999)), "999.0 ms");
        assert_eq!(format_duration(Duration::from_millis(1_500)), "1.50 s");
        assert_eq!(format_duration(Duration::from_secs(1)), "1.00 s");
        assert_eq!(format_duration(Duration::from_secs(90)), "90.00 s");
    }

    #[test]
    fn throughput_is_bytes_over_search_time() {
        let stats = Stats::new();
        stats.bump(&stats.bytes_scanned, 2_000_000);
        stats.record_search_time(Duration::from_secs(2));
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.bytes_per_second(), 1_000_000);

        // Zero search time yields a zero throughput rather than a division
        // by zero.
        let empty = Stats::new().snapshot();
        assert_eq!(empty.bytes_per_second(), 0);
    }

    #[test]
    fn display_renders_an_aligned_table() {
        let stats = Stats::new();
        stats.bump(&stats.files_found, 1_234);
        stats.bump(&stats.match_lines, 56);
        stats.record_search_time(Duration::from_millis(12));
        let table = stats.snapshot().to_string();

        assert!(table.starts_with("searchlight statistics"));
        assert!(table.contains("files found"), "label present: {}", table);
        assert!(table.contains("1,234"));
        assert!(table.contains("matching lines"));
        assert!(table.contains("56"));
        assert!(table.contains("12.0 ms"));
        assert!(!table.contains("i/o errors"), "no error row when clean");

        stats.bump(&stats.errors, 3);
        let table = stats.snapshot().to_string();
        assert!(table.contains("i/o errors"));
        assert!(table.contains("3"));
    }
}
