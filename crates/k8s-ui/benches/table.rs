//! Benchmarks the real `ui::data_table` per-frame cost with 5, 10, and 20 columns,
//! 1,000 and 10,000 rows, and static, scroll, and Frequent Updates workloads.
//!
//! Run `cargo bench -p k8s-ui --bench table`.
//! Run one case with `cargo bench -p k8s-ui --bench table -- 10000x20-frequent-updates`.
//!
//! Notes:
//! - Benchmarks use the release profile. Debug benchmark values are not representative; the macro panics in debug builds because element and layout costs differ.
//! - Linux has no headless renderer. The benchmark measures element construction, layout, prepaint, and paint. It does not include GPU submission.
//! - `fps = 120` uses an 8.3 ms frame budget for overrun counts.

use std::{cell::RefCell, fmt, rc::Rc, time::Duration};

use gpui::{
    AnyElement, App, BenchAppContext, ElementId, Entity, Font, FontWeight, IntoElement,
    ParentElement, Render, Styled, Text, Window, div, px,
};
use k8s_ui::spike::data::{COLUMNS, Health, Lcg, Row, mutate_row, synthetic_rows};
use theme::LoadThemes;
use ui::prelude::*;
use ui::{
    ColumnWidthConfig, ResizableColumnsState, Table, TableInteractionState, TableResizeBehavior,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Workload {
    Static,
    Scroll,
    Storm,
}

impl Workload {
    fn label(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Scroll => "scroll",
            Self::Storm => "frequent-updates",
        }
    }
}

/// Rows changed per frame in the Frequent Updates workload.
const STORM_ROWS_PER_FRAME: usize = 500;
/// Scroll speed in pixels per second. Horizontal speed is 40% of vertical speed.
const SCROLL_SPEED: f32 = 400.0;

#[derive(Clone, Copy, Debug)]
struct Case {
    rows: usize,
    column_count: usize,
    workload: Workload,
}

impl fmt::Display for Case {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}x{}-{}",
            self.rows,
            self.column_count,
            self.workload.label()
        )
    }
}

fn cases() -> Vec<Case> {
    [1_000usize, 10_000]
        .into_iter()
        .flat_map(|rows| {
            [5usize, 10, 20].into_iter().flat_map(move |column_count| {
                [Workload::Static, Workload::Scroll, Workload::Storm].map(move |workload| Case {
                    rows,
                    column_count,
                    workload,
                })
            })
        })
        .collect()
}

struct TableBench {
    workload: Workload,
    row_count: usize,
    column_count: usize,
    rows: Rc<RefCell<Vec<Row>>>,
    columns: Entity<ResizableColumnsState>,
    interaction: Entity<TableInteractionState>,
    rng: Lcg,
    frame: u64,
    scroll_y: f32,
    scroll_x: f32,
    storm_cursor: usize,
}

impl TableBench {
    fn new(row_count: usize, column_count: usize, workload: Workload, cx: &mut App) -> Self {
        let columns = cx.new(|_| {
            ResizableColumnsState::new(
                column_count,
                COLUMNS
                    .iter()
                    .take(column_count)
                    .map(|column| px(column.width))
                    .collect(),
                vec![TableResizeBehavior::MinSize(4.0); column_count],
            )
        });
        let interaction = cx.new(|cx| TableInteractionState::new(cx));
        Self {
            workload,
            row_count,
            column_count,
            rows: Rc::new(RefCell::new(synthetic_rows(row_count, 0x5EED_1234))),
            columns,
            interaction,
            rng: Lcg::new(0xC0FF_EE00),
            frame: 0,
            scroll_y: 0.0,
            scroll_x: 0.0,
            storm_cursor: 0,
        }
    }

    /// Advances one frame of workload. The benchmark renderer calls this method during timing.
    fn tick(&mut self, cx: &App) {
        self.frame += 1;
        match self.workload {
            Workload::Static => {}
            Workload::Scroll => self.advance_scroll(cx),
            Workload::Storm => self.apply_storm(),
        }
    }

    fn advance_scroll(&mut self, cx: &App) {
        let interaction = self.interaction.read(cx);

        let list = &interaction.scroll_handle;
        let max_y = f32::from(list.0.borrow().base_handle.max_offset().y);
        self.scroll_y = (self.scroll_y + SCROLL_SPEED / 60.0) % max_y.max(1.0);
        list.0
            .borrow()
            .base_handle
            .set_offset(gpui::point(px(0.0), px(-self.scroll_y)));

        let horizontal = &interaction.horizontal_scroll_handle;
        let max_x = f32::from(horizontal.max_offset().x);
        self.scroll_x = (self.scroll_x + SCROLL_SPEED * 0.4 / 60.0) % max_x.max(1.0);
        horizontal.set_offset(gpui::point(px(-self.scroll_x), px(0.0)));
    }

