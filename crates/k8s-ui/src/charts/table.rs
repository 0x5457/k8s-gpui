//! Numeric table view for chart data.
//!
//! gpui-kit's `DataTable` owns the header band, the virtualized rows, the cell
//! cursor, and the keyboard navigation. The app supplies the columns, the
//! values, and the data font the column widths follow.

use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableState};
use gpui_kit::component::{Icon, Sizable, Size, h_flex, v_flex};
use gpui_kit::prelude::*;
// Named imports rather than the crate glob: the glob also brings in GPUI's
// `test` attribute macro, which shadows the built-in `#[test]` these table
// layout claims are written with.
use gpui_kit::{App, Div, Hsla, Pixels, Role, SharedString, Stateful, Styled, Window, div, px};

use crate::design::{self, space};
use crate::panels::common::empty_state;
use crate::settings::{self, DataTypography};

use super::ChartData;
use super::TableRow;

/// Characters a time cell holds: `HH:MM:SS`.
const TIME_COLUMNS: f32 = 9.0;
/// Characters a value cell holds: `1023.00 GiB`.
const VALUE_COLUMNS: f32 = 12.0;
/// Height the empty table keeps, so the app's empty state has room to be centred
/// in rather than hung from the top of a panel-sized box.
const EMPTY_STATE_HEIGHT: f32 = 120.0;
/// The time column always comes first and always holds the row order.
const ORDER_COLUMN: usize = 0;
const ORDER_SHORT: &str = "newest first";
const ORDER_DESCRIPTION: &str = "Rows are ordered newest first. This table cannot be re-sorted.";
/// Empty table: the state, and the wait that resolves it.
const EMPTY_TITLE: &str = "No samples yet";

#[derive(IntoElement)]
pub struct ChartTable {
    id: SharedString,
    data: Rc<ChartData>,
}

impl ChartTable {
    pub fn new(id: impl Into<SharedString>, data: Rc<ChartData>) -> Self {
        Self {
            id: id.into(),
            data,
        }
    }
}

fn clamp_selection(
    selected: (usize, usize),
    row_count: usize,
    column_count: usize,
) -> (usize, usize) {
    if row_count == 0 || column_count == 0 {
        return (0, 0);
    }
    (
        selected.0.min(row_count - 1),
        selected.1.min(column_count - 1),
    )
}

impl RenderOnce for ChartTable {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let typography = settings::data_typography(cx);
        let data = self.data;
        let interval_ms = data.interval_ms;
        let headers = data.headers();
        let rows = data.table_rows();
        let row_count = rows.len();
        let column_count = headers.len();
        let label = format!("Metric Samples: {}", headers.join(", "));
        let description = format!(
            "{label}. Use the arrow keys to move between cells, Home and End for the first or last cell in a row, and Page Up and Page Down for a screenful."
        );
        // The table state lives as long as this element does, so the cursor, the
        // scroll offsets, and the column layout survive the live scrape that
        // replaces the samples several times a minute.
        let state = window.use_keyed_state(self.id.clone(), cx, |window, cx| {
            TableState::new(
                ChartTableDelegate::new(
                    rows.clone(),
                    headers.clone(),
                    typography.clone(),
                    interval_ms,
                ),
                window,
                cx,
            )
            .cell_selectable(true)
            // The time column is the row order, so there is no leading row header
            // and no whole-column selection: the reader moves a single cell.
            .row_header(false)
            .row_selectable(false)
            .col_selectable(false)
            // Arrow keys stop at the ends. A table of samples that wraps around
            // reads as a loop, and the reader loses the sample they were on.
            .loop_selection(false)
            // The values are numbers, not a spreadsheet: the order is fixed and
            // the widths come from the data font.
            .sortable(false)
            .col_resizable(false)
            .col_movable(false)
        });
        state.update(cx, |state, cx| {
            if !state.delegate().seated && !rows.is_empty() {
                state.set_selected_cell(0, 0, cx);
            }
            let clamped = clamp_selection(
                state.selected_cell().unwrap_or((0, 0)),
                rows.len(),
                headers.len(),
            );
            if let Some(selected) = state.selected_cell()
                && selected != clamped
            {
                state.set_selected_cell(clamped.0, clamped.1, cx);
            }
            // The component measures the header once, so a new series — a
            // container appearing — or a new data font needs it to measure again.
            let relayout = {
                let delegate = state.delegate_mut();
                let relayout =
                    delegate.headers.len() != headers.len() || delegate.typography != typography;
                delegate.rows = rows;
                delegate.headers = headers;
                delegate.typography = typography.clone();
                delegate.interval_ms = interval_ms;
                delegate.seated = true;
                relayout
            };
            // The selection moves with the arrow keys and the pointer, and a cell
            // is the only thing in this table that knows whether it is the one
            // selected — the component paints the fill and hands the cells
            // nothing. Recording it here is what lets a cell know to read its ink
            // from the selection role instead of the content one, which is the
            // difference between a selected value that can be read and a selected
            // value that is a guess.
            state.delegate_mut().selection = state.selected_cell();
            if relayout {
                state.refresh(cx);
            }
        });
        // The frame around the table owns the visible border, so the component
        // only paints its own rules and the two scrollbars.
        let table = DataTable::new(&state)
            .with_size(Size::Size(typography.row_height()))
            // The stripe comes from the app's row ramp, so this table cannot
            // drift away from the Dock and the resource list.
            .stripe(false)
            .bordered(false)
            .scrollbar_visible(true, true);
        // The component's focus handle is the tab stop and its cells carry the
        // row and cell roles, so the wrapper only names the table.
        v_flex()
            .id(self.id)
            .role(Role::Table)
            .aria_label(label)
            .aria_description(description)
            .aria_keyshortcuts("ArrowLeft ArrowRight ArrowUp ArrowDown Home End PageUp PageDown")
            .aria_row_count(row_count + 1)
            .aria_column_count(column_count)
            .size_full()
            .child(table)
            .into_any_element()
    }
}

