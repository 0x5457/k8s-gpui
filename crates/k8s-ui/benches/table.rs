//! Benchmarks the real table's per-frame cost with 5, 10, and 20 columns,
//! 1,000 and 10,000 rows, and static, scroll, and Frequent Updates workloads.
//!
//! It drives the same `SpikeTable` the interactive spike renders, so the
//! benchmark and the harness measure one table.
//!
//! Run `cargo bench -p k8s-ui --bench table`.
//! Run one case with `cargo bench -p k8s-ui --bench table -- 10000x20-frequent-updates`.
//!
//! Notes:
//! - Benchmarks use the release profile. Debug benchmark values are not representative; the macro panics in debug builds because element and layout costs differ.
//! - Linux has no headless renderer. The benchmark measures element construction, layout, prepaint, and paint. It does not include GPU submission.
//! - `fps = 120` uses an 8.3 ms frame budget for overrun counts.

use std::{
    cell::{Cell, RefCell},
    fmt,
    rc::Rc,
    time::Duration,
};

use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::{Sizable, Size};
use gpui_kit::prelude::*;
use gpui_kit::{BenchAppContext, Entity, SharedString, Window, div, point, px};
use k8s_ui::settings::data_typography;
use k8s_ui::spike::SpikeTable;
use k8s_ui::spike::data::{Lcg, Row, mutate_row, synthetic_rows};

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
    table: Option<Entity<TableState<SpikeTable>>>,
    frame: Rc<Cell<u64>>,
    rng: Lcg,
    scroll_y: f32,
    scroll_x: f32,
    storm_cursor: usize,
}

impl TableBench {
    fn new(row_count: usize, column_count: usize, workload: Workload) -> Self {
        Self {
            workload,
            row_count,
            column_count,
            rows: Rc::new(RefCell::new(synthetic_rows(row_count, 0x5EED_1234))),
            // The table state needs a window, which only the first render has.
            table: None,
            frame: Rc::new(Cell::new(0)),
            rng: Lcg::new(0xC0FF_EE00),
            scroll_y: 0.0,
            scroll_x: 0.0,
            storm_cursor: 0,
        }
    }

    /// Advances one frame of workload. The benchmark renderer calls this method during timing.
    fn tick(&mut self, cx: &mut Context<Self>) {
        self.frame.set(self.frame.get() + 1);
        match self.workload {
            Workload::Static => {}
            Workload::Scroll => self.advance_scroll(cx),
            Workload::Storm => self.apply_storm(),
        }
    }

    fn advance_scroll(&mut self, cx: &mut Context<Self>) {
        let Some(table) = self.table.clone() else {
            return;
        };
        let (max_y, max_x) = table.read_with(cx, |table, _| {
            (
                table
                    .vertical_scroll_handle
                    .0
                    .borrow()
                    .base_handle
                    .max_offset(),
                table.horizontal_scroll_handle.base_handle().max_offset(),
            )
        });
        self.scroll_y = (self.scroll_y + SCROLL_SPEED / 60.0) % f32::from(max_y.y).max(1.0);
        self.scroll_x = (self.scroll_x + SCROLL_SPEED * 0.4 / 60.0) % f32::from(max_x.x).max(1.0);
        table.update(cx, |table, _| {
            table
                .vertical_scroll_handle
                .0
                .borrow_mut()
                .base_handle
                .set_offset(point(px(0.0), px(-self.scroll_y)));
            table
                .horizontal_scroll_handle
                .base_handle()
                .set_offset(point(px(-self.scroll_x), px(0.0)));
        });
    }

    fn apply_storm(&mut self) {
        let count = STORM_ROWS_PER_FRAME.min(self.row_count);
        let frame = self.frame.get();
        let mut rows = self.rows.borrow_mut();
        for offset in 0..count {
            let index = (self.storm_cursor + offset) % self.row_count;
            if let Some(row) = rows.get_mut(index) {
                mutate_row(row, &mut self.rng, frame);
            }
        }
        self.storm_cursor = (self.storm_cursor + count) % self.row_count;
    }
}

impl Render for TableBench {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.table.is_none() {
            let delegate = SpikeTable::new(
                self.row_count,
                self.column_count,
                self.rows.clone(),
                self.frame.clone(),
                Rc::new(Cell::new((0, 0))),
            );
            self.table = Some(window.use_keyed_state(
                SharedString::from("table-bench"),
                cx,
                |window, cx| {
                    TableState::new(delegate, window, cx)
                        // The benchmark has no reader: it drives the scroll from
                        // the bench tick and keeps its own frame counter.
                        .row_selectable(false)
                        .col_selectable(false)
                        .cell_selectable(false)
                        .row_header(false)
                        .sortable(false)
                        .col_movable(false)
                        .col_resizable(true)
                },
            ));
        }
        let state = self
            .table
            .clone()
            .expect("the table state is created on the first frame");
        // The benchmark times the table alone, at the row height the panel gives
        // it, so the numbers are comparable with the interactive spike.
        let row_height = data_typography(cx).row_height();
        div().size_full().child(
            DataTable::new(&state)
                .with_size(Size::Size(row_height))
                .bordered(false),
        )
    }
}

#[gpui_kit::bench(
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

    let mut window = cx.add_empty_window();
    let view = window.update(|window, cx| {
        window.replace_root(cx, |_, _| {
            TableBench::new(case.rows, case.column_count, case.workload)
        })
    });

    cx.bench_renderer(view, |bench, _window, cx| {
        bench.tick(cx);
        cx.notify();
    });
}

gpui_kit::bench_group! {
    name = benches;
    config = criterion::Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3))
        .without_plots();
    targets = table_frames
}
gpui_kit::bench_main!(benches);
