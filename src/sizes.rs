//! Human-readable size parsing and formatting.
//!
//! The `--min-size` / `--max-size` flags accept compact byte counts such as
//! `512`, `10k` or `2g`. The strict CLI grammar lives in
//! [`crate::args::parse_size`]; this module is the rendering-side sibling:
//!
//! * [`parse_flexible`] accepts the documented relaxed forms — decimal
//!   fractions (`1.5m`), unit words (`10 KiB`) and surrounding whitespace —
//!   so every size printed in the docs parses back into a byte count.
//! * [`format_human`] renders byte counts with binary units (`KiB`, `MiB`,
//!   ...), matching the `--stats` table style.
//! * [`format_decimal`] renders byte counts with SI units (`kB`, `MB`, ...),
//!   the conventional style for *throughput* numbers.
//! * [`describe_bounds`] renders a `--min-size`/`--max-size` pair as one
//!   short phrase for the `--stats` diagnostics block.
//!
//! All multipliers are binary (k = 1024) everywhere in searchlight, so a
//! `--max-size 10k` and the rendering of those same 10,240 bytes always
//! agree.

use std::time::Duration;

/// Example sizes rendered in the CLI help text.
pub const SIZE_EXAMPLES: &str = "512, 10k, 1.5m, 2gib";

/// A binary (power-of-two) size unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeUnit {
    /// One byte.
    Byte,
    /// 1024 bytes.
    Kib,
    /// 1024² bytes.
    Mib,
    /// 1024³ bytes.
    Gib,
    /// 1024⁴ bytes.
    Tib,
}

impl SizeUnit {
    /// The unit's size in bytes.
    pub fn factor(self) -> u64 {
        match self {
            SizeUnit::Byte => 1,
            SizeUnit::Kib => 1024,
            SizeUnit::Mib => 1024 * 1024,
            SizeUnit::Gib => 1024 * 1024 * 1024,
            SizeUnit::Tib => 1024u64 * 1024 * 1024 * 1024,
        }
    }

    /// The canonical short label, e.g. `"KiB"`.
    pub fn label(self) -> &'static str {
        match self {
            SizeUnit::Byte => "B",
            SizeUnit::Kib => "KiB",
            SizeUnit::Mib => "MiB",
            SizeUnit::Gib => "GiB",
            SizeUnit::Tib => "TiB",
        }
    }

    /// The largest binary unit in which `bytes` is at least 1.0.
    ///
    /// ```text
    /// best_for(1023)  == Byte
    /// best_for(1024)  == Kib
    /// best_for(5 MiB) == Mib
    /// ```
    pub fn best_for(bytes: u64) -> SizeUnit {
        if bytes >= SizeUnit::Tib.factor() {
            SizeUnit::Tib
        } else if bytes >= SizeUnit::Gib.factor() {
            SizeUnit::Gib
        } else if bytes >= SizeUnit::Mib.factor() {
            SizeUnit::Mib
        } else if bytes >= SizeUnit::Kib.factor() {
            SizeUnit::Kib
        } else {
            SizeUnit::Byte
        }
    }
}

/// Render `bytes` with binary units: `0 B`, `999 B`, `1.5 KiB`, `3.0 MiB`.
///
/// Whole byte counts keep no decimals; every larger unit shows exactly one
/// decimal digit. This mirrors the `--stats` byte formatting.
pub fn format_human(bytes: u64) -> String {
    let unit = SizeUnit::best_for(bytes);
    if unit == SizeUnit::Byte {
        return format!("{} {}", bytes, unit.label());
    }
    let value = bytes as f64 / unit.factor() as f64;
    format!("{:.1} {}", value, unit.label())
}

/// Decimal SI units, largest first; the entry matching `bytes` is used.
const SI_UNITS: [(u64, &str); 4] = [
    (1_000u64, "kB"),
    (1_000u64.pow(2), "MB"),
    (1_000u64.pow(3), "GB"),
    (1_000u64.pow(4), "TB"),
];

