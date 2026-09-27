//! Resolves Alacritty terminal colors as GPUI colors.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use gpui::{Hsla, rgba};

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
/// `DESIGN.md` §3.4 forbids when it asks the semantic hue to survive a
/// lightness adjustment. Past this depth the shipped palettes start losing their
/// colours; at it they still have them.
pub const MAX_DIM_OPACITY: f32 = 0.7;

/// How much quieter a dim cell has to be than the same cell at full strength,
/// as a contrast difference on the surface that binds the two.
///
/// A dim cell is the cell's own colour at [`Palette::dim_opacity`], and that
/// opacity is derived from the contrast floor, so a palette can always satisfy
/// the floor by refusing to dim at all. This is the floor that stops it: two
/// roles one rounding step apart are one value and one colour, which is the
/// failure the number exists to catch. `design.rs` holds its secondary text
/// roles to the same kind of gap for the same reason.
pub const DIM_MIN_QUIET_GAP: f32 = 0.4;

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
    #[cfg(test)]
    pub(crate) fn one_dark() -> Self {
        let ansi = [
            rgba(0x282c34ff), // black
            rgba(0xe06c75ff), // red
            rgba(0x98c379ff), // green
            rgba(0xd19a66ff), // yellow
            rgba(0x61afefff), // blue
            rgba(0xc678ddff), // magenta
            rgba(0x56b6c2ff), // cyan
            rgba(0xabb2bfff), // white
            rgba(0x5c6370ff), // bright black
            rgba(0xe06c75ff), // bright red
            rgba(0x98c379ff), // bright green
            rgba(0xd19a66ff), // bright yellow
            rgba(0x61afefff), // bright blue
            rgba(0xc678ddff), // bright magenta
            rgba(0x56b6c2ff), // bright cyan
            rgba(0xffffffff), // bright white
        ]
        .map(Hsla::from);
        let dim_ansi = [
            rgba(0x282c34ff),
            rgba(0xa05058ff),
            rgba(0x6d8c58ff),
            rgba(0x967048ff),
            rgba(0x467fa8ff),
            rgba(0x8f5aa0ff),
            rgba(0x3f8490ff),
            rgba(0x7a8088ff),
        ]
        .map(Hsla::from);

        Self {
            ansi,
            dim_ansi,
            foreground: rgba(0xabb2bfff).into(),
            bright_foreground: rgba(0xffffffff).into(),
            dim_foreground: rgba(0x5c6370ff).into(),
            background: rgba(0x282c34ff).into(),
            cursor: rgba(0x528bffff).into(),
            selection: rgba(0x3e4451ff).into(),
            search_match: rgba(0xd19a6633).into(),
            search_match_active: rgba(0xe06c7533).into(),
            focus: rgba(0x61afefff).into(),
            scrollbar_thumb: rgba(0x4b5263cc).into(),
            scrollbar_thumb_hover: rgba(0x5c6370cc).into(),
            minimum_contrast: DEFAULT_MINIMUM_CONTRAST,
            dim_opacity: MAX_DIM_OPACITY,
        }
    }

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

    /// The same palette read against a different contrast floor.
    ///
    /// The dim depth is derived from the floor, so a floor that arrives any
    /// other way - a struct update, say - would leave the palette dimming
    /// deeper than the floor allows and every dim cell on screen would be short
    /// of it.
    pub fn with_minimum_contrast(&self, minimum_contrast: f32) -> Palette {
        let mut palette = self.clone();
        palette.minimum_contrast = minimum_contrast.max(DEFAULT_MINIMUM_CONTRAST);
        palette.dim_opacity = palette.solve_dim_opacity().max(MAX_DIM_OPACITY);
        palette
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contrast::theme_color;
    use alacritty_terminal::term::color::Colors;

    const ANSI_NAMES: [NamedColor; 16] = [
        NamedColor::Black,
        NamedColor::Red,
        NamedColor::Green,
        NamedColor::Yellow,
        NamedColor::Blue,
        NamedColor::Magenta,
        NamedColor::Cyan,
        NamedColor::White,
        NamedColor::BrightBlack,
        NamedColor::BrightRed,
        NamedColor::BrightGreen,
        NamedColor::BrightYellow,
        NamedColor::BrightBlue,
        NamedColor::BrightMagenta,
        NamedColor::BrightCyan,
        NamedColor::BrightWhite,
    ];
    const DIM_NAMES: [NamedColor; 8] = [
        NamedColor::DimBlack,
        NamedColor::DimRed,
        NamedColor::DimGreen,
        NamedColor::DimYellow,
        NamedColor::DimBlue,
        NamedColor::DimMagenta,
        NamedColor::DimCyan,
        NamedColor::DimWhite,
    ];

    /// The two search washes `design::search_match` hands a terminal.
    ///
    /// A theme whose match role is the same value as its selection role gets a
    /// thin accent wash instead, because a search hit and a text selection are
    /// different facts and `DESIGN.md` §3.4 says they must not be one colour.
    /// Both K8s Studio appearances ship exactly that collision, so the fallback
    /// is the path every Studio terminal takes.
    fn theme_search_colors(theme_name: &str) -> (Hsla, Hsla) {
        let selection = theme_color(theme_name, "element.selection_background");
        let accent = theme_color(theme_name, "text.accent");
        let match_background = if theme_color(theme_name, "search.match_background") == selection {
            accent.opacity(0.21)
        } else {
            theme_color(theme_name, "search.match_background")
        };
        let mut active = theme_color(theme_name, "search.active_match_background");
        for alpha in [0.19, 0.17] {
            if active != selection && active != match_background {
                break;
            }
            active = accent.opacity(alpha);
        }
        (match_background, active)
    }

    fn studio_palette(theme_name: &str) -> Palette {
        studio_palette_with_contrast(theme_name, DEFAULT_MINIMUM_CONTRAST)
    }

    fn studio_palette_with_contrast(theme_name: &str, minimum_contrast: f32) -> Palette {
        let (search_match, search_match_active) = theme_search_colors(theme_name);
        TerminalTheme {
            ansi: [
                "black",
                "red",
                "green",
                "yellow",
                "blue",
                "magenta",
                "cyan",
                "white",
                "bright_black",
                "bright_red",
                "bright_green",
                "bright_yellow",
                "bright_blue",
                "bright_magenta",
                "bright_cyan",
                "bright_white",
            ]
            .map(|name| theme_color(theme_name, &format!("terminal.ansi.{name}"))),
            dim_ansi: [
                "dim_black",
                "dim_red",
                "dim_green",
                "dim_yellow",
                "dim_blue",
                "dim_magenta",
                "dim_cyan",
                "dim_white",
            ]
            .map(|name| theme_color(theme_name, &format!("terminal.ansi.{name}"))),
            foreground: theme_color(theme_name, "terminal.foreground"),
            bright_foreground: theme_color(theme_name, "terminal.bright_foreground"),
            dim_foreground: theme_color(theme_name, "terminal.dim_foreground"),
            background: theme_color(theme_name, "terminal.background"),
            cursor: theme_color(theme_name, "cursor"),
            selection: theme_color(theme_name, "selection"),
            search_match,
            search_match_active,
            focus: theme_color(theme_name, "border.focused"),
            scrollbar_thumb: theme_color(theme_name, "scrollbar.thumb.background"),
            scrollbar_thumb_hover: theme_color(theme_name, "scrollbar.thumb.hover_background"),
            minimum_contrast,
        }
        .into()
    }

    fn composite(background: Hsla, overlay: Hsla) -> Hsla {
        background.alpha(1.0).blend(overlay).alpha(1.0)
    }

    fn terminal_states(palette: &Palette) -> [Hsla; 4] {
        palette.states()
    }

    fn assert_wcag(label: &str, foreground: Hsla, background: Hsla) {
        assert_wcag_at_least(label, foreground, background, DEFAULT_MINIMUM_CONTRAST);
    }

    fn assert_wcag_at_least(label: &str, foreground: Hsla, background: Hsla, minimum: f32) {
        let ratio = wcag_contrast_ratio(foreground, background);
        assert!(ratio >= minimum, "{label}: {ratio:.2}:1 < {minimum:.2}:1");
    }

    #[test]
    fn selection_foreground_meets_contrast_on_the_composite() {
        for palette in [
            studio_palette("K8s Studio Light"),
            studio_palette("K8s Studio Dark"),
            studio_palette_with_contrast("K8s Studio Light", 7.0),
            studio_palette_with_contrast("K8s Studio Dark", 7.0),
            Palette::one_dark(),
        ] {
            let background = composite(palette.background, palette.selection);
            assert_wcag_at_least(
                "selection foreground",
                palette.selection_foreground(),
                background,
                palette.minimum_contrast,
            );
        }
    }

    #[test]
    fn named_colors_map_to_ansi_palette() {
        let palette = Palette::one_dark();
        let colors = Colors::default();
        assert_eq!(
            palette.resolve(Color::Named(NamedColor::Red), &colors),
            palette.ansi[1]
        );
        assert_eq!(
            palette.resolve(Color::Named(NamedColor::Background), &colors),
            palette.background
        );
    }

    #[test]
    fn indexed_cube_and_grayscale_are_computed() {
        let palette = Palette::one_dark();
        let colors = Colors::default();
        let white = palette.resolve(Color::Indexed(231), &colors);
        assert_eq!(white.to_rgb(), rgba(0xffffffff));
        let black = palette.resolve(Color::Indexed(16), &colors);
        assert_eq!(black.to_rgb(), rgba(0x000000ff));
    }

    #[test]
    fn osc_override_wins_over_palette() {
        let palette = Palette::one_dark();
        let mut colors = Colors::default();
        colors[1] = Some(Rgb { r: 1, g: 2, b: 3 });
        assert_eq!(
            palette.resolve(Color::Indexed(1), &colors),
            rgb_to_hsla(Rgb { r: 1, g: 2, b: 3 })
        );
    }

    #[test]
    fn studio_named_and_dim_colors_meet_wcag_aa_on_all_terminal_states() {
        for theme_name in ["K8s Studio Light", "K8s Studio Dark"] {
            let palette = studio_palette(theme_name);
            assert!((palette.minimum_contrast - DEFAULT_MINIMUM_CONTRAST).abs() < f32::EPSILON);
            let states = terminal_states(&palette);
            let colors = Colors::default();
            for name in ANSI_NAMES
                .into_iter()
                .chain([NamedColor::Foreground, NamedColor::BrightForeground])
            {
                let requested = Color::Named(name);
                let raw = palette.resolve(requested, &colors);
                let adjusted = palette.adjust_fg(raw, palette.background);
                for state in states {
                    assert_wcag(&format!("{theme_name} {name:?}"), adjusted, state);
                    assert_wcag(
                        &format!("{theme_name} dim {name:?}"),
                        adjusted.opacity(palette.dim_opacity),
                        state,
                    );
                }
            }
            for name in DIM_NAMES.into_iter().chain([NamedColor::DimForeground]) {
                let requested = Color::Named(name);
                let raw = palette.resolve(requested, &colors);
                let adjusted = palette
                    .adjust_fg(raw, palette.background)
                    .opacity(palette.dim_opacity);
                for state in states {
                    assert_wcag(&format!("{theme_name} dim {name:?}"), adjusted, state);
                }
            }
        }
    }

    #[test]
    fn configured_minimum_contrast_reaches_each_ansi_cell() {
        let default_palette = studio_palette_with_contrast("K8s Studio Light", 0.0);
        assert_eq!(default_palette.minimum_contrast, DEFAULT_MINIMUM_CONTRAST);
        for theme_name in ["K8s Studio Light", "K8s Studio Dark"] {
            let palette = studio_palette_with_contrast(theme_name, 7.0);
            assert_eq!(palette.minimum_contrast, 7.0);
            let states = terminal_states(&palette);
            let colors = Colors::default();
            for name in ANSI_NAMES
                .into_iter()
                .chain([NamedColor::Foreground, NamedColor::BrightForeground])
            {
                let raw = palette.resolve(Color::Named(name), &colors);
                let adjusted = palette.adjust_fg(raw, palette.background);
                assert!((adjusted.h - raw.h).abs() < f32::EPSILON);
                assert!((adjusted.s - raw.s).abs() < f32::EPSILON);
                for state in states {
                    assert_wcag_at_least(
                        &format!("{theme_name} 7:1 {name:?}"),
                        adjusted,
                        state,
                        7.0,
                    );
                    assert_wcag_at_least(
                        &format!("{theme_name} 7:1 dim {name:?}"),
                        adjusted.opacity(palette.dim_opacity),
                        state,
                        7.0,
                    );
                }
            }
        }
    }

    #[test]
    fn configured_contrast_covers_indexed_and_truecolor_cells() {
        for theme_name in ["K8s Studio Light", "K8s Studio Dark"] {
            let palette = studio_palette_with_contrast(theme_name, 7.0);
            let colors = Colors::default();
            for index in u8::MIN..=u8::MAX {
                let raw = palette.resolve(Color::Indexed(index), &colors);
                let adjusted = palette.adjust_fg(raw, palette.background);
                for state in terminal_states(&palette) {
                    assert_wcag_at_least(
                        &format!("{theme_name} indexed {index}"),
                        adjusted,
                        state,
                        7.0,
                    );
                }
            }
            for rgb in [
                Rgb { r: 0, g: 0, b: 0 },
                Rgb {
                    r: 127,
                    g: 127,
                    b: 127,
                },
                Rgb {
                    r: 255,
                    g: 255,
                    b: 255,
                },
                Rgb { r: 255, g: 0, b: 0 },
                Rgb { r: 0, g: 255, b: 0 },
                Rgb { r: 0, g: 0, b: 255 },
            ] {
                let raw = palette.resolve(Color::Spec(rgb), &colors);
                let adjusted = palette.adjust_fg(raw, palette.background);
                for state in terminal_states(&palette) {
                    assert_wcag_at_least(&format!("{theme_name} truecolor"), adjusted, state, 7.0);
                }
            }
        }
    }

    #[test]
    fn indexed_and_truecolor_fallbacks_meet_wcag_aa() {
        for palette in [
            studio_palette("K8s Studio Light"),
            studio_palette("K8s Studio Dark"),
            Palette::one_dark(),
        ] {
            let colors = Colors::default();
            for index in u8::MIN..=u8::MAX {
                let raw = palette.resolve(Color::Indexed(index), &colors);
                let adjusted = palette.adjust_fg(raw, palette.background);
                for state in terminal_states(&palette) {
                    assert_wcag(&format!("indexed {index}"), adjusted, state);
                    assert_wcag(
                        &format!("dim indexed {index}"),
                        adjusted.opacity(palette.dim_opacity),
                        state,
                    );
                }
            }
            for rgb in [
                Rgb { r: 0, g: 0, b: 0 },
                Rgb {
                    r: 127,
                    g: 127,
                    b: 127,
                },
                Rgb {
                    r: 255,
                    g: 255,
                    b: 255,
                },
                Rgb { r: 255, g: 0, b: 0 },
                Rgb { r: 0, g: 255, b: 0 },
                Rgb { r: 0, g: 0, b: 255 },
            ] {
                let requested = Color::Spec(rgb);
                let raw = palette.resolve(requested, &colors);
                let adjusted = palette.adjust_fg(raw, palette.background);
                for state in terminal_states(&palette) {
                    assert_wcag(
                        &format!("truecolor {} {} {}", rgb.r, rgb.g, rgb.b),
                        adjusted,
                        state,
                    );
                    assert_wcag(
                        &format!("dim truecolor {} {} {}", rgb.r, rgb.g, rgb.b),
                        adjusted.opacity(palette.dim_opacity),
                        state,
                    );
                }
            }
        }
    }

    /// A dim cell has to read as dim.
    ///
    /// `dim_opacity` is derived from the contrast floor, so a palette can always
    /// satisfy the floor by refusing to dim at all, and every ratio assertion
    /// above would stay green. This is the assertion that says the floor is not
    /// the whole contract: the dim cell still has to be a real step below the
    /// same cell at full strength.
    #[test]
    fn dim_cells_stay_a_step_below_their_normal_counterparts() {
        for palette in [
            studio_palette("K8s Studio Light"),
            studio_palette("K8s Studio Dark"),
            studio_palette_with_contrast("K8s Studio Light", 7.0),
            studio_palette_with_contrast("K8s Studio Dark", 7.0),
            Palette::one_dark(),
        ] {
            assert!(
                palette.dim_opacity < 1.0,
                "the terminal dims nothing, so a dim cell and its normal counterpart are one \
                 colour: {}",
                palette.background,
            );
            let colors = Colors::default();
            for name in ANSI_NAMES
                .into_iter()
                .chain([NamedColor::Foreground, NamedColor::BrightForeground])
            {
                let raw = palette.resolve(Color::Named(name), &colors);
                let adjusted = palette.adjust_fg(raw, palette.background);
                for state in terminal_states(&palette) {
                    let gap = wcag_contrast_ratio(adjusted, state)
                        - wcag_contrast_ratio(adjusted.opacity(palette.dim_opacity), state);
                    assert!(
                        gap >= DIM_MIN_QUIET_GAP,
                        "{name:?} at {}:1 dimmed to {}:1 on {state:?} is a {gap:.2} step, under \
                         the {DIM_MIN_QUIET_GAP:.1} that keeps dim and non-dim two values.",
                        wcag_contrast_ratio(adjusted, state),
                        wcag_contrast_ratio(adjusted.opacity(palette.dim_opacity), state),
                    );
                }
            }
        }
    }

    /// Only Increase Contrast moves the dim depth, and it moves it the one way.
    ///
    /// Dimming and contrast draw on the same headroom, so a palette at the
    /// default floor keeps the depth its colours can carry and a palette pushed
    /// to 7:1 has to give the dim up. The cap is the ceiling on how far a dim
    /// cell may go: past it every cell has to be pushed to the lightness extreme
    /// to keep the floor, and a terminal with no hues left is not a palette.
    #[test]
    fn increase_contrast_is_the_only_thing_that_moves_the_dim_depth() {
        for theme_name in ["K8s Studio Light", "K8s Studio Dark"] {
            let palette = studio_palette(theme_name);
            assert!(
                palette.dim_opacity <= MAX_DIM_OPACITY,
                "{theme_name}: the default appearance dims deeper than the depth its colours \
                 can carry, which is how every cell ends up at the lightness extreme.",
            );
            let increased = studio_palette_with_contrast(theme_name, 7.0);
            assert!(
                increased.dim_opacity >= palette.dim_opacity,
                "{theme_name}: raising the floor to 7:1 made the dim *deeper*. The floor and the \
                 dim draw on the same headroom, so one of them has to give way and the floor \
                 is the one with the contract behind it.",
            );
        }
    }
}
