//! Command line parsing.
//!
//! searchlight ships a hand-rolled argument parser — no `clap`, no derive
//! macros — because the flag surface is small enough that a table-driven
//! `match` is clearer than a framework, and because the tool must compile
//! with zero dependencies. The parser supports everything users expect from
//! a POSIX-flavored CLI:
//!
//! * long options with `--flag value` **and** `--flag=value` forms,
//! * clustered short flags (`-inv`, `-wvn`),
//! * attached numeric values (`-C3`, `-j8`, `-m100`),
//! * `--` to stop option parsing (needed when the pattern starts with `-`),
//! * `-h/--help` and `-V/--version` short-circuits (rendered by `main.rs`).
//!
//! Parsing never touches the environment or the filesystem; [`Config`] is a
//! pure value, which keeps the parser fully unit-testable. Cross-flag
//! invariants (thread bounds, size ranges) are checked in
//! [`Config::validate`].

use std::path::PathBuf;

use crate::error::{SearchError, SearchResult};

/// Upper bound for `-j`, keeping thread stacks from exhausting the process.
pub const MAX_THREADS: usize = 1024;

/// How the renderer should decide whether to emit ANSI color escapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorChoice {
    /// Color when stdout is a terminal and `NO_COLOR` is unset (default).
    Auto,
    /// Always emit ANSI escapes, even when piping.
    Always,
    /// Never emit ANSI escapes.
    Never,
}

impl ColorChoice {
    /// Parse the `--color` value: exactly `auto`, `always` or `never`.
    pub fn parse(text: &str) -> SearchResult<ColorChoice> {
        match text {
            "auto" => Ok(ColorChoice::Auto),
            "always" => Ok(ColorChoice::Always),
            "never" => Ok(ColorChoice::Never),
            other => Err(SearchError::arg(format!(
                "--color expects 'auto', 'always' or 'never', got '{}'",
                other
            ))),
        }
    }
}

/// What [`Config::parse_from`] produced.
#[derive(Debug)]
pub enum ParseOutcome {
    /// A fully validated configuration, ready for the search pipeline.
    Config(Config),
    /// `-h`/`--help` was requested; print help and exit 0.
    Help,
    /// `-V`/`--version` was requested; print the version and exit 0.
    Version,
}

/// Everything the search pipeline needs, fully parsed and validated.
///
/// Field names map one-to-one onto the CLI flags:
///
/// * matching: `pattern`, `ignore_case` (-i), `word_regexp` (-w),
///   `invert_match` (-v), `max_count` (-m),
/// * output: `line_numbers` (-n), `byte_offset` (-b), `only_matching` (-o),
///   `count_only` (-c), `files_with_matches` (-l), `json`, `quiet` (-q),
///   `no_messages` (-s), `color`,
/// * context: `before_context` (-B), `after_context` (-A), (`-C` sets both),
/// * filtering: `include_globs` (--glob), `exclude_globs` (--exclude-glob),
///   `max_depth`, `min_size`, `max_size`, `hidden`, `no_ignore`,
///   `follow_symlinks`,
/// * performance/diagnostics: `threads` (-j), `stats`, `paths` (search roots;
///   empty means the current directory).
pub struct Config {
    /// The PATTERN positional argument (compiled later by the pattern engine).
    pub pattern: String,
    /// Paths to search; an empty list means "the current directory".
    pub paths: Vec<PathBuf>,
    /// `-i` / `--ignore-case`: fold case while matching.
    pub ignore_case: bool,
    /// `-w` / `--word-regexp`: require the pattern to match whole words.
    pub word_regexp: bool,
    /// `-v` / `--invert-match`: select lines that do NOT match.
    pub invert_match: bool,
    /// `-n` / `--line-number`: prefix output with 1-based line numbers.
    pub line_numbers: bool,
    /// `-c` / `--count`: print per-file match counts instead of lines.
    pub count_only: bool,
    /// `-l` / `--files-with-matches`: print only matching file paths.
    pub files_with_matches: bool,
    /// `-B N`: lines of context printed before each match.
    pub before_context: usize,
    /// `-A N`: lines of context printed after each match.
    pub after_context: usize,
    /// `--glob PATTERN` (repeatable): only search files matching any glob.
    pub include_globs: Vec<String>,
    /// `--exclude-glob PATTERN` (repeatable): skip files matching any glob.
    pub exclude_globs: Vec<String>,
    /// `--max-depth N`: limit directory recursion depth (1 = top level only).
    pub max_depth: Option<usize>,
    /// `--min-size S`: skip files smaller than S bytes (`k`/`m`/`g` suffixes).
    pub min_size: Option<u64>,
    /// `--max-size S`: skip files larger than S bytes.
    pub max_size: Option<u64>,
    /// `-j N` / `--threads N`: number of worker threads (default: CPUs).
    pub threads: usize,
    /// `--no-ignore`: also descend into VCS/build directories.
    pub no_ignore: bool,
    /// `--hidden`: also search hidden files and directories.
    pub hidden: bool,
    /// `--follow`: traverse symbolic links to directories.
    pub follow_symlinks: bool,
    /// `--color WHEN`: ANSI color control.
    pub color: ColorChoice,
    /// `--stats`: print a timing/counter summary to stderr.
    pub stats: bool,
    /// `--json`: emit JSON-Lines output instead of human text.
    pub json: bool,
    /// `-s` / `--no-messages`: suppress per-file error messages on stderr.
    pub no_messages: bool,
    /// `-m N` / `--max-count N`: stop after N matching lines per file.
    pub max_count: Option<usize>,
    /// `-b` / `--byte-offset`: prefix output with each line's byte offset.
    pub byte_offset: bool,
    /// `-o` / `--only-matching`: print only the matched part of each line.
    pub only_matching: bool,
    /// `-q` / `--quiet`: suppress all output; exit codes carry the result.
    pub quiet: bool,
}

