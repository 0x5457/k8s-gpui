use gpui::Hsla;

/// The floor a resting scrollbar thumb has to reach against the terminal canvas.
///
/// `DESIGN.md` §2 puts graphics, icons, and anything a pointer has to find at
/// 3:1, which is also `design::border::INTERACTIVE_MIN_CONTRAST`. `k8s-term` is
/// a leaf crate and cannot import `k8s-ui`, so the number is restated here as
/// the contract the theme values are measured against.
pub const SCROLLBAR_THUMB_MIN_CONTRAST: f32 = 3.0;

/// The floor the hovered thumb has to reach.
///
/// `DESIGN.md` §5 says a hover must not change what a state means. The shipped
/// pair had the hover *closer* to the canvas than the resting thumb in both
/// appearances, so pointing at the control made it disappear, and a floor
/// above the resting one is the smallest change that makes hover say "here".
pub const SCROLLBAR_THUMB_HOVER_MIN_CONTRAST: f32 = 4.5;

fn linearize(value: f32) -> f32 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn relative_luminance(color: Hsla) -> f32 {
    let color = color.to_rgb();
    0.2126 * linearize(color.r) + 0.7152 * linearize(color.g) + 0.0722 * linearize(color.b)
}

pub fn wcag_contrast_ratio(text_color: Hsla, background_color: Hsla) -> f32 {
    let background = background_color.alpha(1.0);
    let foreground = background.blend(text_color).alpha(1.0);
    let foreground = relative_luminance(foreground);
    let background = relative_luminance(background);
    let (lighter, darker) = if foreground >= background {
        (foreground, background)
    } else {
        (background, foreground)
    };
    (lighter + 0.05) / (darker + 0.05)
}

pub fn ensure_minimum_contrast(foreground: Hsla, background: Hsla, minimum_contrast: f32) -> Hsla {
    ensure_minimum_contrast_at_opacity(foreground, background, minimum_contrast, 1.0)
}

/// A foreground that is painted at less than full opacity, solved so the pair
/// the reader actually sees clears the floor.
///
/// A dim cell is the same colour at a lower alpha, so the floor has to be
/// measured on the composited pair. Solving it at full opacity instead lets the
/// solver hand back a colour that passes the test and is unreadable on screen.
pub fn ensure_minimum_contrast_at_opacity(
    foreground: Hsla,
    background: Hsla,
    minimum_contrast: f32,
    opacity: f32,
) -> Hsla {
    ensure_minimum_rendered_contrast(foreground, background, minimum_contrast, opacity)
}

fn rendered_contrast_ratio(foreground: Hsla, background: Hsla, opacity: f32) -> f32 {
    wcag_contrast_ratio(foreground.opacity(opacity), background)
}

fn ensure_minimum_rendered_contrast(
    foreground: Hsla,
    background: Hsla,
    minimum_contrast: f32,
    opacity: f32,
) -> Hsla {
    if minimum_contrast <= 0.0
        || rendered_contrast_ratio(foreground, background, opacity) >= minimum_contrast
    {
        return foreground;
    }

    let adjusted = adjust_lightness_for_contrast(foreground, background, minimum_contrast, opacity);
    if rendered_contrast_ratio(adjusted, background, opacity) >= minimum_contrast {
        return adjusted;
    }

    let desaturated = adjust_lightness_and_saturation_for_contrast(
        foreground,
        background,
        minimum_contrast,
        opacity,
    );
    if rendered_contrast_ratio(desaturated, background, opacity) >= minimum_contrast {
        return desaturated;
    }

    let black = Hsla {
        h: foreground.h,
        s: foreground.s,
        l: 0.0,
        a: foreground.a,
    };
    let white = Hsla {
        h: foreground.h,
        s: foreground.s,
        l: 1.0,
        a: foreground.a,
    };
    if rendered_contrast_ratio(white, background, opacity)
        > rendered_contrast_ratio(black, background, opacity)
    {
        white
    } else {
        black
    }
}

fn adjust_lightness_for_contrast(
    foreground: Hsla,
    background: Hsla,
    minimum_contrast: f32,
    opacity: f32,
) -> Hsla {
    let black = Hsla {
        h: foreground.h,
        s: foreground.s,
        l: 0.0,
        a: foreground.a,
    };
    let white = Hsla {
        h: foreground.h,
        s: foreground.s,
        l: 1.0,
        a: foreground.a,
    };
    let go_lighter = rendered_contrast_ratio(white, background, opacity)
        > rendered_contrast_ratio(black, background, opacity);
    let (mut low, mut high) = if go_lighter {
        (foreground.l, 1.0)
    } else {
        (0.0, foreground.l)
    };
    let mut best = if go_lighter { white } else { black };

    // `low` is the end already ruled out and `high` the end still standing, and
    // the answer is the *nearest* lightness that clears the floor. `DESIGN.md`
    // §3.4 asks the semantic hue to survive a lightness adjustment, so a
    // candidate that already clears sends the search back towards the colour it
    // started from instead of onwards to the extreme.
    for _ in 0..24 {
        let mid = (low + high) / 2.0;
        let candidate = Hsla {
            h: foreground.h,
            s: foreground.s,
            l: mid,
            a: foreground.a,
        };
        if rendered_contrast_ratio(candidate, background, opacity) >= minimum_contrast {
            best = candidate;
            if go_lighter {
                high = mid;
            } else {
                low = mid;
            }
        } else if go_lighter {
            low = mid;
        } else {
            high = mid;
        }
    }

    best
}

