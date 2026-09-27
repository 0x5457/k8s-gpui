//! Numeric table view for chart data.
//!
//! The table supports keyboard navigation. Small sample windows use regular rows. Use a
//! virtual list for large windows.

use std::rc::Rc;

use gpui::{
    AnyElement, App, ElementId, Entity, FocusHandle, Font, Hsla, IntoElement, MouseButton,
    RenderOnce, Role, ScrollHandle, SharedString, Styled, Window, div, px,
};
use ui::prelude::*;
use ui::{ScrollAxes, Scrollbars, Tooltip, WithScrollbar};

use crate::design::{self, space};
use crate::settings::DataTypography;

use super::ChartData;

/// Characters a time cell holds: `HH:MM:SS`.
const TIME_COLUMNS: f32 = 9.0;
/// Characters a value cell holds: `1023.00 GiB`.
const VALUE_COLUMNS: f32 = 12.0;
const EMPTY_STATE_HEIGHT: f32 = 120.0;
const TABLE_TAB_INDEX: isize = 4;
/// The time column always comes first and always holds the row order.
const ORDER_COLUMN: usize = 0;
const ORDER_SHORT: &str = "newest first";
const ORDER_DESCRIPTION: &str = "Rows are ordered newest first. This table cannot be re-sorted.";

#[derive(IntoElement)]
pub struct ChartTable {
    id: SharedString,
    data: Rc<ChartData>,
}

struct ChartTableState {
    focus: FocusHandle,
    selected_row: usize,
    selected_column: usize,
    scroll: ScrollHandle,
}

/// Background tone of one data row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowTone {
    Rest,
    Stripe,
    Selected,
}

