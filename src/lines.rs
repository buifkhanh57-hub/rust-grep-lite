//! Line iteration with stable numbering and byte offsets.
//!
//! Files are read into memory as one UTF-8 string and then split into logical
//! lines. Each line keeps its 1-based number so output and context windows can
//! be rendered exactly like classic grep. Line splitting understands both
//! Unix (`\n`) and Windows (`\r\n`) terminators; a trailing terminator does
//! **not** produce a phantom empty line, but a final fragment without a
//! terminator is still returned.
//!
//! Two views over the same text are provided:
//!
//! * [`iter_lines`] — the simple `(number, text)` view used everywhere
//!   rendering cares only about content.
//! * [`iter_line_slices`] — additionally records the **byte offset** range of
//!   every line inside the original string. The search layer needs this to
//!   support `-b/--byte-offset` and the absolute offsets exposed by the JSON
//!   output mode.

/// A single logical line of a scanned file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// 1-based line number, identical to what editors display.
    pub number: u64,
    /// The line contents with the terminator (`\n` or `\r\n`) removed.
    pub text: String,
}

/// A logical line plus its byte range inside the source text.
///
/// `start` is the byte offset of the first character of the line; `end` is
/// the byte offset just past the last character *before* the terminator. Both
/// are always valid UTF-8 boundary positions in the scanned text because the
/// terminator characters (`\n`, `\r`) are ASCII and can never occur inside a
/// multi-byte sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineSlice {
    /// 1-based line number.
    pub number: u64,
    /// Byte offset of the first character of the line.
    pub start: usize,
    /// Byte offset one past the last character of the line (terminator
    /// excluded).
    pub end: usize,
    /// The line contents with the terminator removed.
    pub text: String,
}

/// Split `text` into numbered [`Line`] values.
///
/// The implementation walks raw bytes looking for `\n`, which keeps the loop
/// allocation-free apart from the final per-line strings.
///
/// ```
/// use searchlight::lines::iter_lines;
///
/// let lines = iter_lines("a\nbb\n");
/// assert_eq!(lines.len(), 2);
/// assert_eq!(lines[1].number, 2);
/// assert_eq!(lines[1].text, "bb");
/// ```
pub fn iter_lines(text: &str) -> Vec<Line> {
    let bytes = text.as_bytes();
    let mut out: Vec<Line> = Vec::new();
    let mut start = 0usize;
    let mut number = 1u64;
    let mut cursor = 0usize;

    while cursor < bytes.len() {
        if bytes[cursor] == b'\n' {
            // Trim an optional carriage return so Windows files line up.
            let mut end = cursor;
            if end > start && bytes[end - 1] == b'\r' {
                end -= 1;
            }
            out.push(Line {
                number,
                text: text[start..end].to_string(),
            });
            number += 1;
            start = cursor + 1;
        }
        cursor += 1;
    }

    // Emit the trailing fragment when the file does not end with a newline.
    // An empty input produces no lines at all, matching grep behavior.
    if start < text.len() {
        out.push(Line {
            number,
            text: text[start..].to_string(),
        });
    }

    out
}

/// Split `text` into numbered [`LineSlice`] values carrying byte offsets.
///
/// Semantics are identical to [`iter_lines`] — same numbering, same `\r\n`
/// handling, same trailing-fragment rule — with the addition of `start`/`end`
/// byte offsets into `text`.
pub fn iter_line_slices(text: &str) -> Vec<LineSlice> {
    let bytes = text.as_bytes();
    let mut out: Vec<LineSlice> = Vec::new();
    let mut start = 0usize;
    let mut number = 1u64;
    let mut cursor = 0usize;

    while cursor < bytes.len() {
        if bytes[cursor] == b'\n' {
            let mut end = cursor;
            if end > start && bytes[end - 1] == b'\r' {
                end -= 1;
            }
            out.push(LineSlice {
                number,
                start,
                end,
                text: text[start..end].to_string(),
            });
            number += 1;
            start = cursor + 1;
        }
        cursor += 1;
    }

    if start < text.len() {
        out.push(LineSlice {
            number,
            start,
            end: text.len(),
            text: text[start..].to_string(),
        });
    }

    out
}

/// Count the logical lines in `text` without building [`Line`] values.
///
/// Used by the statistics layer when it only needs a magnitude.
pub fn count_lines(text: &str) -> u64 {
    let bytes = text.as_bytes();
    let mut count = 0u64;
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\n' {
            count += 1;
        }
        cursor += 1;
    }
    if bytes.is_empty() {
        return 0;
    }
    if !bytes.ends_with(b"\n") {
        count += 1;
    }
    count
}

