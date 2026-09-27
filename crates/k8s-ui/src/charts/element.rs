//! GPUI rendering for line charts: axes, grid, area, line, crosshair, and tooltip.
//!
//! The element fills the chart area. `plot_rect_for` reserves axis space inside the content mask.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, Context, Element, ElementId, GlobalElementId, Hsla,
    InspectorElementId, IntoElement, KeyDownEvent, LayoutId, Length, MouseMoveEvent, PathBuilder,
    Pixels, Point, Render, Role, ShapedLine, SharedString, Size, Style, Styled, TextAlign, TextRun,
    Window, div, fill, point, px, relative, size,
};
use ui::prelude::*;

use crate::design::{self, space};
use crate::settings::{self, DataTypography};

use super::ChartData;
use super::geometry::{self, PathPoint, PlotRect};

/// Y tick labels use compact units, so `1023Gi` is the longest label possible.
const Y_LABEL_COLUMNS: f32 = 6.0;
/// Line width and hover dot radius.
const LINE_WIDTH: f32 = 1.5;
const DOT_RADIUS: f32 = 3.0;
/// Area fill opacity. Only multi-series charts fill, and the data stays primary.
const AREA_OPACITY: f32 = 0.08;
/// Legend dot size. The gaps around it come from the 4px spacing scale.
const LEGEND_DOT: f32 = 8.0;
const MIN_PLOT_SIZE: f32 = 8.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinePattern {
    Solid,
    Dashed,
    Dotted,
    DashDot,
    DashDotDot,
    Indexed(usize),
}

fn line_pattern_for(index: usize) -> LinePattern {
    match index {
        0 => LinePattern::Solid,
        1 => LinePattern::Dashed,
        2 => LinePattern::Dotted,
        3 => LinePattern::DashDot,
        4 => LinePattern::DashDotDot,
        _ => LinePattern::Indexed(index),
    }
}

/// Gutter reserved for the Y tick labels: the label column plus its trailing gap.
///
/// The labels follow the configured data font, so the gutter is measured from
/// that font instead of a constant tuned for the default size. A constant
/// silently clips the longest label as soon as the user raises the data font
/// size. The gutter stays a pure function of size and typography, so the painted
/// plot always matches the plot used for hover and keyboard scrub.
fn y_axis_gutter(typography: &DataTypography) -> f32 {
    f32::from(typography.columns(Y_LABEL_COLUMNS)) + f32::from(space::SM)
}

/// Chart plot rectangle after axis padding.
pub(crate) fn plot_rect_for(
    size: Size<Pixels>,
    has_legend: bool,
    typography: &DataTypography,
) -> PlotRect {
    let width = f32::from(size.width);
    let height = f32::from(size.height);
    // A legend owns the top band; otherwise the plot starts at the edge gap.
    let top = if has_legend {
        f32::from(space::XL)
    } else {
        f32::from(space::SM)
    };
    let gutter = y_axis_gutter(typography);
    let right = f32::from(space::SM);
    PlotRect {
        x: gutter,
        y: top,
        width: (width - gutter - right).max(MIN_PLOT_SIZE),
        height: (height - top - f32::from(typography.line_height)).max(MIN_PLOT_SIZE),
    }
}

/// Hover position in plot-local coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Hover {
    pub x: f32,
    pub at_ms: i64,
}

