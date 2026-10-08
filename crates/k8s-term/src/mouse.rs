//! Encodes mouse input as xterm mouse-reporting sequences.

use alacritty_terminal::index::Point;
use alacritty_terminal::term::TermMode;
use gpui_kit::{Modifiers, MouseButton};

const MAX_SCROLL_REPEATS: usize = 64;

#[derive(Clone, Copy, Debug)]
enum MouseFormat {
    Sgr,
    Normal(bool),
}

impl MouseFormat {
    fn from_mode(mode: TermMode) -> Self {
        if mode.contains(TermMode::SGR_MOUSE) {
            Self::Sgr
        } else if mode.contains(TermMode::UTF8_MOUSE) {
            Self::Normal(true)
        } else {
            Self::Normal(false)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MouseButtonCode {
    LeftButton = 0,
    MiddleButton = 1,
    RightButton = 2,
    LeftMove = 32,
    MiddleMove = 33,
    RightMove = 34,
    NoneMove = 35,
    ScrollUp = 64,
    ScrollDown = 65,
    Other = 99,
}

impl MouseButtonCode {
    fn from_move_button(button: Option<MouseButton>) -> Self {
        match button {
            Some(MouseButton::Left) => Self::LeftMove,
            Some(MouseButton::Middle) => Self::MiddleMove,
            Some(MouseButton::Right) => Self::RightMove,
            Some(MouseButton::Navigate(_)) => Self::Other,
            None => Self::NoneMove,
        }
    }

    fn from_button(button: MouseButton) -> Self {
        match button {
            MouseButton::Left => Self::LeftButton,
            MouseButton::Middle => Self::MiddleButton,
            MouseButton::Right => Self::RightButton,
            MouseButton::Navigate(_) => Self::Other,
        }
    }

    fn is_other(self) -> bool {
        self == Self::Other
    }
}

/// Reports whether the terminal requests click, motion, or drag mouse input.
pub fn mouse_mode_active(mode: TermMode) -> bool {
    mode.intersects(TermMode::MOUSE_REPORT_CLICK | TermMode::MOUSE_MOTION | TermMode::MOUSE_DRAG)
}

pub(crate) fn should_bypass_local_mouse(
    button: MouseButton,
    modifiers: Modifiers,
    report_to_terminal: bool,
    mode: TermMode,
) -> bool {
    report_to_terminal
        && mouse_mode_active(mode)
        && !modifiers.shift
        && (button == MouseButton::Middle
            || button == MouseButton::Right
            || (button == MouseButton::Left
                && (modifiers.alt || modifiers.platform)
                && !modifiers.secondary()))
}

/// Encodes a mouse press or release. Returns `None` when mouse reporting is off.
pub fn mouse_button_report(
    point: Point,
    button: MouseButton,
    modifiers: Modifiers,
    pressed: bool,
    mode: TermMode,
) -> Option<Vec<u8>> {
    let code = MouseButtonCode::from_button(button);
    if code.is_other() || !mouse_mode_active(mode) {
        return None;
    }
    mouse_report(
        point,
        code,
        pressed,
        modifiers,
        MouseFormat::from_mode(mode),
    )
}

/// Encodes mouse motion in motion or drag mode. Drag mode requires a button.
pub fn mouse_moved_report(
    point: Point,
    button: Option<MouseButton>,
    modifiers: Modifiers,
    mode: TermMode,
) -> Option<Vec<u8>> {
    let code = MouseButtonCode::from_move_button(button);
    if code.is_other()
        || !mode.intersects(TermMode::MOUSE_MOTION | TermMode::MOUSE_DRAG)
        || (mode.contains(TermMode::MOUSE_DRAG) && code == MouseButtonCode::NoneMove)
    {
        return None;
    }
    mouse_report(point, code, true, modifiers, MouseFormat::from_mode(mode))
}

fn scroll_repeats(scroll_lines: i32) -> usize {
    usize::try_from(scroll_lines.unsigned_abs())
        .unwrap_or(usize::MAX)
        .min(MAX_SCROLL_REPEATS)
}

/// Encodes each scrolled line as a terminal mouse report.
pub fn scroll_report(
    point: Point,
    scroll_lines: i32,
    modifiers: Modifiers,
    mode: TermMode,
) -> Vec<Vec<u8>> {
    if !mouse_mode_active(mode) || scroll_lines == 0 {
        return Vec::new();
    }
    let code = if scroll_lines > 0 {
        MouseButtonCode::ScrollUp
    } else {
        MouseButtonCode::ScrollDown
    };
    let Some(report) = mouse_report(point, code, true, modifiers, MouseFormat::from_mode(mode))
    else {
        return Vec::new();
    };
    std::iter::repeat_n(report, scroll_repeats(scroll_lines)).collect()
}

pub fn accumulate_scroll_delta(accumulator: &mut f32, delta: f32, pixels_per_line: f32) -> i32 {
    if !delta.is_finite() || !pixels_per_line.is_finite() || pixels_per_line <= 0.0 {
        return 0;
    }
    if !accumulator.is_finite() || *accumulator <= 0.0 {
        *accumulator = 0.0;
    }
    let amount = delta.abs() / pixels_per_line;
    if !amount.is_finite() {
        *accumulator = 0.0;
        return 0;
    }
    if amount == 0.0 {
        return 0;
    }

    let epsilon = 0.0001;
    let max_lines = MAX_SCROLL_REPEATS as f32;
    if delta > 0.0 {
        let total = if amount >= max_lines - *accumulator {
            max_lines
        } else {
            *accumulator + amount
        };
        if !total.is_finite() || total >= max_lines {
            *accumulator = 0.0;
            return MAX_SCROLL_REPEATS as i32;
        }
        let lines = total.trunc() as i32;
        let remainder = total - lines as f32;
        *accumulator = if remainder.is_finite() && remainder >= epsilon {
            remainder
        } else {
            0.0
        };
        lines
    } else if *accumulator > epsilon {
        if amount <= *accumulator {
            *accumulator = (*accumulator - amount).max(0.0);
            if *accumulator < epsilon {
                *accumulator = 0.0;
            }
            return 0;
        }
        let remainder = amount - *accumulator;
        if !remainder.is_finite() || remainder >= max_lines {
            *accumulator = 0.0;
            return -(MAX_SCROLL_REPEATS as i32);
        }
        let lines = remainder.trunc() as i32;
        let fractional = remainder - lines as f32;
        *accumulator = if fractional.is_finite() && fractional >= epsilon {
            fractional
        } else {
            0.0
        };
        -lines
    } else {
        *accumulator = 0.0;
        if amount >= max_lines {
            return -(MAX_SCROLL_REPEATS as i32);
        }
        let lines = amount.trunc() as i32;
        let remainder = amount - lines as f32;
        *accumulator = if remainder.is_finite() && remainder >= epsilon {
            remainder
        } else {
            0.0
        };
        -lines
    }
}

/// Converts alternate-screen scrolling to arrow key sequences.
pub fn alt_scroll(scroll_lines: i32) -> Vec<u8> {
    let cmd = if scroll_lines > 0 { b'A' } else { b'B' };
    let repeats = scroll_repeats(scroll_lines);
    let mut content = Vec::with_capacity(repeats.saturating_mul(3));
    for _ in 0..repeats {
        content.extend_from_slice(&[0x1b, b'O', cmd]);
    }
    content
}

fn mouse_report(
    point: Point,
    button: MouseButtonCode,
    pressed: bool,
    modifiers: Modifiers,
    format: MouseFormat,
) -> Option<Vec<u8>> {
    if point.line < 0 {
        return None;
    }

    let mut mods = 0u8;
    if modifiers.shift {
        mods += 4;
    }
    if modifiers.alt {
        mods += 8;
    }
    if modifiers.control {
        mods += 16;
    }

    match format {
        MouseFormat::Sgr => {
            Some(sgr_mouse_report(point, button as u8 + mods, pressed).into_bytes())
        }
        MouseFormat::Normal(utf8) => {
            if pressed {
                normal_mouse_report(point, button as u8 + mods, utf8)
            } else {
                normal_mouse_report(point, 3 + mods, utf8)
            }
        }
    }
}

fn normal_mouse_report(point: Point, button: u8, utf8: bool) -> Option<Vec<u8>> {
    let max_point = if utf8 { 2015 } else { 223 };
    if point.line.0 >= max_point || point.column.0 >= max_point as usize {
        return None;
    }

    let mut message = vec![b'\x1b', b'[', b'M', 32 + button];

    let mouse_pos_encode = |pos: usize| -> Vec<u8> {
        let pos = 32 + 1 + pos;
        let first = 0xC0 + pos / 64;
        let second = 0x80 + (pos & 63);
        vec![first as u8, second as u8]
    };

    if utf8 && point.column >= 95 {
        message.append(&mut mouse_pos_encode(point.column.0));
    } else {
        message.push(32 + 1 + point.column.0 as u8);
    }

    if utf8 && point.line >= 95 {
        message.append(&mut mouse_pos_encode(point.line.0 as usize));
    } else {
        message.push(32 + 1 + point.line.0 as u8);
    }

    Some(message)
}

fn sgr_mouse_report(point: Point, button: u8, pressed: bool) -> String {
    let suffix = if pressed { 'M' } else { 'm' };
    format!(
        "\x1b[<{};{};{}{}",
        button,
        point.column + 1,
        point.line + 1,
        suffix
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::index::{Column, Line};

    fn point() -> Point {
        Point::new(Line(2), Column(3))
    }

    #[test]
    fn reporting_mode_bypasses_local_right_and_alt_clicks() {
        let mode = TermMode::MOUSE_REPORT_CLICK;
        assert!(should_bypass_local_mouse(
            MouseButton::Middle,
            Modifiers::default(),
            true,
            mode,
        ));
        assert!(should_bypass_local_mouse(
            MouseButton::Right,
            Modifiers::default(),
            true,
            mode,
        ));
        assert!(should_bypass_local_mouse(
            MouseButton::Left,
            Modifiers {
                alt: true,
                ..Modifiers::default()
            },
            true,
            mode,
        ));
        // The platform key is the secondary key only on macOS, and every caller folds it
        // into `alt` before this check, so it reaches the function on its own only here.
        // Off macOS it is the Super/Win key: a click with it goes to the terminal. On macOS
        // it would read as Command held, which the secondary guard keeps local.
        assert_eq!(
            should_bypass_local_mouse(
                MouseButton::Left,
                Modifiers {
                    platform: true,
                    ..Modifiers::default()
                },
                true,
                mode,
            ),
            cfg!(not(target_os = "macos")),
        );
        assert!(!should_bypass_local_mouse(
            MouseButton::Left,
            Modifiers {
                alt: true,
                ..Modifiers::secondary_key()
            },
            true,
            mode,
        ));
        assert!(!should_bypass_local_mouse(
            MouseButton::Left,
            Modifiers::default(),
            true,
            mode,
        ));
        assert!(!should_bypass_local_mouse(
            MouseButton::Right,
            Modifiers {
                shift: true,
                ..Modifiers::default()
            },
            true,
            mode,
        ));
        assert!(!should_bypass_local_mouse(
            MouseButton::Right,
            Modifiers::default(),
            true,
            TermMode::default(),
        ));
    }

    #[test]
    fn extreme_scroll_input_caps_repeated_output() {
        let mode = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        let reports = scroll_report(point(), i32::MIN, Modifiers::default(), mode);
        assert_eq!(reports.len(), MAX_SCROLL_REPEATS);

        let alternate = alt_scroll(i32::MIN);
        assert_eq!(alternate.len(), MAX_SCROLL_REPEATS * 3);
        assert!(alternate.ends_with(&[0x1b, b'O', b'B']));
    }

    #[test]
    fn scroll_accumulator_never_becomes_negative() {
        let mut accumulator = 0.0;
        assert_eq!(accumulate_scroll_delta(&mut accumulator, 0.6, 1.0), 0);
        assert_eq!(accumulator, 0.6);
        assert_eq!(accumulate_scroll_delta(&mut accumulator, 0.6, 1.0), 1);
        assert_eq!(accumulate_scroll_delta(&mut accumulator, -0.2, 1.0), 0);
        assert!(accumulator >= 0.0);
        assert_eq!(accumulate_scroll_delta(&mut accumulator, -1.0, 1.0), -1);
        assert!(accumulator >= 0.0);
    }

    #[test]
    fn scroll_accumulator_scales_pixels_and_ignores_invalid_units() {
        let mut accumulator = 0.0;
        assert_eq!(accumulate_scroll_delta(&mut accumulator, 18.0, 20.0), 0);
        assert!((accumulator - 0.9).abs() < 0.0001);
        assert_eq!(accumulate_scroll_delta(&mut accumulator, 2.0, 20.0), 1);
        assert!(accumulator.abs() < 0.0001);

        accumulator = 0.4;
        assert_eq!(accumulate_scroll_delta(&mut accumulator, f32::NAN, 20.0), 0);
        assert_eq!(accumulate_scroll_delta(&mut accumulator, 1.0, 0.0), 0);
        assert_eq!(accumulator, 0.4);
    }

    #[test]
    fn scroll_accumulator_cancels_opposite_fractional_delta() {
        let mut accumulator = 0.0;
        assert_eq!(accumulate_scroll_delta(&mut accumulator, 0.75, 1.0), 0);
        assert_eq!(accumulate_scroll_delta(&mut accumulator, -0.25, 1.0), 0);
        assert_eq!(accumulate_scroll_delta(&mut accumulator, 0.5, 1.0), 1);
        assert!(accumulator.abs() < 0.0001);
    }

    #[test]
    fn scroll_accumulator_resets_invalid_state_and_bounds_large_ratios() {
        let mut accumulator = f32::NAN;
        assert_eq!(accumulate_scroll_delta(&mut accumulator, 1.0, 1.0), 1);
        assert!(accumulator.is_finite());
        assert!(accumulator >= 0.0);

        accumulator = -2.0;
        assert_eq!(accumulate_scroll_delta(&mut accumulator, 1.0, 1.0), 1);
        assert_eq!(accumulator, 0.0);

        accumulator = 0.0;
        assert_eq!(
            accumulate_scroll_delta(&mut accumulator, f32::MAX, f32::MIN_POSITIVE),
            0
        );
        assert_eq!(accumulator, 0.0);
    }
}
