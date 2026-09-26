//! ANSI highlighting for match spans.
//!
//! The pattern engine reports matches as **char-index ranges** over a line.
//! This module turns those ranges into terminal color: it owns the palette
//! (one SGR escape sequence per output role) and knows how to paint a line
//! so that exactly the matched characters get the match color, regardless
//! of multi-byte UTF-8 content.
//!
//! Rules that keep the output sane:
//!
//! * Spans are merged first — touching or overlapping ranges (`a|ab`,
//!   alternations with shared characters) never produce nested resets.
//! * Zero-width spans (empty patterns) are skipped: there is nothing to
//!   color, and painting `reset` twice in a row would only add noise.
//! * A colorless palette short-circuits to the plain line, so
//!   `--color never` output is byte-identical to text without matches.
//!
//! [`strip_ansi`] is the inverse for plain-text comparisons (used by tests
//! and by scripts that normalize captured output).

/// The reset sequence that closes any SGR color.
pub const RESET: &str = "\x1b[0m";

/// One ANSI color assignment per output role.
///
/// The default palette follows the classic grep conventions: bold magenta
/// file names, bold green line numbers, cyan separators and bold red
/// matches. Every field holds a full escape sequence (or the empty string
/// for a colorless palette).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    /// File-name prefix.
    pub path: &'static str,
    /// Line-number / byte-offset prefix.
    pub line_number: &'static str,
    /// The `--` group separator.
    pub separator: &'static str,
    /// Matched characters inside a line.
    pub matched: &'static str,
    /// Sequence that closes any open color (usually [`RESET`]).
    pub reset: &'static str,
}

impl Palette {
    /// A palette that emits no escape sequences at all.
    pub fn colorless() -> Palette {
        Palette {
            path: "",
            line_number: "",
            separator: "",
            matched: "",
            reset: "",
        }
    }

    /// The classic grep-like color assignment.
    pub fn classic() -> Palette {
        Palette {
            path: "\x1b[1;35m",
            line_number: "\x1b[1;32m",
            separator: "\x1b[36m",
            matched: "\x1b[1;31m",
            reset: RESET,
        }
    }

    /// `true` when this palette emits escape codes.
    pub fn is_colored(&self) -> bool {
        !self.reset.is_empty()
    }

    /// Wrap `text` between `code` and the palette reset.
    ///
    /// Returns `text` unchanged for a colorless palette or an empty code,
    /// so call sites never need to branch on the color decision.
    pub fn paint(&self, code: &str, text: &str) -> String {
        if !self.is_colored() || code.is_empty() {
            return text.to_string();
        }
        format!("{}{}{}", code, text, self.reset)
    }
}

