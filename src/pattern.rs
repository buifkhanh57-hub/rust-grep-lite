//! The searchlight pattern engine.
//!
//! A small, well-specified matcher used both for content patterns and for
//! glob filters. It is a hand-rolled backtracking engine over a parsed tree —
//! no `regex` crate involved. Supported syntax:
//!
//! | Syntax        | Meaning                                          |
//! |---------------|--------------------------------------------------|
//! | `abc`         | literal characters                               |
//! | `.`           | any character except newline                     |
//! | `*` `+` `?`   | zero-or-more, one-or-more, zero-or-one (greedy)  |
//! | `{m}` `{m,}` `{m,n}` | bounded repetition (expanded at parse time) |
//! | `[abc]`       | character class, ranges like `[a-z]` allowed     |
//! | `[^abc]`      | negated character class                          |
//! | `^` `$`       | start-of-line / end-of-line anchors              |
//! | `a|b`         | alternation (the longest overall match wins)     |
//! | `(...)`       | grouping                                         |
//! | `\b`          | word boundary                                    |
//! | `\d \w \s`    | digit / word / whitespace classes (`\D \W \S` negated) |
//! | `\n \t \r \0` | control character escapes                        |
//! | `\.` `\*` ... | escape any metacharacter                         |
//!
//! Matching semantics, precisely:
//!
//! * Input is treated as a sequence of `char` values (not bytes), so matches
//!   are always on UTF-8 character boundaries.
//! * The engine finds the **leftmost** match; among the candidate end
//!   positions of a leftmost match it prefers the **longest** (POSIX-style).
//! * With `case_insensitive` set, ASCII and Unicode simple case folding is
//!   applied to literals and classes. (Folding uses the first character of
//!   the lowercase mapping, a documented simplification.)
//! * With `word` set, the whole pattern is wrapped in `\b ... \b` boundaries,
//!   mirroring `grep -w`.
//! * Glob patterns (see [`Pattern::from_glob`]) are anchored at both ends and
//!   `*`/`?` never match the path separator `/`.

use crate::error::{SearchError, SearchResult};

/// Maximum `(...)` nesting depth accepted by the parser.
pub const MAX_PATTERN_DEPTH: usize = 48;

/// Maximum expansion size for `{m,n}` repetitions.
pub const MAX_REPEAT_EXPANSION: usize = 4096;

/// Fold a character for case-insensitive comparison.
///
/// Uses the first character of the full lowercase mapping. This is exact for
/// ASCII and for the overwhelming majority of Unicode letters; multi-character
/// foldings (e.g. `'İ'`) are approximated by their first character.
pub fn fold_char(ch: char) -> char {
    let mut iter = ch.to_lowercase();
    match iter.next() {
        Some(first) => first,
        None => ch,
    }
}

/// Returns `true` when `ch` is considered a word character (alphanumeric or
/// underscore). Used by `\b` and by the `-w`/`--word` flag.
pub fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// True when there is a word boundary at `pos`: exactly one side of the
/// position is a word character. String edges count as non-word.
fn at_boundary(chars: &[char], pos: usize) -> bool {
    let before = pos > 0 && is_word_char(chars[pos - 1]);
    let after = pos < chars.len() && is_word_char(chars[pos]);
    before != after
}

/// A character class: a (possibly negated) set of inclusive char ranges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharClass {
    /// When `true`, the class matches anything **not** listed.
    pub negated: bool,
    /// Inclusive ranges; a single character is a range `(c, c)`.
    pub ranges: Vec<(char, char)>,
}

impl CharClass {
    /// Test membership. With `case_insensitive`, both the raw character and
    /// its case-folded form are tested.
    pub fn matches(&self, ch: char, case_insensitive: bool) -> bool {
        let direct = self.ranges.iter().any(|&(lo, hi)| ch >= lo && ch <= hi);
        let hit = if case_insensitive {
            let folded = fold_char(ch);
            direct
                || folded != ch && self.ranges.iter().any(|&(lo, hi)| folded >= lo && folded <= hi)
        } else {
            direct
        };
        hit != self.negated
    }