/// Line chart view with data and hover state.
pub struct LineChartView {
    data: Rc<ChartData>,
    title: SharedString,
    id: SharedString,
    hover: Option<Hover>,
    scrub: Option<i64>,
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
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
            hover: None,
            scrub: None,
            bounds: Rc::new(Cell::new(None)),
        }
    }

    pub fn set_title(&mut self, title: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.title = title.into();
        cx.notify();
    }

    /// Replace the samples without throwing away the reading position.
    ///
    /// A live scrape replaces the window while the user is still reading it. The
    /// mouse hover is transient and the next mouse move recomputes it, so it is
    /// dropped. The keyboard scrub is a position the user chose with arrow keys;
    /// dropping it makes the next Left/Right restart from the newest sample and
    /// the crosshair jumps back, so the timestamp is kept and moved onto the
    /// sample grid by `clamped_scrub` when the window slides.
    pub fn set_data(&mut self, data: impl Into<Rc<ChartData>>, cx: &mut Context<Self>) {
        self.data = data.into();
        self.hover = None;
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

    fn has_legend(&self) -> bool {
        self.data.series.len() > 1
    }

    /// The scrubbed sample, moved onto the samples that exist now.
    ///
    /// The scrape window slides, so a kept timestamp can fall off either end or
    /// stop matching a sample. Returning `None` when there is nothing to read
    /// lets the chart fall back to the mouse hover instead of drawing a crosshair
    /// on a sample that no longer exists.
    fn clamped_scrub(&self, at_ms: Option<i64>) -> Option<i64> {
        nearest_sample_time(&self.data.sample_times(), at_ms?)
    }

    fn active_hover(&self, typography: &DataTypography) -> Option<Hover> {
        let Some(at_ms) = self.clamped_scrub(self.scrub) else {
            return self.hover;
        };
        let bounds = self.bounds.get()?;
        let time_range = self.data.time_range()?;
        let plot = plot_rect_for(bounds.size, self.has_legend(), typography);
        let x = if time_range.1 > time_range.0 {
            ((at_ms - time_range.0) as f32 / (time_range.1 - time_range.0) as f32).clamp(0.0, 1.0)
                * plot.width
        } else {
            plot.width / 2.0
        };
        Some(Hover { x, at_ms })
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
                self.hover = None;
                cx.notify();
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(bounds) = self.bounds.get() else {
            return;
        };
        self.scrub = None;
        let typography = settings::data_typography(cx);
        let plot = plot_rect_for(bounds.size, self.has_legend(), &typography);
        let local_x = f32::from(event.position.x - bounds.origin.x);
        let local_y = f32::from(event.position.y - bounds.origin.y);
        let next = if plot.contains(local_x, local_y) {
            self.data.time_range().map(|time_range| Hover {
                x: local_x - plot.x,
                at_ms: geometry::time_at_x(local_x, time_range, plot),
            })
        } else {
            None
        };
        if next != self.hover {
            self.hover = next;
            cx.notify();
        }
    }

    /// Tooltip with relative time, UTC time, and series values.
    fn tooltip(&self, hover: Hover, typography: &DataTypography, cx: &App) -> Option<AnyElement> {
        let bounds = self.bounds.get()?;
        let time_range = self.data.time_range()?;
        let plot = plot_rect_for(bounds.size, self.has_legend(), typography);
        let colors = cx.theme().colors();
        let container_width = f32::from(bounds.size.width);
        // The overlay has to clear the longest row it can hold: the series name
        // and one value, both in the data font. Measuring it keeps a raised data
        // font from squeezing the value column into nothing.
        let tooltip_width = tooltip_width(typography);
        let left = (hover.x + plot.x + f32::from(space::SM)).clamp(
            0.0,
            (container_width - tooltip_width - f32::from(space::XS)).max(0.0),
        );
        let rows: Vec<AnyElement> = self
            .data
            .series
            .iter()
            .map(|series| {
                let value = geometry::nearest_sample(&series.points, hover.at_ms).map_or_else(
                    || "No sample".to_owned(),
                    |sample| series.unit.format(sample.value),
                );
                h_flex()
                    .items_center()
                    .gap(space::XS)
                    .child(
                        div()
                            .flex_none()
                            .size(px(LEGEND_DOT))
                            .rounded_full()
                            .bg(series.color.color(cx)),
                    )
                    .child(
                        typography.apply(
                            div()
                                .flex_none()
                                .max_w(typography.columns(TOOLTIP_NAME_COLUMNS))
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .text_ellipsis()
                                .text_color(colors.text_muted)
                                .child(series.label.clone()),
                        ),
                    )
                    .child(
                        typography.apply(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .text_ellipsis()
                                .text_right()
                                .text_color(colors.text)
                                .child(value),
                        ),
                    )
                    .into_any_element()
            })
            .collect();

        Some(
            div()
                .absolute()
                .left(px(left))
                .top(px(f32::from(space::SM)))
                .w(px(tooltip_width))
                .p(space::SM)
                .rounded(px(6.0))
                .bg(design::surface::raised(cx))
                .border_1()
                .border_color(colors.border_variant)
                .flex()
                .flex_col()
                .gap(space::XS)
                .child(
                    typography.apply(
                        h_flex()
                            .justify_between()
                            .text_color(colors.text_muted)
                            .child(geometry::format_offset(time_range.1 - hover.at_ms))
                            .child(geometry::format_clock_utc(hover.at_ms)),
                    ),
                )
                .children(rows)
                .into_any_element(),
        )
    }
}

