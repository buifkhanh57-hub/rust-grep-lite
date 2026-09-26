//! `searchlight` binary entry point — wiring only.
//!
//! The pipeline is: parse the command line, validate the search roots,
//! walk the tree in parallel, search the candidate files on the worker
//! pool, render the results and map the outcome onto process exit codes
//! (`0` matches found, `1` no matches, `2` error). All of the actual logic
//! lives in the library so `cargo test` exercises it without spawning the
//! binary.

use std::env;
use std::io::{self, IsTerminal, Write};
use std::process;

use searchlight::args::{ColorChoice, Config, ParseOutcome};
use searchlight::error::io_message;
use searchlight::filters::FileFilter;
use searchlight::output::{RenderOptions, Renderer};
use searchlight::pattern::Pattern;
use searchlight::search::{search_files, FileOutcome, SearchOptions};
use searchlight::sizes;
use searchlight::stats::{format_duration, Stats};
use searchlight::timer::Timer;
use searchlight::walk::{walk, WalkOptions};
use searchlight::{TAGLINE, VERSION};

fn main() {
    let code = run();
    // process::exit skips destructors, so flush the buffered stdout first.
    let _ = io::stdout().flush();
    process::exit(code);
}

/// Run one search, returning the process exit code.
fn run() -> i32 {
    let argv: Vec<String> = env::args().skip(1).collect();
    let config = match Config::parse_from(argv) {
        Ok(ParseOutcome::Config(config)) => config,
        Ok(ParseOutcome::Help) => {
            println!("{}", help_text());
            return 0;
        }
        Ok(ParseOutcome::Version) => {
            println!("searchlight {}", VERSION);
            return 0;
        }
        Err(err) => {
            eprintln!("searchlight: {}", err);
            eprintln!("Try 'searchlight --help' for more information.");
            return err.exit_code();
        }
    };

    let pattern = match Pattern::new(&config.pattern, config.ignore_case, config.word_regexp) {
        Ok(pattern) => pattern,
        Err(err) => {
            eprintln!("searchlight: {}", err);
            return 2;
        }
    };

    let filter = match FileFilter::from_config(&config) {
        Ok(filter) => filter,
        Err(err) => {
            eprintln!("searchlight: {}", err);
            return 2;
        }
    };

    // Fail fast on roots that cannot even be inspected (typo'ed paths are
    // the most common mistake, and exit code 2 must not depend on timing).
    for path in &config.paths {
        if let Err(err) = std::fs::metadata(path) {
            eprintln!("searchlight: {}: {}", path.display(), io_message(&err));
            return 2;
        }
    }

    let stats = Stats::new();
    let mut timer = Timer::start();

    timer.begin("walk");
    let outcome = walk(
        &config.paths,
        WalkOptions {
            max_depth: config.max_depth,
            follow: config.follow_symlinks,
            hidden: config.hidden,
            no_ignore: config.no_ignore,
            threads: config.threads,
        },
        &filter,
        &stats,
    );
    timer.end();

    for failure in &outcome.errors {
        stats.bump(&stats.errors, 1);
        if !config.no_messages {
            eprintln!("searchlight: {}: {}", failure.path.display(), failure.message);
        }
    }

    timer.begin("search");
    let outcomes = search_files(
        &outcome.files,
        &pattern,
        &SearchOptions {
            max_count: config.max_count,
            invert: config.invert_match,
            before: config.before_context,
            after: config.after_context,
        },
        &stats,
        config.threads,
    );
    timer.end();

    let render_options =
        RenderOptions::from_config(&config, resolve_color(&config), with_filename(&config));
    let stdout = io::stdout();
    let mut renderer = Renderer::new(stdout.lock(), render_options);

    let mut found: u64 = 0;
    let mut write_error: Option<io::Error> = None;
    for file_outcome in &outcomes {
        match file_outcome {
            FileOutcome::Scanned(result) => {
                found += result.matches.len() as u64;
                if config.quiet {
                    continue;
                }
                if let Err(err) = renderer.write_result(result) {
                    write_error = Some(err);
                    break;
                }
            }
            FileOutcome::Binary(path) => {
                if !config.no_messages {
                    eprintln!("searchlight: {}: binary file skipped", path.display());
                }
            }
            FileOutcome::Failed(error) => {
                if !config.no_messages {
                    eprintln!("searchlight: {}: {}", error.path.display(), error.message);
                }
            }
        }
    }
    if write_error.is_none() {
        if let Err(err) = renderer.flush() {
            write_error = Some(err);
        }
    }

    if config.stats {
        eprint!("{}", stats.snapshot());
        let timeline = timer.finish();
        eprintln!("total wall time    {}", format_duration(timeline.total));
        eprintln!(
            "size limits        {}",
            sizes::describe_bounds(config.min_size, config.max_size)
        );
    }

    if let Some(err) = write_error {
        // `searchlight foo big-tree | head -n 3` closes the pipe early; that
        // is a normal way to use the tool, not a failure.
        if err.kind() == io::ErrorKind::BrokenPipe {
            return 0;
        }
        eprintln!("searchlight: i/o error: {}", err);
        return 2;
    }

    if found > 0 {
        0
    } else {
        1
    }
}

