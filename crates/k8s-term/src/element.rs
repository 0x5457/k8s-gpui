//! Paints the terminal grid with GPUI.
//!
//! Grid layout runs under the terminal lock. Text shaping runs after unlock and uses the GPUI glyph cache.

use std::sync::Arc;

use alacritty_terminal::index::Point;
use gpui::{
    AnyElement, App, Bounds, ContentMask, CursorStyle, DispatchPhase, Element, ElementId,
    ElementInputHandler, Font, FontStyle, FontWeight, GlobalElementId, Hitbox, HitboxBehavior,
    Hsla, InspectorElementId, IntoElement, LayoutId, Length, Modifiers, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, ScrollWheelEvent, ShapedLine,
    SharedString, Style, TextAlign, TextRun, UnderlineStyle, WeakEntity, Window, fill, point, px,
    relative, size,
};

use crate::layout::{
    BackgroundSpan, BlockRect, CursorLayout, GridLayout, LineLayout, PreeditLayout,
    SearchHighlights, SelectionSpan, layout_grid, text_hash,
};
use crate::mouse::{mouse_button_report, should_bypass_local_mouse};
use crate::palette::Palette;
use crate::scrollbar::{ScrollbarGeometry, scrollbar_geometry};
use crate::selection::grid_point_and_side;
use crate::session::{TermSize, TerminalSession};
use crate::view::{MouseInputMode, TerminalView};

struct FrameText {
    col: usize,
    cells: usize,
    shaped: ShapedLine,
}

struct FrameLine {
    backgrounds: Vec<BackgroundSpan>,
    blocks: Vec<BlockRect>,
    selection: Vec<SelectionSpan>,
    search: Vec<crate::layout::SearchSpan>,
    texts: Vec<FrameText>,
}

struct FrameCursor {
    layout: CursorLayout,
    shaped: ShapedLine,
}

#[allow(clippy::too_many_arguments)]
fn report_mouse_button(
    session: &TerminalSession,
    view: &WeakEntity<TerminalView>,
    position: gpui::Point<Pixels>,
    bounds: Bounds<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
    button: MouseButton,
    modifiers: Modifiers,
    pressed: bool,
    cx: &mut App,
) -> bool {
    let report_to_terminal = view
        .read_with(cx, |view, _| {
            view.mouse_input_mode() == MouseInputMode::ReportToTerminal
        })
        .unwrap_or(false);
    let mode = session.mode();
    let modifiers = Modifiers {
        alt: modifiers.alt || modifiers.platform,
        platform: false,
        ..modifiers
    };
    if !should_bypass_local_mouse(button, modifiers, report_to_terminal, mode) {
        return false;
    }
    let size = session.size();
    let (point, _) = grid_point_and_side(
        position,
        bounds,
        cell_width,
        line_height,
        size.columns,
        size.screen_lines,
        session.display_offset(),
    );
    let Some(report) = mouse_button_report(point, button, modifiers, pressed, mode) else {
        return true;
    };
    session.write(report);
    true
}

pub(crate) struct TerminalElement {
    session: Arc<TerminalSession>,
    view: WeakEntity<TerminalView>,
    focus_handle: gpui::FocusHandle,
    focused: bool,
    cursor_visible: bool,
    font: Font,
    font_size: Pixels,
    cell_width: Pixels,
    line_height: Pixels,
    palette: Palette,
    hover_link: Option<(Point, Point, Hsla)>,
    search: Option<SearchHighlights>,
    preedit: Option<PreeditLayout>,
}

pub(crate) struct TerminalLayout {
    lines: Vec<FrameLine>,
    cursor: Option<FrameCursor>,
    preedit: Option<(PreeditLayout, ShapedLine)>,
    background: Hsla,
    cell_width: Pixels,
    line_height: Pixels,
    hitbox: Hitbox,
    scrollbar: Option<ScrollbarGeometry>,
    scrollbar_cursor_hitbox: Option<Hitbox>,
}

