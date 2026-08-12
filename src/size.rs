use anyhow::{Result, anyhow, bail};

pub fn parse_size(input: &str) -> Result<u64> {
    let normalized = input.trim().to_ascii_uppercase();
    if normalized.is_empty() {
        bail!("size cannot be empty");
    }
    let split = normalized
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(normalized.len());
    let (number, suffix) = normalized.split_at(split);
    let value: f64 = number
        .parse()
        .map_err(|_| anyhow!("invalid size value: {input}"))?;
    if !value.is_finite() || value < 0.0 {
        bail!("invalid size value: {input}");
    }
    let multiplier = match suffix.trim_end_matches('B') {
        "" => 1_u64,
        "K" | "KI" => 1024,
        "M" | "MI" => 1024_u64.pow(2),
        "G" | "GI" => 1024_u64.pow(3),
        "T" | "TI" => 1024_u64.pow(4),
        _ => bail!("unsupported size suffix: {suffix}"),
    };
    let bytes = value * multiplier as f64;
    if bytes > u64::MAX as f64 {
        bail!("size is too large: {input}");
    }
    Ok(bytes.round() as u64)
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_binary_sizes() {
        assert_eq!(parse_size("500M").unwrap(), 500 * 1024 * 1024);
        assert_eq!(parse_size("1.5GiB").unwrap(), 1_610_612_736);
        assert_eq!(parse_size("42").unwrap(), 42);
    }

    #[test]
    fn rejects_unknown_suffixes() {
        assert!(parse_size("10XB").is_err());
    }
}
