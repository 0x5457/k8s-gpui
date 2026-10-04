//! The metrics plot: grid, axes, series marks, legend, and the value overlay.
//!
//! gpui-kit's `Plot` owns the element plumbing, the hover state machine, and
//! the crosshair, dot, and tooltip overlay. The app supplies the part a
//! ready-made chart cannot: one scale shared by every series, gap segments, a
//! tick set in the reader's data font, a legend, and a per-series dash pattern.
//!
//! `plot_rect_for` reserves the axis space inside the content mask.

use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::plot::scale::{Scale, ScaleLinear};
use gpui_kit::component::plot::tooltip::{CrossLine, Dot, PlotHover, Tooltip, TooltipState};
use gpui_kit::component::plot::{
    AXIS_GAP, AxisLabelSide, AxisText, Grid, IntoPlot, Plot, PlotAxis,
};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, App, Bounds, Div, ElementId, Hsla, KeyDownEvent, PathBuilder, Pixels, Point, Role,
    SharedString, Size, TextAlign, Window, div, point, px, size, transparent_black,
};

use k8s_core::metrics::NormalizedPoint;

use crate::design::{self, role, space, text};
use crate::panels::common::empty_state;

use super::ChartData;
use super::geometry::{self, PlotRect};
use super::{SeriesColor, SeriesStroke};

/// Y tick labels use compact units, so `1023Gi` is the longest label possible.
const Y_LABEL_COLUMNS: f32 = 6.0;
/// Line width of every series' stroke.
const LINE_WIDTH: f32 = 1.5;
/// Diameter of the dot on a hovered or scrubbed sample, and of the mark a
/// one-sample series draws in place of the line it has not earned yet.
const DOT_SIZE: f32 = 6.0;
/// Area wash alpha. The line above it is the boundary, so this is a tint under
/// a stroke and not a second mark.
const AREA_OPACITY: f32 = 0.08;
/// Length of the stroke sample a legend entry draws beside its name.
const LEGEND_SWATCH: f32 = 16.0;
const MIN_PLOT_SIZE: f32 = 8.0;
/// gpui-kit's `PlotAxis` draws a y label at `tick - 5`, half of its own default
/// text size, so the tick the app hands it carries the correction back. Without
/// it the label sits half its height below the grid line it belongs to.
const Y_LABEL_NUDGE: f32 = 5.0;
/// Advance of `0` on the UI face, as a fraction of the size — the same constant
/// `settings::MONO_ADVANCE_EM` states for the data face.
///
/// The plot's own labels are drawn in the window's UI face (gpui-kit's axis has
/// no font of its own to give it), so a gutter measured in *data*-font columns
/// was reserving the wrong box: raising the data font size grew a gutter that
/// the labels do not use. The gutter is chrome, so it follows the chrome's own
/// size and stays still when the reader resizes their data font.
const UI_ADVANCE_EM: f32 = 0.6;

/// The size a chart's axis labels are drawn at.
///
/// `text::LABEL`, not `text::MICRO` and not the data font: an axis is how a
/// reader reads a *value*, and `Design guides > Designing data-heavy interfaces`
/// asks for values that can be read without zooming. It is chrome, so it is a
/// product token and not a mirror of the data font — the numbers on the axis and
/// the numbers in the table below it are the same measurements and should not
/// be two sizes for the same quantity.
const AXIS_LABEL: Pixels = text::LABEL;
/// Ink for an axis label.
///
/// The quietest ink in the scale. An axis label is a caption on a scale rather
/// than a value the reader came for: the value is one hover away, and it is also
/// printed in full in the table underneath the chart. `fg_secondary` put a
/// quieter caption in the heavier ink, so the chart's own chrome was the second
/// loudest thing in the panel after the line.
fn axis_ink(cx: &App) -> Hsla {
    role::fg_tertiary(cx)
}

/// Vertical pitch one horizontal rule needs, in plot pixels.
///
/// Four rules is the ceiling and the pitch is what produces them: a 130px plot
/// — the height the Metrics tab draws its charts at — gets three, and only a
/// plot taller than about 190px earns the fourth. A gridline per 40px, which is
/// what the plot used to ask for, put six on the same 130px chart, and a chart
/// with a rule every forty pixels is a spreadsheet drawn in grey.
const Y_TICK_PITCH: f32 = 56.0;
/// The most horizontal rules a chart may draw.
const MAX_Y_TICKS: usize = 4;

