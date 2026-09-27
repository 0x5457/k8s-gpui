//! Table performance spike: 10,000 resource rows with per-frame timing.
//!
//! The table uses a virtual list, a sticky header, and horizontal scrolling. A frame callback
//! drives continuous rendering. The interval between callbacks measures a full frame, including
//! display synchronization. The interval from the callback to the end of paint measures CPU time.
//!
//! Timestamps for element construction, layout and prepaint, and paint identify the costly phase.

pub mod data;
pub mod stats;

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, FontWeight, IntoElement, KeyDownEvent,
    ParentElement, Render, SharedString, Styled, WeakEntity, Window, canvas, div, point, px,
};
use ui::prelude::*;
use ui::{
    ColumnWidthConfig, ResizableColumnsState, Table, TableInteractionState, TableResizeBehavior,
};

use self::{
    data::{COLUMNS, Health, Lcg, Row, synthetic_rows},
    stats::{FrameStats, Summary},
};

const PRINT_INTERVAL: Duration = Duration::from_secs(5);
/// Frames to keep a changed row highlighted.
const HIGHLIGHT_FRAMES: u64 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Static,
    Scroll,
    Storm,
}

impl Mode {
    pub fn from_arg(arg: &str) -> Option<Self> {
        match arg {
            "static" => Some(Self::Static),
            "scroll" => Some(Self::Scroll),
            "storm" => Some(Self::Storm),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Static => "Static",
            Self::Scroll => "Scroll",
            Self::Storm => "Frequent Updates",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SpikeOptions {
    pub mode: Mode,
    /// Scroll speed in pixels per second. Horizontal speed is 40 percent of this value.
    pub scroll_speed: f32,
    /// Rows changed per frame in Frequent Updates mode.
    pub rows_per_frame: usize,
    pub row_count: usize,
}

impl Default for SpikeOptions {
    fn default() -> Self {
        Self {
            mode: Mode::Scroll,
            scroll_speed: 400.0,
            rows_per_frame: 500,
            row_count: 10_000,
        }
    }
}

/// Frame timestamps shared with the canvas paint phases.
struct FrameClock {
    frame_start: Cell<Option<Instant>>,
    build_start: Cell<Option<Instant>>,
    build_end: Cell<Option<Instant>>,
    paint_start: Cell<Option<Instant>>,
    paint_end: Cell<Option<Instant>>,
}

#[derive(Clone, Copy, Default)]
struct Phase {
    interval_ms: f32,
    draw_ms: f32,
    build_ms: f32,
    layout_ms: f32,
    paint_ms: f32,
}

#[derive(Default)]
struct PhaseStats {
    intervals: FrameStats,
    draws: FrameStats,
    builds: FrameStats,
    layouts: FrameStats,
    paints: FrameStats,
}

impl PhaseStats {
    fn reset(&mut self) {
        self.intervals.reset();
        self.draws.reset();
        self.builds.reset();
        self.layouts.reset();
        self.paints.reset();
    }
}

#[derive(Clone, Copy, Default)]
struct PhaseSummary {
    interval: Summary,
    draw: Summary,
    build: Summary,
    layout: Summary,
    paint: Summary,
}

pub struct TableSpike {
    mode: Mode,
    options: SpikeOptions,
    rows: Rc<RefCell<Vec<Row>>>,
    columns: Entity<ResizableColumnsState>,
    interaction: Entity<TableInteractionState>,
    focus_handle: FocusHandle,
    clock: Rc<FrameClock>,
    stats: PhaseStats,
    summary: PhaseSummary,
    last: Phase,
    last_print: Instant,
    frame: u64,
    frame_cell: Rc<Cell<u64>>,
    scroll_y: f32,
    scroll_x: f32,
    storm_cursor: usize,
    rng: Lcg,
    visible: Rc<Cell<(usize, usize)>>,
    viewport: Rc<Cell<(u32, u32)>>,
    scale_factor: f32,
    started: bool,
    focused: bool,
    window_active: bool,
}

impl TableSpike {
    pub fn new(options: SpikeOptions, cx: &mut Context<Self>) -> Self {
        let row_count = options.row_count.max(1);
        let columns = cx.new(|_| {
            ResizableColumnsState::new(
                COLUMNS.len(),
                COLUMNS
                    .iter()
                    .map(|column| px(column.width))
                    .collect::<Vec<_>>(),
                vec![TableResizeBehavior::MinSize(4.0); COLUMNS.len()],
            )
        });
        let interaction = cx.new(|cx| TableInteractionState::new(cx));
        Self {
            mode: options.mode,
            options: SpikeOptions {
                row_count,
                ..options
            },
            rows: Rc::new(RefCell::new(synthetic_rows(row_count, 0x5EED_1234))),
            columns,
            interaction,
            focus_handle: cx.focus_handle(),
            clock: Rc::new(FrameClock {
                frame_start: Cell::new(None),
                build_start: Cell::new(None),
                build_end: Cell::new(None),
                paint_start: Cell::new(None),
                paint_end: Cell::new(None),
            }),
            stats: PhaseStats::default(),
            summary: PhaseSummary::default(),
            last: Phase::default(),
            last_print: Instant::now(),
            frame: 0,
            frame_cell: Rc::new(Cell::new(0)),
            scroll_y: 0.0,
            scroll_x: 0.0,
            storm_cursor: 0,
            rng: Lcg::new(0xC0FF_EE00),
            visible: Rc::new(Cell::new((0, 0))),
            viewport: Rc::new(Cell::new((0, 0))),
            scale_factor: 1.0,
            started: false,
            focused: false,
            window_active: false,
        }
    }

    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.stats.reset();
        self.summary = PhaseSummary::default();
        self.scroll_y = 0.0;
        self.scroll_x = 0.0;
        eprintln!(
            "[table-spike] Mode: {}. Statistics reset. Rows: {}. Speed: {} px/s. Rows per frame: {}.",
            mode.label(),
            self.options.row_count,
            self.options.scroll_speed,
            self.options.rows_per_frame,
        );
    }

    fn on_frame(&mut self, now: Instant, cx: &mut Context<Self>) {
        let previous = self.clock.frame_start.replace(Some(now));
        let paint_end = self.clock.paint_end.take();
        let paint_start = self.clock.paint_start.take();
        let build_start = self.clock.build_start.take();
        let build_end = self.clock.build_end.take();
        self.frame += 1;
        self.frame_cell.set(self.frame);

        if let (Some(start), Some(end)) = (previous, paint_end) {
            let milliseconds = |from: Instant, to: Instant| {
                to.saturating_duration_since(from).as_secs_f32() * 1000.0
            };
            self.last = Phase {
                interval_ms: milliseconds(start, now),
                draw_ms: milliseconds(start, end),
                build_ms: match (build_start, build_end) {
                    (Some(build_start), Some(build_end)) => milliseconds(build_start, build_end),
                    _ => 0.0,
                },
                layout_ms: match (build_end, paint_start) {
                    (Some(build_end), Some(paint_start)) => milliseconds(build_end, paint_start),
                    _ => 0.0,
                },
                paint_ms: match paint_start {
                    Some(paint_start) => milliseconds(paint_start, end),
                    None => 0.0,
                },
            };
            self.stats.intervals.record(self.last.interval_ms);
            self.stats.draws.record(self.last.draw_ms);
            self.stats.builds.record(self.last.build_ms);
            self.stats.layouts.record(self.last.layout_ms);
            self.stats.paints.record(self.last.paint_ms);
        }

        match self.mode {
            Mode::Static => {}
            Mode::Scroll => {
                let dt = self.last.interval_ms.clamp(0.1, 100.0) / 1000.0;
                self.advance_scroll(dt, cx);
            }
            Mode::Storm => self.apply_storm(),
        }

        if now.saturating_duration_since(self.last_print) >= PRINT_INTERVAL {
            self.print_and_reset(now);
        }
    }

    /// Set the list scroll offset directly for prepaint.
    fn advance_scroll(&mut self, dt: f32, cx: &Context<Self>) {
        let interaction = self.interaction.read(cx);

        let list = &interaction.scroll_handle;
        let max_y = f32::from(list.0.borrow().base_handle.max_offset().y);
        self.scroll_y += self.options.scroll_speed * dt;
        if self.scroll_y > max_y {
            self.scroll_y = 0.0;
        }
        list.0
            .borrow()
            .base_handle
            .set_offset(point(px(0.0), px(-self.scroll_y)));

        let horizontal = &interaction.horizontal_scroll_handle;
        let max_x = f32::from(horizontal.max_offset().x);
        self.scroll_x += self.options.scroll_speed * 0.4 * dt;
        if self.scroll_x > max_x {
            self.scroll_x = 0.0;
        }
        horizontal.set_offset(point(px(-self.scroll_x), px(0.0)));
    }

    fn apply_storm(&mut self) {
        let count = self.options.rows_per_frame.min(self.options.row_count);
        if count == 0 {
            return;
        }
        let mut rows = self.rows.borrow_mut();
        for offset in 0..count {
            let index = (self.storm_cursor + offset) % self.options.row_count;
            let frame = self.frame;
            if let Some(row) = rows.get_mut(index) {
                data::mutate_row(row, &mut self.rng, frame);
            }
        }
        self.storm_cursor = (self.storm_cursor + count) % self.options.row_count;
    }

    fn print_and_reset(&mut self, now: Instant) {
        self.summary = PhaseSummary {
            interval: self.stats.intervals.summary(),
            draw: self.stats.draws.summary(),
            build: self.stats.builds.summary(),
            layout: self.stats.layouts.summary(),
            paint: self.stats.paints.summary(),
        };
        self.last_print = now;
        self.stats.reset();

        let summary = self.summary;
        eprintln!(
            "[table-spike] Mode: {} Viewport: {}x{}@{:.1}x Frames: {} FPS: {:.1} \
             Interval ms (Mean: {:.2} P50: {:.2} P95: {:.2} P99: {:.2} Max: {:.2}) \
             Draw ms (Mean: {:.2} P50: {:.2} P95: {:.2} P99: {:.2} Max: {:.2}) \
             Phases ms (Build Mean: {:.2} P95: {:.2} | Layout Mean: {:.2} P95: {:.2} | Paint Mean: {:.2} P95: {:.2}) \
             Rows: {} Visible: {}..{} Scroll: ({:.0},{:.0}) Active: {}",
            self.mode.label(),
            self.viewport.get().0,
            self.viewport.get().1,
            self.scale_factor,
            summary.interval.count,
            summary.interval.fps(),
            summary.interval.mean,
            summary.interval.p50,
            summary.interval.p95,
            summary.interval.p99,
            summary.interval.max,
            summary.draw.mean,
            summary.draw.p50,
            summary.draw.p95,
            summary.draw.p99,
            summary.draw.max,
            summary.build.mean,
            summary.build.p95,
            summary.layout.mean,
            summary.layout.p95,
            summary.paint.mean,
            summary.paint.p95,
            self.options.row_count,
            self.visible.get().0,
            self.visible.get().1,
            self.scroll_y,
            self.scroll_x,
            self.window_active,
        );
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let mode = match event.keystroke.key.as_str() {
            "1" => Mode::Static,
            "2" => Mode::Scroll,
            "3" => Mode::Storm,
            "q" | "escape" => {
                cx.quit();
                return;
            }
            _ => return,
        };
        self.set_mode(mode);
        cx.notify();
    }

    fn overlay_lines(&self) -> Vec<String> {
        let summary = self.summary;
        let last = self.last;
        let (visible_start, visible_end) = self.visible.get();
        let (viewport_width, viewport_height) = self.viewport.get();
        vec![
            format!(
                "Table performance test   Mode: {}   [1 Static / 2 Scroll / 3 Frequent Updates / q Quit]",
                self.mode.label()
            ),
            format!(
                "Frame   Last: {:.1} ms   Last 5 s: Mean: {:.2} P50: {:.2} P95: {:.2} P99: {:.2} Max: {:.2}   FPS: {:.1}",
                last.interval_ms,
                summary.interval.mean,
                summary.interval.p50,
                summary.interval.p95,
                summary.interval.p99,
                summary.interval.max,
                summary.interval.fps(),
            ),
            format!(
                "Draw   Last: {:.1} ms   Last 5 s: Mean: {:.2} P50: {:.2} P95: {:.2} P99: {:.2}   Budget: 8.3 ms",
                last.draw_ms,
                summary.draw.mean,
                summary.draw.p50,
                summary.draw.p95,
                summary.draw.p99,
            ),
            format!(
                "Phases   Last: Build {:.1} + Layout {:.1} + Paint {:.1} ms   Last 5 s: Build {:.1} / Layout {:.1} / Paint {:.1}",
                last.build_ms,
                last.layout_ms,
                last.paint_ms,
                summary.build.mean,
                summary.layout.mean,
                summary.paint.mean,
            ),
            format!(
                "Rows: {}   Visible: {}..{}   Viewport: {} px × {} px @ {:.1}x   Scroll: Y {:.0} px X {:.0} px   Active: {}   Rate: {}",
                self.options.row_count,
                visible_start,
                visible_end,
                viewport_width,
                viewport_height,
                self.scale_factor,
                self.scroll_y,
                self.scroll_x,
                self.window_active,
                self.rate_label(),
            ),
        ]
    }

    fn rate_label(&self) -> String {
        match self.mode {
            Mode::Static => "Continuous Redraw".to_owned(),
            Mode::Scroll => format!("{:.0} px/s", self.options.scroll_speed),
            Mode::Storm => format!("{} Rows per frame", self.options.rows_per_frame),
        }
    }
}

impl Render for TableSpike {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.window_active = window.is_window_active();
        self.scale_factor = window.scale_factor();
        let viewport = window.viewport_size();
        self.viewport.set((
            f32::from(viewport.width).round() as u32,
            f32::from(viewport.height).round() as u32,
        ));
        if !self.focused {
            self.focused = true;
            window.focus(&self.focus_handle, cx);
        }
        if !self.started {
            self.started = true;
            let spike = cx.entity().downgrade();
            window.on_next_frame(move |window, cx| tick(spike, window, cx));
        }

        let background = cx.theme().colors().background;
        let text = cx.theme().colors().text;
        let border = cx.theme().colors().border_variant;
        let highlight = cx.theme().colors().element_selected;

        let clock = self.clock.clone();
        clock.build_start.set(Some(Instant::now()));
        let rows = self.rows.clone();
        let rows_for_highlight = self.rows.clone();
        let frame = self.frame_cell.clone();
        let visible = self.visible.clone();
        let table = Table::new(COLUMNS.len())
            .uniform_list(
                "table-spike-rows",
                self.options.row_count,
                move |range, _window, cx| {
                    visible.set((range.start, range.end));
                    let rows = rows.borrow();
                    range
                        .filter_map(|index| rows.get(index).map(|row| row_cells(row, cx)))
                        .collect()
                },
            )
            .header(header_cells())
            .width_config(ColumnWidthConfig::Resizable(self.columns.clone()))
            .interactable(&self.interaction)
            .map_row(move |(index, row), _window, _cx| {
                let modified = rows_for_highlight
                    .borrow()
                    .get(index)
                    .map_or(0, |row| row.modified);
                let recent =
                    modified != 0 && frame.get().saturating_sub(modified) < HIGHLIGHT_FRAMES;
                if recent {
                    row.bg(highlight).into_any_element()
                } else {
                    row.into_any_element()
                }
            });

        let root = div()
            .relative()
            .size_full()
            .bg(background)
            .text_color(text)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .child(
                canvas(|_, _, _| (), {
                    let clock = clock.clone();
                    move |_, _, _, _| clock.paint_start.set(Some(Instant::now()))
                })
                .absolute()
                .top_0()
                .left_0()
                .size(px(1.0)),
            )
            .child(div().size_full().child(table))
            .child(
                div()
                    .absolute()
                    .top_2()
                    .left_2()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(border)
                    .bg(cx.theme().colors().elevated_surface_background)
                    .text_xs()
                    .children(
                        self.overlay_lines()
                            .into_iter()
                            .map(|line| div().whitespace_nowrap().child(SharedString::from(line))),
                    ),
            )
            // A 1 px canvas timestamps the end of paint for frame timing.
            .child(
                canvas(|_, _, _| (), {
                    let clock = clock.clone();
                    move |_, _, _, _| clock.paint_end.set(Some(Instant::now()))
                })
                .absolute()
                .top_0()
                .left_0()
                .size(px(1.0)),
            );
        clock.build_end.set(Some(Instant::now()));
        root
    }
}

/// Schedule the next frame before advancing the current frame.
fn tick(spike: WeakEntity<TableSpike>, window: &mut Window, cx: &mut App) {
    let Some(entity) = spike.upgrade() else {
        return;
    };
    let next = entity.downgrade();
    window.on_next_frame(move |window, cx| tick(next, window, cx));
    let now = Instant::now();
    entity.update(cx, |spike, cx| {
        spike.on_frame(now, cx);
        cx.notify();
    });
}

fn header_cells() -> Vec<Div> {
    COLUMNS
        .iter()
        .map(|column| div().font_weight(FontWeight::SEMIBOLD).child(column.title))
        .collect()
}

fn row_cells(row: &Row, cx: &App) -> Vec<AnyElement> {
    let muted = cx.theme().colors().text_muted;
    let status = match row.health {
        Health::Ready => gpui::green(),
        Health::Progressing => gpui::yellow(),
        Health::Degraded => gpui::red(),
    };
    vec![
        div().child(row.name.clone()).into_any_element(),
        div()
            .text_color(muted)
            .child(row.namespace.clone())
            .into_any_element(),
        div().child(row.ready.clone()).into_any_element(),
        div()
            .text_color(status)
            .child(row.status.clone())
            .into_any_element(),
        div()
            .text_color(muted)
            .child(row.up_to_date.clone())
            .into_any_element(),
        div()
            .text_color(muted)
            .child(row.available.clone())
            .into_any_element(),
        div()
            .text_color(muted)
            .child(row.restarts.clone())
            .into_any_element(),
        div()
            .text_color(muted)
            .child(row.age.clone())
            .into_any_element(),
        div().child(row.containers.clone()).into_any_element(),
        div()
            .text_color(muted)
            .child(row.image.clone())
            .into_any_element(),
        div()
            .text_color(muted)
            .child(row.pull_policy.clone())
            .into_any_element(),
        div().child(row.strategy.clone()).into_any_element(),
        div()
            .text_color(muted)
            .child(row.selector.clone())
            .into_any_element(),
        div()
            .text_color(muted)
            .child(row.labels.clone())
            .into_any_element(),
        div()
            .text_color(muted)
            .child(row.node.clone())
            .into_any_element(),
        div().child(row.qos.clone()).into_any_element(),
        div()
            .text_color(muted)
            .child(row.cpu_request.clone())
            .into_any_element(),
        div()
            .text_color(muted)
            .child(row.mem_request.clone())
            .into_any_element(),
        div().child(row.ports.clone()).into_any_element(),
        div()
            .text_color(muted)
            .child(row.revision.clone())
            .into_any_element(),
    ]
}

#[cfg(test)]
mod tests {
    use super::Mode;

    #[test]
    fn storm_mode_uses_frequent_updates_copy() {
        assert_eq!(Mode::from_arg("storm"), Some(Mode::Storm));
        assert_eq!(Mode::Storm.label(), "Frequent Updates");
    }
}
