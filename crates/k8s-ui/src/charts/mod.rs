//! Line chart data, geometry, and numeric table support.
//!
//! Data flows from `k8s_core::metrics::TimeSeries` through normalization and downsampling
//! into `Series`. Geometry helpers are pure functions. `LineChartView` renders the chart on
//! gpui-kit's `Plot` primitive, and `ChartTable` provides the numeric values on its `DataTable`.

mod element;
mod geometry;
mod table;

pub use element::LineChartView;
pub use geometry::{
    PlotRect, downsample_points, format_clock_utc, format_offset, format_offset_short,
    nearest_sample, nice_ticks, pad_range, time_ticks, waiting_for_next_scrape,
};
pub use table::ChartTable;

use std::collections::BTreeSet;

use gpui_kit::{App, Hsla, SharedString};
use k8s_core::metrics::{NormalizedPoint, Sample, TimeSeries};

/// Maximum number of points in one plotted series after downsampling.
pub const MAX_PLOT_POINTS: usize = 512;

/// Data unit used for table, tooltip, accessibility, and axis formatting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    /// CPU millicores. The metrics server reports CPU in cores.
    Cpu,
    /// Bytes.
    Memory,
    /// Unitless count.
    Count,
}

impl Unit {
    /// Full value for tables, tooltips, and accessibility text.
    pub fn format(self, value: f64) -> String {
        match self {
            Self::Cpu => format!("{} cores", format_cores(value / 1000.0)),
            Self::Memory => format_bytes(value),
            Self::Count => format!("{value:.0}"),
        }
    }

    /// Compact value for an axis label. A unit the label does not itself carry is
    /// named in [`Unit::axis_name`].
    ///
    /// The Y gutter reserves six columns for a tick label and sizes the whole
    /// plot from that claim, so every formatter this calls keeps its output
    /// inside it in its own lane's terms, and the module test below holds the
    /// claim. The full-precision value is one hover away, and it is printed in
    /// the numeric table under the chart.
    pub fn axis_label(self, value: f64) -> String {
        match self {
            Self::Cpu => format_cores(value / 1000.0),
            Self::Memory => compact_bytes(value),
            Self::Count => compact_count(value),
        }
    }

    /// The variable a vertical axis measures, and the unit of a bare label.
    ///
    /// `charts.md › Best practices` allows short tick labels as long as the unit
    /// is named elsewhere on the chart. A bare `0.050` is not interpretable, so
    /// the CPU axis says what it is counting. Memory and count labels carry
    /// their own unit (`18.4Gi`), so a caption claiming bytes under them would
    /// contradict every tick printed beneath it.
    pub fn axis_name(self) -> &'static str {
        match self {
            Self::Cpu => "CPU (cores)",
            Self::Memory => "Memory",
            Self::Count => "Count",
        }
    }
}

fn format_cores(cores: f64) -> String {
    if cores == 0.0 {
        return "0".to_owned();
    }
    if cores >= 10.0 {
        format!("{cores:.0}")
    } else if cores >= 1.0 {
        format!("{cores:.1}")
    } else if cores >= 0.1 {
        format!("{cores:.2}")
    } else {
        format!("{cores:.3}")
    }
}

const KIB: f64 = 1024.0;
const MIB: f64 = KIB * 1024.0;
const GIB: f64 = MIB * 1024.0;
const TIB: f64 = GIB * 1024.0;

/// One compact axis label's worth of bytes.
///
/// The axis gutter reserves six columns, and this ladder trades precision to
/// stay inside it: one decimal until the digits alone would reach the ceiling,
/// none after — `512.0Gi` is seven columns, and the label would run into the
/// panel edge. The boundary is at the *round-down* of the hundred, not the
/// hundred itself, because `99.95` at one decimal prints `100.0` and the gutter
/// has to hold what the string actually reads. [`Unit::format`] keeps the
/// hundredths a table cell has room for.
fn compact_bytes(bytes: f64) -> String {
    if bytes >= TIB {
        let tib = bytes / TIB;
        if tib >= 99.95 {
            format!("{tib:.0}Ti")
        } else {
            format!("{tib:.1}Ti")
        }
    } else if bytes >= GIB {
        let gib = bytes / GIB;
        if gib >= 99.95 {
            format!("{gib:.0}Gi")
        } else {
            format!("{gib:.1}Gi")
        }
    } else if bytes >= MIB {
        format!("{:.0}Mi", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.0}Ki", bytes / KIB)
    } else {
        format!("{bytes:.0}B")
    }
}

