//! Encodes GPUI key events as terminal input sequences.

use alacritty_terminal::term::TermMode;
use gpui::{Keystroke, Modifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalModifiers {
    None,
    Shift,
    Alt,
    Ctrl,
    CtrlShift,
    Other,
}

impl TerminalModifiers {
    fn new(modifiers: Modifiers) -> Self {
        match (
            modifiers.control,
            modifiers.alt,
            modifiers.shift,
            modifiers.platform,
        ) {
            (false, false, false, false) => Self::None,
            (false, false, true, false) => Self::Shift,
            (false, true, false, false) => Self::Alt,
            (true, false, false, false) => Self::Ctrl,
            (true, false, true, false) => Self::CtrlShift,
            _ => Self::Other,
        }
    }

    fn any(self) -> bool {
        !matches!(self, Self::None)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalShortcut {
    Copy,
    Cut,
    Paste,
    SelectAll,
    Find,
    FindNext,
}

/// Returns the local editing action for a keystroke, or `None` when the key belongs to the
/// child process.
///
/// Two families of chords reach the local editing actions:
///
/// * The platform modifier, which is Command on macOS and Super on Linux and Windows.
/// * Control+Shift+letter, the Linux terminal convention, told apart from a bare
///   Control+letter by the modifier state. See [`is_control_shift_letter`].
///
/// Control+letter without a Shift is never an editing key, so Control+C stays the ETX that
/// interrupts the child process.
pub(crate) fn terminal_shortcut(keystroke: &Keystroke) -> Option<TerminalShortcut> {
    let modifiers = keystroke.modifiers;
    if modifiers.shift && !modifiers.control && !modifiers.alt && !modifiers.platform {
        return keystroke
            .key
            .eq_ignore_ascii_case("insert")
            .then_some(TerminalShortcut::Paste);
    }
    if is_control_shift_letter(keystroke) {
        return match keystroke.key.to_ascii_lowercase().as_str() {
            "c" => Some(TerminalShortcut::Copy),
            "v" => Some(TerminalShortcut::Paste),
            "a" => Some(TerminalShortcut::SelectAll),
            "x" => Some(TerminalShortcut::Cut),
            "f" => Some(TerminalShortcut::Find),
            "g" => Some(TerminalShortcut::FindNext),
            _ => None,
        };
    }
    if modifiers.control || modifiers.alt || modifiers.shift || !modifiers.platform {
        return None;
    }
    match keystroke.key.to_ascii_lowercase().as_str() {
        "c" => Some(TerminalShortcut::Copy),
        "v" => Some(TerminalShortcut::Paste),
        "a" => Some(TerminalShortcut::SelectAll),
        "x" => Some(TerminalShortcut::Cut),
        "f" => Some(TerminalShortcut::Find),
        _ => None,
    }
}

/// Returns true when the keystroke is a Control+Shift+letter chord rather than a bare
/// Control+letter chord.
///
/// XKB folds Shift into the keysym, so `key` and `key_char` read the same for Control+Shift+C and
/// for Control+C. Only the modifier state separates them: it comes from the physical modifier
/// mask rather than from the keysym, and it is the same flag the keymap matcher uses for a
/// `ctrl-shift-c` binding, so the shortcut table and the keymap always agree. Requiring it keeps
/// Control+C as the ETX byte that interrupts the child process.
///
/// The printed character is deliberately not accepted as a second signal. CapsLock already turns
/// Control+C into an upper-case character with the Shift flag clear, so trusting the character
/// would cost every CapsLock user the interrupt.
fn is_control_shift_letter(keystroke: &Keystroke) -> bool {
    let modifiers = keystroke.modifiers;
    modifiers.control
        && modifiers.shift
        && !modifiers.alt
        && !modifiers.platform
        && !modifiers.function
        && is_single_ascii_letter(&keystroke.key)
}

fn is_single_ascii_letter(key: &str) -> bool {
    let mut characters = key.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic())
        && characters.next().is_none()
}

/// Returns true when the keystroke carries text that a keyboard layout produced.
pub(crate) fn is_typed_text(keystroke: &Keystroke) -> bool {
    keystroke
        .key_char
        .as_deref()
        .and_then(|text| text.chars().next())
        .is_some_and(|character| !character.is_control())
}

/// Returns true for the AltGr combination. XKB reports the third level as Control+Alt and
/// some layouts also report the platform modifier, so only the printed character decides.
pub(crate) fn is_altgr(keystroke: &Keystroke) -> bool {
    keystroke.modifiers.control && keystroke.modifiers.alt && is_typed_text(keystroke)
}

/// Returns true when a keystroke is a character the keyboard layout printed, including the
/// AltGr third level. It separates typed text from shortcut chords.
///
/// Only the AltGr combination overrides the chord modifiers. GPUI reports Super and Command as
/// the platform modifier on every platform, so a platform chord stays a chord even when the
/// toolkit hands it a printable character. Alt alone stays text, because the macOS Option level
/// prints a character.
pub(crate) fn is_printed_text(keystroke: &Keystroke) -> bool {
    if !is_typed_text(keystroke) {
        return false;
    }
    is_altgr(keystroke) || !(keystroke.modifiers.control || keystroke.modifiers.platform)
}

/// Returns true when the terminal application asked for the kitty keyboard protocol.
fn kitty_requested(mode: TermMode) -> bool {
    mode.intersects(
        TermMode::DISAMBIGUATE_ESC_CODES
            | TermMode::REPORT_ALL_KEYS_AS_ESC
            | TermMode::REPORT_EVENT_TYPES
            | TermMode::REPORT_ALTERNATE_KEYS
            | TermMode::REPORT_ASSOCIATED_TEXT,
    )
}

/// Returns true when Control+C must be sent as the C0 ETX byte.
///
/// The kitty protocol has its own encoding for Control+C, and an application that asked
/// for disambiguation cannot tell ETX from other C0 bytes. Legacy terminals keep ETX.
fn is_plain_ctrl_c(keystroke: &Keystroke, mode: TermMode) -> bool {
    let modifiers = keystroke.modifiers;
    modifiers.control
        && !modifiers.alt
        && !modifiers.shift
        && !modifiers.platform
        && keystroke.key.eq_ignore_ascii_case("c")
        && !kitty_requested(mode)
}

/// Encodes a key as a terminal sequence. `None` lets the parent handle the key.
///
/// With `DISAMBIGUATE_ESC_CODES`, ambiguous keys use CSI u. Other keys keep legacy sequences.
pub fn encode_key(keystroke: &Keystroke, mode: TermMode, option_as_meta: bool) -> Option<Vec<u8>> {
    if is_altgr(keystroke) {
        return keystroke
            .key_char
            .as_deref()
            .map(|text| text.as_bytes().to_vec());
    }
    if is_plain_ctrl_c(keystroke, mode) {
        return Some(vec![0x03]);
    }
    if let Some(sequence) = encode_kitty(keystroke, mode) {
        return Some(sequence);
    }
    let modifiers = TerminalModifiers::new(keystroke.modifiers);
    let key = keystroke.key.as_str();

    let manual: Option<&'static str> = match (key, modifiers) {
        ("tab", TerminalModifiers::None) => Some("\x09"),
        ("escape", TerminalModifiers::None) => Some("\x1b"),
        ("enter", TerminalModifiers::None) => Some("\x0d"),
        ("enter", TerminalModifiers::Shift) => Some("\x0a"),
        ("enter", TerminalModifiers::Alt) => Some("\x1b\x0d"),
        ("backspace", TerminalModifiers::None) => Some("\x7f"),
        ("tab", TerminalModifiers::Shift) => Some("\x1b[Z"),
        ("backspace", TerminalModifiers::Ctrl) => Some("\x08"),
        ("backspace", TerminalModifiers::Alt) => Some("\x1b\x7f"),
        ("backspace", TerminalModifiers::Shift) => Some("\x7f"),
        ("space", TerminalModifiers::Ctrl) => Some("\x00"),
        ("home", TerminalModifiers::None) if mode.contains(TermMode::APP_CURSOR) => Some("\x1bOH"),
        ("home", TerminalModifiers::None) => Some("\x1b[H"),
        ("end", TerminalModifiers::None) if mode.contains(TermMode::APP_CURSOR) => Some("\x1bOF"),
        ("end", TerminalModifiers::None) => Some("\x1b[F"),
        ("up", TerminalModifiers::None) if mode.contains(TermMode::APP_CURSOR) => Some("\x1bOA"),
        ("up", TerminalModifiers::None) => Some("\x1b[A"),
        ("down", TerminalModifiers::None) if mode.contains(TermMode::APP_CURSOR) => Some("\x1bOB"),
        ("down", TerminalModifiers::None) => Some("\x1b[B"),
        ("right", TerminalModifiers::None) if mode.contains(TermMode::APP_CURSOR) => Some("\x1bOC"),
        ("right", TerminalModifiers::None) => Some("\x1b[C"),
        ("left", TerminalModifiers::None) if mode.contains(TermMode::APP_CURSOR) => Some("\x1bOD"),
        ("left", TerminalModifiers::None) => Some("\x1b[D"),
        ("insert", TerminalModifiers::None) => Some("\x1b[2~"),
        ("delete", TerminalModifiers::None) => Some("\x1b[3~"),
        ("pageup", TerminalModifiers::None) => Some("\x1b[5~"),
        ("pagedown", TerminalModifiers::None) => Some("\x1b[6~"),
        ("f1", TerminalModifiers::None) => Some("\x1bOP"),
        ("f2", TerminalModifiers::None) => Some("\x1bOQ"),
        ("f3", TerminalModifiers::None) => Some("\x1bOR"),
        ("f4", TerminalModifiers::None) => Some("\x1bOS"),
        ("f5", TerminalModifiers::None) => Some("\x1b[15~"),
        ("f6", TerminalModifiers::None) => Some("\x1b[17~"),
        ("f7", TerminalModifiers::None) => Some("\x1b[18~"),
        ("f8", TerminalModifiers::None) => Some("\x1b[19~"),
        ("f9", TerminalModifiers::None) => Some("\x1b[20~"),
        ("f10", TerminalModifiers::None) => Some("\x1b[21~"),
        ("f11", TerminalModifiers::None) => Some("\x1b[23~"),
        ("f12", TerminalModifiers::None) => Some("\x1b[24~"),
        ("a", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x01"),
        ("b", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x02"),
        ("c", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x03"),
        ("d", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x04"),
        ("e", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x05"),
        ("f", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x06"),
        ("g", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x07"),
        ("h", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x08"),
        ("i", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x09"),
        ("j", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x0a"),
        ("k", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x0b"),
        ("l", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x0c"),
        ("m", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x0d"),
        ("n", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x0e"),
        ("o", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x0f"),
        ("p", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x10"),
        ("q", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x11"),
        ("r", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x12"),
        ("s", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x13"),
        ("t", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x14"),
        ("u", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x15"),
        ("v", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x16"),
        ("w", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x17"),
        ("x", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x18"),
        ("y", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x19"),
        ("z", TerminalModifiers::Ctrl | TerminalModifiers::CtrlShift) => Some("\x1a"),
        ("@", TerminalModifiers::Ctrl) => Some("\x00"),
        ("[", TerminalModifiers::Ctrl) => Some("\x1b"),
        ("\\", TerminalModifiers::Ctrl) => Some("\x1c"),
        ("]", TerminalModifiers::Ctrl) => Some("\x1d"),
        ("^", TerminalModifiers::Ctrl) => Some("\x1e"),
        ("_", TerminalModifiers::Ctrl) => Some("\x1f"),
        ("?", TerminalModifiers::Ctrl) => Some("\x7f"),
        _ => None,
    };
    if let Some(sequence) = manual {
        return Some(sequence.as_bytes().to_vec());
    }

    if modifiers.any() {
        let code = modifier_code(keystroke.modifiers);
        let modified = match key {
            "up" => Some(format!("\x1b[1;{code}A")),
            "down" => Some(format!("\x1b[1;{code}B")),
            "right" => Some(format!("\x1b[1;{code}C")),
            "left" => Some(format!("\x1b[1;{code}D")),
            "f1" => Some(format!("\x1b[1;{code}P")),
            "f2" => Some(format!("\x1b[1;{code}Q")),
            "f3" => Some(format!("\x1b[1;{code}R")),
            "f4" => Some(format!("\x1b[1;{code}S")),
            "f5" => Some(format!("\x1b[15;{code}~")),
            "f6" => Some(format!("\x1b[17;{code}~")),
            "f7" => Some(format!("\x1b[18;{code}~")),
            "f8" => Some(format!("\x1b[19;{code}~")),
            "f9" => Some(format!("\x1b[20;{code}~")),
            "f10" => Some(format!("\x1b[21;{code}~")),
            "f11" => Some(format!("\x1b[23;{code}~")),
            "f12" => Some(format!("\x1b[24;{code}~")),
            "insert" => Some(format!("\x1b[2;{code}~")),
            "pageup" => Some(format!("\x1b[5;{code}~")),
            "pagedown" => Some(format!("\x1b[6;{code}~")),
            "end" => Some(format!("\x1b[1;{code}F")),
            "home" => Some(format!("\x1b[1;{code}H")),
            _ => None,
        };
        if let Some(sequence) = modified {
            return Some(sequence.into_bytes());
        }
    }

    if (!cfg!(target_os = "macos") || option_as_meta)
        && keystroke.modifiers.alt
        && key.is_ascii()
        && key.len() == 1
    {
        let ch = key.chars().next()?;
        if modifiers == TerminalModifiers::Alt {
            return Some(format!("\x1b{ch}").into_bytes());
        } else if keystroke.modifiers.shift {
            return Some(format!("\x1b{}", ch.to_ascii_uppercase()).into_bytes());
        } else if keystroke.modifiers.control && ch.is_ascii_lowercase() {
            return Some(format!("\x1b{}", (ch as u8 - b'a' + 1) as char).into_bytes());
        }
    }

    keystroke
        .key_char
        .as_ref()
        .map(|text| text.as_bytes().to_vec())
}

/// Returns true when an active IME composition owns the key, so it must not reach the PTY.
///
/// Enter commits the preview, Escape cancels it, and the caret keys move inside the preview.
/// Typed characters, including AltGr input, arrive later as committed text, so sending the
/// key as well would type every composed character twice.
pub(crate) fn ime_owns_key(composing: bool, keystroke: &Keystroke) -> bool {
    if !composing {
        return false;
    }
    if composition_key(&keystroke.key) {
        return true;
    }
    is_typed_text(keystroke) && (!keystroke.modifiers.control || is_altgr(keystroke))
}

fn composition_key(key: &str) -> bool {
    matches!(
        key,
        "escape"
            | "enter"
            | "return"
            | "up"
            | "arrowup"
            | "down"
            | "arrowdown"
            | "left"
            | "arrowleft"
            | "right"
            | "arrowright"
    )
}

/// Encodes kitty keyboard protocol input.
///
/// GPUI exposes key presses but not release or repeat events, so the event
/// type is reported as a press when the application requested event types.
fn encode_kitty(keystroke: &Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    let disambiguate = mode.contains(TermMode::DISAMBIGUATE_ESC_CODES);
    let report_event_types = mode.contains(TermMode::REPORT_EVENT_TYPES);
    let report_alternate = mode.contains(TermMode::REPORT_ALTERNATE_KEYS);
    let report_text = mode.contains(TermMode::REPORT_ASSOCIATED_TEXT);
    let all_as_escape = mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC) || report_text;
    if !disambiguate && !all_as_escape && !report_event_types && !report_alternate {
        return None;
    }

    let modifiers = keystroke.modifiers;
    let modified = modifiers.modified();
    if let Some((code, final_byte)) = kitty_functional_key(&keystroke.key) {
        if !disambiguate && !all_as_escape {
            return None;
        }
        let modifier_field = kitty_modifier_field(modifiers, report_event_types);
        let mut sequence = format!("\x1b[{code}");
        if !modifier_field.is_empty() {
            sequence.push(';');
            sequence.push_str(&modifier_field);
        }
        sequence.push(final_byte as char);
        return Some(sequence.into_bytes());
    }

    let code = kitty_text_key_code(keystroke)?;
    if !modified && !all_as_escape {
        return None;
    }

    let mut key_field = code.to_string();
    if report_alternate
        && modifiers.shift
        && let Some(shifted) = keystroke
            .key_char
            .as_deref()
            .and_then(|text| text.chars().next())
            .or_else(|| keystroke.key.chars().next())
            .map(|character| character as u32)
            .filter(|shifted| *shifted != code)
    {
        key_field.push(':');
        key_field.push_str(&shifted.to_string());
    }

    let modifier_field = kitty_modifier_field(modifiers, report_event_types);
    let text = if report_text {
        kitty_associated_text(keystroke.key_char.as_deref())
    } else {
        String::new()
    };
    let mut sequence = format!("\x1b[{key_field}");
    if !modifier_field.is_empty() || !text.is_empty() {
        sequence.push(';');
        sequence.push_str(&modifier_field);
    }
    if !text.is_empty() {
        sequence.push(';');
        sequence.push_str(&text);
    }
    sequence.push('u');
    Some(sequence.into_bytes())
}

fn kitty_functional_key(key: &str) -> Option<(u32, u8)> {
    let (code, final_byte) = match key {
        "escape" => (27, b'u'),
        "enter" | "return" => (13, b'u'),
        "tab" => (9, b'u'),
        "backspace" => (127, b'u'),
        "insert" => (2, b'~'),
        "delete" => (3, b'~'),
        "pageup" | "page_up" => (5, b'~'),
        "pagedown" | "page_down" => (6, b'~'),
        "up" => (1, b'A'),
        "down" => (1, b'B'),
        "right" => (1, b'C'),
        "left" => (1, b'D'),
        "home" => (1, b'H'),
        "end" => (1, b'F'),
        // F1 to F4 keep the private-use codepoints. Their legacy CSI P to CSI S forms are
        // indistinguishable from DCH and friends, so they cannot carry a modifier.
        "f1" => (57364, b'~'),
        "f2" => (57365, b'~'),
        "f3" => (57366, b'~'),
        "f4" => (57367, b'~'),
        "f5" => (15, b'~'),

        "f6" => (17, b'~'),
        "f7" => (18, b'~'),
        "f8" => (19, b'~'),
        "f9" => (20, b'~'),
        "f10" => (21, b'~'),
        "f11" => (23, b'~'),
        "f12" => (24, b'~'),
        "menu" => (29, b'~'),
        "f13" => (57376, b'u'),
        "f14" => (57377, b'u'),
        "f15" => (57378, b'u'),
        "f16" => (57379, b'u'),
        "f17" => (57380, b'u'),
        "f18" => (57381, b'u'),
        "f19" => (57382, b'u'),
        "f20" => (57383, b'u'),
        "f21" => (57384, b'u'),
        "f22" => (57385, b'u'),
        "f23" => (57386, b'u'),
        "f24" => (57387, b'u'),
        "kp_0" | "keypad0" | "numpad0" => (57399, b'u'),
        "kp_1" | "keypad1" | "numpad1" => (57400, b'u'),
        "kp_2" | "keypad2" | "numpad2" => (57401, b'u'),
        "kp_3" | "keypad3" | "numpad3" => (57402, b'u'),
        "kp_4" | "keypad4" | "numpad4" => (57403, b'u'),
        "kp_5" | "keypad5" | "numpad5" => (57404, b'u'),
        "kp_6" | "keypad6" | "numpad6" => (57405, b'u'),
        "kp_7" | "keypad7" | "numpad7" => (57406, b'u'),
        "kp_8" | "keypad8" | "numpad8" => (57407, b'u'),
        "kp_9" | "keypad9" | "numpad9" => (57408, b'u'),
        "kp_decimal" | "keypaddecimal" | "numpaddecimal" => (57409, b'u'),
        "kp_divide" | "keypaddivide" | "numpaddivide" => (57410, b'u'),
        "kp_multiply" | "keypadmultiply" | "numpadmultiply" => (57411, b'u'),
        "kp_subtract" | "keypadminus" | "numpadminus" => (57412, b'u'),
        "kp_add" | "keypadplus" | "numpadplus" => (57413, b'u'),
        "kp_enter" | "keypadenter" | "numpadenter" => (57414, b'u'),
        "kp_equals" | "keypadequals" | "numpadequals" => (57415, b'u'),
        _ => return None,
    };
    Some((code, final_byte))
}

fn kitty_text_key_code(keystroke: &Keystroke) -> Option<u32> {
    let key = keystroke.key.as_str();
    if key == "space" {
        return Some(' ' as u32);
    }
    let mut chars = key.chars();
    let character = chars.next()?;
    if chars.next().is_none() {
        return Some(character.to_ascii_lowercase() as u32);
    }
    keystroke
        .key_char
        .as_deref()
        .and_then(|text| text.chars().next())
        .map(|character| character as u32)
}

fn kitty_modifier_field(modifiers: Modifiers, report_event_types: bool) -> String {
    let code = kitty_modifier_code(modifiers);
    if report_event_types {
        // The event type follows the modifier field, so the field is always present.
        format!("{code}:1")
    } else if code == 1 {
        String::new()
    } else {
        code.to_string()
    }
}

fn kitty_associated_text(text: Option<&str>) -> String {
    text.into_iter()
        .flat_map(str::chars)
        .filter(|character| !character.is_control())
        .map(|character| character as u32)
        .map(|codepoint| codepoint.to_string())
        .collect::<Vec<_>>()
        .join(":")
}

/// Kitty modifier bits: `1 + shift + 2*alt + 4*ctrl + 8*super`.
fn kitty_modifier_code(modifiers: Modifiers) -> u32 {
    let mut code = 1;
    if modifiers.shift {
        code += 1;
    }
    if modifiers.alt {
        code += 2;
    }
    if modifiers.control {
        code += 4;
    }
    if modifiers.platform {
        code += 8;
    }
    code
}

fn modifier_code(modifiers: Modifiers) -> u32 {
    let mut code = 0;
    if modifiers.shift {
        code |= 1;
    }
    if modifiers.alt {
        code |= 1 << 1;
    }
    if modifiers.control {
        code |= 1 << 2;
    }
    if modifiers.platform {
        code |= 1 << 3;
    }
    code + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Modifiers;

    fn keystroke(key: &str, key_char: Option<&str>, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            key: key.to_owned(),
            key_char: key_char.map(str::to_owned),
            modifiers,
        }
    }

    /// The character XKB prints for a letter key once Shift is held.
    fn upper(key: &str) -> String {
        key.to_ascii_uppercase()
    }

    #[test]
    fn arrows_follow_app_cursor_mode() {
        let up = keystroke("up", None, Modifiers::default());
        assert_eq!(
            encode_key(&up, TermMode::default(), false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode_key(&up, TermMode::APP_CURSOR, false),
            Some(b"\x1bOA".to_vec())
        );
    }

    #[test]
    fn ctrl_letters_map_to_caret_notation() {
        let ctrl_c = keystroke(
            "c",
            Some("c"),
            Modifiers {
                control: true,
                ..Default::default()
            },
        );
        assert_eq!(
            encode_key(&ctrl_c, TermMode::default(), false),
            Some(vec![0x03])
        );
    }

    #[test]
    fn alt_prefixes_escape() {
        let alt_x = keystroke(
            "x",
            Some("x"),
            Modifiers {
                alt: true,
                ..Default::default()
            },
        );
        assert_eq!(
            encode_key(&alt_x, TermMode::default(), false),
            Some(b"\x1bx".to_vec())
        );
    }

    #[test]
    fn shifted_arrows_include_modifier_code() {
        let shift_up = keystroke(
            "up",
            None,
            Modifiers {
                shift: true,
                ..Default::default()
            },
        );
        assert_eq!(
            encode_key(&shift_up, TermMode::default(), false),
            Some(b"\x1b[1;2A".to_vec())
        );
    }

    #[test]
    fn platform_modifier_is_consistent_in_legacy_and_kitty_paths() {
        let command_up = keystroke(
            "up",
            None,
            Modifiers {
                platform: true,
                ..Default::default()
            },
        );
        assert_eq!(
            encode_key(&command_up, TermMode::default(), false),
            Some(b"\x1b[1;9A".to_vec())
        );
        let command_x = keystroke(
            "x",
            Some("x"),
            Modifiers {
                platform: true,
                ..Default::default()
            },
        );
        assert_eq!(
            encode_key(&command_x, TermMode::DISAMBIGUATE_ESC_CODES, false),
            Some(b"\x1b[120;9u".to_vec())
        );
    }

    #[test]
    fn plain_text_uses_key_char() {
        let a = keystroke("a", Some("a"), Modifiers::default());
        assert_eq!(
            encode_key(&a, TermMode::default(), false),
            Some(b"a".to_vec())
        );
    }

    #[test]
    fn legacy_sequences_unchanged_without_kitty_mode() {
        let ctrl_c = keystroke(
            "c",
            Some("c"),
            Modifiers {
                control: true,
                ..Default::default()
            },
        );
        assert_eq!(
            encode_key(&ctrl_c, TermMode::default(), false),
            Some(vec![0x03])
        );
        let escape = keystroke("escape", None, Modifiers::default());
        assert_eq!(
            encode_key(&escape, TermMode::default(), false),
            Some(b"\x1b".to_vec())
        );
        let shift_enter = keystroke(
            "enter",
            None,
            Modifiers {
                shift: true,
                ..Default::default()
            },
        );
        assert_eq!(
            encode_key(&shift_enter, TermMode::default(), false),
            Some(b"\x0a".to_vec())
        );
    }

    #[test]
    fn kitty_disambiguates_functional_keys() {
        let mode = TermMode::DISAMBIGUATE_ESC_CODES;
        let escape = keystroke("escape", None, Modifiers::default());
        assert_eq!(encode_key(&escape, mode, false), Some(b"\x1b[27u".to_vec()));
        let enter = keystroke("enter", None, Modifiers::default());
        assert_eq!(encode_key(&enter, mode, false), Some(b"\x1b[13u".to_vec()));
        let shift_tab = keystroke(
            "tab",
            None,
            Modifiers {
                shift: true,
                ..Default::default()
            },
        );
        assert_eq!(
            encode_key(&shift_tab, mode, false),
            Some(b"\x1b[9;2u".to_vec())
        );
        let ctrl_backspace = keystroke(
            "backspace",
            None,
            Modifiers {
                control: true,
                ..Default::default()
            },
        );
        assert_eq!(
            encode_key(&ctrl_backspace, mode, false),
            Some(b"\x1b[127;5u".to_vec())
        );
    }

    #[test]
    fn kitty_encodes_modified_text_keys_and_keeps_plain_text() {
        let mode = TermMode::DISAMBIGUATE_ESC_CODES;
        let ctrl_c = keystroke(
            "c",
            Some("c"),
            Modifiers {
                control: true,
                ..Default::default()
            },
        );
        assert_eq!(
            encode_key(&ctrl_c, mode, false),
            Some(b"\x1b[99;5u".to_vec()),
            "an application that asked for disambiguation gets the kitty form of Control+C"
        );
        let alt_x = keystroke(
            "x",
            Some("x"),
            Modifiers {
                alt: true,
                ..Default::default()
            },
        );
        assert_eq!(
            encode_key(&alt_x, mode, false),
            Some(b"\x1b[120;3u".to_vec())
        );
        let plain = keystroke("a", Some("a"), Modifiers::default());
        assert_eq!(
            encode_key(&plain, mode, false),
            Some(b"a".to_vec()),
            "unmodified text keys still send UTF-8"
        );
        let shift_arrow = keystroke(
            "up",
            None,
            Modifiers {
                shift: true,
                ..Default::default()
            },
        );
        assert_eq!(
            encode_key(&shift_arrow, mode, false),
            Some(b"\x1b[1;2A".to_vec()),
            "arrow keys keep CSI 1;mod X"
        );
    }

    #[test]
    fn kitty_uses_private_use_codes_for_the_first_function_keys() {
        let mode = TermMode::DISAMBIGUATE_ESC_CODES;
        for (key, code) in [("f1", 57364), ("f2", 57365), ("f3", 57366), ("f4", 57367)] {
            let press = keystroke(key, None, Modifiers::default());
            assert_eq!(
                encode_key(&press, mode, false),
                Some(format!("\x1b[{code}~").into_bytes()),
                "{key}"
            );
            let ctrl = keystroke(
                key,
                None,
                Modifiers {
                    control: true,
                    ..Default::default()
                },
            );
            assert_eq!(
                encode_key(&ctrl, mode, false),
                Some(format!("\x1b[{code};5~").into_bytes()),
                "{key} with a modifier"
            );
        }
        // Legacy terminals keep the SS3 form for the same keys.
        let f1 = keystroke("f1", None, Modifiers::default());
        assert_eq!(
            encode_key(&f1, TermMode::default(), false),
            Some(b"\x1bOP".to_vec())
        );
    }

    #[test]
    fn kitty_report_all_keys_as_escape() {
        let mode = TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_ALL_KEYS_AS_ESC;
        let plain = keystroke("a", Some("a"), Modifiers::default());
        assert_eq!(encode_key(&plain, mode, false), Some(b"\x1b[97u".to_vec()));
    }

    #[test]
    fn terminal_shortcuts_use_the_platform_key_and_leave_ctrl_c_for_the_shell() {
        let ctrl_c = keystroke(
            "c",
            Some("c"),
            Modifiers {
                control: true,
                ..Default::default()
            },
        );
        assert_eq!(terminal_shortcut(&ctrl_c), None);
        assert_eq!(
            encode_key(&ctrl_c, TermMode::default(), false),
            Some(vec![0x03])
        );

        // XKB folds Shift into the keysym, so Control+Shift+C and Control+C print the same
        // upper-case shape when the modifier state is lost. The plain request stays ETX.
        let xkb_folded_ctrl_c = keystroke(
            "c",
            Some("C"),
            Modifiers {
                control: true,
                ..Default::default()
            },
        );
        assert_eq!(terminal_shortcut(&xkb_folded_ctrl_c), None);
        assert_eq!(
            encode_key(&xkb_folded_ctrl_c, TermMode::default(), false),
            Some(vec![0x03]),
            "an upper-case character alone must not steal the interrupt"
        );

        for (key, shortcut) in [
            ("c", TerminalShortcut::Copy),
            ("v", TerminalShortcut::Paste),
            ("a", TerminalShortcut::SelectAll),
            ("f", TerminalShortcut::Find),
        ] {
            let platform = keystroke(
                key,
                Some(key),
                Modifiers {
                    platform: true,
                    ..Default::default()
                },
            );
            assert_eq!(terminal_shortcut(&platform), Some(shortcut), "{key}");
        }
        let platform_c = keystroke(
            "c",
            Some("c"),
            Modifiers {
                platform: true,
                ..Default::default()
            },
        );
        assert_eq!(
            encode_key(&platform_c, TermMode::default(), false),
            Some(b"c".to_vec()),
            "the platform shortcut never becomes a C0 control byte"
        );

        let ctrl_v = keystroke(
            "v",
            Some("v"),
            Modifiers {
                control: true,
                ..Default::default()
            },
        );
        assert_eq!(terminal_shortcut(&ctrl_v), None);
        assert_eq!(
            encode_key(&ctrl_v, TermMode::default(), false),
            Some(vec![0x16])
        );
        let shift_insert = keystroke(
            "insert",
            None,
            Modifiers {
                shift: true,
                ..Default::default()
            },
        );
        assert_eq!(
            terminal_shortcut(&shift_insert),
            Some(TerminalShortcut::Paste)
        );
        let ctrl_alt_c = keystroke(
            "c",
            Some("c"),
            Modifiers {
                control: true,
                alt: true,
                ..Default::default()
            },
        );
        assert_eq!(terminal_shortcut(&ctrl_alt_c), None);
        let ctrl_altgr_c = keystroke(
            "c",
            Some("C"),
            Modifiers {
                control: true,
                alt: true,
                ..Default::default()
            },
        );
        assert_eq!(
            terminal_shortcut(&ctrl_altgr_c),
            None,
            "the AltGr third level prints a character but is not a shortcut"
        );
    }

    /// The Linux terminal convention: Control+Shift+letter edits locally.
    #[test]
    fn ctrl_shift_letters_reach_the_local_editing_actions() {
        for (key, shortcut) in [
            ("c", TerminalShortcut::Copy),
            ("v", TerminalShortcut::Paste),
            ("a", TerminalShortcut::SelectAll),
            ("f", TerminalShortcut::Find),
            ("g", TerminalShortcut::FindNext),
        ] {
            // The modifier state is the signal. A backend that reports a lower-case key and no
            // character still resolves the chord.
            let plain = keystroke(key, None, Modifiers::control_shift());
            assert_eq!(terminal_shortcut(&plain), Some(shortcut), "flag {key}");
            // XKB folds Shift into the keysym, so the character is the upper-case form.
            let shifted = keystroke(key, Some(&upper(key)), Modifiers::control_shift());
            assert_eq!(terminal_shortcut(&shifted), Some(shortcut), "shifted {key}");
            // A keymap binding spells the chord with the upper-case key, so that form matches too.
            let upper_key = keystroke(&upper(key), None, Modifiers::control_shift());
            assert_eq!(terminal_shortcut(&upper_key), Some(shortcut), "upper {key}");
            // A bigger Shift step is still the same chord.
            let shift_step = keystroke(
                key,
                Some(&upper(key)),
                Modifiers {
                    control: true,
                    shift: true,
                    function: true,
                    ..Default::default()
                },
            );
            assert_eq!(terminal_shortcut(&shift_step), None, "fn {key}");
        }
    }

    /// CapsLock prints an upper-case character with the Shift flag clear. Trusting the character
    /// would take the interrupt away from every CapsLock user.
    #[test]
    fn a_capslock_control_letter_is_not_a_shortcut() {
        for key in ["c", "v", "a", "f", "g"] {
            let caps = keystroke(key, Some(&upper(key)), Modifiers::control());
            assert_eq!(terminal_shortcut(&caps), None, "{key}");
        }
    }

    #[test]
    fn ctrl_shift_chords_never_capture_a_bare_control_letter() {
        // Control+C keeps reaching the child process whatever the character says.
        for key_char in [None, Some("c"), Some("C")] {
            let ctrl_c = keystroke("c", key_char, Modifiers::control());
            assert_eq!(terminal_shortcut(&ctrl_c), None, "{key_char:?}");
            assert_eq!(
                encode_key(&ctrl_c, TermMode::default(), false),
                Some(vec![0x03]),
                "{key_char:?} stays ETX"
            );
        }
        // Every other bare Control+letter stays with the child process too. X is in this list
        // even though Control+Shift+X is a shortcut, because the bare chord is not.
        for (key, byte) in [
            ("v", 0x16),
            ("a", 0x01),
            ("f", 0x06),
            ("g", 0x07),
            ("x", 0x18),
        ] {
            let plain = keystroke(key, Some(key), Modifiers::control());
            assert_eq!(terminal_shortcut(&plain), None, "{key}");
            assert_eq!(
                encode_key(&plain, TermMode::default(), false),
                Some(vec![byte]),
                "{key} stays with the child"
            );
        }
        // Control+Shift+X cuts from the scrollback, joining the copy and paste chords beside it.
        let ctrl_shift_x = keystroke("x", None, Modifiers::control_shift());
        assert_eq!(
            terminal_shortcut(&ctrl_shift_x),
            Some(TerminalShortcut::Cut)
        );
        // An unbound Control+Shift+letter still belongs to the child process.
        let ctrl_shift_z = keystroke("z", None, Modifiers::control_shift());
        assert_eq!(terminal_shortcut(&ctrl_shift_z), None);
        assert_eq!(
            encode_key(&ctrl_shift_z, TermMode::default(), false),
            Some(vec![0x1a])
        );
        // Digits, punctuation, and named keys are not letters, so the shortcut table ignores them.
        for key in ["2", "insert", "f13", "up", "/"] {
            let press = keystroke(key, None, Modifiers::control_shift());
            assert_eq!(terminal_shortcut(&press), None, "{key}");
        }
        // Alt keeps the AltGr third level and Alt chords off the shortcut table.
        let alt = keystroke(
            "c",
            Some("C"),
            Modifiers {
                control: true,
                shift: true,
                alt: true,
                ..Default::default()
            },
        );
        assert_eq!(terminal_shortcut(&alt), None);
        let super_shift = keystroke(
            "c",
            Some("C"),
            Modifiers {
                shift: true,
                platform: true,
                ..Default::default()
            },
        );
        assert_eq!(terminal_shortcut(&super_shift), None);
        // Shift+Insert still pastes, and Control+Shift+Insert is not a letter chord.
        let ctrl_shift_insert = keystroke("insert", None, Modifiers::control_shift());
        assert_eq!(terminal_shortcut(&ctrl_shift_insert), None);
    }

    #[test]
    fn altgr_char_is_sent_as_text_even_with_kitty_mode() {
        let altgr = keystroke(
            "2",
            Some("@"),
            Modifiers {
                control: true,
                alt: true,
                ..Default::default()
            },
        );
        assert_eq!(
            encode_key(&altgr, TermMode::default(), false),
            Some(b"@".to_vec())
        );
        assert_eq!(
            encode_key(&altgr, TermMode::DISAMBIGUATE_ESC_CODES, false),
            Some(b"@".to_vec())
        );
        assert_eq!(terminal_shortcut(&altgr), None);

        // Some layouts also report the platform modifier for the third level.
        let altgr_with_platform = Keystroke {
            modifiers: Modifiers {
                control: true,
                alt: true,
                platform: true,
                ..Default::default()
            },
            ..altgr.clone()
        };
        assert_eq!(
            encode_key(&altgr_with_platform, TermMode::default(), false),
            Some(b"@".to_vec())
        );

        // Control+Alt without a printable character is a shortcut, not a character.
        let control_alt_delete = Keystroke {
            key: "delete".to_owned(),
            key_char: None,
            ..altgr
        };
        assert_eq!(
            encode_key(&control_alt_delete, TermMode::default(), false),
            None
        );
    }

    /// A text field accepts the AltGr third level, and only that Control combination.
    #[test]
    fn alt_gr_characters_are_typed_text() {
        let altgr = keystroke(
            "2",
            Some("@"),
            Modifiers {
                control: true,
                alt: true,
                ..Default::default()
            },
        );
        assert!(is_printed_text(&altgr));
        let altgr_euro = keystroke(
            "e",
            Some("€"),
            Modifiers {
                control: true,
                alt: true,
                ..Default::default()
            },
        );
        assert!(is_printed_text(&altgr_euro));
        // A layout with no third level for the letter still prints the letter itself.
        let altgr_a = keystroke(
            "a",
            Some("a"),
            Modifiers {
                control: true,
                alt: true,
                ..Default::default()
            },
        );
        assert!(is_printed_text(&altgr_a));
        assert_eq!(
            encode_key(&altgr_a, TermMode::default(), false),
            Some(b"a".to_vec()),
            "AltGr+a sends the text a"
        );
        assert!(!is_printed_text(&keystroke(
            "a",
            Some("a"),
            Modifiers::control()
        )));
        // GPUI reports Super and Command as the platform modifier on every platform, so a
        // platform chord is not text even when the toolkit prints a character for it.
        let platform_chord = keystroke("a", Some("a"), Modifiers::super_key());
        assert!(!is_printed_text(&platform_chord));
        let control_alt = keystroke(
            "delete",
            None,
            Modifiers {
                control: true,
                alt: true,
                ..Default::default()
            },
        );
        assert!(!is_printed_text(&control_alt));
    }

    #[test]
    fn ime_composition_owns_commit_cancel_caret_and_typed_keys() {
        let composing = true;
        for key in [
            "enter",
            "return",
            "escape",
            "up",
            "arrowup",
            "down",
            "arrowdown",
            "left",
            "arrowleft",
            "right",
            "arrowright",
        ] {
            let press = keystroke(key, None, Modifiers::default());
            assert!(ime_owns_key(composing, &press), "{key}");
            assert!(!ime_owns_key(false, &press), "{key} without a preview");
        }

        let typed = keystroke("a", Some("a"), Modifiers::default());
        assert!(ime_owns_key(composing, &typed));
        let shifted = keystroke(
            "a",
            Some("A"),
            Modifiers {
                shift: true,
                ..Default::default()
            },
        );
        assert!(ime_owns_key(composing, &shifted));
        let altgr = keystroke(
            "2",
            Some("@"),
            Modifiers {
                control: true,
                alt: true,
                ..Default::default()
            },
        );
        assert!(ime_owns_key(composing, &altgr));

        // Control keys stay available for the child process during a preview.
        let ctrl_c = keystroke(
            "c",
            None,
            Modifiers {
                control: true,
                ..Default::default()
            },
        );
        assert!(!ime_owns_key(composing, &ctrl_c));
        let page_up = keystroke(
            "pageup",
            None,
            Modifiers {
                shift: true,
                ..Default::default()
            },
        );
        assert!(!ime_owns_key(composing, &page_up));
        assert!(!ime_owns_key(false, &typed));
    }

    #[test]
    fn kitty_reports_event_type_and_associated_text() {
        let mode = TermMode::REPORT_ALL_KEYS_AS_ESC
            | TermMode::REPORT_EVENT_TYPES
            | TermMode::REPORT_ASSOCIATED_TEXT;
        let shifted = keystroke(
            "a",
            Some("A"),
            Modifiers {
                shift: true,
                ..Default::default()
            },
        );
        assert_eq!(
            encode_key(&shifted, mode, false),
            Some(b"\x1b[97;2:1;65u".to_vec())
        );
        let plain = keystroke("a", Some("a"), Modifiers::default());
        assert_eq!(
            encode_key(&plain, mode, false),
            Some(b"\x1b[97;1:1;97u".to_vec()),
            "an unmodified key still reports the modifier field before the event type"
        );
    }

    #[test]
    fn kitty_encodes_extended_function_keys() {
        let mode = TermMode::DISAMBIGUATE_ESC_CODES;
        let f13 = keystroke("f13", None, Modifiers::default());
        assert_eq!(encode_key(&f13, mode, false), Some(b"\x1b[57376u".to_vec()));
        let f24 = keystroke(
            "f24",
            None,
            Modifiers {
                shift: true,
                ..Default::default()
            },
        );
        assert_eq!(
            encode_key(&f24, mode, false),
            Some(b"\x1b[57387;2u".to_vec())
        );
    }
}