    /// The class used for glob `*` and `?`: any character except `/`.
    pub fn not_slash() -> CharClass {
        CharClass {
            negated: true,
            ranges: vec![('/', '/')],
        }
    }

    /// `\d` — ASCII digits.
    pub fn digit() -> CharClass {
        CharClass {
            negated: false,
            ranges: vec![('0', '9')],
        }
    }

    /// `\w` — word characters.
    pub fn word() -> CharClass {
        CharClass {
            negated: false,
            ranges: vec![('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')],
        }
    }

    /// `\s` — the usual whitespace set.
    pub fn space() -> CharClass {
        CharClass {
            negated: false,
            ranges: vec![(' ', ' '), ('\t', '\t'), ('\r', '\r'), ('\n', '\n')],
        }
    }
}

/// The parsed pattern tree.
#[derive(Debug, Clone)]
pub enum Node {
    /// Matches the empty string.
    Empty,
    /// A single literal character.
    Literal(char),
    /// `.` — any character except `\n`.
    AnyChar,
    /// A character class.
    Class(CharClass),
    /// `^` — matches only at position 0 (lines are pre-trimmed).
    StartAnchor,
    /// `$` — matches only at end of input.
    EndAnchor,
    /// `\b` — word boundary.
    WordBoundary,
    /// Greedy `*` (zero or more, with backtracking).
    Star(Box<Node>),
    /// Greedy `+` (one or more, with backtracking).
    Plus(Box<Node>),
    /// Greedy `?` (zero or one).
    Optional(Box<Node>),
    /// A sequence matched left to right.
    Concat(Vec<Node>),
    /// Alternation; all branches are explored.
    Alternate(Vec<Node>),
}

/// A compiled pattern ready to be matched against lines.
#[derive(Debug, Clone)]
pub struct Pattern {
    root: Node,
    case_insensitive: bool,
    source: String,
}

impl Pattern {
    /// Compile a content pattern.
    ///
    /// `case_insensitive` enables case folding; `word` wraps the pattern in
    /// word boundaries (the `-w` flag). Compilation fails with
    /// [`SearchError::Pattern`] on syntax errors such as unbalanced brackets.
    pub fn new(pattern: &str, case_insensitive: bool, word: bool) -> SearchResult<Pattern> {
        let mut parser = Parser::new(pattern);
        let root = parser.parse_alternate()?;
        parser.finish()?;
        let root = if word {
            Node::Concat(vec![Node::WordBoundary, root, Node::WordBoundary])
        } else {
            root
        };
        Ok(Pattern {
            root,
            case_insensitive,
            source: pattern.to_string(),
        })
    }

    /// Compile a glob pattern (used by file filters and ignore rules).
    ///
    /// Globs are **fully anchored** (`^...$`). `*` and `?` match any run of
    /// characters *except* the path separator `/`, so `src/*.rs` matches
    /// `src/main.rs` but not `src/deep/x.rs`. Consecutive stars collapse to a
    /// single star. Character classes share the regex syntax.
    pub fn from_glob(glob: &str, case_insensitive: bool) -> SearchResult<Pattern> {
        let body = glob_to_node(glob)?;
        let root = Node::Concat(vec![Node::StartAnchor, body, Node::EndAnchor]);
        Ok(Pattern {
            root,
            case_insensitive,
            source: glob.to_string(),
        })
    }

    /// The original source text of the pattern (used for error messages and
    /// by filters to decide whether a glob contains a `/`).
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Whether this pattern was compiled with case folding enabled.
    pub fn is_case_insensitive(&self) -> bool {
        self.case_insensitive
    }

    /// Returns `true` when the pattern matches anywhere in `line`.
    pub fn is_match(&self, line: &str) -> bool {
        self.find(line).is_some()
    }

