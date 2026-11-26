//! `computer`-aligned keyboard and mouse wire encoding for terminal applications.

use std::fmt::Write as _;

use thiserror::Error;

use super::emulator::{MouseTrackingMode, TerminalModes};

/// Maximum wheel impulses emitted for a single scroll delta axis so large
/// pixel-style deltas cannot flood a PTY.
const MAX_SCROLL_STEPS: u32 = 20;

/// Maximum 1-based coordinate encodable in legacy X10/normal mouse reports
/// (`32 + 223 = 255`).
const MAX_LEGACY_MOUSE_COORD: u16 = 223;

/// Errors returned when parsing or encoding TUI key and mouse inputs.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TuiInputError {
    /// Key specification was empty or ended with a trailing `+`.
    #[error("`tui key {spec}` names no key; expected `KEY` or `MOD+KEY`")]
    MissingKey {
        /// Original key specification string.
        spec: String,
    },
    /// Modifier name in `MOD+KEY` is unrecognized.
    #[error(
        "unknown modifier `{name}`; valid: cmd (command, meta), ctrl (control), alt (option), shift"
    )]
    UnknownModifier {
        /// Invalid modifier token.
        name: String,
    },
    /// Key name is not a recognized W3C `KeyboardEvent.code`, single letter, or digit.
    #[error(
        "unknown key `{key}`; expected a W3C KeyboardEvent.code (Enter, Tab, Escape, Backspace, Delete, Insert, Space, ArrowUp/Down/Left/Right, Home, End, PageUp, PageDown, F1..F12) or a single ASCII letter or digit"
    )]
    UnknownKey {
        /// Unrecognized key token.
        key: String,
    },
    /// Mouse action attempted while the TUI has not enabled mouse tracking.
    #[error(
        "mouse action refused: this program has not enabled terminal mouse tracking (?1000/?1002/?1003); navigate with `tui key` instead"
    )]
    MouseNotEnabled,
    /// Scroll action had zero delta on both axes.
    #[error("scroll delta must be non-zero on at least one axis")]
    ZeroScroll,
}

/// Modifier keys held during a key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    /// Shift modifier.
    pub shift: bool,
    /// Alt / Option modifier.
    pub alt: bool,
    /// Control modifier.
    pub ctrl: bool,
    /// Meta / Command / Super modifier.
    pub meta: bool,
}

impl Modifiers {
    /// Whether no modifier is held.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        !(self.shift || self.alt || self.ctrl || self.meta)
    }

    /// Standard xterm / Kitty modifier parameter (`1 + bitmask`).
    #[must_use]
    pub const fn csi_param(self) -> u8 {
        let mut bits = 0_u8;
        if self.shift {
            bits |= 1;
        }
        if self.alt {
            bits |= 2;
        }
        if self.ctrl {
            bits |= 4;
        }
        if self.meta {
            bits |= 8;
        }
        1 + bits
    }
}

/// Parsed key press aligned with `computer`'s `KEY` / `MOD+KEY` syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPress {
    /// Canonical W3C `KeyboardEvent.code` name (e.g. `KeyL`, `Digit4`, `Enter`, `ArrowUp`).
    pub code: String,
    /// Modifiers held alongside the key.
    pub modifiers: Modifiers,
}

impl KeyPress {
    /// Parses `KEY` or `MOD+MOD+KEY`; modifier names are case-insensitive.
    ///
    /// # Errors
    /// Returns [`TuiInputError::MissingKey`] or [`TuiInputError::UnknownModifier`]
    /// when `spec` is malformed.
    pub fn parse(spec: &str) -> Result<Self, TuiInputError> {
        let mut parts: Vec<&str> = spec.split('+').collect();
        let Some(raw_key) = parts.pop().filter(|k| !k.is_empty()) else {
            return Err(TuiInputError::MissingKey {
                spec: spec.to_owned(),
            });
        };
        let mut modifiers = Modifiers::default();
        for name in parts {
            match name.to_ascii_lowercase().as_str() {
                "cmd" | "command" | "meta" => modifiers.meta = true,
                "ctrl" | "control" => modifiers.ctrl = true,
                "alt" | "option" => modifiers.alt = true,
                "shift" => modifiers.shift = true,
                _ => {
                    return Err(TuiInputError::UnknownModifier {
                        name: name.to_owned(),
                    });
                }
            }
        }
        Ok(Self {
            code: canonical_key_code(raw_key),
            modifiers,
        })
    }
}