/// Horizontal pitch one time label needs, in plot pixels.
///
/// Wider than a value rule because a time label is a clock (`-13m`, `-2h40m`)
/// rather than a bare number, and two clocks three characters apart read as one
/// mangled string.
const X_TICK_PITCH: f32 = 90.0;
/// The most time labels a chart may draw.
const MAX_X_TICKS: usize = 8;

/// The band the legend owns above the plot, when there is more than one series.
///
/// `space::LG` and not the `space::XL` the plot used to reserve: a 24px band for
/// a 14px caption line is 10px of nothing, and on the 132px chart the Metrics
/// tab draws it was a fifth of the whole element.
const LEGEND_BAND: Pixels = space::LG;

/// Gutter reserved for the Y tick labels: the label column plus its trailing gap.
///
/// A pure function of the label's own size, so the painted plot always matches
/// the plot the hover and the keyboard scrub resolve against.
fn y_axis_gutter() -> f32 {
    f32::from(AXIS_LABEL) * UI_ADVANCE_EM * Y_LABEL_COLUMNS + f32::from(space::SM)
}

/// Chart plot rectangle after axis padding.
fn plot_rect_for(size: Size<Pixels>, has_legend: bool) -> PlotRect {
    let width = f32::from(size.width);
    let height = f32::from(size.height);
    // A legend owns the top band; otherwise the plot starts at the edge gap.
    let top = if has_legend {
        f32::from(LEGEND_BAND)
    } else {
        f32::from(space::SM)
    };
    let gutter = y_axis_gutter();
    let right = f32::from(space::SM);
    PlotRect {
        x: gutter,
        y: top,
        width: (width - gutter - right).max(MIN_PLOT_SIZE),
        // gpui-kit's axis owns the band under the plot: it draws the x tick
        // labels there, so the plot hands it the component's axis gap.
        height: (height - top - AXIS_GAP).max(MIN_PLOT_SIZE),
    }
}

/// The frame's scales: where the plot sits, and how a timestamp and a value land
/// inside it.
///
/// Every series shares one time scale and one value scale, which is the reason
/// the app keeps its own plot: a chart built per series would spread two series
/// that sampled at different rates over different widths.
struct PlotScale {
    /// Plot rectangle, in element-local pixels.
    plot: PlotRect,
    /// Every sample timestamp in the data, ascending. It doubles as the time
    /// domain, so the scale can resolve a cursor position back to a timestamp.
    times: Vec<f64>,
    /// Time domain onto the plot width.
    time: ScaleLinear<f64>,
    /// Padded value domain onto the plot height, with the maximum at the top.
    value: ScaleLinear<f64>,
}

impl PlotScale {
    /// Horizontal position inside the plot, in plot-local pixels.
    fn x(&self, at_ms: i64) -> f32 {
        self.time.tick(&(at_ms as f64)).unwrap_or_default()
    }

    /// Vertical position inside the plot, in plot-local pixels.
    fn y(&self, value: f64) -> f32 {
        self.value.tick(&value).unwrap_or_default()
    }

    /// The sample nearest a cursor position.
    fn at_ms(&self, cursor: Point<Pixels>) -> Option<i64> {
        let x = f32::from(cursor.x) - self.plot.x;
        if x < 0.0 {
            return None;
        }
        let (index, _) = self.time.least_index_with_domain(x, &self.times);
        self.times.get(index).map(|at_ms| *at_ms as i64)
    }

    /// The first and last sample time, as the domain the ticks span.
    fn span(&self) -> (i64, i64) {
        (
            self.times.first().copied().unwrap_or_default() as i64,
            self.times.last().copied().unwrap_or_default() as i64,
        )
    }
}

/// Line chart view with data and a keyboard reading position.
pub struct LineChartView {
    data: Rc<ChartData>,
    title: SharedString,
    id: SharedString,
    scrub: Option<i64>,
}

impl Default for LineChartView {
    fn default() -> Self {
        Self::new()
    }
}