    /// Find the leftmost match, returning its `(start, end)` char indices
    /// (`end` exclusive). Returns `None` when there is no match.
    pub fn find(&self, line: &str) -> Option<(usize, usize)> {
        let chars: Vec<char> = line.chars().collect();
        self.find_from(&chars, 0)
    }

    /// Find all non-overlapping matches in `line`.
    ///
    /// After an empty match the cursor advances by one character to guarantee
    /// termination; this mirrors the behavior of classic search tools.
    pub fn find_all(&self, line: &str) -> Vec<(usize, usize)> {
        let chars: Vec<char> = line.chars().collect();
        let mut out: Vec<(usize, usize)> = Vec::new();
        let mut cursor = 0usize;
        while cursor <= chars.len() {
            match self.find_from(&chars, cursor) {
                Some((start, end)) => {
                    out.push((start, end));
                    cursor = if end > start { end } else { start + 1 };
                }
                None => break,
            }
        }
        out
    }

    /// Try every start position from `from` onward; the first position that
    /// produces any end set yields a match. Greedy preference is expressed by
    /// taking the maximum reachable end.
    fn find_from(&self, chars: &[char], from: usize) -> Option<(usize, usize)> {
        for start in from..=chars.len() {
            let mut ends = Vec::new();
            self.collect_ends(&self.root, chars, start, &mut ends);
            if let Some(&best) = ends.iter().max() {
                return Some((start, best));
            }
        }
        None
    }

    /// Case-aware character equality.
    fn chars_eq(&self, a: char, b: char) -> bool {
        if self.case_insensitive {
            fold_char(a) == fold_char(b)
        } else {
            a == b
        }
    }

    /// Compute every input position reachable by matching `node` at `pos`.
    ///
    /// This "set of ends" formulation removes the need for continuation
    /// closures: quantifiers collect reachable positions transitively, and
    /// concatenation folds the set through each item in turn. Positions are
    /// bounded by `chars.len()`, and a `seen` bitmap keeps loops finite even
    /// for zero-width inner expressions such as `(a?)*`.
    fn collect_ends(&self, node: &Node, chars: &[char], pos: usize, out: &mut Vec<usize>) {
        match node {
            Node::Empty => out.push(pos),
            Node::Literal(expected) => {
                if let Some(&actual) = chars.get(pos) {
                    if self.chars_eq(*expected, actual) {
                        out.push(pos + 1);
                    }
                }
            }
            Node::AnyChar => {
                if pos < chars.len() && chars[pos] != '\n' {
                    out.push(pos + 1);
                }
            }
            Node::Class(class) => {
                if let Some(&actual) = chars.get(pos) {
                    if class.matches(actual, self.case_insensitive) {
                        out.push(pos + 1);
                    }
                }
            }
            Node::StartAnchor => {
                if pos == 0 {
                    out.push(pos);
                }
            }
            Node::EndAnchor => {
                if pos == chars.len() {
                    out.push(pos);
                }
            }
            Node::WordBoundary => {
                if at_boundary(chars, pos) {
                    out.push(pos);
                }
            }
            Node::Star(inner) => {
                let mut reached = vec![pos];
                let mut seen = vec![false; chars.len() + 1];
                seen[pos] = true;
                let mut frontier = vec![pos];
                while let Some(p) = frontier.pop() {
                    let mut next = Vec::new();
                    self.collect_ends(inner, chars, p, &mut next);
                    for q in next {
                        // Only positions with forward progress extend the
                        // loop; zero-width applications are already covered
                        // by the "zero repetitions" seed.
                        if q > p && q <= chars.len() && !seen[q] {
                            seen[q] = true;
                            reached.push(q);
                            frontier.push(q);
                        }
                    }
                }
                out.extend(reached);
            }
            Node::Plus(inner) => {
                // One mandatory application seeds the set; the rest behave
                // exactly like the star expansion above.
                let mut first = Vec::new();
                self.collect_ends(inner, chars, pos, &mut first);
                let mut reached = Vec::new();
                let mut seen = vec![false; chars.len() + 1];
                for q in first {
                    if q <= chars.len() && !seen[q] {
                        seen[q] = true;
                        reached.push(q);
                    }
                }
                let mut frontier = reached.clone();
                while let Some(p) = frontier.pop() {
                    let mut next = Vec::new();
                    self.collect_ends(inner, chars, p, &mut next);
                    for q in next {
                        if q > p && q <= chars.len() && !seen[q] {
                            seen[q] = true;
                            reached.push(q);
                            frontier.push(q);
                        }
                    }
                }
                out.extend(reached);
            }
            Node::Optional(inner) => {
                out.push(pos);
                let mut next = Vec::new();
                self.collect_ends(inner, chars, pos, &mut next);
                out.extend(next);
            }
            Node::Concat(items) => {
                let mut current: Vec<usize> = vec![pos];
                for item in items {
                    let mut next: Vec<usize> = Vec::new();
                    let mut seen = vec![false; chars.len() + 1];
                    for &p in &current {
                        let mut ends = Vec::new();
                        self.collect_ends(item, chars, p, &mut ends);
                        for q in ends {
                            if q <= chars.len() && !seen[q] {
                                seen[q] = true;
                                next.push(q);
                            }
                        }
                    }
                    if next.is_empty() {
                        return;
                    }
                    current = next;
                }
                out.extend(current);
            }
            Node::Alternate(branches) => {
                for branch in branches {
                    self.collect_ends(branch, chars, pos, out);
                }
            }
        }
    }
}