fn canonical_key_code(key: &str) -> String {
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => format!("Key{}", c.to_ascii_uppercase()),
        (Some(c), None) if c.is_ascii_digit() => format!("Digit{c}"),
        _ => key.to_owned(),
    }
}

#[derive(Debug, Clone, Copy)]
enum KeyKind {
    Enter,
    Tab,
    Escape,
    Backspace,
    Space,
    Cursor(u8),
    HomeEnd(u8),
    Tilde(u8),
    FnSs3(u8),
    Letter(u8),
    Printable { unshifted: u8, shifted: u8 },
}

fn classify_code(code: &str) -> Option<KeyKind> {
    if let Some(rest) = code.strip_prefix("Key")
        && rest.len() == 1
    {
        let byte = rest.as_bytes()[0];
        if byte.is_ascii_uppercase() {
            return Some(KeyKind::Letter(byte.to_ascii_lowercase()));
        }
    }
    if let Some(rest) = code.strip_prefix("Digit")
        && rest.len() == 1
    {
        let byte = rest.as_bytes()[0];
        if byte.is_ascii_digit() {
            let shifted = match byte {
                b'0' => b')',
                b'1' => b'!',
                b'2' => b'@',
                b'3' => b'#',
                b'4' => b'$',
                b'5' => b'%',
                b'6' => b'^',
                b'7' => b'&',
                b'8' => b'*',
                b'9' => b'(',
                _ => byte,
            };
            return Some(KeyKind::Printable {
                unshifted: byte,
                shifted,
            });
        }
    }
    match code {
        "Enter" | "NumpadEnter" => Some(KeyKind::Enter),
        "Tab" => Some(KeyKind::Tab),
        "Escape" => Some(KeyKind::Escape),
        "Backspace" => Some(KeyKind::Backspace),
        "Space" => Some(KeyKind::Space),
        "ArrowUp" => Some(KeyKind::Cursor(b'A')),
        "ArrowDown" => Some(KeyKind::Cursor(b'B')),
        "ArrowRight" => Some(KeyKind::Cursor(b'C')),
        "ArrowLeft" => Some(KeyKind::Cursor(b'D')),
        "Home" => Some(KeyKind::HomeEnd(b'H')),
        "End" => Some(KeyKind::HomeEnd(b'F')),
        "Insert" => Some(KeyKind::Tilde(2)),
        "Delete" => Some(KeyKind::Tilde(3)),
        "PageUp" => Some(KeyKind::Tilde(5)),
        "PageDown" => Some(KeyKind::Tilde(6)),
        "F1" => Some(KeyKind::FnSs3(b'P')),
        "F2" => Some(KeyKind::FnSs3(b'Q')),
        "F3" => Some(KeyKind::FnSs3(b'R')),
        "F4" => Some(KeyKind::FnSs3(b'S')),
        "F5" => Some(KeyKind::Tilde(15)),
        "F6" => Some(KeyKind::Tilde(17)),
        "F7" => Some(KeyKind::Tilde(18)),
        "F8" => Some(KeyKind::Tilde(19)),
        "F9" => Some(KeyKind::Tilde(20)),
        "F10" => Some(KeyKind::Tilde(21)),
        "F11" => Some(KeyKind::Tilde(23)),
        "F12" => Some(KeyKind::Tilde(24)),
        "Minus" => Some(KeyKind::Printable {
            unshifted: b'-',
            shifted: b'_',
        }),
        "Equal" => Some(KeyKind::Printable {
            unshifted: b'=',
            shifted: b'+',
        }),
        "BracketLeft" => Some(KeyKind::Printable {
            unshifted: b'[',
            shifted: b'{',
        }),
        "BracketRight" => Some(KeyKind::Printable {
            unshifted: b']',
            shifted: b'}',
        }),
        "Backslash" => Some(KeyKind::Printable {
            unshifted: b'\\',
            shifted: b'|',
        }),
        "Semicolon" => Some(KeyKind::Printable {
            unshifted: b';',
            shifted: b':',
        }),
        "Quote" => Some(KeyKind::Printable {
            unshifted: b'\'',
            shifted: b'"',
        }),
        "Backquote" => Some(KeyKind::Printable {
            unshifted: b'`',
            shifted: b'~',
        }),
        "Comma" => Some(KeyKind::Printable {
            unshifted: b',',
            shifted: b'<',
        }),
        "Period" => Some(KeyKind::Printable {
            unshifted: b'.',
            shifted: b'>',
        }),
        "Slash" => Some(KeyKind::Printable {
            unshifted: b'/',
            shifted: b'?',
        }),
        _ => None,
    }
}

