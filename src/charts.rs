//! Terminal charts: readings bucketed over a window, block sparklines,
//! and braille lanes that pack two readings into each character.

/// Mean per bucket, rounded to one decimal; a bucket with no readings
/// stays None so downtime shows as a gap, not an interpolated line.
pub fn bucketize(
    points: &[(i64, f64)],
    since: i64,
    until: i64,
    buckets: usize,
) -> Vec<Option<f64>> {
    let span = (until - since) as f64;
    let mut sums = vec![0.0; buckets];
    let mut counts = vec![0_u32; buckets];
    for &(at, value) in points {
        if at < since || at > until || span <= 0.0 {
            continue;
        }
        let index = (((at - since) as f64 / span) * buckets as f64).floor() as usize;
        let index = index.min(buckets - 1);
        sums[index] += value;
        counts[index] += 1;
    }
    sums.iter()
        .zip(&counts)
        .map(|(sum, count)| (*count > 0).then(|| crate::output::round1(sum / f64::from(*count))))
        .collect()
}

pub struct Summary {
    pub current: Option<f64>,
    pub min: Option<f64>,
    pub avg: Option<f64>,
    pub max: Option<f64>,
}

pub fn summarize(points: &[Option<f64>]) -> Summary {
    let present: Vec<f64> = points.iter().flatten().copied().collect();
    if present.is_empty() {
        return Summary {
            current: None,
            min: None,
            avg: None,
            max: None,
        };
    }
    let sum: f64 = present.iter().sum();
    Summary {
        current: present.last().copied(),
        min: present.iter().copied().reduce(f64::min),
        avg: Some(crate::output::round1(sum / present.len() as f64)),
        max: present.iter().copied().reduce(f64::max),
    }
}

/// Block sparkline scaled to `maximum`, or to the largest reading.
pub fn sparkline(points: &[Option<f64>], unicode: bool, maximum: Option<f64>) -> String {
    let levels: [&str; 8] = if unicode {
        ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"]
    } else {
        [".", ":", "-", "=", "+", "*", "#", "@"]
    };
    let top = maximum.unwrap_or_else(|| points.iter().flatten().copied().fold(0.0, f64::max));
    points
        .iter()
        .map(|point| match point {
            None => " ",
            Some(_) if top <= 0.0 => levels[0],
            Some(value) => levels[((value / top) * 8.0).floor().clamp(0.0, 7.0) as usize],
        })
        .collect()
}

/// Braille: each cell holds two columns of four dots, twice the
/// horizontal resolution of block sparklines.
pub fn braille(points: &[Option<f64>], maximum: Option<f64>) -> String {
    const LEFT: [u32; 5] = [0, 0x40, 0x44, 0x46, 0x47];
    const RIGHT: [u32; 5] = [0, 0x80, 0xa0, 0xb0, 0xb8];
    let top = maximum.unwrap_or_else(|| points.iter().flatten().copied().fold(0.0, f64::max));
    let level = |point: Option<&Option<f64>>| -> usize {
        match point.copied().flatten() {
            Some(value) if top > 0.0 && value > 0.0 => {
                ((value / top) * 4.0).ceil().clamp(1.0, 4.0) as usize
            }
            _ => 0,
        }
    };
    points
        .chunks(2)
        .map(|pair| {
            let code = 0x2800 + LEFT[level(pair.first())] + RIGHT[level(pair.get(1))];
            char::from_u32(code).unwrap_or(' ')
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Empty buckets stay gaps, levels scale to the top,
    /// and braille packs two readings per cell.
    #[test]
    fn readings_bucket_and_draw_with_gaps_left_blank() {
        let points = [(0, 10.0), (5, 30.0), (25, 50.0)];
        assert_eq!(
            bucketize(&points, 0, 40, 4),
            [Some(20.0), None, Some(50.0), None]
        );
        assert_eq!(
            sparkline(
                &[Some(0.0), None, Some(50.0), Some(100.0)],
                true,
                Some(100.0)
            ),
            "▁ ▅█"
        );
        assert_eq!(sparkline(&[Some(1.0)], false, Some(0.0)), ".");
        assert_eq!(
            braille(&[Some(100.0), Some(25.0), None], Some(100.0)),
            "⣇\u{2800}"
        );
        let summary = summarize(&[None, Some(1.0), Some(4.0)]);
        assert_eq!(
            (summary.current, summary.avg, summary.max),
            (Some(4.0), Some(2.5), Some(4.0))
        );
    }
}