impl LineChartView {
    pub fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        Self {
            data: Rc::new(ChartData::default()),
            title: "Metrics".into(),
            id: format!("line-chart-{id}").into(),
            scrub: None,
        }
    }

    pub fn set_title(&mut self, title: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.title = title.into();
        cx.notify();
    }

    /// Replace the samples without throwing away the reading position.
    ///
    /// A live scrape replaces the window while the user is still reading it. The
    /// mouse hover is transient: gpui-kit's plot resolves it from the live cursor
    /// on every frame, so nothing has to be kept or dropped for it. The keyboard
    /// scrub is a position the user chose with arrow keys; dropping it makes the
    /// next Left/Right restart from the newest sample and the crosshair jumps
    /// back, so the timestamp is kept and moved onto the sample grid by
    /// `clamped_scrub` when the window slides.
    pub fn set_data(&mut self, data: impl Into<Rc<ChartData>>, cx: &mut Context<Self>) {
        self.data = data.into();
        self.scrub = self.clamped_scrub(self.scrub);
        cx.notify();
    }

    /// Read-only data shared by the table and time range.
    pub fn data(&self) -> &ChartData {
        &self.data
    }

    /// Shared data handle for the chart and table.
    pub fn data_rc(&self) -> Rc<ChartData> {
        Rc::clone(&self.data)
    }

    /// The scrubbed sample, moved onto the samples that exist now.
    ///
    /// The scrape window slides, so a kept timestamp can fall off either end or
    /// stop matching a sample. Returning `None` when there is nothing to read
    /// lets the chart fall back to the cursor instead of drawing a crosshair on a
    /// sample that no longer exists.
    fn clamped_scrub(&self, at_ms: Option<i64>) -> Option<i64> {
        nearest_sample_time(&self.data.sample_times(), at_ms?)
    }

    fn move_scrub(&mut self, direction: i8, cx: &mut Context<Self>) -> bool {
        let times = self.data.sample_times();
        let current = self.clamped_scrub(self.scrub);
        let Some(target) = scrub_index(&times, current, direction) else {
            return false;
        };
        let next = times[target];
        if self.scrub != Some(next) {
            self.scrub = Some(next);
            cx.notify();
        }
        true
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.control || keystroke.modifiers.alt || keystroke.modifiers.platform {
            return;
        }
        match keystroke.key.as_str() {
            "left" => {
                if self.move_scrub(-1, cx) {
                    cx.stop_propagation();
                }
            }
            "right" => {
                if self.move_scrub(1, cx) {
                    cx.stop_propagation();
                }
            }
            "escape" => {
                self.scrub = None;
                cx.notify();
                cx.stop_propagation();
            }
            _ => {}
        }
    }
}

/// Nearest sample to a timestamp in an ascending time list.
fn nearest_sample_time(times: &[i64], at_ms: i64) -> Option<i64> {
    let after = times.partition_point(|time| *time < at_ms);
    let before = after.checked_sub(1).map(|index| times[index]);
    let after = times.get(after).copied();
    match (before, after) {
        (Some(before), Some(after)) => Some(if at_ms - before <= after - at_ms {
            before
        } else {
            after
        }),
        (Some(before), None) => Some(before),
        (None, after) => after,
    }
}

fn scrub_index(times: &[i64], current: Option<i64>, direction: i8) -> Option<usize> {
    if times.is_empty() {
        return None;
    }
    // A kept scrub can sit off the grid after the window slid, so it starts from
    // the nearest sample instead of falling back to an end of the window.
    let index = current
        .and_then(|at_ms| nearest_sample_time(times, at_ms))
        .and_then(|nearest| times.binary_search(&nearest).ok())
        .unwrap_or(if direction < 0 { 0 } else { times.len() - 1 });
    Some(if direction < 0 {
        index.saturating_sub(1)
    } else {
        (index + 1).min(times.len() - 1)
    })
}