impl TerminalElement {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        session: Arc<TerminalSession>,
        view: WeakEntity<TerminalView>,
        focus_handle: gpui::FocusHandle,
        focused: bool,
        cursor_visible: bool,
        font: Font,
        font_size: Pixels,
        cell_width: Pixels,
        line_height: Pixels,
        palette: Palette,
        hover_link: Option<(Point, Point, Hsla)>,
        search: Option<SearchHighlights>,
        preedit: Option<PreeditLayout>,
    ) -> Self {
        Self {
            session,
            view,
            focus_handle,
            focused,
            cursor_visible,
            font,
            font_size,
            cell_width,
            line_height,
            palette,
            hover_link,
            search,
            preedit,
        }
    }

    fn snapshot(
        &self,
        bounds: Bounds<Pixels>,
        cell_width: Pixels,
        line_height: Pixels,
    ) -> GridLayout {
        let columns = (f32::from(bounds.size.width) / f32::from(cell_width))
            .floor()
            .max(1.0) as usize;
        let rows = (f32::from(bounds.size.height) / f32::from(line_height))
            .floor()
            .max(1.0) as usize;
        self.session.resize(TermSize::new(columns, rows));

        let snapshot = {
            let term = self.session.term().lock();
            crate::layout::CellSnapshot::capture(&term)
        };
        layout_grid(
            &snapshot,
            &self.palette,
            rows,
            columns,
            self.hover_link,
            self.search.as_ref(),
        )
    }

    fn shape(
        &self,
        layout: GridLayout,
        text_system: &gpui::WindowTextSystem,
        cell_width: Pixels,
        columns: usize,
    ) -> (Vec<FrameLine>, Option<FrameCursor>) {
        let mut lines = Vec::with_capacity(layout.lines.len());
        for line in layout.lines {
            let LineLayout {
                backgrounds,
                blocks,
                runs,
                selection,
                search,
            } = line;
            let texts = runs
                .into_iter()
                .filter_map(|run| {
                    let available = columns.saturating_sub(run.col);
                    if available == 0 {
                        return None;
                    }
                    let cells = run.cells.min(available).max(1);
                    let mut run_font = self.font.clone();
                    if run.style.bold {
                        run_font.weight = FontWeight::BOLD;
                    }
                    if run.style.italic {
                        run_font.style = FontStyle::Italic;
                    }
                    let text = run.text;
                    let text_len = text.len();
                    let hash = text_hash(&text);
                    let shaped = text_system.shape_line_by_hash(
                        hash,
                        text_len,
                        self.font_size,
                        &[TextRun {
                            len: text_len,
                            font: run_font,
                            color: run.style.fg,
                            background_color: None,
                            underline: run.style.underline,
                            strikethrough: run.style.strikethrough,
                        }],
                        Some(cell_width * cells as f32),
                        move || SharedString::from(text),
                    );
                    Some(FrameText {
                        col: run.col,
                        cells,
                        shaped,
                    })
                })
                .collect();
            lines.push(FrameLine {
                backgrounds,
                blocks,
                selection,
                search,
                texts,
            });
        }

        let cursor = layout.cursor.map(|mut cursor| {
            let available = columns.saturating_sub(cursor.col).max(1);
            cursor.cells = cursor.cells.min(available).max(1);
            let text: SharedString = cursor.text.clone().into();
            let text_run = TextRun {
                len: text.len(),
                font: self.font.clone(),
                color: cursor.text_color,
                ..Default::default()
            };
            let shaped = text_system.shape_line(
                text,
                self.font_size,
                &[text_run],
                Some(cell_width * cursor.cells as f32),
            );
            FrameCursor {
                layout: cursor,
                shaped,
            }
        });

        (lines, cursor)
    }

    /// Shapes single-line IME preview text with an underline.
    fn shape_preedit(
        &self,
        preedit: &PreeditLayout,
        text_system: &gpui::WindowTextSystem,
        columns: usize,
    ) -> ShapedLine {
        let text: SharedString = preedit.text.clone().into();
        let text_run = TextRun {
            len: text.len(),
            font: self.font.clone(),
            color: self.palette.foreground,
            underline: Some(UnderlineStyle {
                thickness: px(1.0),
                color: Some(self.palette.cursor),
                wavy: false,
            }),
            ..Default::default()
        };
        let available = columns.saturating_sub(preedit.col).max(1);
        let cells = preedit.cells.min(available).max(1);
        text_system.shape_line(
            text,
            self.font_size,
            &[text_run],
            Some(self.cell_width * cells as f32),
        )
    }

    fn paint_scrollbar(
        &self,
        geometry: &ScrollbarGeometry,
        hovered: bool,
        dragging: bool,
        window: &mut Window,
    ) {
        let color = if dragging || hovered {
            self.palette.scrollbar_thumb_hover
        } else {
            self.palette.scrollbar_thumb
        };
        window.paint_quad(fill(geometry.thumb, color));
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = TerminalLayout;

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
        _cx: &mut App,
    ) -> Self::PrepaintState {
        let text_system = window.text_system().clone();
        let cell_width = self.cell_width;
        let line_height = self.line_height;
        let columns = (f32::from(bounds.size.width) / f32::from(cell_width))
            .floor()
            .max(1.0) as usize;

        let grid = self.snapshot(bounds, cell_width, line_height);
        let (lines, cursor) = self.shape(grid, &text_system, cell_width, columns);
        let preedit = self.preedit.as_ref().map(|preedit| {
            (
                preedit.clone(),
                self.shape_preedit(preedit, &text_system, columns),
            )
        });

        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);

        let viewport_lines = lines.len();
        let total_lines = viewport_lines + self.session.history_size();
        let scrollbar = scrollbar_geometry(
            bounds,
            viewport_lines,
            total_lines,
            self.session.display_offset(),
        );
        // The resize cursor covers the scrollbar itself, so hovering terminal text keeps the
        // text cursor. Clicks still use the wider region of `contains_track_at`.
        let scrollbar_cursor_hitbox = scrollbar
            .as_ref()
            .map(|geometry| window.insert_hitbox(geometry.cursor_bounds(), HitboxBehavior::Normal));

        TerminalLayout {
            lines,
            cursor,
            preedit,
            background: self.palette.background,
            cell_width,
            line_height,
            hitbox,
            scrollbar,
            scrollbar_cursor_hitbox,
        }
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
        window.paint_quad(fill(bounds, layout.background));
        window.set_cursor_style(CursorStyle::IBeam, &layout.hitbox);
        let origin = bounds.origin;
        let cell_width = layout.cell_width;
        let line_height = layout.line_height;

        for (line_index, line) in layout.lines.iter().enumerate() {
            let y = origin.y + line_index as f32 * line_height;
            for span in &line.backgrounds {
                let rect = Bounds::new(
                    point(origin.x + span.start as f32 * cell_width, y),
                    size((span.end - span.start + 1) as f32 * cell_width, line_height),
                );
                window.paint_quad(fill(rect, span.color));
            }
            for block in &line.blocks {
                let subcell_width = cell_width / crate::layout::BLOCK_SUBCELL_COLUMNS as f32;
                let subcell_height = line_height / crate::layout::BLOCK_SUBCELL_LINES as f32;
                let rect = Bounds::new(
                    point(
                        origin.x + block.col as f32 * subcell_width,
                        origin.y + block.line as f32 * subcell_height,
                    ),
                    size(
                        subcell_width * block.columns as f32,
                        subcell_height * block.lines as f32,
                    ),
                );
                window.paint_quad(fill(rect, block.color));
            }
            for span in &line.search {
                let color = if span.current {
                    self.palette.search_match_active
                } else {
                    self.palette.search_match
                };
                let rect = Bounds::new(
                    point(origin.x + span.start as f32 * cell_width, y),
                    size((span.end - span.start + 1) as f32 * cell_width, line_height),
                );
                window.paint_quad(fill(rect, color));
            }
            for span in &line.selection {
                let rect = Bounds::new(
                    point(origin.x + span.start as f32 * cell_width, y),
                    size((span.end - span.start + 1) as f32 * cell_width, line_height),
                );
                window.paint_quad(fill(rect, self.palette.selection));
            }
            for text in &line.texts {
                let x = origin.x + text.col as f32 * cell_width;
                let text_bounds = Bounds::new(
                    point(x, y),
                    size(text.cells as f32 * cell_width, line_height),
                );
                window.with_content_mask(
                    Some(ContentMask {
                        bounds: text_bounds,
                    }),
                    |window| {
                        text.shaped
                            .paint(point(x, y), line_height, TextAlign::Left, None, window, cx)
                            .ok();
                    },
                );
            }
        }

        if let Some(cursor) = &layout.cursor {
            let x = origin.x + cursor.layout.col as f32 * cell_width;
            let y = origin.y + cursor.layout.line as f32 * line_height;
            let cursor_width = cell_width * cursor.layout.cells.max(1) as f32;
            let rect = Bounds::new(point(x, y), size(cursor_width, line_height));
            let visible = self.focused && self.cursor_visible;
            match cursor.layout.shape {
                alacritty_terminal::vte::ansi::CursorShape::Block if visible => {
                    window.paint_quad(fill(rect, cursor.layout.color));
                    window.with_content_mask(Some(ContentMask { bounds: rect }), |window| {
                        cursor
                            .shaped
                            .paint(point(x, y), line_height, TextAlign::Left, None, window, cx)
                            .ok();
                    });
                }
                alacritty_terminal::vte::ansi::CursorShape::Block if !self.focused => {
                    paint_hollow_quad(window, rect, cursor.layout.color, layout.background);
                }
                alacritty_terminal::vte::ansi::CursorShape::Underline if visible => {
                    let bar = Bounds::new(
                        point(x, y + line_height - px(2.0)),
                        size(cursor_width, px(2.0)),
                    );
                    window.paint_quad(fill(bar, cursor.layout.color));
                }
                alacritty_terminal::vte::ansi::CursorShape::Beam if visible => {
                    let bar = Bounds::new(point(x, y), size(px(2.0), line_height));
                    window.paint_quad(fill(bar, cursor.layout.color));
                }
                _ => {}
            }
        }

        if let Some((preedit, shaped)) = &layout.preedit {
            let x = origin.x + preedit.col as f32 * cell_width;
            let y = origin.y + preedit.line as f32 * line_height;
            let width = cell_width * preedit.cells.max(1) as f32;
            let rect = Bounds::new(point(x, y), size(width, line_height));
            window.paint_quad(fill(rect, layout.background));
            window.with_content_mask(Some(ContentMask { bounds: rect }), |window| {
                shaped
                    .paint(point(x, y), line_height, TextAlign::Left, None, window, cx)
                    .ok();
            });
        }

        if self
            .view
            .read_with(cx, |view, _| view.hovered_link().is_some())
            .unwrap_or(false)
        {
            window.set_cursor_style(CursorStyle::PointingHand, &layout.hitbox);
        }

        if let Some(scrollbar) = &layout.scrollbar {
            let dragging = self
                .view
                .read_with(cx, |view, _| view.scrollbar_dragging())
                .unwrap_or(false);
            let hovered = self
                .view
                .read_with(cx, |view, _| view.scrollbar_hovered())
                .unwrap_or(false);
            if let Some(cursor_hitbox) = &layout.scrollbar_cursor_hitbox {
                window.set_cursor_style(CursorStyle::ResizeUpDown, cursor_hitbox);
            }
            self.paint_scrollbar(scrollbar, hovered, dragging, window);
        }

        if let Some(view) = self.view.upgrade() {
            window.handle_input(
                &self.focus_handle,
                ElementInputHandler::new(bounds, view),
                cx,
            );
        }

        self.register_mouse_listeners(&layout.hitbox, bounds, window);
    }
}

