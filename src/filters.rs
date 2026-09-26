//! File selection rules: globs, size limits, ignore sets and binary sniffing.
//!
//! The walker consults three layers before a file reaches the search stage:
//!
//! 1. **Glob filters** — the user-facing `--glob` / `--exclude-glob` flags.
//!    Globs without a `/` match the *file name* anywhere in the tree; globs
//!    containing a `/` match the path *relative to the search root*. `*` and
//!    `?` never cross directory separators (see `pattern::Pattern::from_glob`).
//! 2. **Size limits** — `--min-size` / `--max-size` in bytes.
//! 3. **Ignore sets** — `.gitignore`/`.ignore` files found in the search
//!    roots, parsed with a pragmatic subset of gitignore semantics:
//!    comments (`#`), negation (`!`), directory-only patterns (trailing `/`)
//!    and path anchoring (a leading `/` or any inner `/`). The *last*
//!    matching rule wins, exactly like git.
//!
//! Well-known VCS and build directories (`.git`, `node_modules`, `target`,
//! `__pycache__`, ...) are skipped by the walker itself via [`SKIP_DIRS`];
//! `--no-ignore` disables the ignore-file layer but not that built-in list.
//!
//! This module also owns [`looks_binary`], the NUL-byte heuristic used by the
//! search layer to skip binary files.

use crate::args::Config;
use crate::error::{SearchError, SearchResult};
use crate::pattern::Pattern;

/// Number of leading bytes inspected by [`looks_binary`].
pub const BINARY_SNIFF_BYTES: usize = 8192;

/// Directories the walker never descends into.
///
/// These names are overwhelmingly generated artifacts or VCS internals, and
/// skipping them keeps `searchlight foo .` fast in typical repositories.
/// `--no-ignore` does **not** disable this list; pass explicit paths if you
/// truly need to search inside one of these directories.
pub const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".bzr",
    "node_modules",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    "target",
    "vendor",
];

/// A compiled glob plus the rule for where it applies.
#[derive(Debug, Clone)]
pub struct GlobRule {
    pattern: Pattern,
    /// When `true` the glob is matched against the path relative to the
    /// search root; when `false`, against the bare file name.
    full_path: bool,
}

impl GlobRule {
    /// Compile a glob string. Fails when the glob contains malformed
    /// character classes (e.g. a dangling `[`).
    pub fn compile(glob: &str) -> SearchResult<GlobRule> {
        if glob.is_empty() {
            return Err(SearchError::arg("glob patterns must not be empty"));
        }
        let full_path = glob.contains('/');
        let pattern = Pattern::from_glob(glob, false)?;
        Ok(GlobRule {
            pattern,
            full_path,
        })
    }

    /// Test this rule against a path. `rel_path` must use `/` separators.
    pub fn matches(&self, rel_path: &str, file_name: &str) -> bool {
        if self.full_path {
            self.pattern.is_match(rel_path)
        } else {
            self.pattern.is_match(file_name)
        }
    }

    /// The original glob text (for diagnostics).
    pub fn source(&self) -> &str {
        self.pattern.source()
    }
}

/// The complete file-acceptance decision for one candidate file.
#[derive(Debug, Clone)]
pub struct FileFilter {
    include: Vec<GlobRule>,
    exclude: Vec<GlobRule>,
    min_size: Option<u64>,
    max_size: Option<u64>,
}

impl FileFilter {
    /// A filter that accepts every file (no globs, no size limits).
    pub fn empty() -> FileFilter {
        FileFilter {
            include: Vec::new(),
            exclude: Vec::new(),
            min_size: None,
            max_size: None,
        }
    }

