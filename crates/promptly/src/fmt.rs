//! Small number formatters shared by the score and watch renderers.

/// Two-decimal value with thousands separators (e.g. `183,823.53`).
pub fn score(value: f64) -> String {
    // Round the whole value to total cents in one step so a fraction ≥ .995
    // carries into the integer part (e.g. 5.999 → "6.00", not "5.100").
    let total_cents = (value.abs() * 100.0).round() as u128;
    let whole = total_cents / 100;
    let cents = total_cents % 100;
    let sign = if value < 0.0 { "-" } else { "" };
    format!("{sign}{}.{cents:02}", thousands(whole))
}

/// Group a non-negative integer with thousands separators (e.g. `183,823`).
pub fn thousands(mut n: u128) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let mut groups = Vec::new();
    while n > 0 {
        groups.push(format!("{:03}", n % 1000));
        n /= 1000;
    }
    groups.reverse();
    // Trim the leading zeros only on the most-significant group.
    let first = groups[0].trim_start_matches('0');
    let head = if first.is_empty() { "0" } else { first };
    std::iter::once(head.to_string())
        .chain(groups.into_iter().skip(1))
        .collect::<Vec<_>>()
        .join(",")
}

/// A coarse relative age (`42s`, `5m`, `3h`, `4d`) from a millisecond span,
/// clamped at zero. Shared by the `watch`/`score` session-age header so a stale
/// resumed session reads at a glance.
pub fn relative_age(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Compact number: integers print without a decimal, otherwise trimmed to ≤3
/// places (e.g. `1`, `0.6`, `1.67`).
pub fn compact(value: f64) -> String {
    if (value - value.round()).abs() < 1e-9 {
        format!("{}", value.round() as i64)
    } else {
        let s = format!("{value:.3}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_rounds_to_cents_with_separators() {
        assert_eq!(score(183823.5294), "183,823.53");
        assert_eq!(score(100_000_000.0), "100,000,000.00");
        assert_eq!(score(0.0), "0.00");
    }

    #[test]
    fn score_carries_a_rounding_fraction_into_the_whole() {
        // A fraction ≥ .995 must carry into the integer part, not render as ".100".
        assert_eq!(score(5.999), "6.00");
        assert_eq!(score(5.995), "6.00");
        assert_eq!(score(999.999), "1,000.00");
        assert_eq!(score(-5.999), "-6.00");
    }

    #[test]
    fn thousands_groups_by_magnitude() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(7), "7");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(183_823), "183,823");
    }

    #[test]
    fn relative_age_steps_through_the_units() {
        assert_eq!(relative_age(0), "0s");
        assert_eq!(relative_age(-5_000), "0s");
        assert_eq!(relative_age(42_000), "42s");
        assert_eq!(relative_age(5 * 60_000), "5m");
        assert_eq!(relative_age(3 * 3_600_000), "3h");
        assert_eq!(relative_age(4 * 86_400_000), "4d");
    }

    #[test]
    fn compact_trims_trailing_zeros() {
        assert_eq!(compact(1.0), "1");
        assert_eq!(compact(0.6), "0.6");
        assert_eq!(compact(1.67), "1.67");
    }
}