/// One compact axis label's worth of count.
///
/// Plain digits while they fit the six-column gutter — a count axis labels
/// replicas and churn in the hundreds, and `512` reads truer than `0.5M` — and
/// SI suffixes past that, trading precision for columns on the same round-down
/// rule as [`compact_bytes`]. A count this far up the ladder has outgrown a
/// chart of pods, so the ladder ends at the biggest suffix that keeps the
/// gutter's claim.
fn compact_count(value: f64) -> String {
    const M: f64 = 1_000_000.0;
    const G: f64 = 1_000.0 * M;
    const T: f64 = 1_000.0 * G;
    if value < 999_999.5 {
        return format!("{value:.0}");
    }
    let (scaled, suffix) = if value >= T {
        (value / T, "T")
    } else if value >= G {
        (value / G, "G")
    } else {
        (value / M, "M")
    };
    if scaled >= 99.95 {
        format!("{scaled:.0}{suffix}")
    } else {
        format!("{scaled:.1}{suffix}")
    }
}

fn format_bytes(bytes: f64) -> String {
    if bytes >= GIB {
        format!("{:.2} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes / KIB)
    } else {
        format!("{bytes:.0} B")
    }
}

/// The stroke a series is drawn with, as an on/off pattern in data-font pixels.
///
/// Colour is never the only channel, and the redundancy has to live in the
/// **model** rather than in the element: an element that owns the pattern and a
/// series that owns the hue can disagree, and then a legend swatch is a coloured
/// dot beside a dashed line it does not describe. One [`SeriesStroke`] per
/// [`SeriesColor`] makes the pair a single fact, and the legend draws a length
/// of the same stroke the line uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeriesStroke {
    Solid,
    Dashed,
    Dotted,
    DashDot,
    DashDotDot,
}

impl SeriesStroke {
    /// The dash array in alternating on/off lengths, or `None` for a solid line.
    pub fn dash_array(self) -> Option<Vec<gpui_kit::Pixels>> {
        use gpui_kit::px;
        match self {
            // A solid line is the *absence* of a pattern, not a pattern of its
            // own, so the first slot is the only one that carries no redundancy.
            // It is the accent-adjacent primary, so it is also the one a reader
            // finds first, which is where a redundant cue costs least.
            Self::Solid => None,
            Self::Dashed => Some(vec![px(8.0), px(4.0)]),
            // One pixel on at the data font's own line weight: a round dot would
            // need an antialiased cap this painter does not have.
            Self::Dotted => Some(vec![px(1.0), px(4.0)]),
            Self::DashDot => Some(vec![px(8.0), px(3.0), px(1.5), px(3.0)]),
            Self::DashDotDot => Some(vec![px(8.0), px(3.0), px(1.5), px(3.0), px(1.5), px(3.0)]),
        }
    }
}

/// Series colors are a categorical ramp, not a status palette.
///
/// The variant names are historical. What matters is that a series never borrows
/// a status hue: an SRE reading a CPU chart should not see a red line and wonder
/// whether it means "error". Luminance is solved against the plot canvas by
/// [`crate::design::chart::series`], and hue is never the only channel because
/// [`SeriesColor::stroke`] gives every slot a dash pattern of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeriesColor {
    Accent,
    Info,
    Success,
    Warning,
    Error,
}

impl SeriesColor {
    pub fn for_index(index: usize) -> Self {
        match index % crate::design::SERIES_SLOTS {
            0 => Self::Accent,
            1 => Self::Info,
            2 => Self::Success,
            3 => Self::Warning,
            _ => Self::Error,
        }
    }

    fn accent_index(self) -> usize {
        match self {
            Self::Accent => 0,
            Self::Info => 1,
            Self::Success => 2,
            Self::Warning => 3,
            Self::Error => 4,
        }
    }

    pub fn color(self, cx: &App) -> Hsla {
        crate::design::chart::series(self.accent_index(), cx)
    }

    /// The stroke this slot is drawn with.
    ///
    /// Five patterns for [`crate::design::SERIES_SLOTS`] slots, so a repeated
    /// hue always arrives with a repeated stroke and never looks like a new
    /// series — the reason the slot count is five and not the length of the
    /// accent pool.
    pub fn stroke(self) -> SeriesStroke {
        match self {
            Self::Accent => SeriesStroke::Solid,
            Self::Info => SeriesStroke::Dashed,
            Self::Success => SeriesStroke::Dotted,
            Self::Warning => SeriesStroke::DashDot,
            Self::Error => SeriesStroke::DashDotDot,
        }
    }
}