    /// Build a filter from raw glob strings and byte limits.
    ///
    /// Errors surface from glob compilation; the size pair is validated here
    /// so a contradictory `--min-size`/`--max-size` fails fast at startup.
    pub fn new(
        include: &[String],
        exclude: &[String],
        min_size: Option<u64>,
        max_size: Option<u64>,
    ) -> SearchResult<FileFilter> {
        let mut include_rules = Vec::with_capacity(include.len());
        for glob in include {
            include_rules.push(GlobRule::compile(glob)?);
        }
        let mut exclude_rules = Vec::with_capacity(exclude.len());
        for glob in exclude {
            exclude_rules.push(GlobRule::compile(glob)?);
        }
        if let (Some(min), Some(max)) = (min_size, max_size) {
            if min > max {
                return Err(SearchError::arg(format!(
                    "--min-size ({}) exceeds --max-size ({})",
                    min, max
                )));
            }
        }
        Ok(FileFilter {
            include: include_rules,
            exclude: exclude_rules,
            min_size,
            max_size,
        })
    }

    /// Build the filter from a parsed [`Config`].
    pub fn from_config(config: &Config) -> SearchResult<FileFilter> {
        FileFilter::new(
            &config.include_globs,
            &config.exclude_globs,
            config.min_size,
            config.max_size,
        )
    }

    /// `true` when the filter imposes no restriction at all.
    pub fn is_noop(&self) -> bool {
        self.include.is_empty()
            && self.exclude.is_empty()
            && self.min_size.is_none()
            && self.max_size.is_none()
    }

    /// Decide whether a file passes every layer of the filter.
    ///
    /// `rel_path` is the path relative to the search root with `/`
    /// separators (for a file passed explicitly, the file name is used).
    /// `size` is the file length in bytes. Order of checks: size bounds,
    /// include globs (any match required when present), exclude globs (any
    /// match rejects).
    pub fn accepts(&self, rel_path: &str, size: u64) -> bool {
        if let Some(min) = self.min_size {
            if size < min {
                return false;
            }
        }
        if let Some(max) = self.max_size {
            if size > max {
                return false;
            }
        }
        let file_name = base_name(rel_path);
        if !self.include.is_empty() {
            let included = self
                .include
                .iter()
                .any(|rule| rule.matches(rel_path, file_name));
            if !included {
                return false;
            }
        }
        !self
            .exclude
            .iter()
            .any(|rule| rule.matches(rel_path, file_name))
    }
}

/// The portion of `path` after the last `/` (the whole string if none).
fn base_name(path: &str) -> &str {
    match path.rfind('/') {
        Some(idx) => &path[idx + 1..],
        None => path,
    }
}

/// One parsed line of an ignore file.
#[derive(Debug, Clone)]
struct IgnoreRule {
    /// `!pattern` — a previous match is revoked when this rule matches.
    negated: bool,
    /// `pattern/` — only applies to directories.
    dir_only: bool,
    /// The pattern contains a `/`, so it matches against the whole relative
    /// path instead of the file name alone.
    full_path: bool,
    pattern: Pattern,
}

/// A set of ignore rules parsed from `.ignore` / `.gitignore` content.
#[derive(Debug, Clone, Default)]
pub struct IgnoreSet {
    rules: Vec<IgnoreRule>,
}

impl IgnoreSet {
    /// An empty set that ignores nothing.
    pub fn empty() -> IgnoreSet {
        IgnoreSet { rules: Vec::new() }
    }