impl Config {
    /// A configuration with every option at its documented default.
    pub fn with_defaults() -> Config {
        Config {
            pattern: String::new(),
            paths: Vec::new(),
            ignore_case: false,
            word_regexp: false,
            invert_match: false,
            line_numbers: false,
            count_only: false,
            files_with_matches: false,
            before_context: 0,
            after_context: 0,
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
            max_depth: None,
            min_size: None,
            max_size: None,
            threads: default_threads(),
            no_ignore: false,
            hidden: false,
            follow_symlinks: false,
            color: ColorChoice::Auto,
            stats: false,
            json: false,
            no_messages: false,
            max_count: None,
            byte_offset: false,
            only_matching: false,
            quiet: false,
        }
    }

    /// Parse a command line (argv without the program name).
    pub fn parse_from<I>(tokens: I) -> SearchResult<ParseOutcome>
    where
        I: IntoIterator<Item = String>,
    {
        let tokens: Vec<String> = tokens.into_iter().collect();
        let mut config = Config::with_defaults();
        let mut pattern_seen = false;
        let mut only_paths = false;
        let mut index = 0usize;

        while index < tokens.len() {
            let token = tokens[index].clone();

            if only_paths {
                record_positional(&mut config, &mut pattern_seen, token);
                index += 1;
                continue;
            }
            if token == "--" {
                only_paths = true;
                index += 1;
                continue;
            }
            if token == "-h" || token == "--help" {
                return Ok(ParseOutcome::Help);
            }
            if token == "-V" || token == "--version" {
                return Ok(ParseOutcome::Version);
            }
            if let Some(rest) = token.strip_prefix("--") {
                index = parse_long(rest, &tokens, index, &mut config)?;
                continue;
            }
            if token.len() > 1 && token.starts_with('-') {
                index = parse_short_cluster(&token, &tokens, index, &mut config)?;
                continue;
            }
            record_positional(&mut config, &mut pattern_seen, token);
            index += 1;
        }

        if !pattern_seen {
            return Err(SearchError::arg(
                "missing PATTERN (try 'searchlight --help')",
            ));
        }
        config.validate()?;
        Ok(ParseOutcome::Config(config))
    }