/// The rows and columns, and the data font the widths follow.
struct ChartTableDelegate {
    rows: Vec<TableRow>,
    headers: Vec<String>,
    typography: DataTypography,
    /// The scrape interval the data is collected on, which is what the empty
    /// state quotes when it tells the reader how long the wait is.
    interval_ms: i64,
    /// Whether a cursor has been placed yet.
    ///
    /// The component starts in row mode and only enters cell mode on a cell
    /// click, which a reader using only the keyboard cannot make. Seating the
    /// cursor on the first cell once puts the arrow keys in charge of the cell
    /// from the first keypress.
    seated: bool,
    /// The cell the cursor is on, handed down from the component's own state.
    selection: Option<(usize, usize)>,
}

impl ChartTableDelegate {
    fn new(
        rows: Vec<TableRow>,
        headers: Vec<String>,
        typography: DataTypography,
        interval_ms: i64,
    ) -> Self {
        Self {
            rows,
            headers,
            typography,
            interval_ms,
            seated: false,
            selection: None,
        }
    }

    /// One cell's text, and whether it stands in for a sample the series did not
    /// take. An em dash is a gap in the data, so it reads as muted prose rather
    /// than as a number.
    fn cell(&self, row_ix: usize, col_ix: usize) -> (String, bool) {
        let Some(row) = self.rows.get(row_ix) else {
            return ("—".to_owned(), true);
        };
        if col_ix == ORDER_COLUMN {
            return (row.time.clone(), true);
        }
        match row.cells.get(col_ix - 1) {
            Some(value) => (value.clone(), value == "—"),
            None => ("—".to_owned(), true),
        }
    }

    /// The ink one cell's text is drawn in.
    ///
    /// Three cases, and the third is the one the table did not have. A selected
    /// cell is painted with the selection fill by the component, and the content
    /// ink is solved against the *content* surface — so a value on a selection
    /// wash is a value read at whatever contrast the two happen to meet at. The
    /// product already owns the answer in
    /// [`design::text_selection::foreground`], which composites the fill first
    /// and then solves the ink against it, and it is the same role the resource
    /// list and the editor use, so all three agree.
    fn cell_ink(&self, row_ix: usize, col_ix: usize, muted: bool, cx: &App) -> Hsla {
        if self.selection == Some((row_ix, col_ix)) {
            return design::text_selection::foreground(cx);
        }
        if muted {
            design::role::fg_tertiary(cx)
        } else {
            design::role::fg_primary(cx)
        }
    }

    /// Whether a column's cells are numbers, which is what decides its alignment.
    ///
    /// The time column is leading and every other column is trailing, and the
    /// header of each follows its own cells. A column of values that changes
    /// width per row — `1.0 KiB` against `18.4 GiB` — has to be compared
    /// digit by digit, and `Design guides > Alignment details` asks for
    /// comparable numbers to be right-aligned for exactly that reason. The time
    /// column is leading because every one of its cells is the same eight
    /// characters wide, so its alignment is a constant rather than an alignment,
    /// and a clock reads from its left edge.
    fn numeric(column: usize) -> bool {
        column != ORDER_COLUMN
    }
}

