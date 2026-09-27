//! Pure line-chart geometry: ranges, ticks, mapping, downsampling, hit testing, and time labels.
//!
//! This module has no GPUI dependency. The chart element draws the results.

use std::cmp::Ordering;

use k8s_core::metrics::{NormalizedPoint, Sample};

/// Drawing rectangle in pixels, with the origin at the top left.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlotRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl PlotRect {
    pub fn right(&self) -> f32 {
        self.x + self.width
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.right() && y >= self.y && y <= self.bottom()
    }
}

/// Screen coordinate point.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PathPoint {
    pub x: f32,
    pub y: f32,
}

/// Data minimum and maximum with padding.
///
/// CPU and memory metrics are non-negative, so zero is a fixed lower bound.
/// The upper bound has 10 percent headroom. All-zero data uses (0, 1).
pub fn pad_range(min: f64, max: f64) -> (f64, f64) {
    let mut lower = if min >= 0.0 { 0.0 } else { min * 1.1 };
    let mut upper = if max > 0.0 { max * 1.1 } else { 0.0 };
    if upper <= lower {
        upper = lower + 1.0;
    }
    if !lower.is_finite() {
        lower = 0.0;
    }
    if !upper.is_finite() {
        upper = lower + 1.0;
    }
    (lower, upper)
}

/// Value range for normalized points. Missing values are ignored.
pub fn value_range(points: &[NormalizedPoint]) -> (f64, f64) {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for value in points.iter().filter_map(|point| point.value) {
        min = min.min(value);
        max = max.max(value);
    }
    if !min.is_finite() || !max.is_finite() {
        return (0.0, 1.0);
    }
    pad_range(min, max)
}

/// Readable ticks use 1, 2, or 5 times a power of ten and cover the range.
pub fn nice_ticks(min: f64, max: f64, target: usize) -> Vec<f64> {
    if !min.is_finite() || !max.is_finite() || max <= min || target < 2 {
        return Vec::new();
    }
    let step = nice_number((max - min) / (target - 1) as f64);
    if step <= 0.0 {
        return Vec::new();
    }
    let mut ticks = Vec::new();
    let mut tick = (min / step).ceil() * step;
    while tick <= max + step * 1e-9 {
        ticks.push(tick);
        tick += step;
        if ticks.len() >= 64 {
            break;
        }
    }
    ticks
}

/// Round a value to 1, 2, or 5 times a power of ten.
fn nice_number(value: f64) -> f64 {
    if value <= 0.0 || !value.is_finite() {
        return 0.0;
    }
    let exponent = value.log10().floor();
    let magnitude = 10f64.powf(exponent);
    let fraction = value / magnitude;
    let nice = if fraction < 1.5 {
        1.0
    } else if fraction < 3.0 {
        2.0
    } else if fraction < 7.0 {
        5.0
    } else {
        10.0
    };
    nice * magnitude
}

/// Candidate time-axis steps in milliseconds, from 100 ms to 1 day.
const TIME_STEPS_MS: &[i64] = &[
    100, 200, 500, 1_000, 2_000, 5_000, 10_000, 15_000, 30_000, 60_000, 120_000, 300_000, 600_000,
    900_000, 1_800_000, 3_600_000, 7_200_000, 10_800_000, 21_600_000, 43_200_000, 86_400_000,
];

/// Time-axis ticks aligned to step boundaries.
pub fn time_ticks(start_ms: i64, end_ms: i64, target: usize) -> Vec<i64> {
    if end_ms <= start_ms || target < 2 {
        return Vec::new();
    }
    let span = end_ms - start_ms;
    let step = TIME_STEPS_MS
        .iter()
        .copied()
        .find(|step| span / step <= target as i64)
        .unwrap_or(86_400_000);
    let mut tick = start_ms.div_euclid(step) * step;
    if tick < start_ms {
        tick += step;
    }
    let mut ticks = Vec::new();
    while tick <= end_ms {
        ticks.push(tick);
        tick += step;
        if ticks.len() >= 64 {
            break;
        }
    }
    ticks
}

/// Map data coordinates to screen coordinates.
pub fn map_point(
    at_ms: i64,
    value: f64,
    time_range: (i64, i64),
    value_range: (f64, f64),
    rect: PlotRect,
) -> PathPoint {
    let (t0, t1) = time_range;
    let x_ratio = if t1 > t0 {
        (at_ms - t0) as f64 / (t1 - t0) as f64
    } else {
        0.5
    };
    let (v0, v1) = value_range;
    let y_ratio = if v1 > v0 {
        (value - v0) / (v1 - v0)
    } else {
        0.5
    };
    PathPoint {
        x: rect.x + x_ratio as f32 * rect.width,
        y: rect.bottom() - y_ratio as f32 * rect.height,
    }
}