    /// Check cross-flag invariants that no single flag can validate alone.
    pub fn validate(&self) -> SearchResult<()> {
        if self.threads == 0 {
            return Err(SearchError::arg("-j/--threads must be at least 1"));
        }
        if self.threads > MAX_THREADS {
            return Err(SearchError::arg(format!(
                "-j/--threads must be at most {}",
                MAX_THREADS
            )));
        }
        if let (Some(min), Some(max)) = (self.min_size, self.max_size) {
            if min > max {
                return Err(SearchError::arg(format!(
                    "--min-size ({}) exceeds --max-size ({})",
                    min, max
                )));
            }
        }
        Ok(())
    }
}

/// Reasonable fallback when the CPU count cannot be determined.
fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(4)
}

/// Store a positional argument: the first is the pattern, the rest are paths.
fn record_positional(config: &mut Config, pattern_seen: &mut bool, token: String) {
    if *pattern_seen {
        config.paths.push(PathBuf::from(token));
    } else {
        *pattern_seen = true;
        config.pattern = token;
    }
}

/// Pull the next command line token as an option value.
fn next_token(tokens: &[String], cursor: &mut usize, flag: &str) -> SearchResult<String> {
    match tokens.get(*cursor) {
        Some(value) => {
            let value = value.clone();
            *cursor += 1;
            Ok(value)
        }
        None => Err(SearchError::arg(format!(
            "option '{}' requires a value",
            flag
        ))),
    }
}

/// Parse one long option. `rest` excludes the leading `--`. Returns the index
/// of the next unconsumed token.
fn parse_long(
    rest: &str,
    tokens: &[String],
    index: usize,
    config: &mut Config,
) -> SearchResult<usize> {
    let (name, inline) = match rest.split_once('=') {
        Some((name, value)) => (name, Some(value.to_string())),
        None => (rest, None),
    };
    let mut next = index + 1;

    macro_rules! value {
        () => {
            match &inline {
                Some(value) => value.clone(),
                None => next_token(tokens, &mut next, &format!("--{}", name))?,
            }
        };
    }
    macro_rules! no_value {
        () => {
            if inline.is_some() {
                return Err(SearchError::arg(format!(
                    "option '--{}' does not take a value",
                    name
                )));
            }
        };
    }

    match name {
        "ignore-case" => {
            no_value!();
            config.ignore_case = true;
        }
        "word-regexp" => {
            no_value!();
            config.word_regexp = true;
        }
        "invert-match" => {
            no_value!();
            config.invert_match = true;
        }
        "line-number" => {
            no_value!();
            config.line_numbers = true;
        }
        "count" => {
            no_value!();
            config.count_only = true;
        }
        "files-with-matches" => {
            no_value!();
            config.files_with_matches = true;
        }
        "json" => {
            no_value!();
            config.json = true;
        }
        "quiet" => {
            no_value!();
            config.quiet = true;
        }
        "stats" => {
            no_value!();
            config.stats = true;
        }
        "no-messages" => {
            no_value!();
            config.no_messages = true;
        }
        "no-ignore" => {
            no_value!();
            config.no_ignore = true;
        }
        "hidden" => {
            no_value!();
            config.hidden = true;
        }
        "follow" => {
            no_value!();
            config.follow_symlinks = true;
        }
        "byte-offset" => {
            no_value!();
            config.byte_offset = true;
        }
        "only-matching" => {
            no_value!();
            config.only_matching = true;
        }
        "glob" => {
            config.include_globs.push(value!());
        }
        "exclude-glob" => {
            config.exclude_globs.push(value!());
        }
        "max-depth" => {
            let raw = value!();
            config.max_depth = Some(parse_usize(&raw, "--max-depth")?);
        }
        "min-size" => {
            let raw = value!();
            config.min_size = Some(parse_size_or_fail(&raw, "--min-size")?);
        }
        "max-size" => {
            let raw = value!();
            config.max_size = Some(parse_size_or_fail(&raw, "--max-size")?);
        }
        "threads" => {
            let raw = value!();
            config.threads = parse_threads(&raw, "--threads")?;
        }
        "max-count" => {
            let raw = value!();
            let parsed = parse_usize(&raw, "--max-count")?;
            if parsed == 0 {
                return Err(SearchError::arg("-m/--max-count must be at least 1"));
            }
            config.max_count = Some(parsed);
        }
        "color" => {
            let raw = value!();
            config.color = ColorChoice::parse(&raw)?;
        }
        unknown => {
            return Err(SearchError::arg(format!(
                "unrecognized option '--{}'",
                unknown
            )));
        }
    }
    Ok(next)
}