/// A line series with points on a fixed interval grid. Missing values use None.
#[derive(Clone, Debug, PartialEq)]
pub struct Series {
    pub label: SharedString,
    pub unit: Unit,
    pub color: SeriesColor,
    /// Whether the line carries an area wash under it.
    ///
    /// The wash is a tint that says "this is the value's base", and it only
    /// works when the line above it is the boundary — so it is the *model's*
    /// decision, made once, and the element paints it or does not. It used to be
    /// a hint the element second-guessed: the element filled every series only
    /// when there were two or more of them, which is exactly the case where two
    /// washes stack into a third colour the reader then has to interpret, while
    /// the single-series case the callers ask for — one container's CPU, filled
    /// under its own 1.5px line — was the one case the element refused. A fill
    /// is never painted without the line that bounds it.
    pub filled: bool,
    pub points: Vec<NormalizedPoint>,
}

impl Series {
    /// Build a series from a sample buffer. Normalize and downsample the data.
    pub fn from_time_series(
        label: impl Into<SharedString>,
        unit: Unit,
        color: SeriesColor,
        series: &TimeSeries,
        interval_ms: i64,
    ) -> Self {
        Self {
            label: label.into(),
            unit,
            color,
            filled: true,
            points: downsample_points(&series.normalized(interval_ms), MAX_PLOT_POINTS),
        }
    }

    pub fn valued(&self) -> impl Iterator<Item = Sample> + '_ {
        self.points.iter().filter_map(|point| {
            point.value.map(|value| Sample {
                at_ms: point.at_ms,
                value,
            })
        })
    }

    pub fn is_empty(&self) -> bool {
        self.points.iter().all(|point| point.value.is_none())
    }

    pub fn min_max(&self) -> Option<(f64, f64)> {
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        for value in self.points.iter().filter_map(|point| point.value) {
            min = min.min(value);
            max = max.max(value);
        }
        (min.is_finite() && max.is_finite()).then_some((min, max))
    }
}

/// A numeric table row with time and series values.
#[derive(Clone, Debug, PartialEq)]
pub struct TableRow {
    pub at_ms: i64,
    /// UTC time for the table header.
    pub time: String,
    pub cells: Vec<String>,
}

/// Chart data with multiple series that share one unit and Y axis.
#[derive(Clone, Debug, PartialEq)]
pub struct ChartData {
    pub series: Vec<Series>,
    pub interval_ms: i64,
}

impl Default for ChartData {
    fn default() -> Self {
        Self {
            series: Vec::new(),
            interval_ms: 10_000,
        }
    }
}

impl ChartData {
    pub fn new(interval_ms: i64) -> Self {
        Self {
            series: Vec::new(),
            interval_ms,
        }
    }

    pub fn push_series(&mut self, series: Series) {
        self.series.push(series);
    }

    pub fn is_empty(&self) -> bool {
        self.series.iter().all(Series::is_empty)
    }

    /// Time span across all series, including gaps.
    pub fn time_range(&self) -> Option<(i64, i64)> {
        let mut start: Option<i64> = None;
        let mut end: Option<i64> = None;
        for series in &self.series {
            if let (Some(first), Some(last)) = (series.points.first(), series.points.last()) {
                start = Some(start.map_or(first.at_ms, |current| current.min(first.at_ms)));
                end = Some(end.map_or(last.at_ms, |current| current.max(last.at_ms)));
            }
        }
        start.zip(end)
    }

    /// Count valid samples by unique timestamp.
    pub fn sample_count(&self) -> usize {
        self.sample_times().len()
    }

