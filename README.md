# searchlight

> Fast, parallel content search for the terminal — a grep-style tool with a
> hand-rolled pattern engine and **zero dependencies** beyond the Rust
> standard library.

![Version](https://img.shields.io/badge/version-1.0.0-blue)
![Rust](https://img.shields.io/badge/rust-1.70%2B-orange)
![License](https://img.shields.io/badge/license-MIT-green)
![Dependencies](https://img.shields.io/badge/dependencies-0-brightgreen)
![Tests](https://img.shields.io/badge/tests-100%2B%20unit%20assertions-success)

---

## Overview

`searchlight` searches the *contents* of files for a pattern, exactly like
`grep -rn` — but with a fully parallel implementation built only on
`std`: a work-queue directory walker, a scoped thread pool for the content
scan, lock-free statistics, and a miniature regex engine written from
scratch. It exists because a search tool should be able to show you every
line it matches, colorize them, count them, and stream them as JSON —
without pulling in a single external crate.

Everything about the tool is deterministic: file results are always
rendered in sorted path order, no matter which worker thread found them
first, so output is diffable and script-friendly.

```console
$ searchlight -n "TODO" src/
src/config.rs:14:// TODO: make the timeout configurable
src/config.rs:41:    // TODO(bug #12): validate ranges here
src/main.rs:203:        // TODO: remove after migration
```

## Features

- **Parallel by default** — directory walking and content scanning both run
  on a pool of worker threads (`-j` / `--threads`, default: CPU count).
- **Hand-rolled pattern engine** — literals, `.`, character classes,
  greedy quantifiers, `{m,n}` repetition, alternation, groups, anchors,
  `\b` word boundaries and `\d \w \s` classes; no `regex` crate.
- **Classic grep surface** — `-i -w -v -m`, `-n -b -o -c -l -q -s`,
  `-A/-B/-C` context with proper `--` separators and window merging.
- **Smart filtering** — repeatable `--glob` / `--exclude-glob`, size
  windows (`--min-size` / `--max-size`), `--max-depth`, hidden files,
  `.gitignore`/`.ignore` support with negation, and a built-in skip list
  for `node_modules`, `target`, `.git`, ...
- **Binary-aware** — NUL-byte sniffing and UTF-8 validation skip binary
  files automatically instead of spewing garbage.
- **Output modes for every consumer** — human text with optional ANSI
  color, per-file counts, file-name lists, match-only rows, or one JSON
  object per match (`--json`, JSON-Lines).
- **Unicode-correct** — matches operate on `char` boundaries; multi-byte
  text is highlighted and byte-offset-annotated correctly.
- **Observability** — `--stats` prints counters (files found/searched/
  matched/binary, lines and bytes scanned, throughput) plus a phase
  timeline.
- **Safe exit codes** — `0` matches found, `1` no matches, `2` error;
  broken pipes (`... | head`) are treated as success.

## Requirements

- Rust **1.70 or newer** (2021 edition). That is the only requirement:
  searchlight has no third-party dependencies, so `cargo build` works fully
  offline.
- Any OS supported by the Rust standard library (Linux, macOS, Windows).
  No runtime dependencies, no config files, nothing to install besides the
  binary.

## Installation

### With cargo (from source)

```console
$ git clone https://github.com/buifkhanh57-hub/searchlight
$ cd searchlight
$ cargo install --path .
$ searchlight --version
searchlight 1.0.0
```

`cargo install` places the binary in `~/.cargo/bin` (make sure that
directory is on your `PATH`).

### Build without installing

```console
$ cargo build --release
$ ./target/release/searchlight -n "fn main" .
```

The release profile enables `lto = true`, `codegen-units = 1` and symbol
stripping, so the resulting binary is small and fast.

## Quick Start

```console
$ searchlight "SearchOptions" src/          # find a type across a tree
src/search.rs:64:pub struct SearchOptions {
src/search.rs:120:pub fn search_text(path: &Path, text: &str, pattern: &Pattern,
src/search.rs:124:                    options: &SearchOptions) -> FileResult {

$ searchlight -i "error" README.md          # case-insensitive, one file
$ searchlight -n -C2 "panic" src/walk.rs    # two lines of context
$ searchlight -c "use " src/                # per-file match counts
src/args.rs:2
src/error.rs:6
src/walk.rs:9

$ searchlight --json "needle" docs/ | jq .  # machine-readable output
$ searchlight -l --glob "*.rs" "tests" .    # only file names, only Rust
```

## Usage

```
searchlight [OPTIONS] PATTERN [PATH]...
```

`PATTERN` uses the mini-regex syntax documented below. `PATH` may be one
or more files or directories; with no `PATH`, the current directory is
searched.

### Flag reference

| Flag | Description |
|------|-------------|
| `-i`, `--ignore-case` | Fold case (ASCII + Unicode simple folding) while matching |
| `-w`, `--word-regexp` | Wrap the pattern in `\b` word boundaries |
| `-v`, `--invert-match` | Select lines that do **not** match (no highlighting) |
| `-m`, `--max-count N` | Stop after N matching lines per file |
| `-n`, `--line-number` | Prefix each match with its 1-based line number |
| `-b`, `--byte-offset` | Prefix each match with its byte offset |
| `-o`, `--only-matching` | Print only the matched part, one row per match |
| `-c`, `--count` | Print per-file match counts instead of lines |
| `-l`, `--files-with-matches` | Print only the names of matching files |
| `-B N` | N lines of context before each match |
| `-A N` | N lines of context after each match |
| `-C N` | N lines of context before **and** after each match |
| `--color WHEN` | `auto` (default), `always` or `never` |
| `--json` | Emit JSON-Lines (one object per match / count / file) |
| `-q`, `--quiet` | Print nothing; exit codes carry the result |
| `-s`, `--no-messages` | Suppress per-file error messages on stderr |
| `--glob PATTERN` | Search only files matching PATTERN (repeatable) |
| `--exclude-glob PATTERN` | Skip files matching PATTERN (repeatable) |
| `--max-depth N` | Limit recursion depth (1 = top level only) |
| `--min-size S` | Skip files smaller than S (e.g. `10k`, `1.5m`) |
| `--max-size S` | Skip files larger than S |
| `--hidden` | Include hidden files and directories |
| `--no-ignore` | Do not honor `.gitignore` / `.ignore` files |
| `--follow` | Follow symbolic links (with a hard depth ceiling) |
| `-j`, `--threads N` | Worker threads (default: available CPUs, max 1024) |
| `--stats` | Print counters and a phase timeline to stderr |
| `-h`, `--help` | Print the help text |
| `-V`, `--version` | Print version information |

Short boolean flags cluster: `-inv`, `-wvnbo` all work. Value flags accept
both attached (`-C3`, `-j8`, `-m100`) and separated (`-C 3`) forms. Use
`--` to search for a pattern that starts with `-`.

### Output modes and precedence

Combining modes follows grep's precedence: `--files-with-matches` wins
over `--count`, which wins over `--only-matching`. `--json` replaces the
text renderer entirely but keeps the same mode selection (so
`--json -c` emits one `{"path":...,"matches":N}` object per file).

### Exit codes

| Code | Meaning |
|------|---------|
| `0` | At least one match was found (or the pipe closed early) |
| `1` | The search completed but found nothing |
| `2` | An error occurred: bad arguments, unreadable path, pattern error |

### Examples with realistic output

Context windows merge when matches are close; separated groups get the
classic `--` divider:

```console
$ searchlight -n -C1 "beta" sample.txt
1-alpha
2:beta
3-gamma
--
5-epsilon
6:zeta
7-eta
```

Counts and file lists compose with globs and size windows:

```console
$ searchlight -c --glob "*.rs" --min-size 100 "unwrap" src/
src/args.rs:4
src/pattern.rs:12
src/search.rs:9
```

Only-matching rows carry per-match byte offsets, like `grep -ob`:

```console
$ searchlight -nob --glob "*.md" "\bstd\b" .
README.md:210:1:std
docs/internals.md:88:1:std
docs/internals.md:88:40:std
```

JSON output is one object per matching line, with byte ranges and the
matched text per span:

```console
$ searchlight --json "needle" hay/ | head -n 2
{"path":"hay/a.txt","line":2,"offset":12,"text":"the needle here","matches":[{"start":4,"end":10,"offset":16,"text":"needle"}]}
{"path":"hay/b.txt","line":7,"offset":204,"text":"needle two","matches":[{"start":0,"end":6,"offset":204,"text":"needle"}]}
```

Statistics for a mid-sized tree:

```console
$ searchlight --stats "TODO" ~/projects/webapp/
...matches...
searchlight statistics
  files found          1,204
  files searched       1,200
  files with matches      17
  binary files skipped     4
  matching lines          42
  lines scanned       89,551
  bytes scanned       3.1 MiB
  walk time            4.1 ms
  search time         38.9 ms
  search throughput   79.7 MiB /s
total wall time    45.2 ms
size limits        any size
```

## Pattern syntax reference

| Syntax | Meaning |
|--------|---------|
| `abc` | Literal characters |
| `.` | Any character except newline |
| `*` `+` `?` | Greedy zero-or-more, one-or-more, zero-or-one |
| `{m}` `{m,}` `{m,n}` | Bounded repetition (expanded at parse time, max 4096) |
| `[abc]` `[a-z]` | Character class with ranges |
| `[^abc]` | Negated character class |
| `^` `$` | Start-of-line / end-of-line anchors |
| `a\|b` | Alternation; the longest overall match wins (POSIX style) |
| `(...)` | Grouping (nesting depth limit 48) |
| `\b` | Word boundary (also applied whole-pattern by `-w`) |
| `\d` `\w` `\s` | Digit / word / whitespace classes |
| `\D` `\W` `\S` | Negated versions of the classes above |
| `\n` `\t` `\r` `\0` | Control character escapes |
| `\.` `\*` `\\` ... | Escape any metacharacter |

Notes:

- Matching is **leftmost-longest** per start position, like POSIX `awk`,
  so `a\|ab` against `ab` matches the full `ab`.
- Case folding (`-i`) applies to literals *and* classes, using Unicode
  simple folding (first character of the lowercase mapping).
- Globs (`--glob`, ignore files) are anchored on both ends; `*` and `?`
  never cross `/`. A glob containing `/` matches paths relative to the
  search root, otherwise it matches the file name anywhere.
- Ignore files support comments (`#`), negation (`!`), directory-only
  rules (`build/`) and anchoring (a leading or inner `/`). The **last**
  matching rule wins, exactly like git.

## Project Structure

```
searchlight/
├── Cargo.toml            # package manifest: lib + bin targets, release profile
├── LICENSE               # MIT license
├── README.md             # this file
├── .gitignore            # ignores /target, editor droppings
└── src/
    ├── lib.rs            # 126 lines — crate root: module graph, VERSION/TAGLINE, doc examples
    ├── main.rs           # 271 lines — binary wiring: parse → walk → search → render → exit codes
    ├── args.rs           # 795 lines — table-driven CLI parser, Config + validation + tests
    ├── pattern.rs        # 921 lines — mini-regex engine: parser, matcher, globs + tests
    ├── filters.rs        # 479 lines — glob rules, size bounds, gitignore sets, binary sniff
    ├── walk.rs           # 678 lines — parallel walker: monitor queue, scoped workers, tests
    ├── search.rs         # 488 lines — per-file scanning, match spans, context, parallel driver
    ├── output.rs         # 600 lines — text/color/count/files/JSON renderers, window merging
    ├── highlight.rs      # 278 lines — ANSI palette, span merging, unicode-safe painting
    ├── lines.rs          # 325 lines — line splitting with numbering and byte offsets
    ├── json.rs           # 191 lines — minimal JSON model + compact serializer
    ├── stats.rs          # 361 lines — atomic counters, snapshots, human formatting
    ├── sizes.rs          # 295 lines — human size parsing/formatting (binary + SI units)
    ├── timer.rs          # 257 lines — phase stopwatch and aligned timeline report
    └── error.rs          # 257 lines — SearchError enum, exit codes, io message mapping
```

Every module carries its own `#[cfg(test)]` suite — the tests ship with
the code they verify.

## Architecture

The pipeline has five stages; each is a separate module with a pure,
unit-testable core.

### 1. Walking the tree (`walk.rs`)

- User-supplied roots are classified first: plain files bypass the queue
  (they are still glob/size-filtered); directories become seed jobs.
- A fixed worker pool shares one **work queue** — a
  `Mutex<VecDeque<DirJob>>` guarded by a `Condvar`. Each job carries its
  depth and the index of the root it belongs to.
- Termination uses the classic **monitor pattern**: a `pending` counter is
  incremented *before* a job is pushed and decremented after the job is
  fully processed; workers sleep on the condition variable while the queue
  is empty and `pending > 0`, and the last finishing worker wakes everyone
  with `notify_all`. No lost wakeups, no busy waiting.
- Every worker sends accepted files over one `mpsc` channel; the
  coordinating thread collects results after `thread::scope` joins all
  workers and sorts them, so output order is deterministic.

### 2. Searching file contents (`search.rs`)

- The pure core `search_text` splits a string into numbered lines with
  byte offsets (`lines.rs`) and runs the pattern engine per line,
  recording match spans plus `-B/-A` context windows.
- `search_files` distributes files across scoped threads with a single
  shared **atomic cursor** (`AtomicUsize`): each file is one unit of work,
  so no queue is needed. Outcomes flow back through an `mpsc` channel and
  are sorted by path afterwards.
- Binary detection happens before UTF-8 conversion: NUL bytes in the first
  8 KiB, or invalid UTF-8, mark the file as binary and it is skipped.

### 3. Rendering (`output.rs` + `highlight.rs`)

- The renderer merges overlapping context windows: every line prints at
  most once, in ascending order, with `--` separators across gaps — the
  same behavior as `grep -C`.
- Highlights are painted by inserting SGR sequences at char-index spans;
  adjacent spans are merged first so resets never nest. `--color auto`
  colors only when stdout is a terminal and `NO_COLOR` is unset.

### 4. Diagnostics (`stats.rs`, `timer.rs`, `error.rs`)

- All counters are `AtomicU64`; the coordinating thread snapshots them
  once after the pipeline finishes.
- A tiny stopwatch records the walk and search phases; `--stats` prints
  the timeline plus a total.
- One error enum maps every failure onto exit code `2`, with stable,
  lowercase messages for scripting.

## Performance notes

- **Parallelism matches the workload**: walking is I/O bound (capped at 64
  workers internally) while scanning is CPU bound; `-j` controls both
  pools. The default is the machine's available parallelism.
- **No regex backtracking blowups**: quantifiers are compiled to
  position-set operations with a `seen` bitmap, so catastrophic patterns
  like `(a?)*` terminate deterministically; nesting and repetition sizes
  are bounded at parse time.
- **Memory is proportional to what you asked for**: files are read fully
  (they are small after size filtering); `-m` stops scanning early;
  context windows are materialized per match, not per file.
- **Zero dependencies** means zero dependency-induced latency: no
  proc-macro expansion, no feature unification, fast cold builds.
- Typical throughput on a warm cache is tens to hundreds of MB/s per
  core; `--stats` reports the exact number for your machine.

## Testing

All tests are inline `#[cfg(test)]` suites next to the code:

```console
$ cargo test
running 100+ tests
test args::tests::boolean_short_flags_cluster ... ok
test pattern::tests::word_boundaries_via_flag ... ok
test search::tests::parallel_search_is_deterministic_and_counted ... ok
test output::tests::separated_windows_emit_one_separator ... ok
...

test result: ok. 100+ passed; 0 failed
```

The suites cover, among other things:

- argument parsing (clustering, attached values, `--`, error cases),
- the pattern engine (quantifiers, classes, anchors, `\b`, globs,
  multibyte input, malformed patterns),
- walking (depth limits, hidden files, gitignore semantics, symlink
  handling, multi-threaded determinism),
- searching (spans, context clipping, `-m` cutoff, invert, binary
  skipping, parallel/sequential equivalence),
- rendering (window merging, separators, prefixes, coloring, JSON shape),
- formatting utilities (sizes, durations, timelines).

Doc-tests in `lib.rs` and `lines.rs` keep the public examples compiling.

## FAQ

**Why another grep?**
searchlight is a compact, dependency-free reference implementation: one
`cargo build` with no network access, readable modules, and behavior
documented precisely enough to teach how a parallel search tool works.

**Why not use the `regex` crate?**
The whole point is zero dependencies. The built-in engine covers the
practical grep subset (classes, quantifiers, alternation, anchors, `\b`)
with deterministic, bounded behavior.

**How do I search for a pattern that starts with a dash?**
Use `--`: `searchlight -- "-Weird-Flag" docs/`.

**Does `-o` respect `-A/-B/-C`?**
No — `--only-matching` prints one row per match and ignores context
flags, a documented simplification. JSON output always includes the full
line plus all spans, which usually replaces `-o` for tooling.

**Why is my binary file skipped?**
Files containing a NUL byte in their first 8 KiB, or invalid UTF-8, are
skipped (and counted in `--stats`). There is deliberately no `-a` flag;
use a hex-oriented tool for those.

**Can I make output deterministic across machines?**
Yes — results are always sorted by path. Worker count and filesystem
order never affect the rendered output.

**What does `--no-ignore` actually disable?**
The `.gitignore`/`.ignore` layer. The built-in skip list (`.git`,
`node_modules`, `target`, `__pycache__`, ...) stays active — pass explicit
paths if you truly need to search inside those.

**How are exit codes chosen?**
grep-compatible: `0` when at least one line was selected, `1` when none
were, `2` for errors. Piping into `head` (broken pipe) counts as success.

## Roadmap

- [ ] `-r`-style explicit replacement of matches in output (`--replace`).
- [ ] Per-file type aliases (`--type rust` mapping to a glob set).
- [ ] Streaming mode for files larger than memory.
- [ ] `--files-without-match` for inverse file selection.
- [ ] Windows path separator test hardening (currently Unix-focused
      integration tests).
- [ ] Optional PCRE-style lazy quantifiers (`*?`, `+?`).

## License

Released under the MIT License — see [LICENSE](LICENSE).

Copyright (c) 2026 Bui Bao Khanh

---
**by Bui Bao Khanh**