/// Parse a cluster of short flags such as `-inv` or `-C3`. Returns the index
/// of the next unconsumed token.
fn parse_short_cluster(
    token: &str,
    tokens: &[String],
    index: usize,
    config: &mut Config,
) -> SearchResult<usize> {
    let chars: Vec<char> = token.chars().skip(1).collect();
    let mut cursor = 0usize;
    let mut next = index + 1;

    while cursor < chars.len() {
        let flag = chars[cursor];
        match flag {
            'i' => {
                config.ignore_case = true;
                cursor += 1;
            }
            'w' => {
                config.word_regexp = true;
                cursor += 1;
            }
            'n' => {
                config.line_numbers = true;
                cursor += 1;
            }
            'v' => {
                config.invert_match = true;
                cursor += 1;
            }
            'c' => {
                config.count_only = true;
                cursor += 1;
            }
            'l' => {
                config.files_with_matches = true;
                cursor += 1;
            }
            'b' => {
                config.byte_offset = true;
                cursor += 1;
            }
            'o' => {
                config.only_matching = true;
                cursor += 1;
            }
            'q' => {
                config.quiet = true;
                cursor += 1;
            }
            's' => {
                config.no_messages = true;
                cursor += 1;
            }
            'B' | 'A' | 'C' | 'j' | 'm' => {
                if let Some((value, end)) = attached_digits(&chars, cursor + 1) {
                    apply_value_flag(config, flag, &value)?;
                    cursor = end;
                } else {
                    let value = next_token(tokens, &mut next, &format!("-{}", flag))?;
                    apply_value_flag(config, flag, &value)?;
                    cursor = chars.len();
                }
            }
            other => {
                return Err(SearchError::arg(format!(
                    "unrecognized option '-{}'",
                    other
                )));
            }
        }
    }
    Ok(next)
}

/// Apply a short flag that takes a value (`-B`, `-A`, `-C`, `-j`, `-m`).
fn apply_value_flag(config: &mut Config, flag: char, value: &str) -> SearchResult<()> {
    match flag {
        'B' => config.before_context = parse_usize(value, "-B")?,
        'A' => config.after_context = parse_usize(value, "-A")?,
        'C' => {
            let amount = parse_usize(value, "-C")?;
            config.before_context = amount;
            config.after_context = amount;
        }
        'j' => config.threads = parse_threads(value, "-j")?,
        'm' => {
            let parsed = parse_usize(value, "-m")?;
            if parsed == 0 {
                return Err(SearchError::arg("-m/--max-count must be at least 1"));
            }
            config.max_count = Some(parsed);
        }
        _ => {
            return Err(SearchError::arg(format!(
                "option '-{}' does not take a value",
                flag
            )));
        }
    }
    Ok(())
}

/// Collect the run of ASCII digits starting at `start`, if any.
///
/// Returns the parsed digits and the index just past them; this supports the
/// attached form `-C3` alongside the separated form `-C 3`.
fn attached_digits(chars: &[char], start: usize) -> Option<(String, usize)> {
    if start >= chars.len() || !chars[start].is_ascii_digit() {
        return None;
    }
    let mut end = start;
    while end < chars.len() && chars[end].is_ascii_digit() {
        end += 1;
    }
    Some((chars[start..end].iter().collect(), end))
}

/// Parse a non-negative integer, with the flag name embedded in the error.
fn parse_usize(value: &str, flag: &str) -> SearchResult<usize> {
    value.parse::<usize>().map_err(|_| {
        SearchError::arg(format!(
            "option '{}' expects a non-negative integer, got '{}'",
            flag, value
        ))
    })
}