    pub fn sample_times(&self) -> Vec<i64> {
        self.series
            .iter()
            .flat_map(|series| {
                series
                    .points
                    .iter()
                    .filter_map(|point| point.value.map(|_| point.at_ms))
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub fn accessible_value(&self, at_ms: i64) -> String {
        let values = self
            .series
            .iter()
            .map(|series| {
                let value = series
                    .points
                    .binary_search_by_key(&at_ms, |point| point.at_ms)
                    .ok()
                    .and_then(|index| series.points.get(index))
                    .and_then(|point| point.value)
                    .map_or_else(|| "No sample".to_owned(), |value| series.unit.format(value));
                format!("{}: {value}", series.label)
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("{}: {values}", format_clock_utc(at_ms))
    }

    pub fn value_range(&self) -> (f64, f64) {
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        for series in &self.series {
            if let Some((series_min, series_max)) = series.min_max() {
                min = min.min(series_min);
                max = max.max(series_max);
            }
        }
        if !min.is_finite() || !max.is_finite() {
            return (0.0, 1.0);
        }
        pad_range(min, max)
    }

    /// The first series unit, because all series share one Y axis.
    pub fn unit(&self) -> Unit {
        self.series.first().map_or(Unit::Cpu, |series| series.unit)
    }

    /// Headers for the numeric table.
    pub fn headers(&self) -> Vec<String> {
        let mut headers = vec!["Time (UTC)".to_owned()];
        headers.extend(self.series.iter().map(|series| series.label.to_string()));
        headers
    }

    /// Align rows by time, newest first. Missing cells use an em dash.
    pub fn table_rows(&self) -> Vec<TableRow> {
        let mut times: BTreeSet<i64> = BTreeSet::new();
        for series in &self.series {
            for point in &series.points {
                if point.value.is_some() {
                    times.insert(point.at_ms);
                }
            }
        }
        times
            .into_iter()
            .rev()
            .map(|at_ms| {
                let cells = self
                    .series
                    .iter()
                    .map(|series| {
                        series
                            .points
                            .binary_search_by_key(&at_ms, |point| point.at_ms)
                            .ok()
                            .and_then(|index| series.points.get(index))
                            .and_then(|point| point.value)
                            .map_or_else(|| "—".to_owned(), |value| series.unit.format(value))
                    })
                    .collect();
                TableRow {
                    at_ms,
                    time: format_clock_utc(at_ms),
                    cells,
                }
            })
            .collect()
    }

    /// Accessibility text with the first value, last value, minimum, and maximum.
    pub fn accessible_label(&self, title: &str) -> String {
        let mut parts = Vec::new();
        for series in &self.series {
            let values: Vec<f64> = series.valued().map(|sample| sample.value).collect();
            let (Some(first), Some(last)) = (values.first(), values.last()) else {
                continue;
            };
            let (min, max) = series.min_max().unwrap_or((*first, *first));
            parts.push(format!(
                "{}: {} samples. First: {}. Last: {}. Lowest: {}. Highest: {}",
                series.label,
                values.len(),
                series.unit.format(*first),
                series.unit.format(*last),
                series.unit.format(min),
                series.unit.format(max),
            ));
        }
        if parts.is_empty() {
            format!("{title}: no data")
        } else {
            format!("{title}: {}", parts.join(" "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Y gutter reserves six label columns (`element`'s `Y_LABEL_COLUMNS`)
    /// and sizes the whole plot from that claim, so a longer tick is a label
    /// clipped by the panel edge. `nice_ticks` only ever emits `m × 10^e` with
    /// m in {1, 2, 5}, so those are the tick sets each unit is held to, at every
    /// magnitude a pod chart reaches, plus the rungs where a label switches its
    /// unit or its precision — the places the claim used to break.
    #[test]
    fn axis_labels_hold_the_six_column_claim() {
        const COLUMNS: usize = 6;
        let mut nice_ticks = Vec::new();
        for e in 0..=9 {
            let decade = 10f64.powi(e);
            for mantissa in [1.0, 2.0, 5.0] {
                nice_ticks.push(mantissa * decade);
            }
        }
        let rungs: [(Unit, Vec<f64>); 3] = [
            // Millicores: a pod chart on a node-sized box reaches tens of cores,
            // and the sweep carries it to five hundred thousand — past anything
            // a core-count axis will ever name.
            (Unit::Cpu, nice_ticks[..27].to_vec()),
            // Bytes: the precision trade at a hundred gibibytes, and the unit
            // trade at a tebibyte.
            (
                Unit::Memory,
                [
                    nice_ticks.clone(),
                    vec![
                        99.95 * GIB - 1.0,
                        99.95 * GIB,
                        100.0 * GIB,
                        1023.0 * GIB,
                        TIB - 1.0,
                        TIB,
                        99.95 * TIB,
                        100.0 * TIB,
                    ],
                ]
                .concat(),
            ),
            // Counts: the digit ceiling, and every suffix rung past it.
            (
                Unit::Count,
                [
                    nice_ticks,
                    vec![
                        999_999.5 - 1.0,
                        999_999.5,
                        99.95e6 - 1.0,
                        99.95e6,
                        1.0e9,
                        1.0e12,
                    ],
                ]
                .concat(),
            ),
        ];
        for (unit, values) in rungs {
            for value in values {
                let label = unit.axis_label(value);
                assert!(
                    label.chars().count() <= COLUMNS,
                    "{unit:?} tick {value} labels {label:?}, which is {} columns — past the gutter",
                    label.chars().count()
                );
            }
        }
    }
}