/// Characters the series name column keeps before it ellipsizes.
const TOOLTIP_NAME_COLUMNS: f32 = 10.0;
/// Characters the value column keeps, such as `1023.00 GiB`.
const TOOLTIP_VALUE_COLUMNS: f32 = 11.0;

/// Width of the tooltip, from the widest row it can hold.
fn tooltip_width(typography: &DataTypography) -> f32 {
    let content = typography.columns(TOOLTIP_NAME_COLUMNS + TOOLTIP_VALUE_COLUMNS);
    let dots_and_gaps = LEGEND_DOT + f32::from(space::XS) * 2.0;
    f32::from(content) + dots_and_gaps + f32::from(space::SM) * 2.0
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
        let capture = Rc::clone(&self.bounds);
        let typography = settings::data_typography(cx);
        let hover = self.active_hover(&typography);
        let tooltip = hover.and_then(|hover| self.tooltip(hover, &typography, cx));
        let focus_border = design::focus::border(cx);
        let border = cx.theme().colors().border_transparent;

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
            .border_1()
            .border_color(border)
            .focus_visible(move |style| style.border_color(focus_border))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
                if !*hovered && this.hover.is_some() {
                    this.hover = None;
                    cx.notify();
                }
            }))
            .child(LineChartElement {
                data,
                hover,
                bounds: capture,
                typography,
            })
            .children(tooltip)
    }
}

struct LineChartElement {
    data: Rc<ChartData>,
    hover: Option<Hover>,
    /// Bounds updated during prepaint for mouse hit testing.
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    typography: DataTypography,
}

impl IntoElement for LineChartElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

struct ShapedLabel {
    position: f32,
    line: ShapedLine,
}

struct LegendItem {
    x: f32,
    y: f32,
    color: Hsla,
    pattern: LinePattern,
    line: ShapedLine,
}

#[derive(Default)]
struct ChartLayout {
    plot: PlotRect,
    time_range: (i64, i64),
    value_range: (f64, f64),
    /// Line height of the axis labels, which follow the data font.
    label_line_height: Pixels,
    /// Line height of the chart status text, which follows the UI font.
    status_line_height: Pixels,
    y_labels: Vec<ShapedLabel>,
    x_labels: Vec<ShapedLabel>,
    legend: Vec<LegendItem>,
    empty_label: Option<ShapedLine>,
    /// Waiting label when fewer than two samples are available.
    waiting_label: Option<ShapedLine>,
    /// The variable the vertical axis measures, drawn inside the plot's top corner.
    axis_name: Option<ShapedLine>,
}

impl ChartLayout {
    fn is_empty(&self) -> bool {
        self.empty_label.is_some()
    }
}