/// Parse and bound-check a thread count.
fn parse_threads(value: &str, flag: &str) -> SearchResult<usize> {
    let parsed = parse_usize(value, flag)?;
    if parsed == 0 {
        return Err(SearchError::arg(format!("{} must be at least 1", flag)));
    }
    if parsed > MAX_THREADS {
        return Err(SearchError::arg(format!(
            "{} must be at most {}",
            flag, MAX_THREADS
        )));
    }
    Ok(parsed)
}

/// Parse a human size such as `512`, `10k`, `5m` or `1g` into bytes.
///
/// Multipliers are binary (k = 1024, m = 1024², g = 1024³, t = 1024⁴), the
/// suffix is case-insensitive and a bare `b` is a no-op. Returns `None` for
/// anything malformed; overflow-safe via `checked_mul`.
pub fn parse_size(text: &str) -> Option<u64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let split_at = trimmed
        .char_indices()
        .find(|(_, ch)| !ch.is_ascii_digit())
        .map(|(idx, _)| idx)
        .unwrap_or(trimmed.len());
    let (digits, suffix) = trimmed.split_at(split_at);
    if digits.is_empty() {
        return None;
    }
    let multiplier: u64 = match suffix.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" => 1024,
        "m" => 1024 * 1024,
        "g" => 1024 * 1024 * 1024,
        "t" => 1024u64 * 1024 * 1024 * 1024,
        _ => return None,
    };
    digits.parse::<u64>().ok()?.checked_mul(multiplier)
}