impl Render for LineChartView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let data = Rc::clone(&self.data);
        let label: SharedString = data.accessible_label(&self.title).into();
        // `charts.md › Enhancing the accessibility of a chart` asks a chart to
        // identify its type, explain what each axis represents, and give the axis
        // bounds. A description that only names the horizontal axis leaves the
        // quantity and the scale unstated.
        let (low, high) = data.value_range();
        let unit = data.unit();
        let mut description = format!(
            "Line chart. The horizontal axis is time. The vertical axis is {}, and it runs from {} to {}. Use Left and Right to move through samples. Use Escape to clear the selection. The numeric table below shows the same values.",
            unit.axis_name(),
            unit.format(low),
            unit.format(high),
        );
        if data.sample_count() < 2 {
            description = format!(
                "{} {description}",
                geometry::waiting_for_next_scrape(data.interval_ms)
            );
        }
        let scrub = self.clamped_scrub(self.scrub);
        let scrub_value = scrub
            .map(|at_ms| data.accessible_value(at_ms))
            .unwrap_or_default();
        let focus_border = design::focus::border(cx);

        div()
            .id(self.id.clone())
            // A chart the user can focus and scrub publishes a value, so it is a
            // group that owns one. `Role::Image` is for a static picture and
            // cannot carry `aria_value` or a tab stop honestly.
            .role(Role::Group)
            .tab_group()
            .tab_index(4)
            .accessibility_id(self.id.clone())
            .aria_label(label)
            .aria_description(description)
            .aria_keyshortcuts("Left Right Escape")
            .aria_value(scrub_value)
            .relative()
            .size_full()
            // The surface around the chart owns the visible frame, so the chart
            // only needs an edge to paint its focus ring onto.
            .border_1()
            .border_color(transparent_black())
            .focus_visible(move |style| style.border_color(focus_border))
            .on_key_down(cx.listener(Self::on_key_down))
            .child(MetricsPlot {
                data,
                id: ElementId::from(format!("{}-plot", self.id)),
                scrub,
                cursor_inside: false,
                scale: None,
            })
    }
}

/// The chart itself: gpui-kit's plot primitive, fed the app's scales and marks.
#[derive(IntoPlot)]
struct MetricsPlot {
    data: Rc<ChartData>,
    id: ElementId,
    /// The reading position the reader chose with the arrow keys.
    scrub: Option<i64>,
    /// Whether the cursor is over the plot. The crosshair follows the cursor
    /// while it is there, so the scrub only draws once the cursor is gone.
    ///
    /// gpui-kit resolves the hover after the overlay is laid out, so the frame
    /// the cursor arrives on still carries the scrub's overlay under it.
    cursor_inside: bool,
    /// Built once per frame in `prepaint` and read from then on.
    scale: Option<PlotScale>,
}

impl MetricsPlot {
    fn has_legend(&self) -> bool {
        self.data.series.len() > 1
    }

    /// The frame's scales, built in `prepaint` before anything reads them.
    fn scale(&self) -> &PlotScale {
        self.scale
            .as_ref()
            .expect("the plot builds its scales before it reads them")
    }

    fn scale_for(&self, size: Size<Pixels>) -> PlotScale {
        let plot = plot_rect_for(size, self.has_legend());
        let times: Vec<f64> = self
            .data
            .sample_times()
            .into_iter()
            .map(|at_ms| at_ms as f64)
            .collect();
        let (start, end) = self.data.time_range().unwrap_or((0, 0));
        let (low, high) = self.data.value_range();
        PlotScale {
            plot,
            time: ScaleLinear::new(vec![start as f64, end as f64], vec![0., plot.width]),
            value: ScaleLinear::new(vec![low, high], vec![plot.height, 0.]),
            times,
        }
    }

    /// The nearest sample each series has at a timestamp, with its colour.
    ///
    /// The nearest sample is what an overlay reads: a series can be missing the
    /// sample under the cursor, and the reader still needs the nearest value it
    /// actually has.
    fn marks_at(&self, at_ms: i64, scale: &PlotScale, cx: &App) -> Vec<(Point<Pixels>, Hsla)> {
        self.data
            .series
            .iter()
            .filter_map(|series| {
                let sample = geometry::nearest_sample(&series.points, at_ms)?;
                let dot = point(
                    px(scale.plot.x + scale.x(sample.at_ms)),
                    px(scale.plot.y + scale.y(sample.value)),
                );
                Some((dot, series.color.color(cx)))
            })
            .collect()
    }

    /// How far back from the newest sample a timestamp sits.
    fn offset_at(&self, at_ms: i64) -> String {
        match self.data.time_range() {
            Some((_, end)) => geometry::format_offset(end - at_ms),
            None => "now".to_owned(),
        }
    }