/// Resolve `--color` against the environment: `auto` colors only when
/// stdout is a terminal and `NO_COLOR` is unset (the no-color.org rule).
fn resolve_color(config: &Config) -> bool {
    match config.color {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => env::var_os("NO_COLOR").is_none() && io::stdout().is_terminal(),
    }
}

/// Classic grep rule for file-name prefixes: directories and multi-root
/// searches get prefixes, a single explicit file does not. The default
/// (no PATH) searches the current directory and therefore behaves like a
/// directory scan.
fn with_filename(config: &Config) -> bool {
    if config.paths.len() > 1 {
        return true;
    }
    match config.paths.first() {
        Some(path) => path.is_dir(),
        None => true,
    }
}

/// The full `--help` text. Kept in sync with `Config::parse_from` by hand;
/// the flag reference table in the README mirrors it.
fn help_text() -> String {
    format!(
        "\
searchlight {version}
{tagline}

USAGE:
    searchlight [OPTIONS] PATTERN [PATH]...

ARGS:
    PATTERN    Pattern in searchlight mini-regex syntax (see the README).
    PATH...    Files or directories to search; defaults to the current directory.

MATCHING:
    -i, --ignore-case       Fold ASCII/Unicode case while matching
    -w, --word-regexp       Require the pattern to match whole words
    -v, --invert-match      Select lines that do not match
    -m, --max-count N       Stop after N matching lines per file

OUTPUT:
    -n, --line-number       Prefix matches with 1-based line numbers
    -b, --byte-offset       Prefix matches with byte offsets
    -o, --only-matching     Print only the matched part of each line
    -c, --count             Print per-file match counts
    -l, --files-with-matches
                            Print only the names of matching files
    -B N                    N lines of context before each match
    -A N                    N lines of context after each match
    -C N                    N lines of context before and after
    --color WHEN            Colorize: auto, always or never [default: auto]
    --json                  Emit JSON Lines instead of human text
    -q, --quiet             Print nothing; exit codes carry the result
    -s, --no-messages       Suppress error messages on stderr

FILTERING:
    --glob PATTERN          Search only files matching PATTERN (repeatable)
    --exclude-glob PATTERN  Skip files matching PATTERN (repeatable)
    --max-depth N           Limit directory recursion depth (1 = top level)
    --min-size S            Skip files smaller than S
    --max-size S            Skip files larger than S
    --hidden                Also search hidden files and directories
    --no-ignore             Do not honor .gitignore and .ignore rules
    --follow                Follow symbolic links

PERFORMANCE:
    -j, --threads N         Worker threads [default: available CPUs]
    --stats                 Print counters and timings to stderr

OTHER:
    -h, --help              Print this help text
    -V, --version           Print version information

Sizes S accept suffixes such as: {examples}.

Exit status:
    0    at least one match was found
    1    no matches were found
    2    an error occurred (bad arguments, unreadable path, ...)
",
        version = VERSION,
        tagline = TAGLINE,
        examples = sizes::SIZE_EXAMPLES,
    )
}