    /// Parse ignore-file `text` and append its rules to this set.
    ///
    /// Malformed lines (glob syntax errors) are skipped silently: ignore
    /// files are advisory, and one bad line should not abort a search.
    pub fn extend(&mut self, text: &str) {
        for raw_line in text.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (negated, line) = match line.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, line),
            };
            let (dir_only, line) = match line.strip_suffix('/') {
                Some(rest) => (true, rest),
                None => (false, line),
            };
            let (line, anchored) = match line.strip_prefix('/') {
                Some(rest) => (rest, true),
                None => (line, false),
            };
            if line.is_empty() {
                continue;
            }
            let full_path = anchored || line.contains('/');
            let pattern = match Pattern::from_glob(line, false) {
                Ok(pattern) => pattern,
                Err(_) => continue,
            };
            self.rules.push(IgnoreRule {
                negated,
                dir_only,
                full_path,
                pattern,
            });
        }
    }

    /// Parse a complete ignore file in one call.
    pub fn from_text(text: &str) -> IgnoreSet {
        let mut set = IgnoreSet::empty();
        set.extend(text);
        set
    }

    /// `true` when no rules were parsed.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Evaluate the set for a path relative to the ignore file's directory.
    ///
    /// Git semantics: the last matching rule decides, so negations can rescue
    /// paths caught by earlier rules. Directory-only rules are skipped for
    /// files.
    pub fn matches(&self, rel_path: &str, is_dir: bool) -> bool {
        let file_name = base_name(rel_path);
        let mut ignored = false;
        for rule in &self.rules {
            if rule.dir_only && !is_dir {
                continue;
            }
            let target = if rule.full_path {
                rel_path
            } else {
                file_name
            };
            if rule.pattern.is_match(target) {
                ignored = !rule.negated;
            }
        }
        ignored
    }
}