/// Return the byte offset of `char_index` inside `line`.
///
/// The pattern engine works on `char` indices (so highlighting is resilient
/// against multi-byte input), but JSON rendering wants byte offsets for
/// slicing. This helper performs the conversion in O(n) over the line.
///
/// Panics only if `char_index` is greater than the number of characters in
/// the line; callers always pass indices produced by the matcher, which are
/// bounded by the line length.
pub fn byte_index_of_char(line: &str, char_index: usize) -> usize {
    let mut seen = 0usize;
    for (byte_offset, _) in line.char_indices() {
        if seen == char_index {
            return byte_offset;
        }
        seen += 1;
    }
    line.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_unix_lines() {
        let lines = iter_lines("alpha\nbeta\ngamma\n");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].number, 1);
        assert_eq!(lines[0].text, "alpha");
        assert_eq!(lines[2].number, 3);
        assert_eq!(lines[2].text, "gamma");
    }

    #[test]
    fn trailing_newline_does_not_create_empty_line() {
        let lines = iter_lines("one\n");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "one");
    }

    #[test]
    fn missing_trailing_newline_is_preserved() {
        let lines = iter_lines("one\ntwo");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].text, "two");
        assert_eq!(lines[1].number, 2);
    }

    #[test]
    fn windows_line_endings_are_trimmed() {
        let lines = iter_lines("one\r\ntwo\r\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "one");
        assert_eq!(lines[1].text, "two");
    }

    #[test]
    fn empty_input_has_no_lines() {
        assert!(iter_lines("").is_empty());
    }

    #[test]
    fn blank_lines_are_kept() {
        let lines = iter_lines("\n\nx\n");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "");
        assert_eq!(lines[1].text, "");
        assert_eq!(lines[2].text, "x");
    }

    #[test]
    fn multibyte_characters_survive_splitting() {
        let lines = iter_lines("héllo\nwörld✓\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].text, "wörld✓");
        assert_eq!(lines[1].number, 2);
    }

    #[test]
    fn count_lines_matches_iter_lines() {
        let samples = ["", "\n", "a", "a\n", "a\nb\n\n", "\r\nx\r\n"];
        for sample in samples {
            assert_eq!(count_lines(sample), iter_lines(sample).len() as u64);
        }
    }

    #[test]
    fn byte_index_maps_char_positions() {
        let line = "héllo wörld";
        assert_eq!(byte_index_of_char(line, 0), 0);
        // 'é' is two bytes wide, so char index 1 starts at byte 1 and
        // char index 2 starts at byte 3.
        assert_eq!(byte_index_of_char(line, 1), 1);
        assert_eq!(byte_index_of_char(line, 2), 3);
        assert_eq!(byte_index_of_char(line, line.chars().count()), 11);
    }

    #[test]
    fn slices_carry_correct_byte_offsets() {
        //            0123456 7        89012345678
        let text = "alpha\nbeta\ngamma";
        let slices = iter_line_slices(text);
        assert_eq!(slices.len(), 3);

        assert_eq!(slices[0].number, 1);
        assert_eq!(slices[0].start, 0);
        assert_eq!(slices[0].end, 5);
        assert_eq!(slices[0].text, "alpha");

        assert_eq!(slices[1].number, 2);
        assert_eq!(slices[1].start, 6);
        assert_eq!(slices[1].end, 10);
        assert_eq!(slices[1].text, "beta");

        assert_eq!(slices[2].number, 3);
        assert_eq!(slices[2].start, 11);
        assert_eq!(slices[2].end, 16);
        assert_eq!(slices[2].text, "gamma");

        // Slices always describe the original text exactly.
        for slice in &slices {
            assert_eq!(&text[slice.start..slice.end], slice.text);
        }
    }

    #[test]
    fn slices_exclude_crlf_terminators() {
        let text = "one\r\ntwo\r\n";
        let slices = iter_line_slices(text);
        assert_eq!(slices.len(), 2);
        assert_eq!(slices[0].start, 0);
        assert_eq!(slices[0].end, 3, "the \\r must not be part of the line");
        assert_eq!(slices[1].start, 5);
        assert_eq!(slices[1].end, 8);
    }

    #[test]
    fn slices_agree_with_iter_lines() {
        let text = "x\n\nmulti byte ✓ line\nlast";
        let plain = iter_lines(text);
        let slices = iter_line_slices(text);
        assert_eq!(plain.len(), slices.len());
        for (line, slice) in plain.iter().zip(slices.iter()) {
            assert_eq!(line.number, slice.number);
            assert_eq!(line.text, slice.text);
        }
    }

    #[test]
    fn slices_of_empty_input_are_empty() {
        assert!(iter_line_slices("").is_empty());
    }

    #[test]
    fn multibyte_offsets_stay_on_boundaries() {
        // 'é' occupies bytes 1-2, '✓' occupies bytes 9-11.
        let text = "héllo ✓\nnext\n";
        let slices = iter_line_slices(text);
        assert_eq!(slices[0].start, 0);
        assert_eq!(slices[0].end, 8);
        assert_eq!(slices[1].start, 9);
        assert_eq!(&text[slices[1].start..slices[1].end], "next");
    }
}
