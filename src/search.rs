//! Content search over the files discovered by the walker.
//!
//! The module has three layers:
//!
//! 1. [`search_text`] — the pure core. It takes an in-memory string, splits
//!    it into numbered lines (see [`crate::lines`]) and runs the pattern
//!    engine over every line, collecting [`MatchLine`] records that carry
//!    the match spans (char ranges) plus any requested context window.
//! 2. [`search_file`] — the I/O wrapper. It reads one path from disk,
//!    applies the NUL-byte binary sniff (binary files are always skipped,
//!    there is no `-a` flag) and treats invalid UTF-8 the same way.
//! 3. [`search_files`] — the parallel driver. Files are distributed across
//!    a fixed pool of scoped worker threads with a shared atomic cursor:
//!    every file is one unit of work, so no job queue is needed. Outcomes
//!    travel over an `mpsc` channel, are collected on the coordinating
//!    thread and sorted by path so rendering is deterministic no matter
//!    which worker finished first.
//!
//! Counting (`--stats`) happens once, after all workers have finished, on
//! the coordinating thread — worker code stays free of statistics concerns
//! and the counters are race-free by construction.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

use crate::error::io_message;
use crate::filters::looks_binary;
use crate::lines::{iter_line_slices, LineSlice};
use crate::pattern::Pattern;
use crate::stats::Stats;

/// Per-file search settings derived from the CLI configuration.
#[derive(Debug, Clone)]
pub struct SearchOptions {
    /// Stop after this many matching lines per file (`-m/--max-count`).
    pub max_count: Option<usize>,
    /// Select lines that do **not** match (`-v/--invert-match`). Match
    /// spans are left empty because there is nothing to highlight.
    pub invert: bool,
    /// Lines of context before each match (`-B`, `-C`).
    pub before: usize,
    /// Lines of context after each match (`-A`, `-C`).
    pub after: usize,
}

/// A non-matching line kept as context around a [`MatchLine`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextLine {
    /// 1-based line number.
    pub number: u64,
    /// Byte offset of the line start inside the file.
    pub offset: usize,
    /// Line contents with the terminator removed.
    pub text: String,
}

/// One matching line (or, with `-v`, one selected non-matching line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchLine {
    /// 1-based line number.
    pub number: u64,
    /// Byte offset of the line start inside the file.
    pub offset: usize,
    /// Line contents with the terminator removed.
    pub text: String,
    /// Char ranges of each match within `text` (start inclusive, end
    /// exclusive); empty for `-v` selections.
    pub spans: Vec<(usize, usize)>,
    /// Context lines immediately before the match, in ascending order.
    pub before: Vec<ContextLine>,
    /// Context lines immediately after the match, in ascending order.
    pub after: Vec<ContextLine>,
}

/// The full result of scanning one text file.
#[derive(Debug)]
pub struct FileResult {
    /// Path the result belongs to.
    pub path: PathBuf,
    /// Selected lines, in file order.
    pub matches: Vec<MatchLine>,
    /// Logical lines actually examined (fewer than the file's total when
    /// `-m` stopped the scan early).
    pub lines_scanned: u64,
    /// UTF-8 bytes examined.
    pub bytes_scanned: u64,
}

/// A per-file failure (unreadable path, I/O error, ...).
#[derive(Debug)]
pub struct FileError {
    /// The path that failed.
    pub path: PathBuf,
    /// Short human-readable message (no trailing newline).
    pub message: String,
}

/// What a worker produced for one candidate file.
#[derive(Debug)]
pub enum FileOutcome {
    /// The file was read and scanned; matches may be empty.
    Scanned(FileResult),
    /// The file looked binary or was not valid UTF-8 and was skipped.
    Binary(PathBuf),
    /// The file could not be read at all.
    Failed(FileError),
}