/// Heuristically decide whether `bytes` is a binary file.
///
/// A NUL byte anywhere in the first [`BINARY_SNIFF_BYTES`] bytes marks the
/// file as binary — the same rule `grep` uses. Text in any encoding passes;
/// invalid UTF-8 without NUL bytes is rejected later by the UTF-8 conversion
/// in the search layer.
pub fn looks_binary(bytes: &[u8]) -> bool {
    let limit = bytes.len().min(BINARY_SNIFF_BYTES);
    bytes[..limit].contains(&0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(
        include: &[&str],
        exclude: &[&str],
        min: Option<u64>,
        max: Option<u64>,
    ) -> FileFilter {
        let include: Vec<String> = include.iter().map(|s| s.to_string()).collect();
        let exclude: Vec<String> = exclude.iter().map(|s| s.to_string()).collect();
        FileFilter::new(&include, &exclude, min, max).expect("valid globs")
    }

    #[test]
    fn noop_filter_accepts_everything() {
        let f = filter(&[], &[], None, None);
        assert!(f.is_noop());
        assert!(f.accepts("anything/at/all.txt", 0));
        assert!(f.accepts("main.rs", 999_999));
    }

    #[test]
    fn include_globs_restrict_by_file_name() {
        let f = filter(&["*.rs"], &[], None, None);
        assert!(f.accepts("src/main.rs", 10));
        assert!(f.accepts("main.rs", 10));
        assert!(!f.accepts("src/main.c", 10));
        assert!(!f.accepts("docs/readme.md", 10));
    }

    #[test]
    fn include_globs_with_slash_match_the_relative_path() {
        let f = filter(&["src/*.rs"], &[], None, None);
        assert!(f.accepts("src/main.rs", 1));
        // `*` never crosses a separator, so nested files are out.
        assert!(!f.accepts("src/sub/main.rs", 1));
        assert!(!f.accepts("tests/main.rs", 1));
    }

    #[test]
    fn exclude_globs_win_over_includes() {
        let f = filter(&["*.rs"], &["generated_*"], None, None);
        assert!(f.accepts("src/hand.rs", 1));
        assert!(!f.accepts("src/generated_api.rs", 1));
    }

    #[test]
    fn size_bounds_are_inclusive() {
        let f = filter(&[], &[], Some(100), Some(200));
        assert!(!f.accepts("tiny", 99));
        assert!(f.accepts("low", 100));
        assert!(f.accepts("high", 200));
        assert!(!f.accepts("big", 201));

        let min_only = filter(&[], &[], Some(5), None);
        assert!(min_only.accepts("x", u64::MAX));

        let max_only = filter(&[], &[], None, Some(5));
        assert!(max_only.accepts("x", 0));
    }

    #[test]
    fn contradictory_sizes_are_rejected() {
        let err = FileFilter::new(&[], &[], Some(300), Some(100)).expect_err("must fail");
        assert!(err.to_string().contains("exceeds"));
    }

    #[test]
    fn malformed_globs_fail_compilation() {
        let mut globs = Vec::new();
        globs.push("[unclosed".to_string());
        assert!(FileFilter::new(&globs, &[], None, None).is_err());
        assert!(GlobRule::compile("").is_err());
    }

    #[test]
    fn looks_binary_sniffs_nul_bytes() {
        assert!(!looks_binary(b"plain text\nwith lines\r\n"));
        assert!(!looks_binary(b""));
        assert!(!looks_binary("ünïcode ✓".as_bytes()));
        assert!(looks_binary(&[b'a', 0, b'b']));
        let mut big = vec![b'x'; BINARY_SNIFF_BYTES];
        big.push(0);
        assert!(looks_binary(&big), "NUL just past the window is still seen");
        let mut after_window = vec![b'x'; BINARY_SNIFF_BYTES + 64];
        after_window[BINARY_SNIFF_BYTES + 32] = 0;
        assert!(
            !looks_binary(&after_window),
            "NUL far beyond the sniff window is ignored"
        );
    }

    #[test]
    fn ignore_sets_parse_comments_and_blanks() {
        let set = IgnoreSet::from_text("# a comment\n\n   \n*.log\n");
        assert_eq!(set.rules_len(), 1);
        assert!(set.matches("debug.log", false));
        assert!(!set.matches("debug.txt", false));
    }

    #[test]
    fn ignore_negation_lets_the_last_rule_win() {
        let set = IgnoreSet::from_text("*.log\n!important.log\n");
        assert!(set.matches("app.log", false));
        assert!(!set.matches("important.log", false), "negation rescues it");
    }

    #[test]
    fn ignore_dir_only_rules_apply_only_to_directories() {
        let set = IgnoreSet::from_text("build/\n");
        assert!(set.matches("build", true));
        assert!(!set.matches("build", false));
        // A file named build.txt is untouched by `build/`.
        assert!(!set.matches("build.txt", false));
    }

    #[test]
    fn ignore_patterns_with_slash_are_path_anchored() {
        let set = IgnoreSet::from_text("docs/*.tmp\n/rooted\n");
        assert!(set.matches("docs/notes.tmp", false));
        assert!(!set.matches("sub/docs/notes.tmp", false));
        assert!(set.matches("rooted", false));
        assert!(!set.matches("deep/rooted", false));
    }

    #[test]
    fn ignore_set_can_be_extended_across_files() {
        let mut set = IgnoreSet::empty();
        assert!(set.is_empty());
        set.extend("*.log\n");
        set.extend("!keep.log\n");
        assert_eq!(set.rules_len(), 2);
        assert!(set.matches("a.log", false));
        assert!(!set.matches("keep.log", false));
    }

    #[test]
    fn base_name_extraction() {
        assert_eq!(base_name("a/b/c.txt"), "c.txt");
        assert_eq!(base_name("solo"), "solo");
        assert_eq!(base_name("a/"), "");
    }

    #[test]
    fn filter_built_from_config_wires_all_layers() {
        let mut config = Config::with_defaults();
        config.pattern = "x".into();
        config.include_globs = vec!["*.txt".to_string()];
        config.exclude_globs = vec!["skip*".to_string()];
        config.min_size = Some(2);
        let filter = FileFilter::from_config(&config).expect("valid");
        assert!(filter.accepts("keep.txt", 5));
        assert!(!filter.accepts("skipme.txt", 5));
        assert!(!filter.accepts("keep.txt", 1));
    }

    impl IgnoreSet {
        /// Test hook: number of parsed rules.
        fn rules_len(&self) -> usize {
            self.rules.len()
        }
    }
}