    /// The crosshair, the dots, and the value box for one timestamp.
    ///
    /// The cursor and the keyboard build the same overlay, so a reader who scrubs
    /// with the arrow keys sees exactly what a reader with a cursor sees. `focus`
    /// is `None` for the cursor, where gpui-kit already eases the overlay in and
    /// out with the hover.
    fn overlay(
        &self,
        at_ms: i64,
        cursor: Point<Pixels>,
        bounds: Bounds<Pixels>,
        focus: Option<f32>,
        cx: &App,
    ) -> AnyElement {
        let Some(scale) = self.scale.as_ref() else {
            return div().size_full().into_any_element();
        };
        let plot = scale.plot;
        let x = plot.x + scale.x(at_ms);
        // The clock is the fixed reading; the offset says how far back it sits,
        // which is the part a reader cannot work out from the axis.
        let mut tooltip = Tooltip::new(point(px(x), cursor.y), bounds.size)
            .gap(space::SM)
            .cross_line(
                CrossLine::new(point(px(x), px(plot.y)))
                    .band(px(1.))
                    .span(plot.y, plot.height),
            )
            .dots(
                self.marks_at(at_ms, scale, cx)
                    .into_iter()
                    .map(|(dot, color)| Dot::new(dot).size(px(DOT_SIZE)).fill(color)),
            )
            .title(format!(
                "{} · {}",
                geometry::format_clock_utc(at_ms),
                self.offset_at(at_ms)
            ));
        for series in &self.data.series {
            let value = geometry::nearest_sample(&series.points, at_ms).map_or_else(
                || "No sample".to_owned(),
                |sample| series.unit.format(sample.value),
            );
            tooltip = tooltip.row(series.color.color(cx), series.label.clone(), value);
        }
        match focus {
            Some(focus) => tooltip.focus(focus),
            None => tooltip,
        }
        .into_any_element()
    }

    /// The series names, each beside a length of its own stroke.
    ///
    /// The swatch is a *sample of the line*, not a coloured dot. A dot can only
    /// carry hue, so a legend of dots is a legend that works for exactly the
    /// readers the dash patterns exist for: two series that repeat a hue
    /// repeat a stroke, and a row of eight-pixel circles beside a dotted line
    /// and a dashed one is a key that describes nothing. A 16px run of the real
    /// pattern is a key for a 400px line.
    ///
    /// The names are prose, not numbers, so they read in the UI font at the
    /// caption role — the role the series names use everywhere else in the panel
    /// — instead of the data font the axis labels use.
    fn legend(&self, cx: &App) -> AnyElement {
        let band = self.scale().plot.y;
        h_flex()
            .id("chart-legend")
            .role(Role::Group)
            .aria_label("Series in this chart")
            .absolute()
            .top_0()
            .left_0()
            .h(px(band))
            .w_full()
            .max_w_full()
            .overflow_hidden()
            .items_center()
            .justify_center()
            .gap(space::LG)
            .children(self.data.series.iter().map(|series| {
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap(space::XS)
                    .child(stroke_sample(series.color, cx))
                    .child(
                        div()
                            .text_size(text::CAPTION)
                            .line_height(text::CAPTION_LINE_HEIGHT)
                            .text_color(role::fg_secondary(cx))
                            .whitespace_nowrap()
                            .child(series.label.clone()),
                    )
            }))
            .into_any_element()
    }
}

