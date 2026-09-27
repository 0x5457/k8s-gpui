//! Resolves Alacritty terminal colors as GPUI colors.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use gpui_kit::{Hsla, rgba};

use crate::contrast::{
    ensure_minimum_contrast, ensure_minimum_contrast_at_opacity, wcag_contrast_ratio,
};

pub const DEFAULT_MINIMUM_CONTRAST: f32 = 4.5;

/// The shallowest dim a terminal palette is allowed to use, and the shipped one.
///
/// Dimming and contrast draw on the same headroom: a dim cell is
/// `opacity * colour + (1 - opacity) * surface`, so every point of transparency
/// costs the cell contrast on the surface underneath it. Spend all of it on
/// transparency and the palette answers the floor the only way it can - by
/// pushing every cell to the lightness extreme, white on a dark canvas and black
/// on a light one - which is a palette with no hues left and is what
/// The design forbids when it asks the semantic hue to survive a
/// lightness adjustment. Past this depth the shipped palettes start losing their
/// colours; at it they still have them.
pub const MAX_DIM_OPACITY: f32 = 0.7;

/// Terminal colors supplied by the application theme.
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalTheme {
    pub ansi: [Hsla; 16],
    pub dim_ansi: [Hsla; 8],
    pub foreground: Hsla,
    pub bright_foreground: Hsla,
    pub dim_foreground: Hsla,
    pub background: Hsla,
    pub cursor: Hsla,
    pub selection: Hsla,
    pub search_match: Hsla,
    pub search_match_active: Hsla,
    /// Border drawn around a focused terminal. It is the app's focus role, not
    /// the terminal's text colour, so a focused terminal looks like every other
    /// focused surface in the app.
    pub focus: Hsla,
    pub scrollbar_thumb: Hsla,
    pub scrollbar_thumb_hover: Hsla,
    pub minimum_contrast: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    pub ansi: [Hsla; 16],
    pub dim_ansi: [Hsla; 8],
    pub foreground: Hsla,
    pub bright_foreground: Hsla,
    pub dim_foreground: Hsla,
    pub background: Hsla,
    pub cursor: Hsla,
    pub selection: Hsla,
    pub search_match: Hsla,
    pub search_match_active: Hsla,
    pub focus: Hsla,
    pub scrollbar_thumb: Hsla,
    pub scrollbar_thumb_hover: Hsla,
    pub minimum_contrast: f32,
    /// Alpha a dim cell is painted at.
    ///
    /// The contrast floor says how deep the dim may go and [`MAX_DIM_OPACITY`]
    /// says how shallow it wants to, and this is the shallower of the two. At
    /// the default 4.5 floor the cap is what the palette gets; under Increase
    /// Contrast the floor is what it gets, and the dim is the thing that gives
    /// way, which is the right way round for a setting whose whole purpose is
    /// contrast.
    pub dim_opacity: f32,
}

fn rgb_to_hsla(rgb: Rgb) -> Hsla {
    Hsla::from(rgba(u32::from_be_bytes([rgb.r, rgb.g, rgb.b, 0xff])))
}

impl From<TerminalTheme> for Palette {
    fn from(theme: TerminalTheme) -> Self {
        let mut palette = Self {
            ansi: theme.ansi,
            dim_ansi: theme.dim_ansi,
            foreground: theme.foreground,
            bright_foreground: theme.bright_foreground,
            dim_foreground: theme.dim_foreground,
            background: theme.background,
            cursor: theme.cursor,
            selection: theme.selection,
            search_match: theme.search_match,
            search_match_active: theme.search_match_active,
            focus: theme.focus,
            scrollbar_thumb: theme.scrollbar_thumb,
            scrollbar_thumb_hover: theme.scrollbar_thumb_hover,
            minimum_contrast: theme.minimum_contrast.max(DEFAULT_MINIMUM_CONTRAST),
            dim_opacity: MAX_DIM_OPACITY,
        };
        palette.dim_opacity = palette.solve_dim_opacity().max(MAX_DIM_OPACITY);
        palette
    }
}

impl Palette {
    /// Resolves a cell color. OSC colors take priority over the theme palette.
    pub(crate) fn resolve(&self, color: Color, colors: &Colors) -> Hsla {
        match color {
            Color::Spec(rgb) => rgb_to_hsla(rgb),
            Color::Indexed(index) => match colors[index as usize] {
                Some(rgb) => rgb_to_hsla(rgb),
                None => self.indexed(index),
            },
            Color::Named(name) => match colors[name] {
                Some(rgb) => rgb_to_hsla(rgb),
                None => self.named(name),
            },
        }
    }