/// Encodes a `computer`-style key specification (`KEY` or `MOD+KEY`) into PTY
/// input bytes honoring DECCKM (`?1`) and Kitty keyboard protocol flags.
///
/// # Errors
/// Returns [`TuiInputError`] if the specification has an invalid modifier or
/// unknown key name.
pub fn encode_key(spec: &str, modes: &TerminalModes) -> Result<Vec<u8>, TuiInputError> {
    let press = KeyPress::parse(spec)?;
    let Some(kind) = classify_code(&press.code) else {
        return Err(TuiInputError::UnknownKey { key: press.code });
    };
    let kitty_disambiguate = (modes.kitty_keyboard_flags & 1) != 0;
    let modifiers = press.modifiers;
    let csi_mod = modifiers.csi_param();

    if kitty_disambiguate {
        return Ok(encode_key_kitty(kind, modifiers, csi_mod));
    }
    Ok(encode_key_legacy(
        kind,
        modifiers,
        csi_mod,
        modes.cursor_keys_application,
    ))
}

fn encode_csi_u(codepoint: u32, csi_mod: u8) -> Vec<u8> {
    if csi_mod > 1 {
        format!("\x1b[{codepoint};{csi_mod}u").into_bytes()
    } else {
        format!("\x1b[{codepoint}u").into_bytes()
    }
}

fn encode_key_kitty(kind: KeyKind, mods: Modifiers, csi_mod: u8) -> Vec<u8> {
    match kind {
        KeyKind::Escape => encode_csi_u(27, csi_mod),
        KeyKind::Enter => {
            if mods.is_empty() {
                vec![b'\r']
            } else {
                encode_csi_u(13, csi_mod)
            }
        }
        KeyKind::Tab => {
            if mods.is_empty() {
                vec![b'\t']
            } else {
                encode_csi_u(9, csi_mod)
            }
        }
        KeyKind::Backspace => {
            if mods.is_empty() {
                vec![0x7F]
            } else {
                encode_csi_u(127, csi_mod)
            }
        }
        KeyKind::Space => {
            if mods.is_empty() {
                vec![b' ']
            } else {
                encode_csi_u(32, csi_mod)
            }
        }
        KeyKind::Cursor(final_byte) | KeyKind::HomeEnd(final_byte) => {
            if csi_mod > 1 {
                format!("\x1b[1;{csi_mod}{}", char::from(final_byte)).into_bytes()
            } else {
                vec![0x1B, b'[', final_byte]
            }
        }
        KeyKind::FnSs3(final_byte) => {
            if csi_mod > 1 {
                format!("\x1b[1;{csi_mod}{}", char::from(final_byte)).into_bytes()
            } else {
                vec![0x1B, b'O', final_byte]
            }
        }
        KeyKind::Tilde(num) => {
            if csi_mod > 1 {
                format!("\x1b[{num};{csi_mod}~").into_bytes()
            } else {
                format!("\x1b[{num}~").into_bytes()
            }
        }
        KeyKind::Letter(lower) => {
            if mods.ctrl || mods.alt || mods.meta {
                encode_csi_u(u32::from(lower), csi_mod)
            } else if mods.shift {
                vec![lower.to_ascii_uppercase()]
            } else {
                vec![lower]
            }
        }
        KeyKind::Printable { unshifted, shifted } => {
            if mods.ctrl || mods.alt || mods.meta {
                encode_csi_u(u32::from(unshifted), csi_mod)
            } else if mods.shift {
                vec![shifted]
            } else {
                vec![unshifted]
            }
        }
    }
}