impl Plot for MetricsPlot {
    fn prepaint(
        &mut self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Vec<AnyElement> {
        self.scale = Some(self.scale_for(bounds.size));
        let mut children = Vec::new();
        if !self.data.is_empty() {
            if self.has_legend() {
                children.push(self.legend(cx));
            }
            // The keyboard reading position draws the same overlay the cursor
            // does, but only while the cursor is somewhere else.
            if !self.cursor_inside
                && let Some(at_ms) = self.scrub
            {
                let scale = self.scale();
                let cursor = point(px(scale.plot.x + scale.x(at_ms)), px(scale.plot.y));
                children.push(self.overlay(at_ms, cursor, bounds, Some(1.), cx));
            }
        }
        // Two states, and the difference between them is whether the plot has a
        // mark in it. Both used to be one caption block, which made a chart
        // waiting for its first scrape and a chart holding a single sample
        // indistinguishable, and made this the only surface in the product whose
        // empty state was not the app's.
        if self.data.is_empty() {
            children.push(empty_note(geometry::waiting_for_next_scrape(
                self.data.interval_ms,
            )));
        } else if self.data.sample_count() < 2 {
            // One sample repeats tick labels, so the status line says what is
            // missing instead of the axis claiming a range the data never reached.
            children.push(status_note(
                geometry::waiting_for_next_scrape(self.data.interval_ms),
                cx,
            ));
        }
        if children.is_empty() {
            return Vec::new();
        }
        let mut layer = div()
            .size_full()
            .relative()
            .children(children)
            .into_any_element();
        layer.prepaint_as_root(bounds.origin, bounds.size.into(), window, cx);
        vec![layer]
    }

    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        if self.data.is_empty() {
            return;
        }
        let scale = match self.scale.take() {
            Some(scale) => scale,
            None => self.scale_for(bounds.size),
        };
        let plot = scale.plot;
        let plot_bounds = Bounds::new(
            point(px(plot.x), px(plot.y)),
            size(px(plot.width), px(plot.height)),
        );
        let unit = self.data.unit();
        // One sample has no interval to place ticks on, so the axes would read
        // as covering a range the data never reached.
        let plotted = self.data.sample_count() >= 2;
        let (start, end) = scale.span();
        let (low, high) = self.data.value_range();
        let value_ticks = geometry::nice_ticks(low, high, y_tick_count(plot.height))
            .into_iter()
            .map(|tick| (tick, scale.y(tick)))
            .collect::<Vec<_>>();
        let time_ticks = geometry::time_ticks(start, end, x_tick_count(plot.width))
            .into_iter()
            .map(|tick| (tick, scale.x(tick)))
            .collect::<Vec<_>>();

        // One horizontal rule per value tick, and nothing else.
        //
        // The plot drew a rule at *every* x tick as well, so a two-hour window
        // with eight time labels was a chart with fourteen grey lines in it. A
        // time axis is continuous: the reader needs to know where the ends are,
        // which is what the x labels are for, and a rule between every pair of
        // them says nothing except that the series is time. The value rules are
        // the ones that let a reader read a height off the line without moving
        // the pointer, so those stay — three or four of them, and never a
        // vertical one.
        //
        // `role::border_subtle` and not the skin's `border_variant`, because the
        // product draws three strokes and a chart's grid is the quietest of
        // them: it is behind the data, not a boundary a reader interacts with.
        if plotted {
            Grid::new()
                .y(value_ticks.iter().map(|(_, y)| *y).collect::<Vec<_>>())
                .stroke(role::border_subtle(cx))
                .paint(&plot_bounds, window);
        }

        // gpui-kit's axis draws the tick labels only once it knows where the axis
        // line sits, so the line comes first.
        //
        // Neither line is drawn. `UI-SPEC` §4.4 allows a rule on a table
        // header, an input and a panel divider, and an axis line is none of
        // those: a chart on the content plane has no frame to have a border. The
        // one line that *is* meaningful is the value baseline at zero, and it is
        // already one of the rules above — the value range is padded so its low
        // tick is the bottom of the plot, and for the non-negative quantities
        // this product plots that tick is zero. The x axis has no meaningful
        // baseline at all: time has no zero, so the line under the plot was a
        // rule with nothing on the other side of it.
        let mut axis = PlotAxis::new()
            .stroke(role::border_subtle(cx))
            .x(px(plot.height))
            .x_axis(false)
            .y_axis(false);
        if plotted {
            // The correction is against the label's own size, since that is what
            // the component halves when it places the label.
            let nudge = Y_LABEL_NUDGE - f32::from(AXIS_LABEL) / 2.0;
            let last = time_ticks.len().saturating_sub(1);
            axis = axis
                .y(px(0.))
                .y_label_side(AxisLabelSide::Start)
                .y_label(
                    value_ticks
                        .iter()
                        .map(|(tick, y)| {
                            AxisText::new(unit.axis_label(*tick), *y + nudge, axis_ink(cx))
                                .font_size(AXIS_LABEL)
                                .align(TextAlign::Right)
                        })
                        .collect::<Vec<_>>(),
                )
                .x_label(
                    time_ticks
                        .iter()
                        .enumerate()
                        .map(|(index, (tick, x))| {
                            // The edge labels align inward so no glyph crosses the
                            // plot frame.
                            let align = match index {
                                0 if time_ticks.len() == 1 => TextAlign::Center,
                                0 => TextAlign::Left,
                                index if index == last => TextAlign::Right,
                                _ => TextAlign::Center,
                            };
                            AxisText::new(
                                geometry::format_offset_short(end - tick),
                                *x,
                                axis_ink(cx),
                            )
                            .font_size(AXIS_LABEL)
                            .align(align)
                        })
                        .collect::<Vec<_>>(),
                );
        }
        axis.paint(&plot_bounds, window, cx);

        // The wash is the model's decision, taken in
        // [`super::Series::filled`], and the line is always painted above it — a
        // tint with no boundary is not a mark, it is a second background.
        for series in &self.data.series {
            let color = series.color.color(cx);
            let stroke = series.color.stroke();
            for segment in segments(&series.points, &scale) {
                if series.filled {
                    paint_area(window, &segment, plot_bounds, plot.height, color);
                }
                paint_polyline(window, &segment, plot_bounds, color, stroke);
            }
        }
    }

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn tooltip_state(
        &self,
        position: Point<Pixels>,
        _bounds: Bounds<Pixels>,
        cx: &App,
    ) -> Option<TooltipState> {
        let scale = self.scale.as_ref()?;
        // The axis gutter is not the plot: a cursor over a tick label has not
        // landed on a sample.
        if !scale
            .plot
            .contains(f32::from(position.x), f32::from(position.y))
        {
            return None;
        }
        let at_ms = scale.at_ms(position)?;
        let index = scale.times.iter().position(|time| *time as i64 == at_ms)?;
        let dots = self
            .marks_at(at_ms, scale, cx)
            .into_iter()
            .map(|(dot, _)| dot)
            .collect();
        Some(TooltipState::new(
            index,
            point(px(scale.plot.x + scale.x(at_ms)), position.y),
            dots,
        ))
    }