    fn apply_storm(&mut self) {
        let count = STORM_ROWS_PER_FRAME.min(self.row_count);
        let mut rows = self.rows.borrow_mut();
        for offset in 0..count {
            let index = (self.storm_cursor + offset) % self.row_count;
            if let Some(row) = rows.get_mut(index) {
                mutate_row(row, &mut self.rng, self.frame);
            }
        }
        self.storm_cursor = (self.storm_cursor + count) % self.row_count;
    }
}

impl Render for TableBench {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows.clone();
        let column_count = self.column_count;
        let data_font = theme::theme_settings(cx).buffer_font(cx).clone();
        let table = Table::new(column_count)
            .no_ui_font()
            .disable_base_style()
            .uniform_list(
                "table-bench-rows",
                self.row_count,
                move |range, _window, cx| {
                    let rows = rows.borrow();
                    range
                        .filter_map(|index| {
                            rows.get(index)
                                .map(|row| row_cells(index, row, column_count, &data_font, cx))
                        })
                        .collect()
                },
            )
            .header(header_cells(column_count))
            .width_config(ColumnWidthConfig::Resizable(self.columns.clone()))
            .interactable(&self.interaction);
        div().size_full().child(table)
    }
}

#[gpui::bench(
    inputs = cases(),
    input_name = "case",
    group = "resource_table",
    fps = 120,
    sample_size = 10
)]
fn table_frames(case: &Case, cx: &mut BenchAppContext) {
    if cfg!(debug_assertions) {
        eprintln!(
            "Warning: Debug benchmark values are not representative. Run `cargo bench -p k8s-ui --bench table`."
        );
    }
    // ui::data_table reads cx.theme() and ThemeSettings. The benchmark has no app setup, so install the built-in Base theme before timing starts.
    cx.update(|cx| {
        settings::init(cx);
        theme_settings::init(LoadThemes::JustBase, cx);
    });

    let mut window = cx.add_empty_window();
    let view = window.update(|window, cx| {
        window.replace_root(cx, |_, cx| {
            TableBench::new(case.rows, case.column_count, case.workload, cx)
        })
    });

    cx.bench_renderer(view, |bench, _window, cx| {
        bench.tick(cx);
        cx.notify();
    });
}

fn header_cells(column_count: usize) -> Vec<Div> {
    COLUMNS
        .iter()
        .take(column_count)
        .map(|column| div().font_weight(FontWeight::SEMIBOLD).child(column.title))
        .collect()
}

fn row_cells(
    row_index: usize,
    row: &Row,
    column_count: usize,
    data_font: &Font,
    cx: &App,
) -> Vec<AnyElement> {
    let muted = cx.theme().colors().text_muted;
    let status = match row.health {
        Health::Ready => gpui::green(),
        Health::Progressing => gpui::yellow(),
        Health::Degraded => gpui::red(),
    };
    (0..column_count)
        .map(|column| {
            let text = match column {
                0 => row.name.clone(),
                1 => row.namespace.clone(),
                2 => row.ready.clone(),
                3 => row.status.clone(),
                4 => row.up_to_date.clone(),
                5 => row.available.clone(),
                6 => row.restarts.clone(),
                7 => row.age.clone(),
                8 => row.containers.clone(),
                9 => row.image.clone(),
                10 => row.pull_policy.clone(),
                11 => row.strategy.clone(),
                12 => row.selector.clone(),
                13 => row.labels.clone(),
                14 => row.node.clone(),
                15 => row.qos.clone(),
                16 => row.cpu_request.clone(),
                17 => row.mem_request.clone(),
                18 => row.ports.clone(),
                19 => row.revision.clone(),
                _ => unreachable!(),
            };
            let cell_id = ElementId::NamedInteger(
                "bench-cell".into(),
                ((row_index as u64) << 16) | column as u64,
            );
            let cell = div()
                .child(Text::new(cell_id, text))
                .font(data_font.clone())
                .text_size(rems_from_px(12.0_f32))
                .line_height(rems_from_px(14.0_f32))
                .overflow_hidden()
                .px_1()
                .py_0p5();

            match column {
                1 | 4..=7 | 9 | 10 | 12..=14 | 16 | 17 | 19 => {
                    cell.text_color(muted).into_any_element()
                }
                3 => cell.text_color(status).into_any_element(),
                _ => cell.into_any_element(),
            }
        })
        .collect()
}

gpui::bench_group! {
    name = benches;
    config = criterion::Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3))
        .without_plots();
    targets = table_frames
}
gpui::bench_main!(benches);