/// Translate a glob string into an unanchored node tree.
fn glob_to_node(glob: &str) -> SearchResult<Node> {
    let mut parser = Parser::new(glob);
    let mut items: Vec<Node> = Vec::new();
    while parser.pos < parser.src.len() {
        match parser.src[parser.pos] {
            '*' => {
                // Collapse runs of stars; glob semantics have no possessive
                // or lazy variants, so `**` behaves like `*`.
                while parser.pos + 1 < parser.src.len() && parser.src[parser.pos + 1] == '*' {
                    parser.pos += 1;
                }
                parser.pos += 1;
                items.push(Node::Star(Box::new(Node::Class(CharClass::not_slash()))));
            }
            '?' => {
                parser.pos += 1;
                items.push(Node::Class(CharClass::not_slash()));
            }
            '[' => {
                let node = parser.parse_class()?;
                items.push(node);
            }
            plain => {
                parser.pos += 1;
                items.push(Node::Literal(plain));
            }
        }
    }
    Ok(match items.len() {
        0 => Node::Empty,
        1 => items.pop().unwrap_or(Node::Empty),
        _ => Node::Concat(items),
    })
}

/// Recursive-descent parser over the pattern's characters.
struct Parser {
    src: Vec<char>,
    pos: usize,
    depth: usize,
}

