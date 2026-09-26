//! Error handling for searchlight.
//!
//! searchlight deliberately avoids third-party error crates. Everything is
//! funneled through a single [`SearchError`] enum that implements
//! [`std::fmt::Display`] and [`std::error::Error`], plus a small set of
//! convenience constructors so call sites stay terse and readable.
//!
//! Every error also knows which process exit code it maps to:
//!
//! | Situation                     | Exit code |
//! |-------------------------------|-----------|
//! | matches were found            | 0         |
//! | no matches found              | 1         |
//! | any error (usage, I/O, ...)   | 2         |
//!
//! The module additionally exposes [`io_message`], which translates the most
//! common [`io::ErrorKind`] values into short, lowercase phrases. Search
//! errors are reported one line per problem (e.g.
//! `searchlight: build/log: permission denied`), so keeping the messages
//! short and free of newlines matters for scripting.

use std::error::Error;
use std::fmt;
use std::io;

/// Convenience alias used throughout the crate.
pub type SearchResult<T> = Result<T, SearchError>;

/// The single error type for every fallible operation in searchlight.
#[derive(Debug)]
pub enum SearchError {
    /// A filesystem or pipe failure. The wrapped [`io::Error`] is preserved so
    /// callers can inspect the [`io::ErrorKind`] (for example to detect a
    /// broken pipe when the user pipes output into `head`).
    Io(io::Error),
    /// The command line could not be parsed or validated.
    Arg(String),
    /// The search pattern (or a glob derived from one) could not be compiled.
    Pattern(String),
    /// An internal invariant failed at runtime. These should be rare and are
    /// treated as fatal.
    Runtime(String),
}

impl SearchError {
    /// Build an [`SearchError::Arg`] from anything string-like.
    pub fn arg(message: impl Into<String>) -> SearchError {
        SearchError::Arg(message.into())
    }

    /// Build a [`SearchError::Pattern`] from anything string-like.
    pub fn pattern(message: impl Into<String>) -> SearchError {
        SearchError::Pattern(message.into())
    }

    /// Build a [`SearchError::Runtime`] from anything string-like.
    pub fn runtime(message: impl Into<String>) -> SearchError {
        SearchError::Runtime(message.into())
    }

    /// Map this error onto the process exit code the CLI should use.
    ///
    /// All error classes map to `2` (matching the classic grep convention in
    /// which `0` means "matches found", `1` means "no matches" and `2` means
    /// "something went wrong"). The method exists so call sites express intent
    /// through the type instead of hard-coding numbers.
    pub fn exit_code(&self) -> i32 {
        match self {
            SearchError::Io(_) => 2,
            SearchError::Arg(_) => 2,
            SearchError::Pattern(_) => 2,
            SearchError::Runtime(_) => 2,
        }
    }

    /// Returns `true` when the error is a broken pipe on stdout.
    ///
    /// Piping `searchlight foo big-tree | head -n 5` closes stdout early; that
    /// is a normal way to use the tool, so the CLI treats it as success rather
    /// than printing a scary error message.
    pub fn is_broken_pipe(&self) -> bool {
        match self {
            SearchError::Io(err) => err.kind() == io::ErrorKind::BrokenPipe,
            _ => false,
        }
    }

    /// Returns `true` when the underlying I/O failure is a missing path.
    ///
    /// The argument validator uses this to decide whether a user-supplied
    /// PATH simply does not exist (the common typo case) versus a deeper
    /// filesystem problem such as a permission error on a parent directory.
    pub fn is_not_found(&self) -> bool {
        match self {
            SearchError::Io(err) => err.kind() == io::ErrorKind::NotFound,
            _ => false,
        }
    }

    /// A short class label such as `"i/o"`, `"argument"`, `"pattern"` or
    /// `"runtime"`. Useful for structured diagnostics and for tests that want
    /// to assert the error class without matching the full message text.
    pub fn kind_label(&self) -> &'static str {
        match self {
            SearchError::Io(_) => "i/o",
            SearchError::Arg(_) => "argument",
            SearchError::Pattern(_) => "pattern",
            SearchError::Runtime(_) => "runtime",
        }
    }
}

