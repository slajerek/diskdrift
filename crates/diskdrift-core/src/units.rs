//! Size formatting and parsing (spec 3.3).

use serde::{Deserialize, Serialize};

/// Unit system used for human-readable output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SizeUnits {
    /// Powers of 1000 (kB, MB, GB): what Finder and `df -H` show.
    #[default]
    Decimal,
    /// Powers of 1024 (KiB, MiB, GiB).
    Binary,
}

/// Formats a byte count, e.g. `16.2 GB` or `15.1 GiB`.
pub fn format_size(bytes: u64, units: SizeUnits) -> String {
    let (base, names): (f64, [&str; 6]) = match units {
        SizeUnits::Decimal => (1000.0, ["B", "kB", "MB", "GB", "TB", "PB"]),
        SizeUnits::Binary => (1024.0, ["B", "KiB", "MiB", "GiB", "TiB", "PiB"]),
    };
    if bytes < base as u64 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut idx = 0;
    while value >= base && idx < names.len() - 1 {
        value /= base;
        idx += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", names[idx])
    } else {
        format!("{value:.1} {}", names[idx])
    }
}

/// Formats a count with thousands separators, e.g. `1,731,280`.
pub fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Formats a signed byte delta with an explicit sign, e.g. `+1.2 GB`.
pub fn format_delta(delta: i128, units: SizeUnits) -> String {
    let sign = if delta > 0 {
        "+"
    } else if delta < 0 {
        "-"
    } else {
        "±"
    };
    format!("{sign}{}", format_size(delta.unsigned_abs().min(u64::MAX as u128) as u64, units))
}

/// Parses a size such as `120GB`, `1.5 GiB`, `500k` or `4096`.
///
/// Decimal suffixes (`KB`, `MB`, `GB`, `TB`, also `k`, `M`, `G`, `T`) are powers of 1000;
/// binary suffixes (`KiB`, `MiB`, `GiB`, `TiB`) are powers of 1024. A bare number is bytes.
pub fn parse_size(input: &str) -> Result<u64, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty size".into());
    }
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '_'))
        .unwrap_or(s.len());
    let (num, suffix) = s.split_at(split);
    let num: String = num.chars().filter(|&c| c != '_').collect();
    let value: f64 = num
        .parse()
        .map_err(|_| format!("invalid size '{input}': expected a number with an optional unit"))?;
    let multiplier: f64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1e3,
        "m" | "mb" => 1e6,
        "g" | "gb" => 1e9,
        "t" | "tb" => 1e12,
        "p" | "pb" => 1e15,
        "kib" => 1024.0,
        "mib" => 1024f64.powi(2),
        "gib" => 1024f64.powi(3),
        "tib" => 1024f64.powi(4),
        "pib" => 1024f64.powi(5),
        other => {
            return Err(format!(
                "invalid size unit '{other}' in '{input}' (use B, KB, MB, GB, TB or KiB, MiB, GiB, TiB)"
            ));
        }
    };
    let bytes = value * multiplier;
    if !bytes.is_finite() || bytes < 0.0 || bytes > u64::MAX as f64 {
        return Err(format!("size '{input}' is out of range"));
    }
    Ok(bytes.round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_get_separators() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1000), "1,000");
        assert_eq!(format_count(1_731_280), "1,731,280");
    }

    #[test]
    fn formats_decimal_and_binary() {
        assert_eq!(format_size(0, SizeUnits::Decimal), "0 B");
        assert_eq!(format_size(999, SizeUnits::Decimal), "999 B");
        assert_eq!(format_size(16_200_000_000, SizeUnits::Decimal), "16.2 GB");
        assert_eq!(format_size(1024 * 1024, SizeUnits::Binary), "1.0 MiB");
        assert_eq!(format_size(120_000_000_000, SizeUnits::Decimal), "120 GB");
    }

    #[test]
    fn formats_delta_sign() {
        assert_eq!(format_delta(1_500_000, SizeUnits::Decimal), "+1.5 MB");
        assert_eq!(format_delta(-2_000, SizeUnits::Decimal), "-2.0 kB");
        assert_eq!(format_delta(0, SizeUnits::Decimal), "±0 B");
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert_eq!(parse_size("120GB").unwrap(), 120_000_000_000);
        assert_eq!(parse_size("1.5 GiB").unwrap(), 1_610_612_736);
        assert_eq!(parse_size("500k").unwrap(), 500_000);
        assert_eq!(parse_size("1_000 MB").unwrap(), 1_000_000_000);
        assert!(parse_size("12 parsecs").is_err());
        assert!(parse_size("").is_err());
        assert!(parse_size("GB").is_err());
    }
}
