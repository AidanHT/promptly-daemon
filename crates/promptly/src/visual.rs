//! Terminal visuals — meters, sparklines, composition bars, and section rules
//! shared by the command renderers.
//!
//! Everything here is pure text over a [`Style`], so the visuals honor the one
//! `--no-color`/`NO_COLOR`/not-a-TTY decision and stay unit-testable. The glyphs
//! are chosen to degrade gracefully: composition segments use *different* fill
//! characters (`█ ▓ ▒`), so the split stays legible even in plain mode where
//! color can't distinguish them.

use crate::style::Style;

/// The shared visible width of section rules, so every screen's header lines up.
pub const RULE_WIDTH: usize = 56;

/// Eighth-block fills, coarsest last — index `n` is `(n+1)/8` of a cell.
const EIGHTHS: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
/// Sparkline levels, lowest first.
const SPARKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// The empty track a meter fills over.
const TRACK: &str = "░";

/// A horizontal meter filled to `ratio` (clamped to `0..=1`; NaN reads as 0)
/// over a `width`-cell track, at eighth-block resolution. Always exactly
/// `width` glyphs, so meters in adjacent rows keep their columns aligned.
pub fn meter(ratio: f64, width: usize) -> String {
    let ratio = if ratio.is_finite() {
        ratio.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let cells = ratio * width as f64;
    let full = (cells.floor() as usize).min(width);
    let mut out = "█".repeat(full);
    if full < width {
        let eighths = ((cells - full as f64) * 8.0).round() as usize;
        let partial = if eighths > 0 {
            out.push(EIGHTHS[eighths - 1]);
            1
        } else {
            0
        };
        out.push_str(&TRACK.repeat(width - full - partial));
    }
    out
}

/// Apportion `width` cells across `parts` by largest remainder, so the cell
/// counts always sum to exactly `width` (or to zero when every part is ≤ 0).
pub fn split(width: usize, parts: &[f64]) -> Vec<usize> {
    let total: f64 = parts.iter().map(|p| p.max(0.0)).sum();
    if total <= 0.0 || width == 0 {
        return vec![0; parts.len()];
    }
    let quotas: Vec<f64> = parts
        .iter()
        .map(|p| p.max(0.0) / total * width as f64)
        .collect();
    let mut cells: Vec<usize> = quotas.iter().map(|q| q.floor() as usize).collect();
    let mut leftover = width - cells.iter().sum::<usize>();
    // Hand the leftover cells to the largest fractional parts (stable order).
    let mut order: Vec<usize> = (0..parts.len()).collect();
    order.sort_by(|&a, &b| {
        let fa = quotas[a] - quotas[a].floor();
        let fb = quotas[b] - quotas[b].floor();
        fb.partial_cmp(&fa).unwrap_or(std::cmp::Ordering::Equal)
    });
    for idx in order {
        if leftover == 0 {
            break;
        }
        cells[idx] += 1;
        leftover -= 1;
    }
    cells
}

/// The in/out/think token composition as one segmented bar: input `█` in the
/// accent color, output `▓` magenta, thinking `▒` yellow. Empty when nothing
/// has been captured.
pub fn token_mix(style: Style, width: usize, input: f64, output: f64, thinking: f64) -> String {
    let cells = split(width, &[input, output, thinking]);
    let mut out = String::new();
    if cells[0] > 0 {
        out.push_str(&style.accent(&"█".repeat(cells[0])));
    }
    if cells[1] > 0 {
        out.push_str(&style.magenta(&"▓".repeat(cells[1])));
    }
    if cells[2] > 0 {
        out.push_str(&style.yellow(&"▒".repeat(cells[2])));
    }
    out
}

/// The legend matching [`token_mix`]'s glyphs.
pub fn token_mix_legend(style: Style) -> String {
    format!(
        "{} {} {}",
        style.accent("█ in"),
        style.magenta("· ▓ out"),
        style.yellow("· ▒ think"),
    )
}

/// A sparkline of the last `max_points` values, scaled to the window's peak.
/// Empty input renders empty; an all-zero window renders as a flat baseline.
pub fn spark(values: &[u64], max_points: usize) -> String {
    let tail = &values[values.len().saturating_sub(max_points)..];
    let peak = tail.iter().copied().max().unwrap_or(0);
    tail.iter()
        .map(|&v| {
            if peak == 0 {
                SPARKS[0]
            } else {
                SPARKS[((v as f64 / peak as f64) * 7.0).round() as usize]
            }
        })
        .collect()
}

/// A section rule: `── title ───────…` padded to [`RULE_WIDTH`] visible cells,
/// with the title in the accent and the rule receding in dim.
pub fn header(style: Style, title: &str) -> String {
    let tail = RULE_WIDTH.saturating_sub(title.chars().count() + 4).max(2);
    format!(
        "{} {} {}",
        style.dim("──"),
        style.bold(&style.accent(title)),
        style.dim(&"─".repeat(tail)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_is_always_exactly_width_glyphs() {
        for ratio in [0.0, 0.33, 0.5, 0.99, 1.0, 2.0, -1.0, f64::NAN] {
            let bar = meter(ratio, 12);
            assert_eq!(bar.chars().count(), 12, "ratio {ratio}: {bar:?}");
        }
    }

    #[test]
    fn meter_fills_with_the_ratio() {
        assert_eq!(meter(0.0, 4), "░░░░");
        assert_eq!(meter(1.0, 4), "████");
        assert_eq!(meter(0.5, 4), "██░░");
        // A partial cell renders as an eighth-block, not a full/empty jump.
        let third = meter(1.0 / 3.0, 3);
        assert!(third.starts_with('█'));
        assert!(third.ends_with('░'));
        // Out-of-range and NaN clamp instead of panicking.
        assert_eq!(meter(7.5, 4), "████");
        assert_eq!(meter(f64::NAN, 4), "░░░░");
    }

    #[test]
    fn split_sums_to_the_width() {
        for parts in [
            vec![1.0, 1.0, 1.0],
            vec![5000.0, 3000.0, 0.0],
            vec![0.1, 0.0, 99.9],
        ] {
            let cells = split(24, &parts);
            assert_eq!(cells.iter().sum::<usize>(), 24, "{parts:?} → {cells:?}");
        }
        // Nothing to show → no cells at all (not a full-width mystery bar).
        assert_eq!(split(24, &[0.0, 0.0]), vec![0, 0]);
        assert_eq!(split(0, &[1.0]), vec![0]);
    }

    #[test]
    fn split_is_proportional() {
        let cells = split(10, &[3.0, 1.0]);
        assert!(cells[0] > cells[1]);
        assert_eq!(cells.iter().sum::<usize>(), 10);
    }

    #[test]
    fn token_mix_uses_distinct_glyphs_per_segment() {
        let bar = token_mix(Style::plain(), 12, 6.0, 3.0, 3.0);
        assert!(bar.contains('█'));
        assert!(bar.contains('▓'));
        assert!(bar.contains('▒'));
        assert_eq!(bar.chars().count(), 12);
        // No tokens → no bar.
        assert!(token_mix(Style::plain(), 12, 0.0, 0.0, 0.0).is_empty());
    }

    #[test]
    fn spark_scales_to_the_window_peak() {
        let line = spark(&[0, 5, 10], 8);
        let chars: Vec<char> = line.chars().collect();
        assert_eq!(chars.len(), 3);
        assert_eq!(chars[0], '▁');
        assert_eq!(chars[2], '█');
        // Only the last `max_points` values are drawn.
        assert_eq!(spark(&[1, 2, 3, 4], 2).chars().count(), 2);
        assert_eq!(spark(&[], 8), "");
        // All-zero renders a flat baseline, not a divide-by-zero.
        assert_eq!(spark(&[0, 0], 8), "▁▁");
    }

    #[test]
    fn header_pads_to_the_shared_rule_width_and_stays_plain_safe() {
        let line = header(Style::plain(), "projected score");
        assert!(line.starts_with("── projected score "));
        assert_eq!(line.chars().count(), RULE_WIDTH);
        assert!(!line.contains('\x1b'));
    }
}
