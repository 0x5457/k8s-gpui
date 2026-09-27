//! Calculates terminal scrollbar geometry and drag positions.

use gpui_kit::{Bounds, Pixels, px};

pub const SCROLLBAR_WIDTH: Pixels = px(10.0);
pub const SCROLLBAR_HIT_WIDTH: Pixels = px(20.0);
pub const SCROLLBAR_HIT_PADDING: Pixels = px(20.0);
pub const SCROLLBAR_THUMB_HIT_PADDING: Pixels = px(5.0);
pub const SCROLLBAR_MARGIN: Pixels = px(2.0);
pub const MIN_THUMB_HEIGHT: Pixels = px(24.0);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollbarGeometry {
    pub track: Bounds<Pixels>,
    pub thumb: Bounds<Pixels>,
    interactive: Bounds<Pixels>,
}

impl ScrollbarGeometry {
    pub fn contains_thumb(&self, x: Pixels, y: Pixels) -> bool {
        x >= self.thumb.left() - SCROLLBAR_THUMB_HIT_PADDING
            && x <= self.thumb.right() + SCROLLBAR_THUMB_HIT_PADDING
            && y >= self.thumb.top() - SCROLLBAR_THUMB_HIT_PADDING
            && y <= self.thumb.bottom() + SCROLLBAR_THUMB_HIT_PADDING
    }

    pub fn contains_track(&self, x: Pixels) -> bool {
        x >= self.interactive.left() && x <= self.interactive.right()
    }

    pub fn contains_track_at(&self, x: Pixels, y: Pixels) -> bool {
        self.contains_track(x) && y >= self.interactive.top() && y <= self.interactive.bottom()
    }

    /// The region that shows the resize cursor. It stays inside the scrollbar itself, so
    /// hovering terminal text never claims the pointer can resize.
    pub fn cursor_bounds(&self) -> Bounds<Pixels> {
        self.track
    }
}

/// Calculates the track and thumb. Returns `None` when there is no scrollback.
pub fn scrollbar_geometry(
    bounds: Bounds<Pixels>,
    viewport_lines: usize,
    total_lines: usize,
    display_offset: usize,
) -> Option<ScrollbarGeometry> {
    if total_lines <= viewport_lines || viewport_lines == 0 {
        return None;
    }
    let history = total_lines - viewport_lines;
    let track = Bounds::new(
        gpui_kit::point(
            bounds.right() - SCROLLBAR_HIT_WIDTH - SCROLLBAR_MARGIN,
            bounds.top() + SCROLLBAR_MARGIN,
        ),
        gpui_kit::size(
            SCROLLBAR_HIT_WIDTH,
            bounds.size.height - SCROLLBAR_MARGIN * 2.0,
        ),
    );
    if f32::from(track.size.height) <= f32::from(MIN_THUMB_HEIGHT) {
        return None;
    }

    let fraction = viewport_lines as f32 / total_lines as f32;
    let thumb_height = (track.size.height * fraction).max(MIN_THUMB_HEIGHT);
    let travel = track.size.height - thumb_height;
    let scroll_top = history.saturating_sub(display_offset);
    let thumb_offset = if history == 0 {
        px(0.0)
    } else {
        travel * (scroll_top as f32 / history as f32)
    };
    let thumb = Bounds::new(
        gpui_kit::point(
            track.left() + (SCROLLBAR_HIT_WIDTH - SCROLLBAR_WIDTH) / 2.0,
            track.top() + thumb_offset,
        ),
        gpui_kit::size(SCROLLBAR_WIDTH, thumb_height),
    );
    let interactive = Bounds::new(
        gpui_kit::point(track.left() - SCROLLBAR_HIT_PADDING, track.top()),
        gpui_kit::size(
            SCROLLBAR_HIT_WIDTH + SCROLLBAR_HIT_PADDING,
            track.size.height,
        ),
    );
    Some(ScrollbarGeometry {
        track,
        thumb,
        interactive,
    })
}