/// Merge overlapping or touching `(start, end)` char ranges, dropping
/// zero-width spans, so highlighting never nests or empties.
///
/// ```text
/// merge_spans(&[(0, 1), (1, 3), (5, 5)]) == [(0, 3)]
/// ```
pub fn merge_spans(spans: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut sorted: Vec<(usize, usize)> = spans
        .iter()
        .copied()
        .filter(|(start, end)| end > start)
        .collect();
    sorted.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in sorted {
        match merged.last_mut() {
            Some(last) if start <= last.1 => {
                if end > last.1 {
                    last.1 = end;
                }
            }
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// Render `line` with every span highlighted by `palette.matched`.
///
/// The spans are char indices (as produced by
/// [`crate::pattern::Pattern::find_all`]); they are merged first, so the
/// caller may pass arbitrary non-overlapping or overlapping ranges.
pub fn highlight_line(line: &str, spans: &[(usize, usize)], palette: &Palette) -> String {
    let merged = merge_spans(spans);
    if merged.is_empty() || !palette.is_colored() {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len() + 16 * merged.len());
    let mut next = 0usize;
    let mut open_end: Option<usize> = None;
    for (char_index, ch) in line.chars().enumerate() {
        // Open spans starting at this position (they are sorted, so at most
        // the span at `next` can start here).
        while next < merged.len() && merged[next].0 <= char_index {
            if merged[next].1 > merged[next].0 {
                out.push_str(palette.matched);
                open_end = Some(merged[next].1);
            }
            next += 1;
        }
        out.push(ch);
        if open_end == Some(char_index + 1) {
            out.push_str(palette.reset);
            open_end = None;
        }
    }
    if open_end.is_some() {
        // The final match runs to the end of the line.
        out.push_str(palette.reset);
    }
    out
}

/// Remove ANSI SGR escape sequences from `text`.
///
/// Understands full CSI sequences (`ESC [ ... final-byte`) and swallows
/// two-character escapes (`ESC x`). Everything else — including every
/// valid UTF-8 character — passes through untouched.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                // CSI sequences end with a byte in 0x40..=0x7E.
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            // Two-character escape such as ESC M: drop the second char.
            chars.next();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colorless_palette_paints_nothing() {
        let palette = Palette::colorless();
        assert!(!palette.is_colored());
        assert_eq!(palette.paint(palette.matched, "hello"), "hello");
        assert_eq!(palette.paint(palette.path, "x.txt"), "x.txt");
    }

    #[test]
    fn classic_palette_wraps_with_reset() {
        let palette = Palette::classic();
        assert!(palette.is_colored());
        assert_eq!(
            palette.paint(palette.matched, "he"),
            "\x1b[1;31mhe\x1b[0m"
        );
        // An empty code paints nothing even when coloring is enabled.
        assert_eq!(palette.paint("", "plain"), "plain");
    }

    #[test]
    fn merge_spans_unions_touching_ranges() {
        assert_eq!(merge_spans(&[]), Vec::new());
        assert_eq!(merge_spans(&[(0, 1)]), vec![(0, 1)]);
        assert_eq!(merge_spans(&[(0, 1), (1, 3)]), vec![(0, 3)]);
        assert_eq!(merge_spans(&[(0, 5), (2, 3)]), vec![(0, 5)]);
        assert_eq!(merge_spans(&[(5, 6), (0, 1)]), vec![(0, 1), (5, 6)]);
        // Zero-width spans disappear.
        assert_eq!(merge_spans(&[(2, 2), (3, 5)]), vec![(3, 5)]);
        assert_eq!(merge_spans(&[(4, 4)]), Vec::new());
    }

    #[test]
    fn highlight_paints_exactly_the_match() {
        let palette = Palette::classic();
        assert_eq!(
            highlight_line("hello world", &[(6, 11)], &palette),
            "hello \x1b[1;31mworld\x1b[0m"
        );
        // A match running to the end of the line still gets its reset.
        assert_eq!(
            highlight_line("abc", &[(1, 3)], &palette),
            "a\x1b[1;31mbc\x1b[0m"
        );
    }

    #[test]
    fn highlight_is_unicode_safe() {
        let palette = Palette::classic();
        // 'é' is one char (two bytes); the span must color only that char.
        assert_eq!(
            highlight_line("héllo", &[(1, 2)], &palette),
            "h\x1b[1;31mé\x1b[0mllo"
        );
        let wide = "a✓b✓c";
        assert_eq!(
            highlight_line(wide, &[(1, 2), (3, 4)], &palette),
            "a\x1b[1;31m✓\x1b[0mb\x1b[1;31m✓\x1b[0mc"
        );
    }

    #[test]
    fn adjacent_spans_render_as_one_block() {
        let palette = Palette::classic();
        assert_eq!(
            highlight_line("abcd", &[(0, 1), (1, 3)], &palette),
            "\x1b[1;31mabc\x1b[0md"
        );
    }

    #[test]
    fn empty_and_zero_width_spans_change_nothing() {
        let palette = Palette::classic();
        assert_eq!(highlight_line("abc", &[], &palette), "abc");
        assert_eq!(highlight_line("abc", &[(1, 1)], &palette), "abc");
        // A colorless palette short-circuits even with real spans.
        assert_eq!(
            highlight_line("abc", &[(0, 3)], &Palette::colorless()),
            "abc"
        );
    }

    #[test]
    fn strip_removes_all_escape_forms() {
        assert_eq!(strip_ansi("\x1b[1;31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi("a\x1b[36m--\x1b[0mb"), "a--b");
        assert_eq!(strip_ansi("\x1b[1m\x1b[32mx\x1b[0m"), "x");
        assert_eq!(strip_ansi("\x1bM"), "", "two-char escape swallowed");
        assert_eq!(strip_ansi("plain text ✓"), "plain text ✓");
    }

    #[test]
    fn highlight_then_strip_round_trips() {
        let palette = Palette::classic();
        let line = "gr(e|a)y héllo";
        let highlighted = highlight_line(line, &[(0, 4), (5, 10)], &palette);
        assert_eq!(strip_ansi(&highlighted), line);
    }
}