impl Parser {
    fn new(pattern: &str) -> Parser {
        Parser {
            src: pattern.chars().collect(),
            pos: 0,
            depth: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.src.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek();
        if ch.is_some() {
            self.pos += 1;
        }
        ch
    }

    /// After a successful top-level parse the cursor must sit at the end;
    /// anything left over is a stray `)` (the concat loop stops at `|` and
    /// `)` only).
    fn finish(&self) -> SearchResult<()> {
        if self.pos < self.src.len() {
            Err(SearchError::pattern(format!(
                "unexpected '{}' at position {}",
                self.src[self.pos], self.pos
            )))
        } else {
            Ok(())
        }
    }

    fn parse_alternate(&mut self) -> SearchResult<Node> {
        let mut branches = vec![self.parse_concat()?];
        while self.peek() == Some('|') {
            self.pos += 1;
            branches.push(self.parse_concat()?);
        }
        Ok(match branches.len() {
            1 => branches.pop().unwrap_or(Node::Empty),
            _ => Node::Alternate(branches),
        })
    }

    fn parse_concat(&mut self) -> SearchResult<Node> {
        let mut items: Vec<Node> = Vec::new();
        loop {
            match self.peek() {
                None | Some('|') | Some(')') => break,
                _ => {}
            }
            let atom = self.parse_atom()?;
            let atom = self.parse_quantifiers(atom)?;
            items.push(atom);
        }
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.pop().unwrap_or(Node::Empty),
            _ => Node::Concat(items),
        })
    }

    /// Apply any run of postfix quantifiers to `atom`. A `{` that does not
    /// introduce a valid repetition is left in the stream and becomes a
    /// literal on the next atom cycle (grep-like leniency).
    fn parse_quantifiers(&mut self, mut atom: Node) -> SearchResult<Node> {
        loop {
            match self.peek() {
                Some('*') => {
                    self.pos += 1;
                    atom = Node::Star(Box::new(atom));
                }
                Some('+') => {
                    self.pos += 1;
                    atom = Node::Plus(Box::new(atom));
                }
                Some('?') => {
                    self.pos += 1;
                    atom = Node::Optional(Box::new(atom));
                }
                Some('{') => match self.parse_repetition()? {
                    Some((min, max)) => atom = expand_repetition(atom, min, max),
                    None => break,
                },
                _ => break,
            }
        }
        Ok(atom)
    }

    /// Parse `{m}`, `{m,}` or `{m,n}`. Returns `Ok(None)` (without consuming
    /// input) when the braces do not form a repetition.
    fn parse_repetition(&mut self) -> SearchResult<Option<(usize, Option<usize>)>> {
        let start = self.pos;
        self.pos += 1; // consume '{'
        let min = match self.parse_digits() {
            Some(v) => v,
            None => {
                self.pos = start;
                return Ok(None);
            }
        };
        let max = if self.peek() == Some(',') {
            self.pos += 1;
            self.parse_digits()
        } else {
            Some(min)
        };
        if self.peek() != Some('}') {
            self.pos = start;
            return Ok(None);
        }
        self.pos += 1;
        if min > MAX_REPEAT_EXPANSION {
            return Err(SearchError::pattern(format!(
                "repetition count {} too large (max {})",
                min, MAX_REPEAT_EXPANSION
            )));
        }
        if let Some(mx) = max {
            if mx > MAX_REPEAT_EXPANSION {
                return Err(SearchError::pattern(format!(
                    "repetition count {} too large (max {})",
                    mx, MAX_REPEAT_EXPANSION
                )));
            }
            if mx < min {
                return Err(SearchError::pattern(format!(
                    "repetition maximum {} smaller than minimum {}",
                    mx, min
                )));
            }
        }
        Ok(Some((min, max)))
    }

    fn parse_digits(&mut self) -> Option<usize> {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
        }
        if start == self.pos {
            return None;
        }
        let text: String = self.src[start..self.pos].iter().collect();
        text.parse::<usize>().ok()
    }

    fn parse_atom(&mut self) -> SearchResult<Node> {
        let ch = match self.bump() {
            Some(c) => c,
            None => return Ok(Node::Empty),
        };
        match ch {
            '(' => {
                self.depth += 1;
                if self.depth > MAX_PATTERN_DEPTH {
                    return Err(SearchError::pattern(
                        "pattern nesting too deep (possible catastrophic form)",
                    ));
                }
                let inner = self.parse_alternate()?;
                if self.peek() != Some(')') {
                    return Err(SearchError::pattern("missing closing ')'"));
                }
                self.pos += 1;
                self.depth -= 1;
                Ok(inner)
            }
            '[' => self.parse_class(),
            '.' => Ok(Node::AnyChar),
            '^' => Ok(Node::StartAnchor),
            '$' => Ok(Node::EndAnchor),
            '\\' => self.parse_escape(),
            plain => Ok(Node::Literal(plain)),
        }
    }

    fn parse_escape(&mut self) -> SearchResult<Node> {
        let esc = match self.bump() {
            Some(c) => c,
            None => return Err(SearchError::pattern("dangling escape at end of pattern")),
        };
        Ok(match esc {
            'n' => Node::Literal('\n'),
            't' => Node::Literal('\t'),
            'r' => Node::Literal('\r'),
            '0' => Node::Literal('\0'),
            'b' => Node::WordBoundary,
            'd' => Node::Class(CharClass::digit()),
            'w' => Node::Class(CharClass::word()),
            's' => Node::Class(CharClass::space()),
            'D' => Node::Class(CharClass {
                negated: true,
                ranges: vec![('0', '9')],
            }),
            'W' => Node::Class(CharClass {
                negated: true,
                ranges: vec![('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')],
            }),
            'S' => Node::Class(CharClass {
                negated: true,
                ranges: vec![(' ', ' '), ('\t', '\t'), ('\r', '\r'), ('\n', '\n')],
            }),
            // Any other escaped character is that character literally; this
            // covers `\.` `\*` `\+` `\?` `\(` `\)` `\[` `\]` `\{` `\}` `\|`
            // `\^` `\$` and `\\` in one arm.
            other => Node::Literal(other),
        })
    }

    /// Parse a `[...]` class; the leading `[` must be the current character.
    fn parse_class(&mut self) -> SearchResult<Node> {
        self.pos += 1; // consume '['
        let negated = if self.peek() == Some('^') {
            self.pos += 1;
            true
        } else {
            false
        };
        let mut ranges: Vec<(char, char)> = Vec::new();
        let mut first = true;
        loop {
            let ch = match self.peek() {
                Some(c) => c,
                None => return Err(SearchError::pattern("unclosed character class")),
            };
            if ch == ']' && !first {
                self.pos += 1;
                break;
            }
            first = false;
            if ch == '\\' {
                self.pos += 1;
                let esc = match self.bump() {
                    Some(c) => c,
                    None => return Err(SearchError::pattern("dangling escape in class")),
                };
                match esc {
                    'n' => ranges.push(('\n', '\n')),
                    't' => ranges.push(('\t', '\t')),
                    'r' => ranges.push(('\r', '\r')),
                    'd' => ranges.push(('0', '9')),
                    'w' => ranges.extend([('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')]),
                    's' => ranges.extend([(' ', ' '), ('\t', '\t'), ('\r', '\r'), ('\n', '\n')]),
                    other => ranges.push((other, other)),
                }
                continue;
            }
            // Range form `a-z`, but only when '-' is not the final member
            // (so `[a-]` treats '-' as a literal, like POSIX tools).
            if self.pos + 2 < self.src.len()
                && self.src[self.pos + 1] == '-'
                && self.src[self.pos + 2] != ']'
            {
                let lo = ch;
                let hi = self.src[self.pos + 2];
                if hi < lo {
                    return Err(SearchError::pattern(format!(
                        "invalid character range '{}-{}'",
                        lo, hi
                    )));
                }
                ranges.push((lo, hi));
                self.pos += 3;
                continue;
            }
            ranges.push((ch, ch));
            self.pos += 1;
        }
        Ok(Node::Class(CharClass { negated, ranges }))
    }
}

