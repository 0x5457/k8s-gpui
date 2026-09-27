//! Pure chart math: the plot rectangle, value ranges, ticks, downsampling, hit
//! testing, and time labels.
//!
//! This module has no GPUI dependency. gpui-kit's `ScaleLinear` maps a domain
//! onto a pixel range, so the app only keeps what the scale cannot express: the
//! padded value range, the readable tick sets, the sample downsample, and the
//! labels. The plot element in `element` draws the results.

use k8s_core::metrics::{NormalizedPoint, Sample};

/// Drawing rectangle in pixels, with the origin at the top left.
///
/// The plot is inset inside its element so the axis labels, the legend, and the
/// status line have somewhere to live without overlapping a mark.
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

/// Hard backstop on a tick set, past the count the drawn size can carry.
///
/// A chart 4px tall still asks for ticks, and a range spanning nine orders of
/// magnitude would otherwise answer with hundreds of them. The count the caller
/// asks for is the real bound — see [`nice_ticks`] — so this only ever fires on
/// a plot too small to have a scale.
const MAX_TICKS: usize = 64;

/// Readable ticks use 1, 2, or 5 times a power of ten and cover the range.
///
/// `target` is not a hint, it is the number of labels the drawn height can
/// carry: the caller measures it from the plot rectangle, so a 3-tick chart asks
/// for 3 and a 4-tick chart asks for 4. Three things follow from taking it that
/// way, and all three used to be violated:
///
/// - **A tick is a whole number of steps**, indexed rather than accumulated, so
///   the set cannot drift off its own grid the way a `while tick <= max` loop
///   does.
/// - **A tick is rounded to the precision its step is stated in**, which is what
///   actually makes it a round number *as a value*. `0.2` has no exact binary
///   form, so `3 × 0.2` is `0.6000000000000001`, and that value is what a caller
///   that quotes the tick — a table cell, an accessible description — would read.
///   The label formatter rounds it away on screen and nothing else does.
/// - **The set is widened until it fits.** A range whose low end sits far from
///   its high end needs more ticks at the step that spans it than the plot has
///   room for, and the old loop stopped at 64 — a rule about arithmetic rather
///   than about the size the chart is drawn at. The step now climbs the same
///   1, 2, 5 ladder until the set fits `target`, so every label is a round number
///   *and* there are no more of them than the plot can hold.
pub fn nice_ticks(min: f64, max: f64, target: usize) -> Vec<f64> {
    if !min.is_finite() || !max.is_finite() || max <= min || target < 2 {
        return Vec::new();
    }
    let capacity = target.min(MAX_TICKS);
    let mut step = nice_number((max - min) / (capacity - 1) as f64);
    // Bounded because `wider_nice` climbs by at least a decade, and a range
    // that needs more than that has no scale at this size.
    for _ in 0..16 {
        if !step.is_finite() || step <= 0.0 || tick_count(min, max, step) <= capacity {
            break;
        }
        step = wider_nice(step);
    }
    if !step.is_finite() || step <= 0.0 {
        return Vec::new();
    }
    let first = (min / step).ceil();
    let last = (max / step).floor();
    if !first.is_finite() || !last.is_finite() || last < first {
        return Vec::new();
    }
    let decimals = step_decimals(step);
    let count = ((last - first) as usize + 1).min(capacity);
    (0..count)
        .map(|index| round_to((first + index as f64) * step, decimals))
        .collect()
}

/// The number of decimal places a step of this size is stated in.
///
/// A step on the 1, 2, 5 ladder is always `m × 10^e` with `m` in `{1, 2, 5}`, so
/// its decimal places are exactly `max(0, -e)`: `0.2` and `0.05` have one and
/// two, `2` and `50` have none.
fn step_decimals(step: f64) -> i32 {
    if step <= 0.0 || !step.is_finite() {
        return 0;
    }
    (-(step.log10().floor() as i32)).max(0)
}

/// `value` at `decimals` decimal places, as the nearest representable value.
///
/// Scaling, rounding and scaling back, rather than a format: a tick is a number
/// the rest of the app does arithmetic and comparison on, so it has to be the
/// value the reader sees and not a string that prints like it.
fn round_to(value: f64, decimals: i32) -> f64 {
    let scale = 10f64.powi(decimals);
    (value * scale).round() / scale
}

/// How many ticks `step` places inside `min..=max`.
fn tick_count(min: f64, max: f64, step: f64) -> usize {
    if step <= 0.0 || !step.is_finite() {
        return usize::MAX;
    }
    let first = (min / step).ceil();
    let last = (max / step).floor();
    if !first.is_finite() || !last.is_finite() || last < first {
        return 0;
    }
    // Saturating rather than wrapping: a range too wide for the size is widened,
    // not truncated.
    (last - first) as usize + 1
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

/// The next rung up the same 1, 2, 5 ladder.
///
/// A decade, re-niced rather than doubled: `2` has to become `5` and not `4`,
/// because `4` is the one value on the ladder that a reader cannot recognise as
/// a step. The ladder is closed under this move, so a widened step is still a
/// round number.
fn wider_nice(step: f64) -> f64 {
    let wider = nice_number(step * 10.0);
    if wider > step { wider } else { step * 10.0 }
}

/// Candidate time-axis steps in milliseconds, from 100 ms to 1 day.
const TIME_STEPS_MS: &[i64] = &[
    100, 200, 500, 1_000, 2_000, 5_000, 10_000, 15_000, 30_000, 60_000, 120_000, 300_000, 600_000,
    900_000, 1_800_000, 3_600_000, 7_200_000, 10_800_000, 21_600_000, 43_200_000, 86_400_000,
];

/// Time-axis ticks aligned to step boundaries.
///
/// Every tick is a whole number of steps from one step boundary, computed as
/// `base + index * step` rather than by repeated addition, so a step that is not
/// a round number of milliseconds cannot drift the labels off their own grid.
/// `target` bounds the set for the same reason [`nice_ticks`] bounds the value
/// axis: it is the number of labels the drawn width can carry.
pub fn time_ticks(start_ms: i64, end_ms: i64, target: usize) -> Vec<i64> {
    if end_ms <= start_ms || target < 2 {
        return Vec::new();
    }
    let capacity = target.min(MAX_TICKS);
    let span = end_ms - start_ms;
    let step = TIME_STEPS_MS
        .iter()
        .copied()
        .find(|step| span / step <= capacity as i64)
        .unwrap_or(86_400_000);
    if step <= 0 {
        return Vec::new();
    }
    let base = start_ms.div_euclid(step) * step;
    // The first boundary at or after the window: a window that starts mid-step
    // would otherwise label a position the plot does not draw.
    let mut index = i64::from(base < start_ms);
    let mut ticks = Vec::new();
    while let Some(offset) = index.checked_mul(step) {
        let Some(tick) = base.checked_add(offset) else {
            break;
        };
        if tick > end_ms {
            break;
        }
        ticks.push(tick);
        if ticks.len() >= capacity {
            break;
        }
        index += 1;
    }
    ticks
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