fn encode_key_legacy(kind: KeyKind, mods: Modifiers, csi_mod: u8, decckm: bool) -> Vec<u8> {
    match kind {
        KeyKind::Enter => {
            if mods.alt {
                vec![0x1B, b'\r']
            } else {
                vec![b'\r']
            }
        }
        KeyKind::Tab => {
            if mods.shift && !mods.ctrl && !mods.alt {
                b"\x1b[Z".to_vec()
            } else if mods.alt {
                vec![0x1B, b'\t']
            } else {
                vec![b'\t']
            }
        }
        KeyKind::Escape => vec![0x1B],
        KeyKind::Backspace => {
            let base = if mods.ctrl { 0x08 } else { 0x7F };
            if mods.alt {
                vec![0x1B, base]
            } else {
                vec![base]
            }
        }
        KeyKind::Space => {
            let base = if mods.ctrl { 0x00 } else { b' ' };
            if mods.alt {
                vec![0x1B, base]
            } else {
                vec![base]
            }
        }
        KeyKind::Cursor(final_byte) | KeyKind::HomeEnd(final_byte) => {
            if csi_mod > 1 {
                format!("\x1b[1;{csi_mod}{}", char::from(final_byte)).into_bytes()
            } else if decckm {
                vec![0x1B, b'O', final_byte]
            } else {
                vec![0x1B, b'[', final_byte]
            }
        }
        KeyKind::FnSs3(final_byte) => {
            if csi_mod > 1 {
                format!("\x1b[1;{csi_mod}{}", char::from(final_byte)).into_bytes()
            } else {
                vec![0x1B, b'O', final_byte]
            }
        }
        KeyKind::Tilde(num) => {
            if csi_mod > 1 {
                format!("\x1b[{num};{csi_mod}~").into_bytes()
            } else {
                format!("\x1b[{num}~").into_bytes()
            }
        }
        KeyKind::Letter(lower) => {
            let mut out = Vec::with_capacity(2);
            if mods.alt {
                out.push(0x1B);
            }
            if mods.ctrl {
                out.push((lower - b'a') + 1);
            } else if mods.shift {
                out.push(lower.to_ascii_uppercase());
            } else {
                out.push(lower);
            }
            out
        }
        KeyKind::Printable { unshifted, shifted } => {
            let mut out = Vec::with_capacity(2);
            if mods.alt {
                out.push(0x1B);
            }
            if mods.ctrl {
                let ctrl_byte = match unshifted {
                    b'2' => 0x00,
                    b'[' => 0x1B,
                    b'\\' => 0x1C,
                    b']' => 0x1D,
                    b'6' => 0x1E,
                    b'-' | b'/' => 0x1F,
                    other => other,
                };
                out.push(ctrl_byte);
            } else if mods.shift {
                out.push(shifted);
            } else {
                out.push(unshifted);
            }
            out
        }
    }
}