fn adjust_lightness_and_saturation_for_contrast(
    foreground: Hsla,
    background: Hsla,
    minimum_contrast: f32,
    opacity: f32,
) -> Hsla {
    for saturation in [1.0, 0.8, 0.6, 0.4, 0.2, 0.0] {
        let candidate = Hsla {
            h: foreground.h,
            s: foreground.s * saturation,
            l: foreground.l,
            a: foreground.a,
        };
        let adjusted =
            adjust_lightness_for_contrast(candidate, background, minimum_contrast, opacity);
        if rendered_contrast_ratio(adjusted, background, opacity) >= minimum_contrast {
            return adjusted;
        }
    }

    foreground
}

/// Reads one role out of the shipped K8s Studio theme.
///
/// `k8s-term` has no theme crate and no JSON parser, so the tests that check a
/// token against the product theme scan the same file `k8s-ui` reads. The key
/// is the theme file's own spelling: `terminal.ansi.black` for a role, `cursor`
/// for the one inside `players`.
#[cfg(test)]
pub(crate) fn theme_color(theme_name: &str, key: &str) -> Hsla {
    const THEME_JSON: &str = include_str!("../../k8s-app/assets/themes/k8s-studio.json");
    let start = THEME_JSON
        .find(&format!("\"name\": \"{theme_name}\""))
        .expect("theme name");
    let section = &THEME_JSON[start..];
    let end = section[1..]
        .find("\n      \"name\":")
        .map(|offset| offset + 1)
        .unwrap_or(section.len());
    let (_, value) = section[..end]
        .split_once(&format!("\"{key}\": \"#"))
        .unwrap_or_else(|| panic!("theme color {theme_name}/{key}"));
    gpui::rgba(u32::from_str_radix(&value[..8], 16).expect("theme color")).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::rgba;

    #[test]
    fn wcag_ratio_of_identical_colors_is_one() {
        let color: Hsla = rgba(0x808080ff).into();
        assert!((wcag_contrast_ratio(color, color) - 1.0).abs() < 0.0001);
    }

    #[test]
    fn black_on_white_has_wcag_ratio_of_twenty_one() {
        let black: Hsla = rgba(0x000000ff).into();
        let white: Hsla = rgba(0xffffffff).into();
        assert!((wcag_contrast_ratio(black, white) - 21.0).abs() < 0.0001);
    }

    #[test]
    fn wcag_ratio_composites_translucent_foreground() {
        let background: Hsla = rgba(0xffffffff).into();
        let foreground: Hsla = rgba(0x77777780).into();
        let composited = background.blend(foreground);
        assert!(
            (wcag_contrast_ratio(foreground, background)
                - wcag_contrast_ratio(composited, background))
            .abs()
                < 0.0001
        );
    }

    #[test]
    fn ensure_minimum_contrast_raises_low_contrast() {
        let foreground: Hsla = rgba(0x333333ff).into();
        let background: Hsla = rgba(0x2b2b2bff).into();
        let before = wcag_contrast_ratio(foreground, background);
        let adjusted = ensure_minimum_contrast(foreground, background, 4.5);
        assert!(wcag_contrast_ratio(adjusted, background) >= before);
        assert!(wcag_contrast_ratio(adjusted, background) >= 4.5);
    }

    #[test]
    fn ensure_minimum_contrast_at_opacity_accounts_for_rendered_alpha() {
        let foreground: Hsla = rgba(0x777777ff).into();
        let background: Hsla = rgba(0x202020ff).into();
        let adjusted = ensure_minimum_contrast_at_opacity(foreground, background, 4.5, 0.7);
        assert!(wcag_contrast_ratio(adjusted.opacity(0.7), background) >= 4.5);
    }

    #[test]
    fn ensure_minimum_contrast_keeps_readable_color() {
        let foreground: Hsla = rgba(0xffffffff).into();
        let background: Hsla = rgba(0x000000ff).into();
        assert_eq!(
            ensure_minimum_contrast(foreground, background, 4.5),
            foreground
        );
    }

    /// The scrollbar thumb is the one graphic in the terminal a pointer has to
    /// find, and it is painted straight onto the terminal canvas, so the theme
    /// values have to clear the floors against *that* surface on their own.
    ///
    /// Both shipped pairs failed: the resting thumb was below 3:1 in both
    /// appearances, and each hover was quieter than its own resting state, so
    /// pointing at the thumb made it disappear. `k8s-app` used to repair this
    /// in a resolver, which made it a second source of truth for two theme
    /// values; the theme carries the answer now and this is what holds it there.
    #[test]
    fn the_scrollbar_thumb_is_findable_and_hover_answers_rest() {
        for theme_name in ["K8s Studio Light", "K8s Studio Dark"] {
            let canvas = theme_color(theme_name, "terminal.background").alpha(1.0);
            let rest = wcag_contrast_ratio(
                theme_color(theme_name, "scrollbar.thumb.background"),
                canvas,
            );
            let hover = wcag_contrast_ratio(
                theme_color(theme_name, "scrollbar.thumb.hover_background"),
                canvas,
            );
            assert!(
                rest >= SCROLLBAR_THUMB_MIN_CONTRAST,
                "{theme_name}: the resting scrollbar thumb sits at {rest:.2}:1 on the terminal \
                 canvas, below the {SCROLLBAR_THUMB_MIN_CONTRAST:.1}:1 a pointer needs to find it.",
            );
            assert!(
                hover >= SCROLLBAR_THUMB_HOVER_MIN_CONTRAST,
                "{theme_name}: the hovered scrollbar thumb sits at {hover:.2}:1 on the terminal \
                 canvas, below the {SCROLLBAR_THUMB_HOVER_MIN_CONTRAST:.1}:1 hover has to reach \
                 to say more than the resting state does.",
            );
            assert!(
                hover > rest,
                "{theme_name}: hovering moves the scrollbar thumb from {rest:.2}:1 to \
                 {hover:.2}:1 against the canvas, so the pointer makes the control quieter.",
            );
        }
    }
}