/// Render `bytes` with SI (decimal) units: `999 B`, `1.5 kB`, `2.0 MB`.
///
/// Used for throughput figures, where SI units are the convention.
pub fn format_decimal(bytes: u64) -> String {
    if bytes < 1_000 {
        return format!("{} B", bytes);
    }
    let mut chosen: (u64, &str) = SI_UNITS[0];
    for candidate in SI_UNITS {
        if bytes >= candidate.0 {
            chosen = candidate;
        }
    }
    let value = bytes as f64 / chosen.0 as f64;
    format!("{:.1} {}", value, chosen.1)
}

/// Render a byte throughput: `format_rate(2_000_000, 2s)` is `"1.0 MB/s"`.
///
/// A zero (or unmeasurable) elapsed time yields `"0 B/s"` rather than an
/// infinity.
pub fn format_rate(bytes: u64, elapsed: Duration) -> String {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return "0 B/s".to_string();
    }
    let per_second = (bytes as f64 / secs) as u64;
    format!("{}/s", format_decimal(per_second))
}

/// Parse a relaxed human size, the documented superset of the strict CLI
/// grammar.
///
/// Accepted forms (case-insensitive, all whitespace ignored):
///
/// * plain byte counts: `512`, `512B`, `512 bytes`
/// * binary multiples: `10k`, `10kb`, `10 KiB`, `1.5m`, `2gib`, `1t`
/// * decimal fractions for any multiplier: `0.5g`, `1.25 MiB`
///
/// Every multiplier is binary (`k` = 1024) to stay consistent with
/// [`crate::args::parse_size`]. Returns `None` for anything malformed or
/// overflowing `u64`.
pub fn parse_flexible(text: &str) -> Option<u64> {
    let cleaned: String = text
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    let split = cleaned
        .char_indices()
        .find(|(_, ch)| !(ch.is_ascii_digit() || *ch == '.'))
        .map(|(idx, _)| idx)
        .unwrap_or(cleaned.len());
    let (number, suffix) = cleaned.split_at(split);
    if number.is_empty() {
        return None;
    }
    let amount: f64 = number.parse().ok()?;
    if !amount.is_finite() || amount < 0.0 {
        return None;
    }
    let factor: u64 = match suffix {
        "" | "b" | "byte" | "bytes" => 1,
        "k" | "kb" | "kib" => SizeUnit::Kib.factor(),
        "m" | "mb" | "mib" => SizeUnit::Mib.factor(),
        "g" | "gb" | "gib" => SizeUnit::Gib.factor(),
        "t" | "tb" | "tib" => SizeUnit::Tib.factor(),
        _ => return None,
    };
    let scaled = amount * factor as f64;
    if scaled >= u64::MAX as f64 {
        return None;
    }
    Some(scaled as u64)
}

