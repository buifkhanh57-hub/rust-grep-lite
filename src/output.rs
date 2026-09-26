//! Rendering of search results: plain text, colored text, counts and JSON.
//!
//! The renderer is a thin, stateful writer over any [`std::io::Write`]
//! target. It owns three decisions so `main.rs` stays trivial:
//!
//! * **Mode dispatch** — `--files-with-matches`, `--count` and
//!   `--only-matching` take precedence in that order (mirroring grep), with
//!   `--json` selecting a completely different writer.
//! * **Prefix composition** — file name, byte offset and line number
//!   prefixes are assembled in grep's order (`path:offset:number:`), with
//!   `:` for matching lines and `-` for context lines.
//! * **Context window merging** — when several matches sit close together
//!   their `-B/-A` windows overlap; the renderer prints every line at most
//!   once, in ascending order, and emits the classic `--` separator across
//!   gaps.
//!
//! Highlighting itself lives in [`crate::highlight`]; the renderer simply
//! feeds it the spans recorded by [`crate::search`].

use std::io::{self, Write};
use std::path::Path;

use crate::args::Config;
use crate::highlight::{highlight_line, Palette};
use crate::json::Json;
use crate::lines::byte_index_of_char;
use crate::search::{ContextLine, FileResult, MatchLine};

/// Everything the renderer needs to know besides the results themselves.
#[derive(Debug, Clone)]
pub struct RenderOptions {
    /// Emit ANSI color (already resolved against `--color` and the tty).
    pub color: bool,
    /// Prefix lines with `number:`.
    pub line_numbers: bool,
    /// Prefix lines with `offset:`.
    pub byte_offset: bool,
    /// Print only the matched part of each line, one row per span.
    pub only_matching: bool,
    /// Prefix lines with `path:`.
    pub with_filename: bool,
    /// Print per-file match counts.
    pub count_only: bool,
    /// Print only the names of matching files.
    pub files_with_matches: bool,
    /// Emit JSON Lines instead of human text.
    pub json: bool,
}

impl RenderOptions {
    /// Derive the rendering flags from a parsed [`Config`].
    ///
    /// `color` is the resolved color decision (see `main.rs`) and
    /// `with_filename` the classic grep rule: prefixes appear for directory
    /// scans and multi-root searches, but not when exactly one explicit
    /// file is searched. Mode precedence mirrors grep:
    /// `--files-with-matches` beats `--count`, which beats
    /// `--only-matching`.
    pub fn from_config(config: &Config, color: bool, with_filename: bool) -> RenderOptions {
        let files_with_matches = config.files_with_matches;
        let count_only = config.count_only && !files_with_matches;
        let only_matching = config.only_matching && !files_with_matches && !count_only;
        RenderOptions {
            color,
            line_numbers: config.line_numbers,
            byte_offset: config.byte_offset,
            only_matching,
            with_filename,
            count_only,
            files_with_matches,
            json: config.json,
        }
    }
}

/// Writes rendered results to any [`Write`] sink.
pub struct Renderer<W: Write> {
    out: W,
    options: RenderOptions,
    palette: Palette,
}

impl<W: Write> Renderer<W> {
    /// Build a renderer for `out`; the palette follows `options.color`.
    pub fn new(out: W, options: RenderOptions) -> Renderer<W> {
        let palette = if options.color {
            Palette::classic()
        } else {
            Palette::colorless()
        };
        Renderer { out, options, palette }
    }