    fn hover(&mut self, hover: Option<&PlotHover>, _window: &mut Window, _cx: &mut App) {
        self.cursor_inside = hover.is_some_and(PlotHover::is_hovered);
    }

    fn tooltip(
        &self,
        state: &TooltipState,
        cursor: Point<Pixels>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let at_ms = *self.scale.as_ref()?.times.get(state.index)? as i64;
        Some(self.overlay(at_ms, cursor, bounds, None, cx))
    }
}

/// The chart with nothing on it at all.
///
/// The plot used to answer with two lines of caption type centred in an empty
/// rectangle, which is a caption on nothing, and it was the only empty state in
/// the product that was not the app's. This chart's own table, a few lines below
/// it, already answers the same wait with the shared primitive, so the two now
/// speak one language and a reader who has seen one has seen the other.
///
/// `LoaderCircle` is the honest glyph: the state really is a wait for a scrape,
/// and the primitive turns it into the app's spinner, which is the one that stops
/// for a reader who asked for less motion.
fn empty_note(hint: String) -> AnyElement {
    empty_state(IconName::LoaderCircle, "No samples yet", hint)
}

/// A caption on a plot that has one mark in it.
///
/// **A caption and not an empty state**, and the distinction is the whole of it: a
/// single sample puts a dot on the plot and the reader is looking at a chart, so
/// what the line has to say is what the chart is missing, which is the next
/// scrape, and not the fact that there is nothing here. The empty plot gets the
/// empty state above because on that one there genuinely is nothing.
///
/// Prose rather than data, so it reads in the UI font at the caption role and
/// matches the panel text around the chart instead of arriving as the only
/// monospaced sentence on the surface. The ink is `fg_tertiary`, because this is
/// a caption and the chart behind it is holding a single sample: a status line in
/// `fg_secondary` is the loudest thing on a panel whose real content is one point.
fn status_note(line: String, cx: &App) -> AnyElement {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap(space::XS)
        .child(
            div()
                .id("chart-status")
                .flex()
                .flex_col()
                .gap(space::XS)
                .text_size(text::CAPTION)
                .line_height(text::CAPTION_LINE_HEIGHT)
                .text_color(role::fg_tertiary(cx))
                .child(SharedString::from(line)),
        )
        .into_any_element()
}

