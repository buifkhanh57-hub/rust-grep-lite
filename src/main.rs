// rust-grep-lite — a miniature grep clone in safe Rust.
// Usage: minigrep <pattern> <file1> [file2 ...]
// Build: cargo build --release

use std::env;
use std::fs;
use std::process::exit;

const RESET: &str = "\x1b[0m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";

fn parse_args() -> (String, Vec<String>) {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: minigrep <pattern> <file1> [file2 ...]");
        eprintln!("options:");
        eprintln!("  -i          case-insensitive matching");
        eprintln!("  -n          suppress line numbers");
        eprintln!("example: minigrep -i error server.log");
        exit(2);
    }
    (args[0].clone(), args[1..].to_vec())
}

struct Config {
    pattern: String,
    files: Vec<String>,
    ignore_case: bool,
    show_numbers: bool,
}

impl Config {
    fn from_flags(args: &[String]) -> Config {
        let mut pattern = String::new();
        let mut files = Vec::new();
        let mut ignore_case = false;
        let mut show_numbers = true;

        for arg in args {
            match arg.as_str() {
                "-i" => ignore_case = true,
                "-n" => show_numbers = false,
                other if pattern.is_empty() => pattern = other.to_string(),
                other => files.push(other.to_string()),
            }
        }
        Config { pattern, files, ignore_case, show_numbers }
    }

    fn matches(&self, line: &str) -> bool {
        if self.ignore_case {
            line.to_lowercase().contains(&self.pattern.to_lowercase())
        } else {
            line.contains(&self.pattern)
        }
    }
}

fn highlight(line: &str, pattern: &str, ignore_case: bool) -> String {
    let (hay, needle) = if ignore_case {
        (line.to_lowercase(), pattern.to_lowercase())
    } else {
        (line.to_string(), pattern.to_string())
    };
    let mut out = String::new();
    let mut i = 0;
    while i < line.len() {
        if hay[i..].starts_with(&needle) {
            out.push_str(RED);
            out.push_str(&line[i..i + needle.len()]);
            out.push_str(RESET);
            i += needle.len();
        } else {
            let ch = line[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn process_file(path: &str, cfg: &Config) -> std::io::Result<usize> {
    let content = fs::read_to_string(path)?;
    let mut hits = 0;
    for (idx, line) in content.lines().enumerate() {
        if cfg.matches(line) {
            hits += 1;
            print!("{GREEN}{path}{RESET}");
            if cfg.show_numbers {
                print!(":{CYAN}{}{RESET}", idx + 1);
            }
            println!(": {}", highlight(line, &cfg.pattern, cfg.ignore_case));
        }
    }
    Ok(hits)
}

fn main() {
    let (raw0, raw_rest) = parse_args();
    let _ = raw0;
    let cfg = Config::from_flags(&raw_rest);

    if cfg.pattern.is_empty() || cfg.files.is_empty() {
        eprintln!("error: pattern and at least one file are required");
        exit(2);
    }

    let mut total = 0usize;
    let mut had_error = false;
    for file in &cfg.files {
        match process_file(file, &cfg) {
            Ok(hits) => total += hits,
            Err(err) => {
                eprintln!("minigrep: {file}: {err}");
                had_error = true;
            }
        }
    }

    if total > 0 {
        println!("{GREEN}-- {total} matching line(s) --{RESET}");
    }
    exit(if had_error { 1 } else if total == 0 { 3 } else { 0 });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substring_match() {
        let cfg = Config {
            pattern: "err".into(),
            files: vec![],
            ignore_case: false,
            show_numbers: true,
        };
        assert!(cfg.matches("server error 500"));
        assert!(!cfg.matches("all good"));
    }

    #[test]
    fn case_insensitive_match() {
        let cfg = Config {
            pattern: "ERR".into(),
            files: vec![],
            ignore_case: true,
            show_numbers: true,
        };
        assert!(cfg.matches("server error 500"));
    }
}