impl TableDelegate for ChartTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.headers.len()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        let name = self.headers.get(col_ix).cloned().unwrap_or_default();
        Column::new(SharedString::from(format!("chart-column-{col_ix}")), name)
            // The width follows the data font rather than a constant tuned for the
            // default size, because a raised data font would otherwise ellipsize the
            // longest value and the longest timestamp.
            .width(column_width(col_ix, &self.typography))
            .resizable(false)
            .movable(false)
    }

    fn render_header(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        // One header treatment, and the same one the resource list has: the
        // band's *own* plane is the plane the rows sit on, and a 1px
        // `border.subtle` rule under it is the whole of the separation.
        //
        // The component fills the band with its own `table.head` token, which the
        // product projects onto `surface.background` — a second surface one step
        // above the content plane the rows are painted on, differing by about
        // 1.04:1 in the dark appearance. That is below anything a reader can see
        // and above the point at which they start wondering why the header is a
        // slightly different colour, so the band takes the rows' own surface and
        // the rule does the work. One rule, drawn once, by the band that owns it:
        // the resource list had a second one inset by the cell padding and it read
        // as a dashed rule with a notch over every divider.
        div()
            .id("chart-header-row")
            .bg(design::role::surface_content(cx))
            .border_b_1()
            .border_color(design::role::border_subtle(cx))
    }

    /// Column heading for the value table.
    ///
    /// `text::CAPTION` semibold in `fg.tertiary`, the treatment the resource
    /// list's column headings use, and trailing for the numeric columns because
    /// their cells are. The time column carries a marker for its fixed order
    /// instead of a sort control the table cannot honour, in a lane of its own so
    /// the word stays on the same edge in every row of the band.
    ///
    /// The heading reads in the UI font while the cells below read in the data
    /// font, and the column *width* follows the data font so a raised size cannot
    /// ellipsize the values under the headings.
    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let ordered = col_ix == ORDER_COLUMN;
        let text = self.headers.get(col_ix).cloned().unwrap_or_default();
        let mut head = h_flex()
            .id(("chart-header-cell", col_ix))
            .h_full()
            .items_center()
            .gap(space::XS)
            .when(ChartTableDelegate::numeric(col_ix), |head| {
                head.justify_end()
            })
            .text_size(design::text::CAPTION)
            .line_height(design::text::CAPTION_LINE_HEIGHT)
            .font_weight(design::text::SEMIBOLD)
            .text_color(design::role::fg_tertiary(cx))
            .aria_label(if ordered {
                format!("{text}, {ORDER_SHORT}")
            } else {
                text.clone()
            })
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    // The resource table's header treatment: CAPTION uppercased.
                    // The aria label above keeps the spoken sentence case.
                    .child(text.to_uppercase()),
            );
        if ordered {
            head = head.aria_description(ORDER_DESCRIPTION).child(
                Icon::new(IconName::ArrowDown)
                    .with_size(Size::Size(design::icon::IN_ROW))
                    // Incidental: the marker repeats the order the column's spoken
                    // label already names, and the table cannot be sorted by a
                    // click, so it is decoration on the word rather than a control.
                    .text_color(design::icon::incidental(cx)),
            );
        }
        head.into_any_element()
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        // Every row is the same plane. The stripe was here to keep the *columns*
        // readable, and it did that by drawing a second background under a
        // surface the component already owns — so a chart table, the Dock's log
        // table and the resource table each had their own idea of what a row
        // looks like, and one of them striped. `UI-SPEC` §4.4 gives a table row
        // height and hover and nothing else, and the column headings plus the
        // tabular figures do the alignment work the stripe was doing.
        //
        // Hover and the keyboard cursor are the component's own row states, which
        // the product theme projects onto `element.hover` and `element.selected`:
        // the same two washes the resource list takes, from the same theme. They
        // are applied *after* this style, so a row background here cannot silence
        // them — and that ordering is why this is a base colour and not a
        // hover-group wash.
        div()
            .id(("chart-row", row_ix))
            .bg(design::role::surface_content(cx))
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let (text, muted) = self.cell(row_ix, col_ix);
        let ink = self.cell_ink(row_ix, col_ix, muted, cx);
        // The grid position, the way the resource list numbers it: the header is
        // a row of the grid, so the first sample is row 2 and the time column is
        // column 1. The component sets `Role::Row` on the row and nothing at all
        // on a cell, so a screen reader reading this table was told a row and
        // had to invent the position inside it.
        //
        // A cell needs an id to carry a role, which is also what makes it
        // addressable; the two indices are packed into one `u64` so a cell in a
        // 10,000-row table has a distinct name.
        let cell_key = ((row_ix as u64) << 32) | col_ix as u64;
        self.typography
            .apply(
                div()
                    .id(("chart-cell", cell_key))
                    .role(Role::Cell)
                    .aria_row_index(row_ix + 2)
                    .aria_column_index(col_ix + 1)
                    .min_w_0()
                    .h_full()
                    .flex()
                    .items_center()
                    .when(ChartTableDelegate::numeric(col_ix), |cell| {
                        cell.justify_end()
                    })
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_color(ink)
                    .child(text),
            )
            .into_any_element()
    }

    fn render_empty(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        // The product's own empty state, not a second one: a 24px `fg.tertiary`
        // glyph, a `title` line, and one sentence that names the wait. The table
        // used to print `No Samples Yet` in `text_muted` with no glyph and no
        // second line, which is a heading, not a state.
        //
        // The loader is the honest glyph here — the state genuinely is "waiting
        // for the first scrape" — and it is the one glyph that earns the sweep,
        // because a table that fills with samples a few seconds later is the one
        // case where a spinner is real waiting and not decoration.
        div()
            .id("chart-empty-state")
            .role(Role::Status)
            .aria_label(EMPTY_TITLE)
            .size_full()
            .min_h(px(EMPTY_STATE_HEIGHT))
            .flex()
            .items_center()
            .justify_center()
            .child(empty_state(
                IconName::LoaderCircle,
                EMPTY_TITLE,
                super::geometry::waiting_for_next_scrape(self.interval_ms),
            ))
            .into_any_element()
    }
}