impl TerminalElement {
    fn register_mouse_listeners(
        &self,
        hitbox: &Hitbox,
        bounds: Bounds<Pixels>,
        window: &mut Window,
    ) {
        let view = self.view.clone();
        let session = self.session.clone();
        let hitbox = hitbox.clone();
        let focus = self.focus_handle.clone();
        let cell_width = self.cell_width;
        let line_height = self.line_height;

        window.on_mouse_event({
            let view = view.clone();
            let session = session.clone();
            let hitbox = hitbox.clone();
            let focus = focus.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                    return;
                }
                window.focus(&focus, cx);
                if report_mouse_button(
                    &session,
                    &view,
                    event.position,
                    bounds,
                    cell_width,
                    line_height,
                    event.button,
                    event.modifiers,
                    true,
                    cx,
                ) {
                    return;
                }
                if let Some(link) = view
                    .update(cx, |view, cx| view.mouse_down(event, bounds, window, cx))
                    .ok()
                    .flatten()
                    && crate::view::link_open_allowed(&link, event.modifiers)
                {
                    cx.open_url(&link);
                }
            }
        });

        window.on_mouse_event({
            let view = view.clone();
            let hitbox = hitbox.clone();
            move |event: &MouseMoveEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                    return;
                }
                let _ = view.update(cx, |view, cx| view.mouse_move(event, bounds, true, cx));
            }
        });

        window.on_mouse_event({
            let view = view.clone();
            let session = session.clone();
            let hitbox = hitbox.clone();
            move |event: &MouseUpEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                    return;
                }
                if report_mouse_button(
                    &session,
                    &view,
                    event.position,
                    bounds,
                    cell_width,
                    line_height,
                    event.button,
                    event.modifiers,
                    false,
                    cx,
                ) {
                    return;
                }
                if let Some(link) = view
                    .update(cx, |view, cx| view.mouse_up(event, bounds, cx))
                    .ok()
                    .flatten()
                    && crate::view::link_open_allowed(&link, event.modifiers)
                {
                    cx.open_url(&link);
                }
            }
        });

        window.on_mouse_event({
            let view = view.clone();
            let hitbox = hitbox.clone();
            move |event: &ScrollWheelEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                    return;
                }
                let _ = view.update(cx, |view, cx| view.scroll_wheel(event, bounds, cx));
            }
        });
    }
}

fn paint_hollow_quad(window: &mut Window, rect: Bounds<Pixels>, color: Hsla, background: Hsla) {
    let border = px(1.0);
    window.paint_quad(fill(rect, color));
    window.paint_quad(fill(
        Bounds::new(
            point(rect.origin.x + border, rect.origin.y + border),
            size(
                rect.size.width - border * 2.0,
                rect.size.height - border * 2.0,
            ),
        ),
        background,
    ));
}

impl From<TerminalElement> for AnyElement {
    fn from(element: TerminalElement) -> Self {
        element.into_any_element()
    }
}