/// Row tone for the value table.
///
/// The cursor highlight follows focus, so a table nobody has entered keeps its
/// resting state and the newest sample row does not read as a selected row.
fn row_tone(focused: bool, cursor: bool, striped: bool) -> RowTone {
    if focused && cursor {
        RowTone::Selected
    } else if striped {
        RowTone::Stripe
    } else {
        RowTone::Rest
    }
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

fn next_cell(
    selected: (usize, usize),
    key: &str,
    row_count: usize,
    column_count: usize,
    whole_table: bool,
) -> Option<(usize, usize)> {
    if row_count == 0 || column_count == 0 {
        return None;
    }
    let (row, column) = selected;
    let next = match key {
        "left" => (row, column.saturating_sub(1)),
        "right" => (row, (column + 1).min(column_count - 1)),
        "up" => (row.saturating_sub(1), column),
        "down" => ((row + 1).min(row_count - 1), column),
        "home" if whole_table => (0, 0),
        "home" => (row, 0),
        "end" if whole_table => (row_count - 1, column_count - 1),
        "end" => (row, column_count - 1),
        "pageup" => (row.saturating_sub(10), column),
        "pagedown" => ((row + 10).min(row_count - 1), column),
        _ => return None,
    };
    Some(next)
}

impl RenderOnce for ChartTable {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let canvas = design::surface::canvas(cx);
        let ui_font = theme::theme_settings(cx).ui_font(cx).clone();
        let typography = crate::settings::data_typography(cx);
        let row_height = typography.row_height();
        let headers = self.data.headers();
        let rows = self.data.table_rows();
        let row_count = rows.len();
        let column_count = headers.len();
        let state = window.use_keyed_state((self.id.clone(), 0usize), cx, |_window, cx| {
            ChartTableState {
                focus: cx.focus_handle().tab_stop(true).tab_index(TABLE_TAB_INDEX),
                selected_row: 0,
                selected_column: 0,
                scroll: ScrollHandle::new(),
            }
        });
        let (selected_row, selected_column) = state.update(cx, |state, _| {
            let selected = clamp_selection(
                (state.selected_row, state.selected_column),
                row_count,
                column_count,
            );
            state.selected_row = selected.0;
            state.selected_column = selected.1;
            selected
        });
        let focus = state.read(cx).focus.clone();
        let scroll = state.read(cx).scroll.clone();
        let focused = focus.is_focused(window);
        let table_width = table_width(column_count, &typography);
        let table_label = format!("Metric Samples: {}", headers.join(", "));
        let table_description = format!(
            "{table_label}. Use arrow keys to move between cells. Use Home and End for the first or last cell in a row. Use Control+Home or Control+End for the first or last cell in the table."
        );
        // The header and row states only light up while the table holds the cursor.
        // At rest the table reads as a plain value list, like every other data table.
        let header_background = design::surface::control(cx);
        let selected_background = design::row_selected_bg(cx);
        let selected_cell_background = design::surface::selected(cx);
        let row_hover_background = design::row_hover_bg(cx);
        // One stripe alpha for every data surface, so this table cannot drift
        // away from the tables in the Dock and the resource list.
        let stripe_background = design::row_stripe_bg(cx);
        let focus_border = design::focus::border(cx);
        let header = h_flex()
            .id("chart-header-row")
            .role(Role::Row)
            .aria_row_index(1)
            .flex_none()
            .w(px(table_width))
            .h(row_height)
            .px(space::SM)
            .gap(space::SM)
            .items_stretch()
            .border_b_1()
            .border_color(colors.border_variant)
            .children(headers.into_iter().enumerate().map(|(index, header)| {
                header_cell(
                    header,
                    index,
                    focused && index == selected_column,
                    &ui_font,
                    &typography,
                    &colors,
                    header_background,
                    selected_cell_background,
                )
            }));
        let data_state = state.clone();
        let data_focus = focus.clone();
        let data_rows = rows.into_iter().enumerate().map(move |(row_index, row)| {
            let tone = row_tone(focused, row_index == selected_row, row_index % 2 != 0);
            let selected = tone == RowTone::Selected;
            let background = match tone {
                RowTone::Selected => selected_background,
                RowTone::Stripe => stripe_background,
                RowTone::Rest => canvas.alpha(1.0),
            };
            let hover_background = if selected {
                selected_background
            } else {
                row_hover_background
            };
            let row_label = format!(
                "{}: {}",
                row.time,
                row.cells
                    .iter()
                    .map(|cell| if cell == "—" {
                        "No sample"
                    } else {
                        cell.as_str()
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let row_element = h_flex()
                .id(("chart-row", row_index))
                .role(Role::Row)
                .aria_row_index(row_index + 2)
                .aria_label(row_label)
                // Selection only exists while the table holds the cursor, so the row
                // must not claim it while the table is at rest.
                .aria_selected(focused && row_index == selected_row)
                .flex_none()
                .w(px(table_width))
                .h(row_height)
                .px(space::SM)
                .gap(space::SM)
                .bg(background)
                .border_b_1()
                .border_color(colors.border_variant)
                .child(data_cell(
                    row.time,
                    0,
                    row_index,
                    &typography,
                    if row_index == selected_row && selected_column == 0 {
                        colors.text
                    } else {
                        colors.text_muted
                    },
                    row_index == selected_row && selected_column == 0,
                    focused && row_index == selected_row && selected_column == 0,
                    selected_cell_background,
                    focus_border,
                    data_state.clone(),
                    data_focus.clone(),
                ))
                .children(row.cells.into_iter().enumerate().map(|(index, value)| {
                    let column = index + 1;
                    let cursor = row_index == selected_row && column == selected_column;
                    data_cell(
                        value.clone(),
                        column,
                        row_index,
                        &typography,
                        if cursor {
                            colors.text
                        } else if value == "—" {
                            colors.text_muted
                        } else {
                            colors.text
                        },
                        cursor,
                        focused && cursor,
                        selected_cell_background,
                        focus_border,
                        data_state.clone(),
                        data_focus.clone(),
                    )
                }));
            if selected {
                row_element
            } else {
                row_element.hover(move |style| style.bg(hover_background))
            }
        });
        let content = v_flex()
            .id("chart-table-content")
            .w(px(table_width))
            .min_w(px(table_width))
            .flex_grow_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&scroll)
            .custom_scrollbars(
                Scrollbars::new(ScrollAxes::Both)
                    .tracked_scroll_handle(&scroll)
                    .id(format!("chart-scroll-{}", self.id)),
                window,
                cx,
            )
            .restrict_scroll_to_axis()
            .child(header)
            .children(data_rows);
        let content = if row_count == 0 {
            content.child(chart_empty_state(cx))
        } else {
            content
        };
        let key_state = state.clone();
        let root_focus = focus.clone();
        v_flex()
            .id(self.id)
            .role(Role::Table)
            .aria_label(table_label)
            .aria_description(table_description)
            .aria_keyshortcuts(
                "ArrowLeft ArrowRight ArrowUp ArrowDown Home End Control+Home Control+End PageUp PageDown",
            )
            .aria_row_count(row_count + 1)
            .aria_column_count(column_count)
            .tab_group()
            .tab_index(TABLE_TAB_INDEX)
            .track_focus(&focus)
            .on_key_down(move |event, _window, cx| {
                let key = event.keystroke.key.as_str();
                let whole_table = event.keystroke.modifiers.control;
                if event.keystroke.modifiers.alt
                    || event.keystroke.modifiers.platform
                    || (whole_table && key != "home" && key != "end")
                {
                    return;
                }
                let Some(next) = next_cell(
                    (selected_row, selected_column),
                    key,
                    row_count,
                    column_count,
                    whole_table,
                ) else {
                    return;
                };
                key_state.update(cx, |state, cx| {
                    if state.selected_row == next.0 && state.selected_column == next.1 {
                        false
                    } else {
                        state.selected_row = next.0;
                        state.selected_column = next.1;
                        state.scroll.scroll_to_item(next.0 + 1);
                        cx.notify();
                        true
                    }
                });
                cx.stop_propagation();
            })
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                window.focus(&root_focus, cx);
                cx.stop_propagation();
            })
            .size_full()
            .child(content)
            .into_any_element()
    }
}