impl Element for LineChartElement {
    type RequestLayoutState = ();
    type PrepaintState = ChartLayout;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = Style {
            size: size(
                Length::Definite(relative(1.0)),
                Length::Definite(relative(1.0)),
            ),
            ..Default::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.bounds.set(Some(bounds));
        let local_plot = plot_rect_for(bounds.size, self.data.series.len() > 1, &self.typography);
        // paint_* uses window coordinates. Translate the local plot to the element origin.
        let plot = PlotRect {
            x: local_plot.x + f32::from(bounds.origin.x),
            y: local_plot.y + f32::from(bounds.origin.y),
            ..local_plot
        };
        let font = self.typography.font.clone();
        let font_size = self.typography.size;
        let line_height = self.typography.line_height;
        let text_color = cx.theme().colors().text_muted;
        let text_system = window.text_system();
        // Chart status text is prose, not data. It reads in the UI font at the
        // metadata role so it matches the panel text around the chart instead of
        // arriving as the only monospaced sentence on the surface.
        let status_font = theme::theme_settings(cx).ui_font(cx).clone();
        let status_size = design::text::METADATA;
        let status_line_height = design::text::METADATA_LINE_HEIGHT;

        let mut layout = ChartLayout {
            plot,
            label_line_height: line_height,
            status_line_height,
            time_range: self.data.time_range().unwrap_or((0, 0)),
            value_range: self.data.value_range(),
            ..Default::default()
        };

        let unit = self.data.unit();
        let shape = |text: SharedString| -> ShapedLine {
            let run = TextRun {
                len: text.len(),
                font: font.clone(),
                color: text_color,
                ..Default::default()
            };
            text_system.shape_line(text, font_size, &[run], None)
        };
        let shape_status = |text: SharedString| -> ShapedLine {
            let run = TextRun {
                len: text.len(),
                font: status_font.clone(),
                color: text_color,
                ..Default::default()
            };
            text_system.shape_line(text, status_size, &[run], None)
        };

        if self.data.is_empty() {
            let text: SharedString = "No Samples Yet".into();
            layout.empty_label = Some(shape_status(text));
            layout.waiting_label = Some(shape_status(
                geometry::waiting_for_next_scrape(self.data.interval_ms).into(),
            ));
            return layout;
        }

        // The tick labels stay short so the plot keeps its width, which leaves the
        // quantity unstated. `charts.md › Best practices` allows a longer label
        // inside the plot area when it does not obscure the data, and the range
        // headroom above the highest value is exactly that empty strip.
        layout.axis_name = Some(shape_status(unit.axis_name().into()));

        // One sample repeats tick labels, so show the next-sample status instead.
        if self.data.sample_count() < 2 {
            layout.waiting_label = Some(shape_status(
                geometry::waiting_for_next_scrape(self.data.interval_ms).into(),
            ));
        } else {
            for tick in geometry::nice_ticks(
                layout.value_range.0,
                layout.value_range.1,
                y_tick_count(plot.height),
            ) {
                let y = geometry::map_point(
                    layout.time_range.0,
                    tick,
                    layout.time_range,
                    layout.value_range,
                    plot,
                )
                .y;
                layout.y_labels.push(ShapedLabel {
                    position: y,
                    line: shape(unit.axis_label(tick).into()),
                });
            }

            for tick in geometry::time_ticks(
                layout.time_range.0,
                layout.time_range.1,
                x_tick_count(plot.width),
            ) {
                let x = geometry::map_point(
                    tick,
                    layout.value_range.0,
                    layout.time_range,
                    layout.value_range,
                    plot,
                )
                .x;
                layout.x_labels.push(ShapedLabel {
                    position: x,
                    line: shape(geometry::format_offset_short(layout.time_range.1 - tick).into()),
                });
            }
        }

        if self.data.series.len() > 1 {
            // Centre the legend inside the top band the plot reserved for it, and
            // drop the items that no longer fit: a raised data font widens the
            // labels, and a legend that paints past the frame is worse than a
            // legend that lists fewer series. The values stay in the table below.
            let legend_y = plot.y - f32::from(space::XL) + f32::from(space::SM);
            let labels = self
                .data
                .series
                .iter()
                .map(|series| shape(series.label.clone()))
                .collect::<Vec<_>>();
            let mut x = legend_start_x(
                &labels
                    .iter()
                    .map(|line| f32::from(line.width()))
                    .collect::<Vec<_>>(),
                plot,
            );
            for (index, series) in self.data.series.iter().enumerate() {
                let line = labels[index].clone();
                if index > 0 && x > plot.right() {
                    break;
                }
                let width = f32::from(line.width());
                layout.legend.push(LegendItem {
                    x,
                    y: legend_y,
                    color: series.color.color(cx),
                    pattern: line_pattern_for(index),
                    line,
                });
                x += LEGEND_DOT + f32::from(space::XS) + width + f32::from(space::LG);
            }
        }

        layout
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        layout: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let colors = cx.theme().colors();
        if layout.is_empty() {
            if let Some(label) = &layout.empty_label {
                let width = f32::from(label.width());
                let x = f32::from(bounds.origin.x) + (f32::from(bounds.size.width) - width) / 2.0;
                let y = f32::from(bounds.origin.y)
                    + (f32::from(bounds.size.height) - f32::from(layout.status_line_height)) / 2.0;
                let _ = label.paint(
                    point(px(x), px(y)),
                    layout.status_line_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
            if let Some(label) = &layout.waiting_label {
                paint_waiting_label(
                    label,
                    layout.plot,
                    bounds,
                    layout.status_line_height,
                    window,
                    cx,
                );
            }
            return;
        }

        let plot = layout.plot;
        let grid = colors.border_variant;
        let axis = colors.border;
        let inset = f32::from(space::SM);

        if let Some(axis_name) = &layout.axis_name {
            paint_axis_name(
                axis_name,
                plot,
                inset,
                layout.status_line_height,
                window,
                cx,
            );
        }

        // Grid lines start at the plot edge, so they never reach the label gutter.
        for label in &layout.y_labels {
            paint_hline(window, plot.x, label.position, plot.width, grid);
        }
        for label in &layout.x_labels {
            paint_vline(window, label.position, plot.y, plot.height, grid);
        }
        paint_vline(window, plot.x, plot.y, plot.height, axis);
        paint_hline(window, plot.x, plot.bottom() - 1.0, plot.width, axis);

        let area_fill = draws_area_fill(self.data.series.len());
        for (index, series) in self.data.series.iter().enumerate() {
            let color = series.color.color(cx);
            let segments =
                geometry::map_points(&series.points, layout.time_range, layout.value_range, plot);
            if series.filled && area_fill {
                for segment in &segments {
                    paint_area(window, segment, plot.bottom(), color);
                }
            }
            for segment in &segments {
                paint_polyline(window, segment, color, line_pattern_for(index));
            }
        }

        if let Some(hover) = self.hover {
            let x = plot.x + hover.x;
            paint_vline(window, x, plot.y, plot.height, design::chart::crosshair(cx));
            for series in &self.data.series {
                let Some(sample) = geometry::nearest_sample(&series.points, hover.at_ms) else {
                    continue;
                };
                let dot = geometry::map_point(
                    sample.at_ms,
                    sample.value,
                    layout.time_range,
                    layout.value_range,
                    plot,
                );
                paint_dot(window, dot, DOT_RADIUS, series.color.color(cx));
            }
        }

        let line_height = layout.label_line_height;
        // Y labels are numbers, so they align to their trailing edge and keep the
        // reserved gap before the plot.
        for label in &layout.y_labels {
            let _ = label.line.paint(
                point(
                    px(f32::from(bounds.origin.x)),
                    px(label.position - f32::from(line_height) / 2.0),
                ),
                line_height,
                TextAlign::Right,
                Some(px(y_axis_gutter(&self.typography) - inset)),
                window,
                cx,
            );
        }
        for label in &layout.x_labels {
            let x = tick_label_x(label.position, f32::from(label.line.width()), plot, inset);
            let _ = label.line.paint(
                point(px(x), px(plot.bottom())),
                line_height,
                TextAlign::Left,
                None,
                window,
                cx,
            );
        }
        if let Some(label) = &layout.waiting_label {
            paint_waiting_label(label, plot, bounds, layout.status_line_height, window, cx);
        }
        for item in &layout.legend {
            paint_polyline(
                window,
                &[
                    PathPoint {
                        x: item.x,
                        y: item.y,
                    },
                    PathPoint {
                        x: item.x + LEGEND_DOT,
                        y: item.y,
                    },
                ],
                item.color,
                item.pattern,
            );
            let _ = item.line.paint(
                point(
                    px(item.x + LEGEND_DOT + f32::from(space::XS)),
                    px(item.y - f32::from(line_height) / 2.0),
                ),
                line_height,
                TextAlign::Left,
                None,
                window,
                cx,
            );
        }
    }
}

/// Start x for the legend row.
///
/// The legend is centred in the plot while it fits, and pinned to the leading
/// edge once a wider data font pushes it past the frame. Centring rather than
/// left-aligning keeps the legend a single band under the plot instead of
/// drifting away from the marks it names.
fn legend_start_x(widths: &[f32], plot: PlotRect) -> f32 {
    let gap = f32::from(space::LG);
    let total = LEGEND_DOT
        + f32::from(space::XS) * widths.len() as f32
        + widths.iter().sum::<f32>()
        + gap * widths.len().saturating_sub(1) as f32;
    plot.x + ((plot.width - total) / 2.0).max(0.0)
}

fn y_tick_count(height: f32) -> usize {
    ((height / 40.0).round() as usize).clamp(2, 6)
}

fn x_tick_count(width: f32) -> usize {
    ((width / 90.0).round() as usize).clamp(2, 8)
}

/// Start x for an X tick label.
///
/// Edge labels align inward so no glyph touches the plot frame: the first label
/// starts `inset` after the plot's leading edge and the last label ends `inset`
/// before its trailing edge.
fn tick_label_x(center: f32, width: f32, plot: PlotRect, inset: f32) -> f32 {
    let leading = plot.x + inset;
    if center - width / 2.0 < leading {
        return leading;
    }
    if center + width / 2.0 > plot.right() - inset {
        return (plot.right() - inset - width).max(leading);
    }
    center - width / 2.0
}

/// Whether an area fill belongs under the lines.
///
/// A single series is a line mark on a grid. A filled area there covers most of
/// the plot and outweighs the line that carries the data, so the fill is only
/// kept when two or more series share the plot.
fn draws_area_fill(series_count: usize) -> bool {
    series_count > 1
}

fn paint_waiting_label(
    label: &ShapedLine,
    plot: PlotRect,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
    window: &mut Window,
    cx: &mut App,
) {
    let width = f32::from(label.width());
    let left = f32::from(bounds.origin.x);
    let right = left + f32::from(bounds.size.width);
    let center = plot.x + plot.width / 2.0;
    let x = (center - width / 2.0).clamp(left, (right - width).max(left));
    let _ = label.paint(
        point(px(x), px(plot.bottom())),
        line_height,
        TextAlign::Left,
        None,
        window,
        cx,
    );
}

/// Paints the vertical axis's variable inside the plot's top-left corner.
///
/// The label gutter is sized for the tick values, which are deliberately short,
/// so a full name does not fit beside them. The plot's top strip is empty
/// because `pad_range` always leaves headroom above the highest sample, which
/// makes the corner the one place the name can live without either overflowing
/// the gutter or covering a mark.
fn paint_axis_name(
    label: &ShapedLine,
    plot: PlotRect,
    inset: f32,
    line_height: Pixels,
    window: &mut Window,
    cx: &mut App,
) {
    let _ = label.paint(
        point(px(plot.x + inset), px(plot.y + inset)),
        line_height,
        TextAlign::Left,
        None,
        window,
        cx,
    );
}

fn paint_hline(window: &mut Window, x: f32, y: f32, width: f32, color: Hsla) {
    window.paint_quad(fill(
        Bounds::new(point(px(x), px(y)), size(px(width), px(1.0))),
        color,
    ));
}

fn paint_vline(window: &mut Window, x: f32, y: f32, height: f32, color: Hsla) {
    window.paint_quad(fill(
        Bounds::new(point(px(x), px(y)), size(px(1.0), px(height))),
        color,
    ));
}

fn paint_area(window: &mut Window, points: &[PathPoint], baseline: f32, color: Hsla) {
    let (Some(first), Some(last)) = (points.first(), points.last()) else {
        return;
    };
    let mut builder = PathBuilder::fill();
    builder.move_to(point(px(first.x), px(baseline)));
    for vertex in points {
        builder.line_to(point(px(vertex.x), px(vertex.y)));
    }
    builder.line_to(point(px(last.x), px(baseline)));
    builder.close();
    if let Ok(path) = builder.build() {
        window.paint_path(path, color.opacity(AREA_OPACITY));
    }
}

fn paint_polyline(window: &mut Window, points: &[PathPoint], color: Hsla, pattern: LinePattern) {
    let Some(first) = points.first() else {
        return;
    };
    if points.len() == 1 {
        paint_dot(window, *first, LINE_WIDTH, color);
        return;
    }
    let mut builder = PathBuilder::stroke(px(LINE_WIDTH));
    if let Some(dashes) = dash_array(pattern) {
        builder = builder.dash_array(&dashes);
    }
    builder.move_to(point(px(first.x), px(first.y)));
    for vertex in points.iter().skip(1) {
        builder.line_to(point(px(vertex.x), px(vertex.y)));
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

fn dash_array(pattern: LinePattern) -> Option<Vec<Pixels>> {
    match pattern {
        LinePattern::Solid => None,
        LinePattern::Dashed => Some(vec![px(8.0), px(4.0)]),
        LinePattern::Dotted => Some(vec![px(1.0), px(4.0)]),
        LinePattern::DashDot => Some(vec![px(8.0), px(3.0), px(1.5), px(3.0)]),
        LinePattern::DashDotDot => Some(vec![px(8.0), px(3.0), px(1.5), px(3.0), px(1.5), px(3.0)]),
        LinePattern::Indexed(index) => Some(indexed_dash_array(index)),
    }
}

fn indexed_dash_array(index: usize) -> Vec<Pixels> {
    let mut value = index;
    let mut dashes = Vec::with_capacity(16);
    for _ in 0..8 {
        dashes.push(px(1.0 + (value % 8) as f32));
        value /= 8;
        dashes.push(px(2.0 + (value % 8) as f32));
        value /= 8;
    }
    dashes
}

fn paint_dot(window: &mut Window, center: PathPoint, radius: f32, color: Hsla) {
    const STEPS: usize = 12;
    let vertices: Vec<Point<Pixels>> = (0..STEPS)
        .map(|index| {
            let angle = std::f32::consts::TAU * index as f32 / STEPS as f32;
            point(
                px(center.x + radius * angle.cos()),
                px(center.y + radius * angle.sin()),
            )
        })
        .collect();
    let mut builder = PathBuilder::fill();
    builder.add_polygon(&vertices, true);
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charts::SeriesColor;
    use crate::settings::test_data_typography;

    #[test]
    fn series_use_distinct_line_patterns() {
        let colors = [
            SeriesColor::Accent,
            SeriesColor::Info,
            SeriesColor::Success,
            SeriesColor::Warning,
            SeriesColor::Error,
        ];
        let patterns = colors
            .iter()
            .enumerate()
            .map(|(index, _)| line_pattern_for(index))
            .collect::<Vec<_>>();

        for left in 0..patterns.len() {
            for right in left + 1..patterns.len() {
                assert_ne!(
                    patterns[left], patterns[right],
                    "{:?} and {:?} collide",
                    colors[left], colors[right]
                );
            }
        }

        assert_ne!(line_pattern_for(0), line_pattern_for(1));
        assert!(dash_array(LinePattern::Solid).is_none());
        assert_eq!(
            dash_array(LinePattern::Dashed).map(|dashes| dashes.len()),
            Some(2)
        );
        assert_eq!(
            dash_array(LinePattern::Dotted).map(|dashes| dashes.len()),
            Some(2)
        );
        assert_eq!(
            dash_array(LinePattern::DashDot).map(|dashes| dashes.len()),
            Some(4)
        );
        assert_eq!(
            dash_array(LinePattern::DashDotDot).map(|dashes| dashes.len()),
            Some(6)
        );

        let indexed = (5..37)
            .map(line_pattern_for)
            .map(dash_array)
            .collect::<Vec<_>>();
        for left in 0..indexed.len() {
            for right in left + 1..indexed.len() {
                assert_ne!(indexed[left], indexed[right]);
            }
        }
    }

    #[test]
    fn plot_rect_uses_data_line_height() {
        let chart_size = size(px(200.0), px(100.0));
        let typography = test_data_typography(12., 24.);
        assert_eq!(plot_rect_for(chart_size, false, &typography).height, 68.0);
        assert_eq!(plot_rect_for(chart_size, true, &typography).height, 52.0);
    }

    #[test]
    fn plot_rect_reserves_the_widest_y_label() {
        let chart_size = size(px(600.0), px(100.0));
        let typography = test_data_typography(12., 18.);
        let plot = plot_rect_for(chart_size, false, &typography);
        let widest = f32::from(typography.columns(Y_LABEL_COLUMNS));
        assert_eq!(plot.x, widest + f32::from(space::SM));
        assert!(
            plot.x > widest,
            "the gutter must hold the longest label plus a gap"
        );
        assert_eq!(
            plot.x + plot.width,
            f32::from(px(600.0)) - f32::from(space::SM),
            "the plot keeps the trailing gap on the right"
        );
    }

    /// The Y gutter has to follow the data font, otherwise the longest tick
    /// label is clipped as soon as the user raises the data font size.
    #[test]
    fn the_gutter_grows_with_the_data_font() {
        let chart_size = size(px(600.0), px(100.0));
        let small = test_data_typography(12., 18.);
        let large = test_data_typography(24., 36.);
        let small_plot = plot_rect_for(chart_size, false, &small);
        let large_plot = plot_rect_for(chart_size, false, &large);
        assert_eq!(
            large_plot.x,
            2. * small_plot.x - f32::from(space::SM),
            "a twice-as-large data font must reserve a twice-as-wide label column"
        );
        for (typography, plot) in [(&small, small_plot), (&large, large_plot)] {
            assert!(
                plot.x > f32::from(typography.columns(Y_LABEL_COLUMNS)),
                "the gutter must hold the longest label plus a gap"
            );
            assert!(
                plot.width >= MIN_PLOT_SIZE,
                "the plot keeps a usable width at any data font size"
            );
        }
    }

    #[test]
    fn tick_labels_align_inward_at_the_plot_edges() {
        let typography = test_data_typography(12., 18.);
        let plot = PlotRect {
            x: y_axis_gutter(&typography),
            y: 8.0,
            width: 147.0,
            height: 40.0,
        };
        let inset = f32::from(space::SM);
        // The first tick sits on the leading edge, so its label starts inside.
        let first = tick_label_x(plot.x, 30.0, plot, inset);
        assert_eq!(first, plot.x + inset);
        // The last tick sits on the trailing edge, so its label ends inside.
        let last = tick_label_x(plot.right(), 22.0, plot, inset);
        assert_eq!(last, plot.right() - inset - 22.0);
        // A label in the middle stays centred on its tick.
        let middle = tick_label_x(plot.x + 70.0, 22.0, plot, inset);
        assert_eq!(middle, plot.x + 70.0 - 11.0);
        // A label wider than the plot cannot start before it.
        let wide = tick_label_x(plot.x + 4.0, 400.0, plot, inset);
        assert_eq!(wide, plot.x + inset);
    }

    #[test]
    fn the_legend_centres_until_it_no_longer_fits() {
        let plot = PlotRect {
            x: 53.0,
            y: 8.0,
            width: 400.0,
            height: 40.0,
        };
        let narrow = legend_start_x(&[20.0, 20.0], plot);
        let centred = plot.x + (plot.width - (LEGEND_DOT + 8.0 + 40.0 + 16.0)) / 2.0;
        assert_eq!(narrow, centred);
        // A legend wider than the plot pins to the leading edge instead of
        // painting outside the chart frame.
        let wide = legend_start_x(&[400.0, 400.0], plot);
        assert_eq!(wide, plot.x);
        assert!(wide >= plot.x);
    }

    #[test]
    fn a_single_series_keeps_the_line_only() {
        assert!(
            !draws_area_fill(1),
            "one series must not fill the plot under the line"
        );
        assert!(draws_area_fill(2));
    }

    #[test]
    fn keyboard_scrub_steps_through_sample_times() {
        let times = [0, 10, 20, 30];
        assert_eq!(scrub_index(&times, None, -1), Some(0));
        assert_eq!(scrub_index(&times, None, 1), Some(3));
        assert_eq!(scrub_index(&times, Some(20), -1), Some(1));
        assert_eq!(scrub_index(&times, Some(20), 1), Some(3));
        assert_eq!(scrub_index(&[], Some(0), 1), None);
    }

    /// A live scrape slides the window, so a kept scrub timestamp can stop
    /// matching a sample. It moves onto the grid instead of being discarded.
    #[test]
    fn a_kept_scrub_moves_onto_the_nearest_sample() {
        let times = [0, 10, 20, 30];
        assert_eq!(nearest_sample_time(&times, 12), Some(10));
        assert_eq!(nearest_sample_time(&times, 18), Some(20));
        assert_eq!(nearest_sample_time(&times, 20), Some(20));
        // Off either end of the sliding window, it clamps to the last sample.
        assert_eq!(nearest_sample_time(&times, 12_000), Some(30));
        assert_eq!(nearest_sample_time(&times, -5), Some(0));
        // No samples left to read.
        assert_eq!(nearest_sample_time(&[], 5), None);
    }

    #[test]
    fn scrub_steps_from_the_nearest_sample_after_the_window_slid() {
        let times = [10, 20, 30];
        // 12 kept from a wider window resolves to 10, so Right moves to 20.
        assert_eq!(scrub_index(&times, Some(12), 1), Some(1));
        assert_eq!(scrub_index(&times, Some(12), -1), Some(0));
    }

    #[test]
    fn the_tooltip_grows_with_the_data_font() {
        let small = tooltip_width(&test_data_typography(12., 18.));
        let large = tooltip_width(&test_data_typography(24., 36.));
        assert!(
            large > small,
            "a raised data font must widen the tooltip instead of clipping the value"
        );
        assert!(small >= f32::from(space::SM) * 2.0 + LEGEND_DOT);
    }
}