/// Search one in-memory text, the pure heart of the module.
///
/// The routine walks the line slices once; for every line it asks the
/// pattern engine for a match (or, with `invert`, for the absence of one)
/// and records the exact match spans for highlighting. `max_count` stops
/// the scan after the given number of selected lines, mirroring
/// `grep -m N`.
pub fn search_text(path: &Path, text: &str, pattern: &Pattern, options: &SearchOptions) -> FileResult {
    let slices = iter_line_slices(text);
    let mut matches: Vec<MatchLine> = Vec::new();
    let mut examined: u64 = 0;

    for (index, slice) in slices.iter().enumerate() {
        if let Some(limit) = options.max_count {
            if matches.len() >= limit {
                break;
            }
        }
        examined += 1;
        let hit = pattern.is_match(&slice.text);
        let selected = if options.invert { !hit } else { hit };
        if !selected {
            continue;
        }
        let spans: Vec<(usize, usize)> = if options.invert {
            Vec::new()
        } else {
            pattern.find_all(&slice.text)
        };
        matches.push(build_match(&slices, index, spans, options));
    }

    FileResult {
        path: path.to_path_buf(),
        matches,
        lines_scanned: examined,
        bytes_scanned: text.len() as u64,
    }
}

/// Assemble one [`MatchLine`] with its context window.
fn build_match(
    slices: &[LineSlice],
    index: usize,
    spans: Vec<(usize, usize)>,
    options: &SearchOptions,
) -> MatchLine {
    let slice = &slices[index];
    let mut before: Vec<ContextLine> = Vec::new();
    if options.before > 0 {
        let start = index.saturating_sub(options.before);
        for line in &slices[start..index] {
            before.push(ContextLine {
                number: line.number,
                offset: line.start,
                text: line.text.clone(),
            });
        }
    }
    let mut after: Vec<ContextLine> = Vec::new();
    if options.after > 0 {
        let end = slices.len().min(index.saturating_add(1).saturating_add(options.after));
        for line in &slices[index + 1..end] {
            after.push(ContextLine {
                number: line.number,
                offset: line.start,
                text: line.text.clone(),
            });
        }
    }
    MatchLine {
        number: slice.number,
        offset: slice.start,
        text: slice.text.clone(),
        spans,
        before,
        after,
    }
}

/// Read one path from disk and search it, applying the binary/UTF-8 sniff.
pub fn search_file(path: &Path, pattern: &Pattern, options: &SearchOptions) -> FileOutcome {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) => {
            return FileOutcome::Failed(FileError {
                path: path.to_path_buf(),
                message: io_message(&err),
            });
        }
    };
    if looks_binary(&bytes) {
        return FileOutcome::Binary(path.to_path_buf());
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => return FileOutcome::Binary(path.to_path_buf()),
    };
    FileOutcome::Scanned(search_text(path, &text, pattern, options))
}

/// Search every file in `files` on a pool of `threads` scoped workers.
///
/// The returned outcomes are sorted by path; counters in `stats` are
/// updated once, after the workers have joined. `threads` is clamped to at
/// least one worker and at most `files.len()` (extra threads would idle).
pub fn search_files(
    files: &[PathBuf],
    pattern: &Pattern,
    options: &SearchOptions,
    stats: &Stats,
    threads: usize,
) -> Vec<FileOutcome> {
    if files.is_empty() {
        return Vec::new();
    }
    let workers = threads.max(1).min(files.len());
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<FileOutcome>();

    thread::scope(|scope| {
        let next = &next;
        for _ in 0..workers {
            let sender = tx.clone();
            scope.spawn(move || {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= files.len() {
                        break;
                    }
                    let outcome = search_file(&files[index], pattern, options);
                    let _ = sender.send(outcome);
                }
            });
        }
    });
    // Every worker-side sender died with the scope; dropping ours lets the
    // receive loop below observe the end of the stream.
    drop(tx);

    let mut outcomes: Vec<FileOutcome> = Vec::new();
    for outcome in rx {
        outcomes.push(outcome);
    }
    outcomes.sort_by(|left, right| outcome_path(left).cmp(outcome_path(right)));

    for outcome in &outcomes {
        match outcome {
            FileOutcome::Scanned(result) => {
                stats.bump(&stats.files_searched, 1);
                stats.bump(&stats.lines_scanned, result.lines_scanned);
                stats.bump(&stats.bytes_scanned, result.bytes_scanned);
                stats.bump(&stats.match_lines, result.matches.len() as u64);
                if !result.matches.is_empty() {
                    stats.bump(&stats.files_matched, 1);
                }
            }
            FileOutcome::Binary(_) => {
                stats.bump(&stats.files_binary, 1);
            }
            FileOutcome::Failed(_) => {
                stats.bump(&stats.errors, 1);
            }
        }
    }
    outcomes
}