/// Map normalized points to line segments. Gaps start a new segment.
pub fn map_points(
    points: &[NormalizedPoint],
    time_range: (i64, i64),
    value_range: (f64, f64),
    rect: PlotRect,
) -> Vec<Vec<PathPoint>> {
    let mut segments: Vec<Vec<PathPoint>> = Vec::new();
    let mut current: Vec<PathPoint> = Vec::new();
    for point in points {
        match point.value {
            Some(value) => {
                current.push(map_point(point.at_ms, value, time_range, value_range, rect))
            }
            None => {
                if !current.is_empty() {
                    segments.push(std::mem::take(&mut current));
                }
            }
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

/// Map a screen x coordinate to data time. Values outside the plot clamp to an endpoint.
pub fn time_at_x(x: f32, time_range: (i64, i64), rect: PlotRect) -> i64 {
    let (t0, t1) = time_range;
    if rect.width <= 0.0 || t1 <= t0 {
        return t0;
    }
    let ratio = ((x - rect.x) / rect.width).clamp(0.0, 1.0) as f64;
    t0 + (ratio * (t1 - t0) as f64).round() as i64
}

/// Nearest valid sample by time.
///
/// Missing values are skipped. The linear scan stays within MAX_PLOT_POINTS.
pub fn nearest_sample(points: &[NormalizedPoint], at_ms: i64) -> Option<Sample> {
    points
        .iter()
        .filter_map(|point| {
            point.value.map(|value| Sample {
                at_ms: point.at_ms,
                value,
            })
        })
        .min_by_key(|sample| (sample.at_ms - at_ms).abs())
}

/// Find the point nearest to an x coordinate.
pub fn nearest_by_x(points: &[PathPoint], x: f32) -> Option<usize> {
    points
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            (a.x - x)
                .abs()
                .partial_cmp(&(b.x - x).abs())
                .unwrap_or(Ordering::Equal)
        })
        .map(|(index, _)| index)
}

/// Downsample normalized points.
/// Keep the minimum and maximum valid point in each bucket, plus the final point.
pub fn downsample_points(points: &[NormalizedPoint], max_points: usize) -> Vec<NormalizedPoint> {
    if max_points < 4 || points.len() <= max_points {
        return points.to_vec();
    }
    let buckets = max_points / 2;
    let bucket_size = points.len().div_ceil(buckets);
    let mut out: Vec<NormalizedPoint> = Vec::with_capacity(max_points + 1);
    for chunk in points.chunks(bucket_size) {
        let mut low: Option<&NormalizedPoint> = None;
        let mut high: Option<&NormalizedPoint> = None;
        for point in chunk {
            let Some(value) = point.value else { continue };
            if low.is_none_or(|candidate| candidate.value.is_some_and(|current| value < current)) {
                low = Some(point);
            }
            if high.is_none_or(|candidate| candidate.value.is_some_and(|current| value > current)) {
                high = Some(point);
            }
        }
        let mut keep = [low, high];
        keep.sort_by_key(|point| point.map(|point| point.at_ms));
        for point in keep.into_iter().flatten() {
            if out.last().is_none_or(|last| last.at_ms != point.at_ms) {
                out.push(*point);
            }
        }
    }
    if let Some(last) = points.last()
        && out.last().is_none_or(|tail| tail.at_ms != last.at_ms)
    {
        out.push(*last);
    }
    out
}

/// Format a short offset for an axis label, such as -13m.
pub fn format_offset_short(delta_ms: i64) -> String {
    let seconds = (delta_ms / 1000).max(0);
    if seconds == 0 {
        return "now".to_owned();
    }
    if seconds < 60 {
        return format!("-{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("-{minutes}m");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("-{hours}h");
    }
    format!("-{}d", hours / 24)
}

/// Waiting message when fewer than two samples are available.
///
/// `interval_ms` is the current sample interval. Invalid values use 10 seconds.
pub fn waiting_for_next_scrape(interval_ms: i64) -> String {
    let seconds = if interval_ms > 0 {
        ((interval_ms + 500) / 1000).max(1)
    } else {
        10
    };
    format!("Waiting for the next sample ({seconds}s)")
}

/// Format an offset from the latest sample for a tooltip.
pub fn format_offset(delta_ms: i64) -> String {
    let seconds = (delta_ms / 1000).max(0);
    if seconds == 0 {
        return "now".to_owned();
    }
    if seconds < 60 {
        return format!("-{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        let rest = seconds % 60;
        return if rest == 0 {
            format!("-{minutes}m")
        } else {
            format!("-{minutes}m{rest}s")
        };
    }
    let hours = minutes / 60;
    if hours < 24 {
        let rest = minutes % 60;
        return if rest == 0 {
            format!("-{hours}h")
        } else {
            format!("-{hours}h{rest}m")
        };
    }
    let days = hours / 24;
    let rest = hours % 24;
    if rest == 0 {
        format!("-{days}d")
    } else {
        format!("-{days}d{rest}h")
    }
}

/// Convert Unix milliseconds to UTC for tables and tooltips.
///
/// This function uses UTC because the local time zone is not available.
pub fn format_clock_utc(at_ms: i64) -> String {
    jiff::Timestamp::from_millisecond(at_ms)
        .map(|timestamp| {
            timestamp
                .to_zoned(jiff::tz::TimeZone::UTC)
                .strftime("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| "—".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(at_ms: i64, value: Option<f64>) -> NormalizedPoint {
        NormalizedPoint { at_ms, value }
    }

    fn sample(at_ms: i64, value: f64) -> Sample {
        Sample { at_ms, value }
    }

    const RECT: PlotRect = PlotRect {
        x: 10.0,
        y: 5.0,
        width: 200.0,
        height: 100.0,
    };

    #[test]
    fn pad_range_is_zero_based_and_leaves_headroom() {
        assert_eq!(pad_range(2.0, 4.0), (0.0, 4.4));
        assert_eq!(
            pad_range(0.0, 0.0),
            (0.0, 1.0),
            "all-zero data must keep a nonzero height"
        );
        assert_eq!(pad_range(-5.0, -2.0), (-5.5, 0.0));
    }

    #[test]
    fn value_range_ignores_gaps_and_empty() {
        let points = [
            point(0, Some(10.0)),
            point(1000, None),
            point(2000, Some(20.0)),
        ];
        assert_eq!(value_range(&points), (0.0, 22.0));
        assert_eq!(value_range(&[]), (0.0, 1.0));
        assert_eq!(value_range(&[point(0, None)]), (0.0, 1.0));
    }

    #[test]
    fn nice_ticks_are_1_2_5_and_cover_bounds() {
        let ticks = nice_ticks(0.0, 4.4, 4);
        assert_eq!(ticks, [0.0, 1.0, 2.0, 3.0, 4.0]);
        let ticks = nice_ticks(0.0, 0.165, 4);
        assert_eq!(ticks.len(), 4);
        assert!((ticks[1] - 0.05).abs() < 1e-12);
        assert!(ticks.first().is_some_and(|tick| *tick >= 0.0));
        assert!(ticks.last().is_some_and(|tick| *tick <= 0.165 + 1e-12));
        assert!(
            nice_ticks(1.0, 1.0, 4).is_empty(),
            "zero range has no ticks"
        );
        assert!(nice_ticks(0.0, 1.0, 1).is_empty(), "target is too small");
    }

    #[test]
    fn time_ticks_align_to_wall_clock_steps() {
        let start = 1_790_157_600_000;
        let end = start + 10 * 60 * 1000;
        let ticks = time_ticks(start, end, 5);
        assert_eq!(ticks.first(), Some(&start), "start is on a minute boundary");
        assert_eq!(ticks.last(), Some(&(start + 10 * 60 * 1000)));
        assert_eq!(ticks.len(), 6);
        assert_eq!(
            ticks[1] - ticks[0],
            120_000,
            "10 minute window uses a 2m step"
        );
    }

    #[test]
    fn time_ticks_skip_past_boundary() {
        let ticks = time_ticks(1_500, 3_600, 3);
        assert_eq!(ticks, [2_000, 3_000], "unaligned start rounds up");
    }

    #[test]
    fn map_point_hits_rect_corners() {
        let time = (0, 1000);
        let value = (0.0, 10.0);
        let bottom_left = map_point(0, 0.0, time, value, RECT);
        assert_eq!(bottom_left, PathPoint { x: 10.0, y: 105.0 });
        let top_right = map_point(1000, 10.0, time, value, RECT);
        assert_eq!(top_right, PathPoint { x: 210.0, y: 5.0 });
    }

    #[test]
    fn map_point_centers_degenerate_ranges() {
        let mapped = map_point(5, 1.0, (5, 5), (1.0, 1.0), RECT);
        assert_eq!(mapped.x, 110.0);
        assert_eq!(mapped.y, 55.0);
    }

    #[test]
    fn map_points_breaks_on_gaps() {
        let points = [
            point(0, Some(1.0)),
            point(1000, Some(2.0)),
            point(2000, None),
            point(3000, Some(3.0)),
        ];
        let segments = map_points(&points, (0, 3000), (0.0, 3.3), RECT);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].len(), 2);
        assert_eq!(segments[1].len(), 1);
    }

    #[test]
    fn time_at_x_is_inverse_of_map_point() {
        let time = (0, 10_000);
        let mapped = map_point(4_000, 0.0, time, (0.0, 1.0), RECT);
        assert_eq!(time_at_x(mapped.x, time, RECT), 4_000);
        assert_eq!(
            time_at_x(-100.0, time, RECT),
            0,
            "out-of-range value clamps to the start"
        );
        assert_eq!(
            time_at_x(9_999.0, time, RECT),
            10_000,
            "out-of-range value clamps to the end"
        );
    }

    #[test]
    fn nearest_sample_skips_gaps() {
        let points = [
            point(0, Some(1.0)),
            point(1000, None),
            point(2000, None),
            point(3000, Some(4.0)),
        ];
        assert_eq!(
            nearest_sample(&points, 900),
            Some(sample(0, 1.0)),
            "missing values are skipped"
        );
        assert_eq!(nearest_sample(&points, 2600), Some(sample(3000, 4.0)));
        assert_eq!(nearest_sample(&[point(0, None)], 0), None);
    }

    #[test]
    fn nearest_by_x_picks_closest() {
        let points = [
            PathPoint { x: 0.0, y: 0.0 },
            PathPoint { x: 10.0, y: 0.0 },
            PathPoint { x: 20.0, y: 0.0 },
        ];
        assert_eq!(nearest_by_x(&points, 9.0), Some(1));
        assert_eq!(nearest_by_x(&points, 100.0), Some(2));
        assert_eq!(nearest_by_x(&[], 1.0), None);
    }

    #[test]
    fn downsample_keeps_spikes_and_endpoints() {
        let mut points: Vec<NormalizedPoint> = (0..1000)
            .map(|index| point(index * 1000, Some(1.0)))
            .collect();
        points[500].value = Some(99.0);
        let thinned = downsample_points(&points, 100);
        assert!(thinned.len() <= 101, "limit and final point");
        assert_eq!(thinned.first().map(|p| p.at_ms), Some(0));
        assert_eq!(thinned.last().map(|p| p.at_ms), Some(999_000));
        assert!(
            thinned.iter().any(|p| p.value == Some(99.0)),
            "spike must remain"
        );
    }

    #[test]
    fn downsample_passes_through_small_input() {
        let points = [point(0, Some(1.0)), point(1000, Some(2.0))];
        assert_eq!(downsample_points(&points, 100), points);
        assert_eq!(downsample_points(&points, 3), points);
    }

    #[test]
    fn downsample_skips_gap_only_buckets() {
        let points = [
            point(0, Some(1.0)),
            point(1000, None),
            point(2000, None),
            point(3000, Some(2.0)),
            point(4000, Some(3.0)),
            point(5000, Some(4.0)),
            point(6000, Some(5.0)),
            point(7000, Some(6.0)),
            point(8000, Some(7.0)),
            point(9000, Some(8.0)),
        ];
        let thinned = downsample_points(&points, 6);
        assert!(thinned.iter().all(|point| point.value.is_some()));
        assert_eq!(thinned.last().map(|point| point.at_ms), Some(9000));
    }

    #[test]
    fn offsets_render_compact_units() {
        assert_eq!(format_offset(0), "now");
        assert_eq!(format_offset(30_000), "-30s");
        assert_eq!(format_offset(90_000), "-1m30s");
        assert_eq!(format_offset(600_000), "-10m");
        assert_eq!(format_offset(3 * 3_600_000 + 1_800_000), "-3h30m");
        assert_eq!(format_offset(26 * 3_600_000), "-1d2h");
        assert_eq!(
            format_offset(-5),
            "now",
            "future and negative values use now"
        );
    }

    #[test]
    fn short_offsets_keep_only_the_largest_unit() {
        assert_eq!(format_offset_short(0), "now");
        assert_eq!(format_offset_short(45_000), "-45s");
        assert_eq!(format_offset_short(790_000), "-13m");
        assert_eq!(format_offset_short(3 * 3_600_000 + 1_800_000), "-3h");
        assert_eq!(format_offset_short(26 * 3_600_000), "-1d");
    }

    #[test]
    fn waiting_copy_names_the_scrape_interval() {
        assert_eq!(
            waiting_for_next_scrape(10_000),
            "Waiting for the next sample (10s)"
        );
        assert_eq!(
            waiting_for_next_scrape(30_000),
            "Waiting for the next sample (30s)"
        );
        assert_eq!(
            waiting_for_next_scrape(0),
            "Waiting for the next sample (10s)",
            "unknown interval uses the 10 second default"
        );
        assert_eq!(
            waiting_for_next_scrape(-5),
            "Waiting for the next sample (10s)"
        );
    }

    #[test]
    fn clock_formats_utc() {
        assert_eq!(format_clock_utc(1_790_157_600_000), "10:00:00");
        assert_eq!(format_clock_utc(i64::MAX), "—");
    }

    #[test]
    fn sample_is_comparable_after_normalization() {
        let series = [sample(0, 1.0), sample(1000, 2.0)];
        assert_eq!(series[1].value, 2.0);
    }
}