/// Maps a track position to a display offset. Zero is the bottom.
pub fn display_offset_for_thumb_top(
    geometry: &ScrollbarGeometry,
    thumb_top: Pixels,
    viewport_lines: usize,
    total_lines: usize,
) -> usize {
    let history = total_lines.saturating_sub(viewport_lines);
    if history == 0 {
        return 0;
    }
    let travel = geometry.track.size.height - geometry.thumb.size.height;
    if f32::from(travel) <= 0.0 {
        return 0;
    }
    let ratio = ((thumb_top - geometry.track.top()) / travel).clamp(0.0, 1.0);
    let scroll_top = (ratio * history as f32).round() as usize;
    history.saturating_sub(scroll_top.min(history))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{point, size};

    fn bounds() -> Bounds<Pixels> {
        Bounds::new(point(px(0.0), px(0.0)), size(px(800.0), px(600.0)))
    }

    #[test]
    fn no_scrollbar_without_history() {
        assert!(scrollbar_geometry(bounds(), 40, 40, 0).is_none());
    }

    #[test]
    fn track_hit_extends_once_and_thumb_hit_stays_small() {
        let geometry = scrollbar_geometry(bounds(), 40, 400, 0).expect("geometry");
        let thumb = geometry.thumb;
        assert!(SCROLLBAR_HIT_PADDING >= px(20.0));
        assert!(geometry.contains_track(geometry.track.left() - px(20.0)));
        assert!(!geometry.contains_track(geometry.track.right() + px(0.5)));
        assert!(!geometry.contains_track(geometry.track.right() + px(20.0)));
        assert!(geometry.contains_thumb(thumb.left() - px(5.0), thumb.top() - px(5.0)));
        assert!(!geometry.contains_thumb(thumb.left() - px(5.5), thumb.top() - px(5.0)));
        assert!(geometry.contains_track_at(geometry.track.left() - px(20.0), geometry.track.top()));
        assert!(!geometry.contains_track_at(
            geometry.track.left() - px(20.0),
            geometry.track.top() - px(0.5)
        ));
        assert!(
            !geometry
                .contains_track_at(geometry.track.left() - px(20.5), geometry.track.center().y)
        );
        assert!(
            !geometry
                .contains_track_at(geometry.track.right() + px(0.5), geometry.track.center().y)
        );
        assert_eq!(
            scrollbar_geometry(bounds(), 40, 400, 0)
                .expect("geometry")
                .thumb,
            thumb
        );
    }

    #[test]
    fn cursor_bounds_stay_inside_the_scrollbar() {
        let geometry = scrollbar_geometry(bounds(), 40, 400, 0).expect("geometry");
        let cursor = geometry.cursor_bounds();

        assert_eq!(cursor, geometry.track);
        assert!(geometry.contains_track_at(cursor.center().x, cursor.center().y));
        assert!(!cursor.contains(&gpui_kit::point(
            geometry.track.left() - px(0.5),
            geometry.track.center().y
        )));
        assert!(!cursor.contains(&gpui_kit::point(
            geometry.track.right() + px(0.5),
            geometry.track.center().y
        )));
        assert!(!cursor.contains(&gpui_kit::point(
            geometry.track.center().x,
            geometry.track.top() - px(0.5)
        )));
        assert!(cursor.contains(&geometry.thumb.center()));
    }

    #[test]
    fn dragging_maps_back_to_display_offset() {
        let geometry = scrollbar_geometry(bounds(), 40, 400, 0).expect("geometry");
        let top_offset = display_offset_for_thumb_top(&geometry, geometry.track.top(), 40, 400);
        assert_eq!(top_offset, 360);
        let bottom_offset =
            display_offset_for_thumb_top(&geometry, geometry.track.bottom(), 40, 400);
        assert_eq!(bottom_offset, 0);
    }
}