    pub(crate) fn selection_foreground(&self) -> Hsla {
        let background = self.background.alpha(1.0).blend(self.selection).alpha(1.0);
        ensure_minimum_contrast(self.foreground, background, self.minimum_contrast)
    }

    pub(crate) fn adjust_fg(&self, fg: Hsla, background: Hsla) -> Hsla {
        [
            background,
            background.alpha(1.0).blend(self.selection).alpha(1.0),
            background.alpha(1.0).blend(self.search_match).alpha(1.0),
            background
                .alpha(1.0)
                .blend(self.search_match_active)
                .alpha(1.0),
        ]
        .into_iter()
        .fold(fg, |foreground, state| {
            ensure_minimum_contrast_at_opacity(
                foreground,
                state,
                self.minimum_contrast,
                self.dim_opacity,
            )
        })
    }

    /// The surfaces a cell can be painted on: the terminal canvas and the three
    /// washes that sit on it.
    fn states(&self) -> [Hsla; 4] {
        let background = self.background.alpha(1.0);
        [
            background,
            background.blend(self.selection).alpha(1.0),
            background.blend(self.search_match).alpha(1.0),
            background.blend(self.search_match_active).alpha(1.0),
        ]
    }

    /// The deepest dim this palette's own surfaces still permit.
    ///
    /// The extreme colour is the best any cell could be, so the alpha at which
    /// *it* clears the floor on the hardest surface is the alpha below which no
    /// cell can. When even full opacity cannot clear it the palette has no dim
    /// left to spend and says so with an alpha of 1, which
    /// `dim_cells_stay_a_step_below_their_normal_counterparts` catches: a
    /// terminal whose surfaces sit near mid-tone cannot host a 7:1 body cell at
    /// all, and hiding that behind a dimmer cell is not a fix.
    fn solve_dim_opacity(&self) -> f32 {
        let states = self.states();
        let extreme = Hsla {
            h: self.background.h,
            s: 0.0,
            l: if self.background.l >= 0.5 { 0.0 } else { 1.0 },
            a: self.background.a,
        };
        let clears = |opacity: f32| {
            states.into_iter().all(|state| {
                wcag_contrast_ratio(extreme.opacity(opacity), state) >= self.minimum_contrast
            })
        };
        if !clears(1.0) {
            return 1.0;
        }
        let (mut too_deep, mut shallowest) = (0.0, 1.0);
        for _ in 0..24 {
            let candidate = (too_deep + shallowest) / 2.0;
            if clears(candidate) {
                shallowest = candidate;
            } else {
                too_deep = candidate;
            }
        }
        shallowest
    }

    fn named(&self, name: NamedColor) -> Hsla {
        match name {
            NamedColor::Black => self.ansi[0],
            NamedColor::Red => self.ansi[1],
            NamedColor::Green => self.ansi[2],
            NamedColor::Yellow => self.ansi[3],
            NamedColor::Blue => self.ansi[4],
            NamedColor::Magenta => self.ansi[5],
            NamedColor::Cyan => self.ansi[6],
            NamedColor::White => self.ansi[7],
            NamedColor::BrightBlack => self.ansi[8],
            NamedColor::BrightRed => self.ansi[9],
            NamedColor::BrightGreen => self.ansi[10],
            NamedColor::BrightYellow => self.ansi[11],
            NamedColor::BrightBlue => self.ansi[12],
            NamedColor::BrightMagenta => self.ansi[13],
            NamedColor::BrightCyan => self.ansi[14],
            NamedColor::BrightWhite => self.ansi[15],
            NamedColor::Foreground => self.foreground,
            NamedColor::BrightForeground => self.bright_foreground,
            NamedColor::DimForeground => self.dim_foreground,
            NamedColor::Background => self.background,
            NamedColor::Cursor => self.cursor,
            NamedColor::DimBlack => self.dim_ansi[0],
            NamedColor::DimRed => self.dim_ansi[1],
            NamedColor::DimGreen => self.dim_ansi[2],
            NamedColor::DimYellow => self.dim_ansi[3],
            NamedColor::DimBlue => self.dim_ansi[4],
            NamedColor::DimMagenta => self.dim_ansi[5],
            NamedColor::DimCyan => self.dim_ansi[6],
            NamedColor::DimWhite => self.dim_ansi[7],
        }
    }

    fn indexed(&self, index: u8) -> Hsla {
        match index {
            0..=15 => self.ansi[index as usize],
            16..=231 => {
                let index = index - 16;
                let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                rgb_to_hsla(Rgb {
                    r: level(index / 36),
                    g: level((index % 36) / 6),
                    b: level(index % 6),
                })
            }
            _ => {
                let value = 8 + (index - 232) * 10;
                rgb_to_hsla(Rgb {
                    r: value,
                    g: value,
                    b: value,
                })
            }
        }
    }
}
