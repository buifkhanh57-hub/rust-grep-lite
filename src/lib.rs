//! searchlight — a fast, parallel content search for the terminal.
//!
//! searchlight is a grep-style tool built entirely on the Rust standard
//! library: no `regex`, no `clap`, no dependencies at all. The crate is
//! organized as a pipeline of independent modules so every stage can be
//! tested in isolation:
//!
//! ```text
//!            ┌─────────┐        ┌─────────┐
//!  argv ───► │  args   │ ─────► │ pattern │ (content pattern + globs)
//!            └─────────┘        └────┬────┘
//!                                    │
//!            ┌─────────┐        ┌────▼────┐        ┌────────┐        ┌────────┐
//!   fs  ───► │ filters │ ─────► │  walk   │ ─────► │ search │ ─────► │ output │ ──► stdout
//!            └─────────┘        └─────────┘        └───┬────┘        └────────┘
//!            (.gitignore,                       worker pool     │
//!             globs, sizes)                     via thread::scope│
//!                                                          ┌─────▼─────┐
//!                                                          │ stats/timer│ ──► stderr
//!                                                          └───────────┘
//! ```
//!
//! * [`args`] parses and validates the command line into a
//!   [`Config`](args::Config) — long flags, clustered short flags and
//!   attached values, with zero dependencies.
//! * [`pattern`] is the hand-rolled backtracking matcher used for content
//!   patterns **and** for glob filters; it supports classes, quantifiers,
//!   alternation, anchors and word boundaries.
//! * [`filters`] combines `--glob`/`--exclude-glob`, size limits and a
//!   pragmatic subset of gitignore semantics, plus the NUL-byte binary
//!   sniff.
//! * [`walk`] discovers candidate files on a shared work queue processed by
//!   a pool of scoped worker threads.
//! * [`search`] scans each file, recording matches, their char spans and
//!   context windows; its driver parallelizes across files with an atomic
//!   work cursor and an `mpsc` channel.
//! * [`output`] renders results as plain or colored text, counts, file
//!   lists or JSON Lines; [`highlight`] paints the matched spans.
//! * [`stats`] and [`timer`] collect lock-free counters and wall-clock
//!   timings for `--stats`.
//! * [`error`] funnels every failure into one enum with stable exit codes
//!   (`0` matches found, `1` no matches, `2` error).
//! * [`json`] and [`lines`] are the small utility layers underneath
//!   (JSON model/serializer; line splitting with byte offsets).
//!
//! The binary in `main.rs` only wires these stages together.
//!
//! # Example
//!
//! Search an in-memory buffer the same way the CLI does:
//!
//! ```
//! use searchlight::pattern::Pattern;
//! use searchlight::search::{search_text, SearchOptions};
//!
//! let pattern = Pattern::new(r"\bErr(or)?\b", false, false).unwrap();
//! let options = SearchOptions {
//!     max_count: None,
//!     invert: false,
//!     before: 0,
//!     after: 0,
//! };
//! let result = search_text(
//!     std::path::Path::new("demo.log"),
//!     "info: ok\nerror: failed\nwarning: meh\n",
//!     &pattern,
//!     &options,
//! );
//! assert_eq!(result.matches.len(), 1);
//! assert_eq!(result.matches[0].number, 2);
//! assert_eq!(result.matches[0].spans, vec![(0, 5)]);
//! ```
//!
//! Walk a directory tree in parallel and count the candidate files:
//!
//! ```
//! use searchlight::filters::FileFilter;
//! use searchlight::stats::Stats;
//! use searchlight::walk::{walk, WalkOptions};
//!
//! let stats = Stats::new();
//! let outcome = walk(
//!     &[std::path::PathBuf::from("src")],
//!     WalkOptions {
//!         max_depth: None,
//!         follow: false,
//!         hidden: false,
//!         no_ignore: false,
//!         threads: 2,
//!     },
//!     &FileFilter::empty(),
//!     &stats,
//! );
//! // `src` exists inside this repository, so at least this file is found.
//! assert!(outcome.files.iter().any(|path| path.ends_with("lib.rs")));
//! ```
//!
//! # Exit codes
//!
//! | Code | Meaning                                 |
//! |------|-----------------------------------------|
//! | 0    | at least one match was found            |
//! | 1    | no matches were found                   |
//! | 2    | an error occurred (bad args, I/O, ...)  |

#![forbid(unsafe_code)]

pub mod args;
pub mod error;
pub mod filters;
pub mod highlight;
pub mod json;
pub mod lines;
pub mod output;
pub mod pattern;
pub mod search;
pub mod sizes;
pub mod stats;
pub mod timer;
pub mod walk;

/// The crate version, as declared in `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// One-line description used by `--help` and `--version` output.
pub const TAGLINE: &str = "Fast, parallel content search for the terminal";