/// Expand `{m,n}` repetitions into plain concatenation at parse time:
/// `a{2,4}` becomes `a a a? a?` and `a{2,}` becomes `a a a*`.
fn expand_repetition(atom: Node, min: usize, max: Option<usize>) -> Node {
    let mut items: Vec<Node> = Vec::new();
    for _ in 0..min {
        items.push(atom.clone());
    }
    match max {
        None => items.push(Node::Star(Box::new(atom))),
        Some(upper) => {
            for _ in 0..(upper - min) {
                items.push(Node::Optional(Box::new(atom.clone())));
            }
        }
    }
    if items.is_empty() {
        // Only reachable for {0,0}, which matches the empty string.
        return Node::Empty;
    }
    if items.len() == 1 {
        items.pop().unwrap_or(Node::Empty)
    } else {
        Node::Concat(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(pattern: &str, line: &str) -> Option<(usize, usize)> {
        Pattern::new(pattern, false, false)
            .expect("test pattern should compile")
            .find(line)
    }

    #[test]
    fn plain_literal_matches() {
        assert_eq!(find("abc", "xxabcyy"), Some((2, 5)));
        assert_eq!(find("abc", "abcdef"), Some((0, 3)));
        assert_eq!(find("abc", "abx"), None);
    }

    #[test]
    fn leftmost_longest_match_is_returned() {
        // The star prefers the longest run at the leftmost start.
        assert_eq!(find("a*", "aaa"), Some((0, 3)));
        // Alternation prefers the longest overall match (documented POSIX
        // style, unlike leftmost-first engines).
        assert_eq!(find("a|ab", "ab"), Some((0, 2)));
    }

    #[test]
    fn case_insensitive_folding() {
        let ci = Pattern::new("hello", true, false).unwrap();
        assert!(ci.is_match("say HeLLo!"));
        let cs = Pattern::new("hello", false, false).unwrap();
        assert!(!cs.is_match("say HeLLo!"));
        // Classes also fold.
        let class = Pattern::new("[a-z]+", true, false).unwrap();
        assert_eq!(class.find("ABC"), Some((0, 3)));
    }

    #[test]
    fn dot_matches_anything_but_newline() {
        assert_eq!(find("a.c", "abc"), Some((0, 3)));
        assert_eq!(find("a.c", "a\nc"), None);
    }

    #[test]
    fn star_plus_optional_quantifiers() {
        assert_eq!(find("a*b", "aaab"), Some((0, 4)));
        assert_eq!(find("a*b", "b"), Some((0, 1)));
        assert_eq!(find("a+b", "b"), None);
        assert_eq!(find("a+b", "aab"), Some((0, 3)));
        assert_eq!(find("colou?r", "color"), Some((0, 5)));
        assert_eq!(find("colou?r", "colour"), Some((0, 6)));
    }

    #[test]
    fn character_classes() {
        assert_eq!(find("[a-c]x", "bx"), Some((0, 2)));
        assert_eq!(find("[a-c]x", "dx"), None);
        assert_eq!(find("[^a-c]x", "ax"), None);
        assert_eq!(find("[a-]b", "a-b"), Some((1, 3)));
        assert_eq!(find("[]]x", "]x"), Some((0, 2)));
    }

    #[test]
    fn anchors_pin_the_match() {
        assert_eq!(find("^ab$", "ab"), Some((0, 2)));
        assert_eq!(find("^ab$", "abc"), None);
        assert_eq!(find("^b", "ab"), None);
        assert_eq!(find("c$", "abc"), Some((2, 3)));
        assert_eq!(find("^$", ""), Some((0, 0)));
        assert_eq!(find("^$", "a"), None);
    }

    #[test]
    fn alternation_and_groups() {
        assert_eq!(find("cat|dog", "hotdog"), Some((3, 6)));
        assert_eq!(find("(ab)+", "ababab"), Some((0, 6)));
        assert_eq!(find("(a|b)*c", "abbac"), Some((0, 5)));
        assert_eq!(find("gr(e|a)y", "grey"), Some((0, 4)));
    }

    #[test]
    fn bounded_repetition_expands_correctly() {
        assert_eq!(find("a{2}", "a"), None);
        assert_eq!(find("a{2}", "aa"), Some((0, 2)));
        assert_eq!(find("a{2,3}", "aaaa"), Some((0, 3)));
        assert_eq!(find("a{2,}", "a"), None);
        assert_eq!(find("a{2,}", "aaaaa"), Some((0, 5)));
        // A malformed brace group is a literal, matching grep leniency.
        assert_eq!(find("a{,2}", "a{,2}"), Some((0, 5)));
    }

    #[test]
    fn escape_sequences() {
        assert_eq!(find(r"\d+", "abc123"), Some((3, 6)));
        assert_eq!(find(r"\w+", "  hello_world!"), Some((2, 13)));
        assert_eq!(find(r"\s", "a b"), Some((1, 2)));
        assert_eq!(find(r"\D+", "123abc456"), Some((3, 6)));
        assert_eq!(find(r"a\.b", "a.b"), Some((0, 3)));
        assert_eq!(find(r"a\.b", "axb"), None);
        assert_eq!(find(r"a\\b", "a\\b"), Some((0, 3)));
    }

    #[test]
    fn word_boundaries_via_flag() {
        let word = Pattern::new("cat", false, true).unwrap();
        assert!(word.is_match("a cat naps"));
        assert!(!word.is_match("category"));
        assert!(!word.is_match("tomcat"));
        assert!(word.is_match("cat"));
        // Explicit \b inside the pattern works too.
        let explicit = Pattern::new(r"\bcat\b", false, false).unwrap();
        assert!(explicit.is_match("a cat naps"));
        assert!(!explicit.is_match("concatenate"));
    }

    #[test]
    fn globs_are_fully_anchored() {
        let rs = Pattern::from_glob("*.rs", false).unwrap();
        assert!(rs.is_match("main.rs"));
        assert!(rs.is_match(".rs"));
        assert!(!rs.is_match("main.rs.txt"));
        assert!(!rs.is_match("src/main.rs"), "unanchored '*' must not match basename-only globs against full paths");

        let nested = Pattern::from_glob("src/*.rs", false).unwrap();
        assert!(nested.is_match("src/main.rs"));
        assert!(!nested.is_match("src/sub/main.rs"), "'*' must not cross '/'");
        assert!(!nested.is_match("other/main.rs"));

        let q = Pattern::from_glob("fo?", false).unwrap();
        assert!(q.is_match("foo"));
        assert!(!q.is_match("fo"));
        assert!(!q.is_match("fooo"));
    }

    #[test]
    fn find_all_is_non_overlapping() {
        let pat = Pattern::new("aa", false, false).unwrap();
        assert_eq!(pat.find_all("aaaa"), vec![(0, 2), (2, 4)]);
        assert_eq!(pat.find_all("aaa"), vec![(0, 2)]);
        let single = Pattern::new("a", false, false).unwrap();
        assert_eq!(single.find_all("aba"), vec![(0, 1), (2, 3)]);
        // Zero-width matches advance by one to guarantee termination.
        let empty = Pattern::new("", false, false).unwrap();
        assert_eq!(empty.find_all("ab").len(), 3);
    }

    #[test]
    fn multibyte_lines_use_char_indices() {
        let pat = Pattern::new("é", false, false).unwrap();
        // "héllo" — char indices, not byte offsets.
        assert_eq!(pat.find("héllo"), Some((1, 2)));
    }

    #[test]
    fn invalid_patterns_are_rejected() {
        assert!(Pattern::new("[abc", false, false).is_err());
        assert!(Pattern::new("(ab", false, false).is_err());
        assert!(Pattern::new("ab)", false, false).is_err());
        assert!(Pattern::new("[z-a]", false, false).is_err());
        assert!(Pattern::new(r"\", false, false).is_err());
        assert!(Pattern::new("a{4,2}", false, false).is_err());
        assert!(Pattern::new("((((((((((((((((((((((((((((((((((((((((((((((((a))))))))))))))))))))))))))))))))))))))))))))))))", false, false).is_err());
    }

    #[test]
    fn empty_pattern_matches_every_line() {
        let pat = Pattern::new("", false, false).unwrap();
        assert!(pat.is_match("anything"));
        assert!(pat.is_match(""));
    }

    #[test]
    fn class_case_folding_includes_unicode() {
        let pat = Pattern::new("é", true, false).unwrap();
        assert!(pat.is_match("E"));
    }
}