    /// The palette in use (for callers that paint standalone strings).
    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    /// Flush the underlying writer.
    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }

    /// Render one scanned file. Binary and failed outcomes never reach the
    /// renderer; `main.rs` reports them on stderr.
    pub fn write_result(&mut self, result: &FileResult) -> io::Result<()> {
        let path = display_path(&result.path);
        if self.options.json {
            return self.write_json(result, &path);
        }
        if self.options.files_with_matches {
            if !result.matches.is_empty() {
                let line = format!("{}\n", self.palette.paint(self.palette.path, &path));
                self.out.write_all(line.as_bytes())?;
            }
            return Ok(());
        }
        if self.options.count_only {
            let line = if self.options.with_filename {
                format!(
                    "{}:{}\n",
                    self.palette.paint(self.palette.path, &path),
                    result.matches.len()
                )
            } else {
                format!("{}\n", result.matches.len())
            };
            self.out.write_all(line.as_bytes())?;
            return Ok(());
        }
        if self.options.only_matching {
            return self.write_only_matching(result, &path);
        }
        self.write_lines(result, &path)
    }

    /// Full-line rendering with context windows and `--` separators.
    fn write_lines(&mut self, result: &FileResult, path: &str) -> io::Result<()> {
        let mut last: Option<u64> = None;
        for entry in &result.matches {
            for context in &entry.before {
                self.write_context_line(path, context, &mut last)?;
            }
            self.write_match_line(path, entry, &mut last)?;
            for context in &entry.after {
                self.write_context_line(path, context, &mut last)?;
            }
        }
        Ok(())
    }

    /// Print one context line, unless the previous window already covered
    /// it; emits the separator when the window jumps.
    fn write_context_line(
        &mut self,
        path: &str,
        line: &ContextLine,
        last: &mut Option<u64>,
    ) -> io::Result<()> {
        if !self.advance_window(line.number, last)? {
            return Ok(());
        }
        let mut out_line = self.line_prefix(path, line.offset, line.number, '-');
        out_line.push_str(&line.text);
        out_line.push('\n');
        self.out.write_all(out_line.as_bytes())?;
        *last = Some(line.number);
        Ok(())
    }

    /// Print one matching line (with highlighting), unless the previous
    /// window already covered it.
    fn write_match_line(
        &mut self,
        path: &str,
        entry: &MatchLine,
        last: &mut Option<u64>,
    ) -> io::Result<()> {
        if !self.advance_window(entry.number, last)? {
            return Ok(());
        }
        let mut out_line = self.line_prefix(path, entry.offset, entry.number, ':');
        out_line.push_str(&highlight_line(&entry.text, &entry.spans, &self.palette));
        out_line.push('\n');
        self.out.write_all(out_line.as_bytes())?;
        *last = Some(entry.number);
        Ok(())
    }

    /// Decide whether `number` still needs printing after the window that
    /// ended at `*last`. Returns `false` when the line is already covered;
    /// emits the `--` separator across gaps.
    fn advance_window(&mut self, number: u64, last: &mut Option<u64>) -> io::Result<bool> {
        match *last {
            Some(prev) if number <= prev => return Ok(false),
            Some(prev) if number > prev + 1 => self.write_separator()?,
            _ => {}
        }
        Ok(true)
    }

    /// The classic `--` separator between non-adjacent groups.
    fn write_separator(&mut self) -> io::Result<()> {
        let line = format!("{}\n", self.palette.paint(self.palette.separator, "--"));
        self.out.write_all(line.as_bytes())
    }

    /// `--only-matching`: one row per span, each carrying its own prefix.
    /// Context flags are ignored in this mode (documented simplification).
    fn write_only_matching(&mut self, result: &FileResult, path: &str) -> io::Result<()> {
        for entry in &result.matches {
            for span in &entry.spans {
                let matched = span_text(&entry.text, *span);
                let offset = entry.offset + byte_index_of_char(&entry.text, span.0);
                let mut line = self.line_prefix(path, offset, entry.number, ':');
                line.push_str(&self.palette.paint(self.palette.matched, &matched));
                line.push('\n');
                self.out.write_all(line.as_bytes())?;
            }
        }
        Ok(())
    }

    /// JSON Lines rendering; mirrors the text modes one-to-one.
    fn write_json(&mut self, result: &FileResult, path: &str) -> io::Result<()> {
        if self.options.files_with_matches {
            if result.matches.is_empty() {
                return Ok(());
            }
            let value = Json::object(vec![("path", Json::string(path))]);
            return self.write_json_line(&value);
        }
        if self.options.count_only {
            let value = Json::object(vec![
                ("path", Json::string(path)),
                ("matches", Json::Uint(result.matches.len() as u64)),
            ]);
            return self.write_json_line(&value);
        }
        for entry in &result.matches {
            let ranges: Vec<Json> = entry
                .spans
                .iter()
                .filter(|(start, end)| end > start)
                .map(|(start, end)| {
                    let byte_start = byte_index_of_char(&entry.text, *start);
                    let byte_end = byte_index_of_char(&entry.text, *end);
                    Json::object(vec![
                        ("start", Json::Uint(byte_start as u64)),
                        ("end", Json::Uint(byte_end as u64)),
                        ("offset", Json::Uint((entry.offset + byte_start) as u64)),
                        ("text", Json::string(span_text(&entry.text, (*start, *end)))),
                    ])
                })
                .collect();
            let value = Json::object(vec![
                ("path", Json::string(path)),
                ("line", Json::Uint(entry.number)),
                ("offset", Json::Uint(entry.offset as u64)),
                ("text", Json::string(entry.text.as_str())),
                ("matches", Json::Array(ranges)),
            ]);
            self.write_json_line(&value)?;
        }
        Ok(())
    }

    /// Write one JSON object plus the line terminator.
    fn write_json_line(&mut self, value: &Json) -> io::Result<()> {
        let mut line = value.to_json_string();
        line.push('\n');
        self.out.write_all(line.as_bytes())
    }

    /// Assemble the `path:offset:number:marker` prefix in grep's order.
    fn line_prefix(&self, path: &str, offset: usize, number: u64, marker: char) -> String {
        let mut prefix = String::new();
        if self.options.with_filename {
            prefix.push_str(&self.palette.paint(self.palette.path, path));
            prefix.push(':');
        }
        if self.options.byte_offset {
            prefix.push_str(&self.palette.paint(self.palette.line_number, &offset.to_string()));
            prefix.push(marker);
        }
        if self.options.line_numbers {
            prefix.push_str(&self.palette.paint(self.palette.line_number, &number.to_string()));
            prefix.push(marker);
        }
        prefix
    }
}