/// Column heading for the value table.
///
/// The treatment matches the other data tables in the app: a surface band, bold
/// metadata, and muted text that steps up to full text for the cursor column. The
/// time column carries a marker for its fixed order instead of a sort control the
/// table cannot honour. The heading reads in the UI font at the metadata role
/// while the cells below read in the data font, and the width follows the data
/// font so a raised size cannot ellipsize the values under the headings.
#[allow(clippy::too_many_arguments)]
fn header_cell(
    text: String,
    column: usize,
    active: bool,
    font: &Font,
    typography: &DataTypography,
    colors: &theme::ThemeColors,
    background: Hsla,
    active_background: Hsla,
) -> AnyElement {
    let ordered = column == ORDER_COLUMN;
    let aria = if ordered {
        format!("{text}, {ORDER_SHORT}")
    } else {
        text.clone()
    };
    let tooltip = if ordered {
        format!("{text} — {ORDER_SHORT}")
    } else {
        text.clone()
    };
    let label = div()
        .min_w_0()
        .overflow_hidden()
        .text_ellipsis()
        .whitespace_nowrap()
        .child(text);
    // The cursor column steps up to the selected surface and full-strength text.
    let (band, foreground) = if active {
        (active_background, colors.text)
    } else {
        (background, colors.text_muted)
    };
    let mut cell = h_flex()
        .id(("chart-header-cell", column))
        .role(Role::ColumnHeader)
        .aria_label(aria)
        .aria_column_index(column + 1)
        .flex_none()
        .w(column_width(column, typography))
        .items_center()
        .gap(space::XS)
        .bg(band)
        .font(font.clone())
        .text_size(rems_from_px(f32::from(design::text::METADATA)))
        .line_height(rems_from_px(f32::from(design::text::METADATA_LINE_HEIGHT)))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(foreground)
        .child(label);
    if active {
        cell = cell.aria_selected(true);
    }
    if ordered {
        cell = cell.aria_description(ORDER_DESCRIPTION).child(
            Icon::new(IconName::ArrowDown)
                .size(IconSize::XSmall)
                .color(Color::Muted),
        );
    }
    cell.tooltip(Tooltip::text(tooltip)).into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn data_cell(
    text: String,
    column: usize,
    row: usize,
    typography: &DataTypography,
    color: Hsla,
    selected: bool,
    focused: bool,
    selected_background: Hsla,
    focus_color: Hsla,
    state: Entity<ChartTableState>,
    focus: FocusHandle,
) -> AnyElement {
    let mut cell = typography.apply(
        div()
            .id(ElementId::NamedInteger(
                "chart-cell".into(),
                ((row as u64) << 16) | column as u64,
            ))
            .role(Role::Cell)
            .aria_label(if text == "—" {
                "No sample".to_owned()
            } else {
                text.clone()
            })
            .aria_column_index(column + 1)
            .aria_selected(selected)
            .relative()
            .flex_none()
            .w(column_width(column, typography))
            .text_color(color)
            .whitespace_nowrap()
            .overflow_hidden()
            .text_ellipsis(),
    );
    // The cursor background follows focus, so a table nobody has entered keeps its
    // resting state instead of showing a stray selection slab.
    if selected && focused {
        cell = cell.bg(selected_background).aria_active_descendant();
    }
    if focused {
        cell = cell.child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .bottom_0()
                .w(design::border::FOCUS_RAIL)
                .bg(focus_color),
        );
    }
    let click_state = state.clone();
    let click_focus = focus.clone();
    let cell = cell.on_mouse_down(MouseButton::Left, move |_, window, cx| {
        let changed = click_state.update(cx, |state, cx| {
            if state.selected_row == row && state.selected_column == column {
                false
            } else {
                state.selected_row = row;
                state.selected_column = column;
                state.scroll.scroll_to_item(row + 1);
                cx.notify();
                true
            }
        });
        window.focus(&click_focus, cx);
        if changed {
            cx.stop_propagation();
        }
    });
    let tooltip_text = if text == "—" {
        "No sample".to_owned()
    } else {
        text.clone()
    };
    cell.tooltip(Tooltip::text(tooltip_text))
        .child(text)
        .into_any_element()
}