/// The path an outcome belongs to, used for deterministic ordering.
fn outcome_path(outcome: &FileOutcome) -> &Path {
    match outcome {
        FileOutcome::Scanned(result) => &result.path,
        FileOutcome::Binary(path) => path,
        FileOutcome::Failed(error) => &error.path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    /// Create a unique scratch directory for one test.
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("searchlight-search-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn options() -> SearchOptions {
        SearchOptions {
            max_count: None,
            invert: false,
            before: 0,
            after: 0,
        }
    }

    fn pattern_of(source: &str) -> Pattern {
        Pattern::new(source, false, false).expect("test pattern compiles")
    }

    #[test]
    fn literal_search_finds_lines_and_spans() {
        let pattern = pattern_of("alpha");
        let result = search_text(
            Path::new("sample.txt"),
            "alpha\nbeta gamma\nalphabet\n",
            &pattern,
            &options(),
        );
        assert_eq!(result.matches.len(), 2);
        assert_eq!(result.matches[0].number, 1);
        assert_eq!(result.matches[0].offset, 0);
        assert_eq!(result.matches[0].spans, vec![(0, 5)]);
        assert_eq!(result.matches[1].number, 3);
        assert_eq!(result.matches[1].offset, 17, "byte offset of line 3");
        assert_eq!(result.matches[1].spans, vec![(0, 5)]);
        assert_eq!(result.lines_scanned, 3);
        assert_eq!(result.bytes_scanned, 26);
    }

    #[test]
    fn one_line_can_hold_several_spans() {
        let pattern = pattern_of("a");
        let result = search_text(Path::new("x"), "axaya", &pattern, &options());
        assert_eq!(result.matches[0].spans, vec![(0, 1), (2, 3), (4, 5)]);
    }

    #[test]
    fn context_windows_clip_at_file_edges() {
        let pattern = pattern_of("3");
        let mut opts = options();
        opts.before = 2;
        opts.after = 2;
        let result = search_text(Path::new("x"), "1\n2\n3\n4\n5\n", &pattern, &opts);
        let entry = &result.matches[0];
        assert_eq!(entry.before.iter().map(|l| l.number).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(entry.after.iter().map(|l| l.number).collect::<Vec<_>>(), vec![4, 5]);
        assert_eq!(entry.before[0].text, "1");
        assert_eq!(entry.after[1].text, "5");
    }

    #[test]
    fn max_count_stops_the_scan_early() {
        let pattern = pattern_of("a");
        let mut opts = options();
        opts.max_count = Some(2);
        let result = search_text(Path::new("x"), "a\na\na\n", &pattern, &opts);
        assert_eq!(result.matches.len(), 2);
        assert_eq!(result.lines_scanned, 2, "line 3 was never examined");
    }

    #[test]
    fn invert_selects_non_matching_lines() {
        let pattern = pattern_of("a");
        let mut opts = options();
        opts.invert = true;
        let result = search_text(Path::new("x"), "a\nb\nc\n", &pattern, &opts);
        assert_eq!(result.matches.len(), 2);
        assert_eq!(result.matches[0].number, 2);
        assert_eq!(result.matches[1].number, 3);
        assert!(result.matches.iter().all(|entry| entry.spans.is_empty()));
    }

    #[test]
    fn binary_files_are_skipped() {
        let root = temp_dir("binary");
        let path = root.join("blob.bin");
        fs::write(&path, b"ok\x00binary").expect("write binary");
        let outcome = search_file(&path, &pattern_of("ok"), &options());
        assert!(matches!(outcome, FileOutcome::Binary(_)));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn non_utf8_files_are_skipped() {
        let root = temp_dir("utf8");
        let path = root.join("latin.txt");
        // No NUL bytes, but not valid UTF-8 either.
        fs::write(&path, [0xff_u8, 0xfe, 0xfd]).expect("write bytes");
        let outcome = search_file(&path, &pattern_of("x"), &options());
        assert!(matches!(outcome, FileOutcome::Binary(_)));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unreadable_files_report_errors() {
        let missing = std::env::temp_dir().join("searchlight-missing-file-xyz");
        let outcome = search_file(&missing, &pattern_of("x"), &options());
        match outcome {
            FileOutcome::Failed(error) => {
                assert_eq!(error.path, missing);
                assert_eq!(error.message, "no such file or directory");
            }
            other => panic!("expected Failed, got {:?}", other),
        }
    }

    #[test]
    fn empty_files_yield_no_matches() {
        let pattern = pattern_of("anything");
        let result = search_text(Path::new("empty.txt"), "", &pattern, &options());
        assert!(result.matches.is_empty());
        assert_eq!(result.lines_scanned, 0);
        assert_eq!(result.bytes_scanned, 0);
    }

    #[test]
    fn parallel_search_is_deterministic_and_counted() {
        let root = temp_dir("parallel");
        let mut files: Vec<PathBuf> = Vec::new();
        for index in 0..12 {
            let path = root.join(format!("file{:02}.txt", index));
            let text = if index % 2 == 0 {
                "needle here\nother line\n"
            } else {
                "nothing to see\n"
            };
            fs::write(&path, text).expect("write fixture");
            files.push(path);
        }
        files.sort();

        let stats = Stats::new();
        let pattern = pattern_of("needle");
        let outcomes = search_files(&files, &pattern, &options(), &stats, 4);

        assert_eq!(outcomes.len(), 12);
        let mut scanned = 0usize;
        let mut total_matches = 0usize;
        for outcome in &outcomes {
            if let FileOutcome::Scanned(result) = outcome {
                scanned += 1;
                total_matches += result.matches.len();
            }
        }
        assert_eq!(scanned, 12);
        assert_eq!(total_matches, 6, "one match per even-numbered file");
        assert_eq!(stats.snapshot().files_matched, 6);
        assert_eq!(stats.snapshot().match_lines, 6);
        assert_eq!(stats.snapshot().files_binary, 0);
        assert_eq!(stats.snapshot().errors, 0);

        let paths: Vec<&Path> = outcomes.iter().map(outcome_path).collect();
        assert!(paths.as_slice().is_sorted(), "outcomes sorted by path");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn search_files_on_an_empty_list_is_empty() {
        let stats = Stats::new();
        let pattern = pattern_of("x");
        let outcomes = search_files(&[], &pattern, &options(), &stats, 4);
        assert!(outcomes.is_empty());
        assert_eq!(stats.snapshot().files_searched, 0);
    }

    #[test]
    fn search_text_agrees_with_search_file_for_utf8() {
        let root = temp_dir("agree");
        let path = root.join("same.txt");
        fs::write(&path, "needle\nplain\n").expect("write");
        let pattern = pattern_of("needle");
        let via_file = match search_file(&path, &pattern, &options()) {
            FileOutcome::Scanned(result) => result,
            other => panic!("expected Scanned, got {:?}", other),
        };
        let text = fs::read_to_string(&path).expect("read");
        let via_text = search_text(&path, &text, &pattern, &options());
        assert_eq!(via_file.matches.len(), via_text.matches.len());
        assert_eq!(via_file.matches[0].spans, via_text.matches[0].spans);
        assert_eq!(via_file.matches[0].number, via_text.matches[0].number);
        let _ = fs::remove_dir_all(&root);
    }
}