/// Convert a path to its display string (lossy on non-UTF-8 names).
fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Extract the char range `span` from `line` as an owned string.
fn span_text(line: &str, span: (usize, usize)) -> String {
    line.chars().skip(span.0).take(span.1 - span.0).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn opts() -> RenderOptions {
        RenderOptions {
            color: false,
            line_numbers: false,
            byte_offset: false,
            only_matching: false,
            with_filename: false,
            count_only: false,
            files_with_matches: false,
            json: false,
        }
    }

    fn result_of(matches: Vec<MatchLine>) -> FileResult {
        FileResult {
            path: PathBuf::from("sample.txt"),
            matches,
            lines_scanned: 6,
            bytes_scanned: 64,
        }
    }

    fn line(number: u64, text: &str) -> MatchLine {
        MatchLine {
            number,
            offset: 0,
            text: text.to_string(),
            spans: Vec::new(),
            before: Vec::new(),
            after: Vec::new(),
        }
    }

    fn context(number: u64, text: &str) -> ContextLine {
        ContextLine {
            number,
            offset: 0,
            text: text.to_string(),
        }
    }

    fn render(options: RenderOptions, result: &FileResult) -> String {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut renderer = Renderer::new(&mut buf, options);
            renderer.write_result(result).expect("render into buffer");
            renderer.flush().expect("flush buffer");
        }
        String::from_utf8(buf).expect("utf8 output")
    }

    #[test]
    fn matches_render_with_line_numbers_and_separator() {
        let mut options = opts();
        options.line_numbers = true;
        let result = result_of(vec![line(1, "alpha"), line(3, "gamma")]);
        assert_eq!(render(options, &result), "1:alpha\n--\n3:gamma\n");
    }

    #[test]
    fn touching_context_windows_merge_without_separator() {
        let mut options = opts();
        options.line_numbers = true;

        let mut first = line(2, "beta");
        first.before = vec![context(1, "alpha")];
        first.after = vec![context(3, "gamma")];

        let mut second = line(5, "epsilon");
        second.before = vec![context(4, "delta")];
        second.after = vec![context(6, "zeta")];

        let result = result_of(vec![first, second]);
        assert_eq!(
            render(options, &result),
            "1:alpha\n2:beta\n3:gamma\n4:delta\n5:epsilon\n6:zeta\n"
        );
    }

    #[test]
    fn separated_windows_emit_one_separator() {
        let mut options = opts();
        options.line_numbers = true;

        let mut first = line(2, "beta");
        first.before = vec![context(1, "alpha")];
        first.after = vec![context(3, "gamma")];

        let mut second = line(6, "zeta");
        second.before = vec![context(5, "epsilon")];
        second.after = vec![context(7, "eta")];

        let result = result_of(vec![first, second]);
        assert_eq!(
            render(options, &result),
            "1:alpha\n2:beta\n3:gamma\n--\n5:epsilon\n6:zeta\n7:eta\n"
        );
    }

    #[test]
    fn covered_matches_and_context_never_repeat() {
        let mut options = opts();
        options.line_numbers = true;

        let mut first = line(2, "beta");
        first.after = vec![context(3, "gamma"), context(4, "delta")];

        let mut second = line(3, "gamma");
        second.after = vec![context(4, "delta"), context(5, "epsilon")];

        let result = result_of(vec![first, second]);
        assert_eq!(render(options, &result), "2:beta\n3:gamma\n4:delta\n5:epsilon\n");
    }

    #[test]
    fn context_lines_use_a_dash_marker() {
        let mut options = opts();
        options.line_numbers = true;
        let mut entry = line(2, "beta");
        entry.before = vec![context(1, "alpha")];
        let result = result_of(vec![entry]);
        assert_eq!(render(options, &result), "1-alpha\n2:beta\n");
    }

    #[test]
    fn highlighting_applies_only_when_color_is_on() {
        let mut options = opts();
        options.line_numbers = true;
        options.color = true;
        let mut entry = line(1, "hello");
        entry.spans = vec![(0, 2)];
        let result = result_of(vec![entry]);
        let text = render(options, &result);
        assert!(text.contains("\x1b[1;32m1\x1b[0m:"), "colored prefix: {}", text);
        assert!(text.contains("\x1b[1;31mhe\x1b[0mllo"), "colored match: {}", text);
        assert!(crate::highlight::strip_ansi(&text).contains("1:hello"));
    }

    #[test]
    fn colorless_output_has_no_escapes() {
        let mut options = opts();
        options.color = false;
        options.line_numbers = true;
        let mut entry = line(1, "hello");
        entry.spans = vec![(0, 2)];
        let text = render(options, &result_of(vec![entry]));
        assert_eq!(text, "1:hello\n");
    }

    #[test]
    fn count_mode_prints_counts_with_optional_filename() {
        let mut options = opts();
        options.count_only = true;
        let result = result_of(vec![line(1, "a"), line(2, "b")]);
        assert_eq!(render(options.clone(), &result), "2\n");

        options.with_filename = true;
        assert_eq!(render(options, &result), "sample.txt:2\n");
    }

    #[test]
    fn files_with_matches_mode_prints_only_hit_paths() {
        let mut options = opts();
        options.files_with_matches = true;
        options.with_filename = true;

        let hit = result_of(vec![line(1, "a")]);
        assert_eq!(render(options.clone(), &hit), "sample.txt\n");

        let miss = result_of(vec![]);
        assert_eq!(render(options, &miss), "");
    }

    #[test]
    fn only_matching_prints_one_row_per_span() {
        let mut options = opts();
        options.only_matching = true;
        options.with_filename = true;
        options.line_numbers = true;
        options.byte_offset = true;

        let mut entry = line(1, "ab");
        entry.spans = vec![(0, 1), (1, 2)];
        let result = result_of(vec![entry]);
        assert_eq!(
            render(options, &result),
            "sample.txt:0:1:a\nsample.txt:1:1:b\n"
        );
    }

    #[test]
    fn json_mode_emits_spans_with_byte_offsets() {
        let mut options = opts();
        options.json = true;
        options.with_filename = true;
        let mut entry = MatchLine {
            number: 2,
            offset: 6,
            text: "beta".to_string(),
            spans: vec![(0, 2)],
            before: Vec::new(),
            after: Vec::new(),
        };
        entry.before = vec![context(1, "alpha")];
        let result = result_of(vec![entry]);
        assert_eq!(
            render(options, &result),
            "{\"path\":\"sample.txt\",\"line\":2,\"offset\":6,\"text\":\"beta\",\
             \"matches\":[{\"start\":0,\"end\":2,\"offset\":6,\"text\":\"be\"}]}\n"
        );
    }

    #[test]
    fn json_count_and_files_modes() {
        let mut options = opts();
        options.json = true;
        options.count_only = true;
        options.with_filename = true;
        let result = result_of(vec![line(1, "a"), line(2, "b")]);
        assert_eq!(render(options.clone(), &result), "{\"path\":\"sample.txt\",\"matches\":2}\n");

        options.count_only = false;
        options.files_with_matches = true;
        assert_eq!(render(options, &result), "{\"path\":\"sample.txt\"}\n");
    }

    #[test]
    fn json_files_mode_silently_skips_empty_files() {
        let mut options = opts();
        options.json = true;
        options.files_with_matches = true;
        let miss = result_of(vec![]);
        assert_eq!(render(options, &miss), "");
    }

    #[test]
    fn json_spans_skip_zero_width_matches() {
        let mut options = opts();
        options.json = true;
        let mut entry = line(1, "ab");
        entry.spans = vec![(0, 0), (1, 2)];
        let text = render(options, &result_of(vec![entry]));
        assert!(text.contains("\"start\":1,\"end\":2"), "got: {}", text);
        assert!(!text.contains("\"start\":0,\"end\":0"));
    }

    #[test]
    fn from_config_applies_grep_mode_precedence() {
        let mut config = Config::with_defaults();
        config.pattern = "x".to_string();
        config.count_only = true;
        config.files_with_matches = true;
        config.only_matching = true;
        let options = RenderOptions::from_config(&config, false, true);
        assert!(options.files_with_matches);
        assert!(!options.count_only, "-l wins over -c");
        assert!(!options.only_matching, "-l wins over -o");

        let mut config = Config::with_defaults();
        config.pattern = "x".to_string();
        config.count_only = true;
        config.only_matching = true;
        let options = RenderOptions::from_config(&config, false, true);
        assert!(options.count_only);
        assert!(!options.only_matching, "-c wins over -o");
    }

    #[test]
    fn prefix_order_is_path_offset_number() {
        let mut options = opts();
        options.with_filename = true;
        options.byte_offset = true;
        options.line_numbers = true;
        let mut entry = line(7, "text");
        entry.offset = 42;
        let text = render(options, &result_of(vec![entry]));
        assert_eq!(text, "sample.txt:42:7:text\n");
    }

    #[test]
    fn renderer_palette_follows_the_color_flag() {
        let mut buf: Vec<u8> = Vec::new();
        let renderer = Renderer::new(&mut buf, opts());
        assert!(!renderer.palette().is_colored());

        let mut color = opts();
        color.color = true;
        let mut buf: Vec<u8> = Vec::new();
        let renderer = Renderer::new(&mut buf, color);
        assert!(renderer.palette().is_colored());
    }
}