/// Translate an [`io::Error`] into a short, lowercase phrase.
///
/// The most frequent error kinds get stable, grep-style wording so scripts
/// can grep for them; anything else falls back to the error's own `Display`
/// text. The mapping is intentionally small — exotic kinds are rare in the
/// code paths searchlight exercises (metadata, read_dir, read).
pub fn io_message(err: &io::Error) -> String {
    match err.kind() {
        io::ErrorKind::NotFound => "no such file or directory".to_string(),
        io::ErrorKind::PermissionDenied => "permission denied".to_string(),
        io::ErrorKind::BrokenPipe => "broken pipe".to_string(),
        _ => err.to_string(),
    }
}

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SearchError::Io(err) => write!(f, "i/o error: {}", err),
            SearchError::Arg(msg) => write!(f, "argument error: {}", msg),
            SearchError::Pattern(msg) => write!(f, "pattern error: {}", msg),
            SearchError::Runtime(msg) => write!(f, "runtime error: {}", msg),
        }
    }
}

impl Error for SearchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            SearchError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for SearchError {
    fn from(err: io::Error) -> SearchError {
        SearchError::Io(err)
    }
}

impl From<io::ErrorKind> for SearchError {
    fn from(kind: io::ErrorKind) -> SearchError {
        SearchError::Io(io::Error::from(kind))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_includes_class_prefix() {
        let err = SearchError::arg("missing PATTERN");
        assert_eq!(err.to_string(), "argument error: missing PATTERN");

        let err = SearchError::pattern("unclosed class");
        assert_eq!(err.to_string(), "pattern error: unclosed class");

        let err = SearchError::runtime("boom");
        assert_eq!(err.to_string(), "runtime error: boom");
    }

    #[test]
    fn exit_code_is_two_for_every_error_class() {
        assert_eq!(SearchError::arg("x").exit_code(), 2);
        assert_eq!(SearchError::pattern("x").exit_code(), 2);
        assert_eq!(SearchError::runtime("x").exit_code(), 2);
        let io_err = SearchError::Io(io::Error::new(io::ErrorKind::Other, "disk"));
        assert_eq!(io_err.exit_code(), 2);
    }

    #[test]
    fn broken_pipe_is_detected() {
        let pipe = SearchError::Io(io::Error::from(io::ErrorKind::BrokenPipe));
        assert!(pipe.is_broken_pipe());

        let other = SearchError::Io(io::Error::from(io::ErrorKind::PermissionDenied));
        assert!(!other.is_broken_pipe());

        assert!(!SearchError::arg("x").is_broken_pipe());
    }

    #[test]
    fn io_error_converts_via_from() {
        fn failing() -> io::Result<()> {
            Err(io::Error::from(io::ErrorKind::NotFound))
        }
        let converted: SearchError = failing().unwrap_err().into();
        assert!(matches!(converted, SearchError::Io(_)));
    }

    #[test]
    fn kind_label_reflects_the_class() {
        assert_eq!(SearchError::arg("x").kind_label(), "argument");
        assert_eq!(SearchError::pattern("x").kind_label(), "pattern");
        assert_eq!(SearchError::runtime("x").kind_label(), "runtime");
        let io_err = SearchError::Io(io::Error::from(io::ErrorKind::Other, ));
        assert_eq!(io_err.kind_label(), "i/o");
    }

    #[test]
    fn not_found_detection() {
        let missing = SearchError::Io(io::Error::from(io::ErrorKind::NotFound));
        assert!(missing.is_not_found());
        let denied = SearchError::Io(io::Error::from(io::ErrorKind::PermissionDenied));
        assert!(!denied.is_not_found());
        assert!(!SearchError::arg("x").is_not_found());
    }

    #[test]
    fn io_message_maps_common_kinds() {
        let not_found = io::Error::from(io::ErrorKind::NotFound);
        assert_eq!(io_message(&not_found), "no such file or directory");

        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        assert_eq!(io_message(&denied), "permission denied");

        let pipe = io::Error::from(io::ErrorKind::BrokenPipe);
        assert_eq!(io_message(&pipe), "broken pipe");

        // Anything unusual keeps the underlying Display text so no detail is
        // lost in logs.
        let custom = io::Error::new(io::ErrorKind::Other, "disk on fire");
        assert_eq!(io_message(&custom), "disk on fire");
    }

    #[test]
    fn error_kinds_are_preserved_through_from() {
        let converted: SearchError = io::ErrorKind::PermissionDenied.into();
        match &converted {
            SearchError::Io(err) => assert_eq!(err.kind(), io::ErrorKind::PermissionDenied),
            _ => panic!("expected Io variant"),
        }
    }

    #[test]
    fn source_chain_surfaces_the_io_error() {
        let err = SearchError::Io(io::Error::from(io::ErrorKind::NotFound));
        let source = std::error::Error::source(&err);
        assert!(source.is_some(), "Io errors must expose their cause");
        let plain = SearchError::arg("x");
        assert!(std::error::Error::source(&plain).is_none());
    }
}
