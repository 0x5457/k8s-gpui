//! Line chart data, geometry, and numeric table support.
//!
//! Data flows from `k8s_core::metrics::TimeSeries` through normalization and downsampling
//! into `Series`. Geometry helpers are pure functions. `LineChartView` renders the chart and
//! `ChartTable` provides the numeric values.

mod element;
mod geometry;
mod table;

pub use element::LineChartView;
pub use geometry::{
    PathPoint, PlotRect, downsample_points, format_clock_utc, format_offset, format_offset_short,
    map_point, map_points, nearest_by_x, nearest_sample, nice_ticks, pad_range, time_at_x,
    time_ticks, value_range, waiting_for_next_scrape,
};
pub use table::ChartTable;

use std::collections::BTreeSet;

use gpui::{App, Hsla, SharedString};
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

    /// Compact value for an axis label. The unit lives in [`Unit::axis_name`].
    pub fn axis_label(self, value: f64) -> String {
        match self {
            Self::Cpu => format_cores(value / 1000.0),
            Self::Memory => compact_bytes(value),
            Self::Count => format!("{value:.0}"),
        }
    }

    /// The variable a vertical axis measures, drawn on the axis itself.
    ///
    /// `charts.md › Best practices` allows short tick labels as long as the unit
    /// is named elsewhere on the chart. A bare "0.050" is not interpretable and
    /// the legend names containers rather than the quantity, so the axis has to
    /// carry the variable.
    pub fn axis_name(self) -> &'static str {
        match self {
            Self::Cpu => "CPU (cores)",
            Self::Memory => "Memory (bytes)",
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

fn compact_bytes(bytes: f64) -> String {
    if bytes >= GIB {
        format!("{:.1}Gi", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.0}Mi", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.0}Ki", bytes / KIB)
    } else {
        format!("{bytes:.0}B")
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

/// Series colors are a categorical ramp, not a status palette.
///
/// The variant names are historical. What matters is that a series never borrows
/// a status hue: an SRE reading a CPU chart should not see a red line and wonder
/// whether it means "error". Luminance is solved against the plot canvas by
/// [`crate::design::chart::series`], and hue is never the only channel because
/// `element` also varies the dash pattern per index.
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
}

/// A line series with points on a fixed interval grid. Missing values use None.
#[derive(Clone, Debug, PartialEq)]
pub struct Series {
    pub label: SharedString,
    pub unit: Unit,
    pub color: SeriesColor,
    /// Optional area fill below the line. The chart only fills when it plots two
    /// or more series, so a single series stays a line on a grid.
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

    fn series(label: &str, unit: Unit, values: &[(i64, Option<f64>)]) -> Series {
        Series {
            label: label.into(),
            unit,
            color: SeriesColor::Accent,
            filled: false,
            points: values
                .iter()
                .map(|(at_ms, value)| NormalizedPoint {
                    at_ms: *at_ms,
                    value: *value,
                })
                .collect(),
        }
    }

    #[test]
    fn categorical_series_indices_are_stable() {
        assert_eq!(SeriesColor::for_index(0), SeriesColor::Accent);
        assert_eq!(SeriesColor::for_index(1), SeriesColor::Info);
        assert_eq!(SeriesColor::for_index(2), SeriesColor::Success);
        assert_eq!(SeriesColor::for_index(3), SeriesColor::Warning);
        assert_eq!(SeriesColor::for_index(4), SeriesColor::Error);
        assert_eq!(SeriesColor::for_index(5), SeriesColor::Accent);
    }

    #[test]
    fn sample_times_and_accessible_values_follow_the_union_of_series() {
        let data = ChartData {
            series: vec![
                series("app", Unit::Cpu, &[(0, Some(100.0)), (20_000, Some(200.0))]),
                series("sidecar", Unit::Cpu, &[(10_000, Some(50.0))]),
            ],
            interval_ms: 10_000,
        };
        assert_eq!(data.sample_times(), [0, 10_000, 20_000]);
        let value = data.accessible_value(10_000);
        assert!(value.contains("00:00:10"));
        assert!(value.contains("app: No sample"));
        assert!(value.contains("sidecar: 0.050 cores"));
    }

    #[test]
    fn unit_formats_cpu_memory_and_count() {
        assert_eq!(Unit::Cpu.format(250.0), "0.25 cores");
        assert_eq!(Unit::Cpu.format(1500.0), "1.5 cores");
        assert_eq!(Unit::Cpu.format(0.0), "0 cores");
        assert_eq!(Unit::Cpu.axis_label(50.0), "0.050");
        assert_eq!(Unit::Memory.format(134_217_728.0), "128.0 MiB");
        assert_eq!(Unit::Memory.axis_label(134_217_728.0), "128Mi");
        assert_eq!(Unit::Memory.axis_label(512.0), "512B");
        assert_eq!(Unit::Count.format(12.4), "12");
    }

    #[test]
    fn from_time_series_normalizes_gaps() {
        let mut raw = TimeSeries::new(16);
        raw.push(0, 100.0);
        raw.push(10_000, 200.0);
        raw.push(30_000, 300.0);
        let series = Series::from_time_series("CPU", Unit::Cpu, SeriesColor::Accent, &raw, 10_000);
        assert_eq!(series.points.len(), 4);
        assert_eq!(
            series.points[2].value, None,
            "missing samples remain empty slots"
        );
        assert_eq!(series.min_max(), Some((100.0, 300.0)));
        assert_eq!(series.valued().count(), 3);
        assert!(!series.is_empty());
    }

    #[test]
    fn table_rows_align_series_and_mark_missing() {
        let data = ChartData {
            series: vec![
                series("CPU", Unit::Cpu, &[(0, Some(100.0)), (10_000, Some(200.0))]),
                series("Memory", Unit::Memory, &[(10_000, Some(1024.0))]),
            ],
            interval_ms: 10_000,
        };
        let rows = data.table_rows();
        assert_eq!(rows.len(), 2, "align rows by the union of timestamps");
        assert_eq!(rows[0].at_ms, 10_000, "newest row comes first");
        assert_eq!(rows[0].cells, ["0.20 cores", "1.0 KiB"]);
        assert_eq!(rows[1].cells, ["0.10 cores", "—"]);
        assert_eq!(rows[0].time, format_clock_utc(10_000));
        assert_eq!(
            data.headers(),
            ["Time (UTC)", "CPU", "Memory"],
            "headers and cells keep the same order"
        );
    }

    #[test]
    fn accessible_label_summarizes_values() {
        let data = ChartData {
            series: vec![series(
                "CPU",
                Unit::Cpu,
                &[
                    (0, Some(100.0)),
                    (10_000, Some(500.0)),
                    (20_000, Some(300.0)),
                ],
            )],
            interval_ms: 10_000,
        };
        let label = data.accessible_label("Pod CPU");
        assert!(label.starts_with("Pod CPU: CPU: 3 samples"));
        assert!(label.contains("First: 0.10 cores. Last: 0.30 cores"));
        assert!(label.contains("Lowest: 0.10 cores. Highest: 0.50 cores"));
        assert_eq!(ChartData::default().accessible_label("CPU"), "CPU: no data");
    }

    #[test]
    fn sample_count_reports_scrapes_not_series() {
        let one_scrape = ChartData {
            series: vec![
                series("app", Unit::Cpu, &[(0, Some(1.0))]),
                series("sidecar", Unit::Cpu, &[(0, Some(2.0))]),
            ],
            interval_ms: 10_000,
        };
        assert_eq!(one_scrape.sample_count(), 1);
        let two_scrapes = ChartData {
            series: vec![series(
                "app",
                Unit::Cpu,
                &[(0, Some(1.0)), (10_000, Some(2.0))],
            )],
            interval_ms: 10_000,
        };
        assert_eq!(two_scrapes.sample_count(), 2);
        let staggered = ChartData {
            series: vec![
                series("app", Unit::Cpu, &[(0, Some(1.0))]),
                series("sidecar", Unit::Cpu, &[(10_000, Some(2.0))]),
            ],
            interval_ms: 10_000,
        };
        assert_eq!(
            staggered.sample_count(),
            2,
            "samples from different series count together"
        );
        assert_eq!(ChartData::default().sample_count(), 0);
    }

    #[test]
    fn time_and_value_ranges_cover_all_series() {
        let data = ChartData {
            series: vec![
                series("a", Unit::Cpu, &[(0, Some(10.0)), (10_000, Some(20.0))]),
                series("b", Unit::Cpu, &[(5_000, Some(100.0)), (15_000, Some(5.0))]),
            ],
            interval_ms: 5_000,
        };
        assert_eq!(data.time_range(), Some((0, 15_000)));
        let (min, max) = data.value_range();
        assert_eq!(min, 0.0);
        assert!(
            (max - 110.0).abs() < 1e-9,
            "upper bound leaves 10 percent headroom: {max}"
        );
        assert_eq!(data.unit(), Unit::Cpu);
        assert!(!data.is_empty());
        assert!(ChartData::default().time_range().is_none());
    }

    #[test]
    fn all_gap_series_counts_as_empty() {
        let data = ChartData {
            series: vec![series("a", Unit::Cpu, &[(0, None)])],
            interval_ms: 1_000,
        };
        assert!(data.is_empty());
    }
}