/// Encodes literal text for PTY input, wrapping in bracketed paste markers
/// (`\x1b[200~...\x1b[201~`) when `?2004` is active and sanitizing any embedded
/// end-of-paste marker.
#[must_use]
pub fn encode_type(text: &str, bracketed_paste: bool) -> Vec<u8> {
    if !bracketed_paste {
        return text.as_bytes().to_vec();
    }
    let sanitized = text.replace("\x1b[201~", "");
    let mut out = Vec::with_capacity(sanitized.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(sanitized.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

const fn require_mouse(modes: &TerminalModes) -> Result<(), TuiInputError> {
    if modes.is_mouse_enabled() {
        Ok(())
    } else {
        Err(TuiInputError::MouseNotEnabled)
    }
}

fn append_mouse_event(
    out: &mut Vec<u8>,
    cb: u8,
    col_0based: u16,
    row_0based: u16,
    released: bool,
    modes: &TerminalModes,
) {
    let cx = col_0based.saturating_add(1);
    let cy = row_0based.saturating_add(1);
    if modes.mouse_sgr {
        let suffix = if released { 'm' } else { 'M' };
        let mut buf = String::new();
        let _ = write!(buf, "\x1b[<{cb};{cx};{cy}{suffix}");
        out.extend_from_slice(buf.as_bytes());
    } else {
        let wire_cb = if released { 3 } else { cb };
        let wire_x = u8::try_from(cx.min(MAX_LEGACY_MOUSE_COORD)).unwrap_or(223);
        let wire_y = u8::try_from(cy.min(MAX_LEGACY_MOUSE_COORD)).unwrap_or(223);
        out.extend_from_slice(&[
            0x1B,
            b'[',
            b'M',
            32_u8.saturating_add(wire_cb),
            32_u8.saturating_add(wire_x),
            32_u8.saturating_add(wire_y),
        ]);
    }
}

/// Encodes a left mouse click (press + release) at 0-based `(col, row)`.
///
/// # Errors
/// Returns [`TuiInputError::MouseNotEnabled`] when mouse tracking is off.
pub fn encode_mouse_click(
    col: u16,
    row: u16,
    modes: &TerminalModes,
) -> Result<Vec<u8>, TuiInputError> {
    require_mouse(modes)?;
    let mut out = Vec::new();
    append_mouse_event(&mut out, 0, col, row, false, modes);
    append_mouse_event(&mut out, 0, col, row, true, modes);
    Ok(out)
}

/// Encodes a left double-click (two press + release pairs) at 0-based `(col, row)`.
///
/// # Errors
/// Returns [`TuiInputError::MouseNotEnabled`] when mouse tracking is off.
pub fn encode_mouse_double_click(
    col: u16,
    row: u16,
    modes: &TerminalModes,
) -> Result<Vec<u8>, TuiInputError> {
    require_mouse(modes)?;
    let mut out = Vec::new();
    for _ in 0..2 {
        append_mouse_event(&mut out, 0, col, row, false, modes);
        append_mouse_event(&mut out, 0, col, row, true, modes);
    }
    Ok(out)
}

/// Encodes a right mouse click (press + release) at 0-based `(col, row)`.
///
/// # Errors
/// Returns [`TuiInputError::MouseNotEnabled`] when mouse tracking is off.
pub fn encode_mouse_right_click(
    col: u16,
    row: u16,
    modes: &TerminalModes,
) -> Result<Vec<u8>, TuiInputError> {
    require_mouse(modes)?;
    let mut out = Vec::new();
    append_mouse_event(&mut out, 2, col, row, false, modes);
    append_mouse_event(&mut out, 2, col, row, true, modes);
    Ok(out)
}

/// Encodes a left-button mouse drag from 0-based `from` to `to`.
///
/// Emits intermediate motion events (`cb = 32`) when button-event (`?1002`) or
/// any-event (`?1003`) tracking is active.
///
/// # Errors
/// Returns [`TuiInputError::MouseNotEnabled`] when mouse tracking is off.
pub fn encode_mouse_drag(
    from: (u16, u16),
    to: (u16, u16),
    modes: &TerminalModes,
) -> Result<Vec<u8>, TuiInputError> {
    require_mouse(modes)?;
    let mut out = Vec::new();
    append_mouse_event(&mut out, 0, from.0, from.1, false, modes);
    if matches!(
        modes.mouse_tracking,
        MouseTrackingMode::ButtonEvent | MouseTrackingMode::AnyEvent
    ) {
        let mid = (from.0.midpoint(to.0), from.1.midpoint(to.1));
        if mid != from && mid != to {
            append_mouse_event(&mut out, 32, mid.0, mid.1, false, modes);
        }
        append_mouse_event(&mut out, 32, to.0, to.1, false, modes);
    }
    append_mouse_event(&mut out, 0, to.0, to.1, true, modes);
    Ok(out)
}

/// Encodes mouse wheel scroll impulses at 0-based `(col, row)`.
///
/// Positive `dy` scrolls down (`cb = 65`), negative `dy` scrolls up (`cb = 64`);
/// positive `dx` scrolls right (`cb = 67`), negative `dx` scrolls left (`cb = 66`).
///
/// # Errors
/// Returns [`TuiInputError::MouseNotEnabled`] when mouse tracking is off, or
/// [`TuiInputError::ZeroScroll`] when both `dx` and `dy` are zero.
pub fn encode_mouse_scroll(
    col: u16,
    row: u16,
    dx: i32,
    dy: i32,
    modes: &TerminalModes,
) -> Result<Vec<u8>, TuiInputError> {
    require_mouse(modes)?;
    if dx == 0 && dy == 0 {
        return Err(TuiInputError::ZeroScroll);
    }
    let mut out = Vec::new();
    if dy != 0 {
        let cb = if dy > 0 { 65 } else { 64 };
        let steps = dy.unsigned_abs().min(MAX_SCROLL_STEPS);
        for _ in 0..steps {
            append_mouse_event(&mut out, cb, col, row, false, modes);
        }
    }
    if dx != 0 {
        let cb = if dx > 0 { 67 } else { 66 };
        let steps = dx.unsigned_abs().min(MAX_SCROLL_STEPS);
        for _ in 0..steps {
            append_mouse_event(&mut out, cb, col, row, false, modes);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_legacy_decckm_and_kitty_keys() {
        let mut modes = TerminalModes::default();
        assert_eq!(encode_key("ArrowUp", &modes).unwrap(), b"\x1b[A");
        assert_eq!(encode_key("Enter", &modes).unwrap(), b"\r");
        assert_eq!(encode_key("ctrl+c", &modes).unwrap(), b"\x03");
        assert_eq!(encode_key("alt+x", &modes).unwrap(), b"\x1bx");
        assert_eq!(encode_key("shift+Tab", &modes).unwrap(), b"\x1b[Z");
        assert_eq!(encode_key("ctrl+ArrowRight", &modes).unwrap(), b"\x1b[1;5C");
        assert_eq!(encode_key("F5", &modes).unwrap(), b"\x1b[15~");
        assert_eq!(encode_key("shift+g", &modes).unwrap(), b"G");

        // DECCKM (?1) switches unmodified arrows/Home/End to SS3.
        modes.cursor_keys_application = true;
        assert_eq!(encode_key("ArrowDown", &modes).unwrap(), b"\x1bOB");
        assert_eq!(encode_key("Home", &modes).unwrap(), b"\x1bOH");

        // Kitty disambiguate flag (1) emits CSI u for Escape and modified keys.
        modes.kitty_keyboard_flags = 1;
        assert_eq!(encode_key("Escape", &modes).unwrap(), b"\x1b[27u");
        assert_eq!(encode_key("ctrl+c", &modes).unwrap(), b"\x1b[99;5u");
        assert_eq!(encode_key("shift+Enter", &modes).unwrap(), b"\x1b[13;2u");
        assert_eq!(encode_key("Enter", &modes).unwrap(), b"\r");
    }

    #[test]
    fn rejects_invalid_key_specs() {
        let modes = TerminalModes::default();
        assert!(matches!(
            encode_key("", &modes),
            Err(TuiInputError::MissingKey { .. })
        ));
        assert!(matches!(
            encode_key("cmd+", &modes),
            Err(TuiInputError::MissingKey { .. })
        ));
        assert!(matches!(
            encode_key("hyper+a", &modes),
            Err(TuiInputError::UnknownModifier { .. })
        ));
        assert!(matches!(
            encode_key("NotAKey", &modes),
            Err(TuiInputError::UnknownKey { .. })
        ));
    }

    #[test]
    fn encodes_type_with_bracketed_paste_sanitization() {
        assert_eq!(encode_type("hello 世界", false), "hello 世界".as_bytes());
        assert_eq!(
            encode_type("safe\x1b[201~injected", true),
            b"\x1b[200~safeinjected\x1b[201~"
        );
    }

    #[test]
    fn refuses_mouse_when_disabled_and_encodes_sgr_and_x10() {
        let mut modes = TerminalModes::default();
        assert_eq!(
            encode_mouse_click(5, 3, &modes),
            Err(TuiInputError::MouseNotEnabled)
        );
        assert_eq!(
            encode_mouse_scroll(5, 3, 0, -2, &modes),
            Err(TuiInputError::MouseNotEnabled)
        );

        // X10 / normal encoding (0-based 5,3 -> 1-based 6,4 -> bytes 38,36).
        modes.mouse_tracking = MouseTrackingMode::Normal;
        assert_eq!(
            encode_mouse_click(5, 3, &modes).unwrap(),
            vec![0x1B, b'[', b'M', 32, 38, 36, 0x1B, b'[', b'M', 35, 38, 36]
        );

        // SGR encoding (?1006).
        modes.mouse_tracking = MouseTrackingMode::ButtonEvent;
        modes.mouse_sgr = true;
        assert_eq!(
            encode_mouse_click(5, 3, &modes).unwrap(),
            b"\x1b[<0;6;4M\x1b[<0;6;4m"
        );
        assert_eq!(
            encode_mouse_right_click(5, 3, &modes).unwrap(),
            b"\x1b[<2;6;4M\x1b[<2;6;4m"
        );
        assert_eq!(
            encode_mouse_double_click(0, 0, &modes).unwrap(),
            b"\x1b[<0;1;1M\x1b[<0;1;1m\x1b[<0;1;1M\x1b[<0;1;1m"
        );
        assert_eq!(
            encode_mouse_drag((1, 1), (5, 1), &modes).unwrap(),
            b"\x1b[<0;2;2M\x1b[<32;4;2M\x1b[<32;6;2M\x1b[<0;6;2m"
        );
        assert_eq!(
            encode_mouse_scroll(2, 4, 0, -2, &modes).unwrap(),
            b"\x1b[<64;3;5M\x1b[<64;3;5M"
        );
        assert_eq!(
            encode_mouse_scroll(2, 4, 0, 0, &modes),
            Err(TuiInputError::ZeroScroll)
        );
    }
}