/// Column width from the configured data font.
///
/// `TIME_COLUMNS` and `VALUE_COLUMNS` hold the widest text each column can
/// receive, so a column is sized for its own characters and the component's
/// cell padding sits on top of it.
fn column_width(column: usize, typography: &DataTypography) -> Pixels {
    if column == ORDER_COLUMN {
        typography.columns(TIME_COLUMNS)
    } else {
        typography.columns(VALUE_COLUMNS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charts::{Series, SeriesColor, Unit};
    use crate::settings::test_data_typography;
    use k8s_core::metrics::NormalizedPoint;

    #[test]
    fn chart_rows_follow_data_line_height() {
        // The floor is the product's default table row: `UI-SPEC` §4.4 sets it
        // at 32 (comfortable) with 28 as the `normal` step, so an 18px data line
        // is carried by the row rather than setting it. A line taller than the
        // floor wins, plus the pixel of slack a full-height cell needs.
        assert_eq!(design::row_height(px(18.)), px(32.));
        assert_eq!(design::row_height(px(36.)), px(37.));
    }

    #[test]
    fn chart_table_columns_hold_their_widest_value() {
        let typography = test_data_typography(12., 18.);
        let advance = f32::from(typography.columns(1.0));
        // `HH:MM:SS` and `1023.00 GiB` must both fit without an ellipsis. The
        // table supplies its own cell padding on top of these column widths.
        assert!(f32::from(column_width(0, &typography)) >= 8.0 * advance);
        assert!(f32::from(column_width(1, &typography)) >= 11.0 * advance);
        assert_eq!(column_width(2, &typography), column_width(1, &typography));
    }

    #[test]
    fn chart_table_columns_widen_with_the_data_font() {
        let small = test_data_typography(12., 18.);
        let large = test_data_typography(24., 36.);
        for column in 0..3 {
            assert_eq!(
                f32::from(column_width(column, &large)),
                2.0 * f32::from(column_width(column, &small)),
                "a twice-as-large data font must double the column instead of clipping"
            );
        }
    }

    #[test]
    fn chart_table_selection_clamps_when_data_shrinks() {
        assert_eq!(clamp_selection((8, 6), 2, 3), (1, 2));
        assert_eq!(clamp_selection((8, 6), 0, 3), (0, 0));
    }

    #[test]
    fn memory_only_series_keeps_a_numeric_row() {
        let data = ChartData {
            series: vec![Series {
                label: "Memory".into(),
                unit: Unit::Memory,
                color: SeriesColor::Info,
                filled: false,
                points: vec![NormalizedPoint {
                    at_ms: 1_000,
                    value: Some(1024.0),
                }],
            }],
            interval_ms: 1_000,
        };
        let rows = data.table_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cells, ["1.0 KiB"]);
    }
}
