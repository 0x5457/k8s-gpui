//! Maps mouse positions to grid points and drag scrolling.

use std::cmp;

use alacritty_terminal::index::{Column, Line, Point, Side};
use gpui_kit::{Bounds, Pixels, Point as GpuiPoint, px};

/// Maps a window position to a grid point and selection side.
pub fn grid_point_and_side(
    position: GpuiPoint<Pixels>,
    bounds: Bounds<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
    columns: usize,
    rows: usize,
    display_offset: usize,
) -> (Point, Side) {
    let local = position - bounds.origin;
    let columns = columns.max(1);
    let rows = rows.max(1);
    let mut column = (f32::from(local.x) / f32::from(cell_width))
        .floor()
        .max(0.0) as usize;
    let cell_x = cmp::max(px(0.), local.x) % cell_width;
    let mut side = if cell_x > cell_width / 2.0 {
        Side::Right
    } else {
        Side::Left
    };

    let last_column = columns - 1;
    if column > last_column {
        column = last_column;
        side = Side::Right;
    }

    let raw_line = (f32::from(local.y) / f32::from(line_height)).floor() as i32;
    let bottommost_line = i32::try_from(rows - 1).unwrap_or(i32::MAX);
    let line = raw_line.clamp(0, bottommost_line);
    if raw_line > bottommost_line {
        side = Side::Right;
    } else if raw_line < 0 {
        side = Side::Left;
    }

    let display_offset = i32::try_from(display_offset).unwrap_or(i32::MAX);
    let grid_line = line
        .checked_sub(display_offset)
        .unwrap_or(i32::MIN)
        .max(i32::MIN + 1);
    (Point::new(Line(grid_line), Column(column)), side)
}

/// Returns lines to scroll when a drag leaves the viewport.
pub fn drag_line_delta(
    position: GpuiPoint<Pixels>,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
) -> Option<i32> {
    let top = bounds.origin.y;
    let bottom = bounds.bottom_left().y;
    let scroll_lines = if position.y < top {
        let scroll_delta = (top - position.y).pow(1.1);
        (scroll_delta / line_height).ceil() as i32
    } else if position.y > bottom {
        let scroll_delta = -((position.y - bottom).pow(1.1));
        (scroll_delta / line_height).floor() as i32
    } else {
        return None;
    };
    Some(scroll_lines.clamp(-3, 3))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{point, size};

    fn bounds() -> Bounds<Pixels> {
        Bounds::new(point(px(100.0), px(50.0)), size(px(800.0), px(300.0)))
    }

    #[test]
    fn maps_position_to_column_and_line() {
        let (point, side) = grid_point_and_side(
            point(px(100.0 + 8.5), px(50.0 + 20.5)),
            bounds(),
            px(10.0),
            px(20.0),
            80,
            15,
            0,
        );
        assert_eq!(point.column, Column(0));
        assert_eq!(point.line, Line(1));
        assert_eq!(side, Side::Right);
    }

    #[test]
    fn clamps_to_grid_bounds() {
        let (point, side) = grid_point_and_side(
            point(px(2000.0), px(2000.0)),
            bounds(),
            px(10.0),
            px(20.0),
            80,
            15,
            0,
        );
        assert_eq!(point.column, Column(79));
        assert_eq!(point.line, Line(14));
        assert_eq!(side, Side::Right);
    }

    #[test]
    fn applies_display_offset() {
        let (point, _) = grid_point_and_side(
            point(px(105.0), px(55.0)),
            bounds(),
            px(10.0),
            px(20.0),
            80,
            15,
            7,
        );
        assert_eq!(point.line, Line(-7));
    }

    #[test]
    fn clamps_drag_above_viewport_before_applying_offset() {
        let (point, side) = grid_point_and_side(
            point(px(200.0), px(0.0)),
            bounds(),
            px(10.0),
            px(20.0),
            80,
            15,
            7,
        );
        assert_eq!(point, Point::new(Line(-7), Column(10)));
        assert_eq!(side, Side::Left);
    }

    #[test]
    fn drag_below_bottom_scrolls_down() {
        let delta = drag_line_delta(
            point(px(400.0), px(50.0 + 300.0 + 40.0)),
            bounds(),
            px(20.0),
        );
        assert!(delta.is_some_and(|lines| lines < 0));
    }

    #[test]
    fn drag_inside_does_not_scroll() {
        assert_eq!(
            drag_line_delta(point(px(400.0), px(200.0)), bounds(), px(20.0)),
            None
        );
    }

    #[test]
    fn drag_scroll_is_clamped() {
        let delta = drag_line_delta(
            point(px(400.0), px(50.0 + 300.0 + 5000.0)),
            bounds(),
            px(20.0),
        );
        assert_eq!(delta, Some(-3));
    }

    #[test]
    fn pointer_coordinates_never_wrap_at_integer_limits() {
        let (point, side) = grid_point_and_side(
            point(px(2000.0), px(2000.0)),
            bounds(),
            px(10.0),
            px(20.0),
            1,
            1,
            usize::MAX,
        );
        assert_eq!(point.column, Column(0));
        assert!(point.line.0 > i32::MIN);
        assert_eq!(side, Side::Right);
    }
}
