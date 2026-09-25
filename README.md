# rust-grep-lite

`minigrep` — a miniature grep clone written in safe Rust with ANSI colored output.

## Build & run
```bash
cargo build --release
./target/release/minigrep pattern file.txt
./target/release/minigrep -i error server.log
```

## Flags
| Flag | Effect                   |
|------|--------------------------|
| `-i` | Case-insensitive match   |
| `-n` | Hide line numbers        |

Exit codes: `0` matches found · `1` I/O error · `3` no matches · `2` usage error.

Run tests with `cargo test`.