/// Screen points for one series, split where a sample is missing.
///
/// A gap in the metrics stream is not a zero, so the stroke breaks instead of
/// drawing a straight line through samples that were never taken.
fn segments(points: &[NormalizedPoint], scale: &PlotScale) -> Vec<Vec<Point<Pixels>>> {
    let mut segments: Vec<Vec<Point<Pixels>>> = Vec::new();
    let mut current: Vec<Point<Pixels>> = Vec::new();
    for sample in points {
        match sample.value {
            Some(value) => current.push(point(px(scale.x(sample.at_ms)), px(scale.y(value)))),
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

/// Horizontal rules this plot may draw, from its own height.
///
/// The count is the drawn size's answer, not a constant: `Y_TICK_PITCH` and
/// `MAX_Y_TICKS` together are what keep a rule from arriving every forty pixels
/// on a tall chart and every twenty on a short one. Two is the floor because a
/// single rule is not a scale.
fn y_tick_count(height: f32) -> usize {
    ((height / Y_TICK_PITCH).round() as usize).clamp(2, MAX_Y_TICKS)
}

fn x_tick_count(width: f32) -> usize {
    ((width / X_TICK_PITCH).round() as usize).clamp(2, MAX_X_TICKS)
}

/// A short run of a series' own stroke, for a legend entry.
///
/// Built out of the dash array the line itself is painted with, so a swatch is a
/// picture of the mark rather than a second description of it. Laid out as boxes
/// rather than stroked as a path because a `div` cannot carry a stroke, and a
/// legend is the one place in a chart where a two-element row is cheaper than a
/// custom element.
///
/// It starts *on*, which is what `PathBuilder::dash_array` does: a swatch that
/// began in the gap would read as an empty space for the one pattern with the
/// longest gap in it, which is the pattern a reader most needs to identify.
fn stroke_sample(series: SeriesColor, cx: &App) -> Div {
    let color = series.color(cx);
    let band = h_flex()
        .flex_none()
        .items_center()
        .h(px(LINE_WIDTH))
        .w(px(LEGEND_SWATCH))
        .overflow_hidden();
    let Some(dashes) = series.stroke().dash_array() else {
        // A solid line is one run the width of the swatch.
        return band.bg(color);
    };
    let mut row = band;
    for (index, run) in dashes.iter().enumerate() {
        let segment = div().h_full().w(*run);
        row = row.child(if index % 2 == 0 {
            segment.bg(color)
        } else {
            segment
        });
    }
    row
}

fn paint_area(
    window: &mut Window,
    points: &[Point<Pixels>],
    origin: Bounds<Pixels>,
    baseline: f32,
    color: Hsla,
) {
    let (Some(first), Some(last)) = (points.first(), points.last()) else {
        return;
    };
    let at = |position: Point<Pixels>| position + origin.origin;
    let mut builder = PathBuilder::fill();
    builder.move_to(at(point(px(f32::from(first.x)), px(baseline))));
    for vertex in points.iter() {
        builder.line_to(at(*vertex));
    }
    builder.line_to(at(point(px(f32::from(last.x)), px(baseline))));
    builder.close();
    if let Ok(path) = builder.build() {
        window.paint_path(path, color.opacity(AREA_OPACITY));
    }
}

fn paint_polyline(
    window: &mut Window,
    points: &[Point<Pixels>],
    origin: Bounds<Pixels>,
    color: Hsla,
    pattern: SeriesStroke,
) {
    let Some(first) = points.first() else {
        return;
    };
    if points.len() == 1 {
        // A one-sample series has no line to draw, so it reads as a mark, and it
        // is the mark the hover draws on a sample at the same size. A speck at
        // the line weight is a smudge on an otherwise empty plot rather than a
        // reading a reader could point at.
        paint_dot(window, *first, origin, DOT_SIZE / 2.0, color);
        return;
    }
    let mut builder = PathBuilder::stroke(px(LINE_WIDTH));
    if let Some(dashes) = pattern.dash_array() {
        builder = builder.dash_array(&dashes);
    }
    builder.move_to(*first + origin.origin);
    for vertex in points.iter().skip(1) {
        builder.line_to(*vertex + origin.origin);
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

fn paint_dot(
    window: &mut Window,
    center: Point<Pixels>,
    origin: Bounds<Pixels>,
    radius: f32,
    color: Hsla,
) {
    const STEPS: usize = 12;
    let center = center + origin.origin;
    let vertices: Vec<Point<Pixels>> = (0..STEPS)
        .map(|index| {
            let angle = std::f32::consts::TAU * index as f32 / STEPS as f32;
            point(
                px(f32::from(center.x) + radius * angle.cos()),
                px(f32::from(center.y) + radius * angle.sin()),
            )
        })
        .collect();
    let mut builder = PathBuilder::fill();
    builder.add_polygon(&vertices, true);
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}