/// Column width from the configured data font.
///
/// The cell widths follow the data font instead of a constant tuned for the
/// default size, because a raised data font would otherwise ellipsize the
/// longest value and the longest timestamp. `TIME_COLUMNS` and `VALUE_COLUMNS`
/// hold the widest text each column can receive, plus one column of slack so
/// the value is not flush against the cell edge.
fn column_width(column: usize, typography: &DataTypography) -> gpui::Pixels {
    if column == ORDER_COLUMN {
        typography.columns(TIME_COLUMNS)
    } else {
        typography.columns(VALUE_COLUMNS)
    }
}

fn table_width(column_count: usize, typography: &DataTypography) -> f32 {
    let column_count = column_count.max(1);
    let data_columns = column_count - 1;
    // The row supplies the outer padding and the inter-cell gaps, so a column
    // only has to hold its own characters.
    f32::from(typography.columns(TIME_COLUMNS))
        + f32::from(typography.columns(VALUE_COLUMNS)) * data_columns as f32
        + f32::from(space::SM) * (data_columns + 2) as f32
}

fn chart_empty_state(cx: &App) -> AnyElement {
    v_flex()
        .id("chart-empty-state")
        .role(Role::Status)
        .aria_label("No Metric Samples")
        .flex_1()
        .min_h(px(EMPTY_STATE_HEIGHT))
        .items_center()
        .justify_center()
        .gap(space::XS)
        .child(
            div()
                .font_ui(cx)
                .text_size(rems_from_px(f32::from(design::text::METADATA)))
                .line_height(rems_from_px(f32::from(design::text::METADATA_LINE_HEIGHT)))
                .text_color(cx.theme().colors().text_muted)
                .child("No Samples Yet"),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charts::{Series, SeriesColor, Unit};
    use crate::settings::test_data_typography;
    use k8s_core::metrics::NormalizedPoint;

    #[test]
    fn chart_rows_follow_data_line_height() {
        assert_eq!(design::row_height(px(18.)), px(28.));
        assert_eq!(design::row_height(px(36.)), px(36.));
    }

    #[test]
    fn chart_table_columns_hold_their_widest_value() {
        let typography = test_data_typography(12., 18.);
        let advance = f32::from(typography.columns(1.0));
        // `HH:MM:SS` and `1023.00 GiB` must both fit without an ellipsis. The row
        // supplies its own padding and gaps on top of these column widths.
        assert!(f32::from(column_width(0, &typography)) >= 8.0 * advance);
        assert!(f32::from(column_width(1, &typography)) >= 11.0 * advance);
        assert_eq!(column_width(2, &typography), column_width(1, &typography));
        assert_eq!(
            table_width(1, &typography),
            f32::from(column_width(0, &typography)) + f32::from(space::SM) * 2.0
        );
        assert_eq!(
            table_width(3, &typography),
            f32::from(column_width(0, &typography))
                + f32::from(column_width(1, &typography)) * 2.0
                + f32::from(space::SM) * 4.0
        );
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
        assert_eq!(
            table_width(3, &large),
            2.0 * table_width(3, &small) - f32::from(space::SM) * 4.0
        );
    }

    #[test]
    fn chart_table_selection_clamps_when_data_shrinks() {
        assert_eq!(clamp_selection((8, 6), 2, 3), (1, 2));
        assert_eq!(clamp_selection((8, 6), 0, 3), (0, 0));
    }

    #[test]
    fn the_newest_row_only_highlights_while_the_table_has_focus() {
        assert_eq!(
            row_tone(false, true, false),
            RowTone::Rest,
            "a table at rest must not paint a selected row"
        );
        assert_eq!(row_tone(false, true, true), RowTone::Stripe);
        assert_eq!(row_tone(true, true, false), RowTone::Selected);
        assert_eq!(row_tone(true, false, true), RowTone::Stripe);
        assert_eq!(row_tone(false, false, true), RowTone::Stripe);
    }

    #[test]
    fn chart_table_navigation_clamps_without_wrapping() {
        assert_eq!(next_cell((1, 1), "left", 3, 3, false), Some((1, 0)));
        assert_eq!(next_cell((1, 1), "up", 3, 3, false), Some((0, 1)));
        assert_eq!(next_cell((2, 2), "right", 3, 3, false), Some((2, 2)));
        assert_eq!(next_cell((2, 2), "down", 3, 3, false), Some((2, 2)));
        assert_eq!(next_cell((1, 1), "home", 3, 3, false), Some((1, 0)));
        assert_eq!(next_cell((1, 1), "end", 3, 3, true), Some((2, 2)));
        assert_eq!(next_cell((1, 1), "x", 3, 3, false), None);
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