/// [`parse_size`] with the flag name folded into the error message.
fn parse_size_or_fail(text: &str, flag: &str) -> SearchResult<u64> {
    parse_size(text).ok_or_else(|| {
        SearchError::arg(format!(
            "option '{}' expects a size like 512, 10k, 2m or 1g, got '{}'",
            flag, text
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn config_of(items: &[&str]) -> Config {
        match Config::parse_from(args(items)).expect("parse should succeed") {
            ParseOutcome::Config(config) => config,
            other => panic!("expected Config, got {:?}", other),
        }
    }

    fn error_of(items: &[&str]) -> String {
        Config::parse_from(args(items))
            .expect_err("parse should fail")
            .to_string()
    }

    #[test]
    fn minimal_invocation_sets_pattern_and_defaults() {
        let config = config_of(&["needle"]);
        assert_eq!(config.pattern, "needle");
        assert!(config.paths.is_empty(), "no path means cwd later");
        assert_eq!(config.threads, default_threads());
        assert_eq!(config.color, ColorChoice::Auto);
        assert_eq!(config.before_context, 0);
    }

    #[test]
    fn pattern_and_paths_are_split_positionally() {
        let config = config_of(&["foo", "src", "docs"]);
        assert_eq!(config.pattern, "foo");
        assert_eq!(
            config.paths,
            vec![PathBuf::from("src"), PathBuf::from("docs")]
        );
    }

    #[test]
    fn boolean_short_flags_cluster() {
        let config = config_of(&["-invqw", "x"]);
        assert!(config.ignore_case);
        assert!(config.line_numbers);
        assert!(config.invert_match);
        assert!(config.quiet);
        assert!(config.word_regexp);
        assert!(!config.count_only);
    }

    #[test]
    fn context_flags_accept_attached_and_separate_values() {
        let config = config_of(&["-C2", "x"]);
        assert_eq!(config.before_context, 2);
        assert_eq!(config.after_context, 2);

        let config = config_of(&["-B", "3", "-A1", "x"]);
        assert_eq!(config.before_context, 3);
        assert_eq!(config.after_context, 1);

        // -C applied first, then -B overrides only the before side.
        let config = config_of(&["-C", "4", "-B", "1", "x"]);
        assert_eq!(config.before_context, 1);
        assert_eq!(config.after_context, 4);
    }

    #[test]
    fn long_options_support_both_value_forms() {
        let config = config_of(&["--color=always", "--max-depth", "3", "x"]);
        assert_eq!(config.color, ColorChoice::Always);
        assert_eq!(config.max_depth, Some(3));

        let config = config_of(&["--color", "never", "--max-depth=2", "x"]);
        assert_eq!(config.color, ColorChoice::Never);
        assert_eq!(config.max_depth, Some(2));
    }

    #[test]
    fn repeatable_globs_and_sizes_accumulate() {
        let config = config_of(&[
            "--glob",
            "*.rs",
            "--glob",
            "*.toml",
            "--exclude-glob",
            "target/*",
            "--min-size",
            "512",
            "--max-size",
            "10m",
            "-m5",
            "-j8",
            "x",
        ]);
        assert_eq!(config.include_globs, vec!["*.rs", "*.toml"]);
        assert_eq!(config.exclude_globs, vec!["target/*"]);
        assert_eq!(config.min_size, Some(512));
        assert_eq!(config.max_size, Some(10 * 1024 * 1024));
        assert_eq!(config.max_count, Some(5));
        assert_eq!(config.threads, 8);
    }

    #[test]
    fn parse_size_forms() {
        assert_eq!(parse_size("512"), Some(512));
        assert_eq!(parse_size("10k"), Some(10_240));
        assert_eq!(parse_size("5M"), Some(5 * 1024 * 1024));
        assert_eq!(parse_size("2g"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("1t"), Some(1_099_511_627_776));
        assert_eq!(parse_size("64b"), Some(64));
        assert_eq!(parse_size(" 16 "), Some(16));
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("k"), None);
        assert_eq!(parse_size("12x"), None);
        assert_eq!(parse_size("-5"), None);
        assert_eq!(parse_size("99999999999999999999999g"), None);
    }

    #[test]
    fn double_dash_makes_everything_positional() {
        let config = config_of(&["--", "-weird-pattern", "-file"]);
        assert_eq!(config.pattern, "-weird-pattern");
        assert_eq!(config.paths, vec![PathBuf::from("-file")]);
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert!(matches!(
            Config::parse_from(args(&["--help"])),
            Ok(ParseOutcome::Help)
        ));
        assert!(matches!(
            Config::parse_from(args(&["-h"])),
            Ok(ParseOutcome::Help)
        ));
        assert!(matches!(
            Config::parse_from(args(&["--version"])),
            Ok(ParseOutcome::Version)
        ));
        assert!(matches!(
            Config::parse_from(args(&["-V"])),
            Ok(ParseOutcome::Version)
        ));
    }

    #[test]
    fn invalid_command_lines_are_rejected() {
        assert!(error_of(&["src/"]).contains("missing PATTERN"));
        assert!(error_of(&["--nope", "x"]).contains("--nope"));
        assert!(error_of(&["-Z", "x"]).contains("-Z"));
        assert!(error_of(&["--max-depth"]).contains("requires a value"));
        assert!(error_of(&["--json=1", "x"]).contains("does not take a value"));
        assert!(error_of(&["--max-depth", "-1", "x"]).contains("non-negative"));
        assert!(error_of(&["--max-depth", "abc", "x"]).contains("non-negative"));
        assert!(error_of(&["-j", "0", "x"]).contains("at least 1"));
        assert!(error_of(&["-j4096", "x"]).contains("at most"));
        assert!(error_of(&["-m", "0", "x"]).contains("at least 1"));
        assert!(error_of(&["--color", "sometimes", "x"]).contains("--color"));
        assert!(error_of(&["--max-size", "10x", "x"]).contains("size like"));
    }

    #[test]
    fn validate_catches_contradictory_sizes() {
        let mut config = Config::with_defaults();
        config.pattern = "x".to_string();
        config.min_size = Some(200);
        config.max_size = Some(100);
        assert!(config.validate().is_err());

        config.max_size = Some(300);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn diagnostic_flags_map_onto_fields() {
        let config = config_of(&[
            "-qsnbo",
            "--stats",
            "--json",
            "--no-ignore",
            "--hidden",
            "--follow",
            "x",
        ]);
        assert!(config.quiet);
        assert!(config.no_messages);
        assert!(config.line_numbers);
        assert!(config.byte_offset);
        assert!(config.only_matching);
        assert!(config.stats);
        assert!(config.json);
        assert!(config.no_ignore);
        assert!(config.hidden);
        assert!(config.follow_symlinks);
    }
}