/// Render a `--min-size`/`--max-size` pair as one short human phrase.
///
/// Used by the `--stats` summary so users can see which size window a run
/// applied without re-deriving it from the raw byte counts.
pub fn describe_bounds(min: Option<u64>, max: Option<u64>) -> String {
    match (min, max) {
        (None, None) => "any size".to_string(),
        (Some(min), None) => format!(">= {}", format_human(min)),
        (None, Some(max)) => format!("<= {}", format_human(max)),
        (Some(min), Some(max)) => format!("{} - {}", format_human(min), format_human(max)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_factors_are_powers_of_two() {
        assert_eq!(SizeUnit::Byte.factor(), 1);
        assert_eq!(SizeUnit::Kib.factor(), 1024);
        assert_eq!(SizeUnit::Mib.factor(), 1024 * 1024);
        assert_eq!(SizeUnit::Gib.factor(), 1024 * 1024 * 1024);
        assert_eq!(SizeUnit::Tib.factor(), 1_099_511_627_776);
    }

    #[test]
    fn best_unit_grows_with_the_value() {
        assert_eq!(SizeUnit::best_for(0), SizeUnit::Byte);
        assert_eq!(SizeUnit::best_for(1023), SizeUnit::Byte);
        assert_eq!(SizeUnit::best_for(1024), SizeUnit::Kib);
        assert_eq!(SizeUnit::best_for(5 * 1024 * 1024), SizeUnit::Mib);
        assert_eq!(SizeUnit::best_for(u64::MAX), SizeUnit::Tib);
        assert_eq!(SizeUnit::Mib.label(), "MiB");
    }

    #[test]
    fn human_format_uses_binary_units() {
        assert_eq!(format_human(0), "0 B");
        assert_eq!(format_human(999), "999 B");
        assert_eq!(format_human(1024), "1.0 KiB");
        assert_eq!(format_human(1536), "1.5 KiB");
        assert_eq!(format_human(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(format_human(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn decimal_format_uses_si_units() {
        assert_eq!(format_decimal(0), "0 B");
        assert_eq!(format_decimal(999), "999 B");
        assert_eq!(format_decimal(1_500), "1.5 kB");
        assert_eq!(format_decimal(1_500_000), "1.5 MB");
        assert_eq!(format_decimal(2_000_000_000), "2.0 GB");
        assert_eq!(format_decimal(1_000_000_000_000), "1.0 TB");
    }

    #[test]
    fn rate_format_divides_by_seconds() {
        let two_seconds = Duration::from_secs(2);
        assert_eq!(format_rate(2_000_000, two_seconds), "1.0 MB/s");
        assert_eq!(format_rate(1, two_seconds), "0.5 kB/s");
        assert_eq!(format_rate(1_000, Duration::ZERO), "0 B/s");
    }

    #[test]
    fn flexible_parser_accepts_documented_forms() {
        assert_eq!(parse_flexible("512"), Some(512));
        assert_eq!(parse_flexible("10k"), Some(10_240));
        assert_eq!(parse_flexible("10kb"), Some(10_240));
        assert_eq!(parse_flexible("10 KiB"), Some(10_240));
        assert_eq!(parse_flexible("1.5m"), Some(1_572_864));
        assert_eq!(parse_flexible("2GIB"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_flexible("512bytes"), Some(512));
        assert_eq!(parse_flexible("  1 tb "), Some(1_099_511_627_776));
        assert_eq!(parse_flexible("0.5g"), Some(512 * 1024 * 1024));
        assert_eq!(parse_flexible("512B"), Some(512));
    }

    #[test]
    fn flexible_parser_rejects_malformed_input() {
        assert_eq!(parse_flexible(""), None);
        assert_eq!(parse_flexible("   "), None);
        assert_eq!(parse_flexible("k"), None);
        assert_eq!(parse_flexible("12x"), None);
        assert_eq!(parse_flexible("-5"), None);
        assert_eq!(parse_flexible("1.2.3"), None);
        assert_eq!(parse_flexible("nan"), None);
        assert_eq!(parse_flexible("1e3"), None);
        assert_eq!(parse_flexible("999999999999t"), None, "overflows u64");
    }

    #[test]
    fn bounds_phrases_cover_every_combination() {
        assert_eq!(describe_bounds(None, None), "any size");
        assert_eq!(describe_bounds(Some(10_240), None), ">= 10.0 KiB");
        assert_eq!(describe_bounds(None, Some(5 * 1024 * 1024)), "<= 5.0 MiB");
        assert_eq!(
            describe_bounds(Some(1024), Some(2048)),
            "1.0 KiB - 2.0 KiB"
        );
    }

    #[test]
    fn size_examples_themselves_parse() {
        for example in SIZE_EXAMPLES.split(", ") {
            assert!(
                parse_flexible(example).is_some(),
                "example '{}' must parse",
                example
            );
        }
    }
}
