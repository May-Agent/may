//! Viewer input into a session desktop: `DataChannel` messages become
//! `CGEvent`s posted at the HID tap.
//!
//! The display's stream pixels are its points, so an incoming `(x, y)`
//! lands in global space at `CGDisplayBounds(display).origin + (x, y)`.
//! Modifier bits arrive in the web client's CDP-order bitmap
//! (Alt=1, Ctrl=2, Meta=4, Shift=8).

use std::ffi::c_void;

use serde::Deserialize;

// ── CoreGraphics event FFI ─────────────────────────────────────────────────────

type CGEventRef = *mut c_void;
type CGEventSourceRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

#[repr(C)]
struct CGSize {
    width: f64,
    height: f64,
}

// CGEventTypes we post.
const K_CG_EVENT_LEFT_MOUSE_DOWN: u32 = 1;
const K_CG_EVENT_LEFT_MOUSE_UP: u32 = 2;
const K_CG_EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
const K_CG_EVENT_RIGHT_MOUSE_UP: u32 = 4;
const K_CG_EVENT_MOUSE_MOVED: u32 = 5;
const K_CG_EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
const K_CG_EVENT_RIGHT_MOUSE_DRAGGED: u32 = 7;
const K_CG_EVENT_OTHER_MOUSE_DOWN: u32 = 25;
const K_CG_EVENT_OTHER_MOUSE_UP: u32 = 26;
const K_CG_EVENT_OTHER_MOUSE_DRAGGED: u32 = 27;

// CGMouseButton.
const K_CG_MOUSE_BUTTON_LEFT: u32 = 0;
const K_CG_MOUSE_BUTTON_RIGHT: u32 = 1;
const K_CG_MOUSE_BUTTON_CENTER: u32 = 2;

const K_CG_HID_EVENT_TAP: u32 = 0;
const K_CG_SCROLL_EVENT_UNIT_PIXEL: u32 = 1;

const K_CG_EVENT_FLAG_MASK_SHIFT: u64 = 0x0002_0000;
const K_CG_EVENT_FLAG_MASK_CONTROL: u64 = 0x0004_0000;
const K_CG_EVENT_FLAG_MASK_ALTERNATE: u64 = 0x0008_0000;
const K_CG_EVENT_FLAG_MASK_COMMAND: u64 = 0x0010_0000;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventCreateMouseEvent(
        source: CGEventSourceRef,
        event_type: u32,
        position: CGPoint,
        button: u32,
    ) -> CGEventRef;
    fn CGEventCreateScrollWheelEvent(
        source: CGEventSourceRef,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
    ) -> CGEventRef;
    fn CGEventCreateKeyboardEvent(source: CGEventSourceRef, keycode: u16, down: bool)
    -> CGEventRef;
    fn CGEventKeyboardSetUnicodeString(event: CGEventRef, len: usize, chars: *const u16);
    fn CGEventSetFlags(event: CGEventRef, flags: u64);
    fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    fn CGEventPost(tap: u32, event: CGEventRef);
    fn CGDisplayBounds(display: u32) -> CGRect;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
}

// kCGMouseEventClickState: lets double/triple clicks through.
const K_CG_MOUSE_EVENT_CLICK_STATE: u32 = 0x36;

// ── DataChannel messages (same wire shape the browser input uses) ──────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum InputMessage {
    Mouse(MouseInput),
    Keyboard(KeyboardInput),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MouseInput {
    pub event_type: MouseEventKind,
    pub x: f64,
    pub y: f64,
    #[serde(default)]
    pub button: Option<MouseButtonInput>,
    #[serde(default)]
    pub click_count: Option<i32>,
    #[serde(default)]
    pub delta_x: Option<f64>,
    #[serde(default)]
    pub delta_y: Option<f64>,
    #[serde(default)]
    pub modifiers: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MouseEventKind {
    #[serde(rename = "mousePressed")]
    Pressed,
    #[serde(rename = "mouseReleased")]
    Released,
    #[serde(rename = "mouseMoved")]
    Moved,
    #[serde(rename = "mouseWheel")]
    Wheel,
}

/// Where a mouse message sits in a press/drag/release sequence.
#[derive(Clone, Copy)]
pub enum MousePhase {
    /// A button went down.
    Pressed,
    /// The pointer moved (a drag while a button is held).
    Moved,
    /// A button came up.
    Released,
    /// The wheel scrolled.
    Wheeled,
}

impl MouseInput {
    /// The message's place in the drag sequence.
    pub const fn event_kind(&self) -> MousePhase {
        match self.event_type {
            MouseEventKind::Pressed => MousePhase::Pressed,
            MouseEventKind::Released => MousePhase::Released,
            MouseEventKind::Moved => MousePhase::Moved,
            MouseEventKind::Wheel => MousePhase::Wheeled,
        }
    }

    /// The button this press/drag concerns — the caller stores it as the
    /// held state until the matching release.
    pub fn held(&self) -> Option<HeldButton> {
        self.button.as_ref().map(|button| match button {
            MouseButtonInput::Left => HeldButton::Left,
            MouseButtonInput::Right => HeldButton::Right,
            MouseButtonInput::Middle => HeldButton::Middle,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseButtonInput {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyboardInput {
    pub event_type: KeyboardEventKind,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub modifiers: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum KeyboardEventKind {
    KeyDown,
    KeyUp,
}

// ── Posting ────────────────────────────────────────────────────────────────────

/// The button a peer is holding between press and release — moves
/// while held are drags, not plain motion.
#[derive(Clone, Copy)]
pub enum HeldButton {
    /// Left button drag.
    Left,
    /// Right button drag.
    Right,
    /// Middle button drag.
    Middle,
}

/// Posts one input message at the display's global origin offset.
/// `held` is the caller's drag state: set on press, cleared on release.
///
/// # Errors
/// Returns the message text when the event kind is unknown — the client
/// sent something the desktop does not consume.
pub fn post_input(
    display_id: u32,
    message: InputMessage,
    held: Option<HeldButton>,
) -> Result<(), String> {
    match message {
        InputMessage::Mouse(input) => post_mouse(display_id, input, held),
        InputMessage::Keyboard(input) => post_keyboard(&input),
    }
}

fn post_mouse(display_id: u32, input: MouseInput, held: Option<HeldButton>) -> Result<(), String> {
    let bounds = unsafe { CGDisplayBounds(display_id) };
    let point = CGPoint {
        x: bounds.origin.x + input.x,
        y: bounds.origin.y + input.y,
    };
    let button = input.button.unwrap_or(MouseButtonInput::Left);
    let flags = cg_flags(input.modifiers.unwrap_or(0));

    let event = match input.event_type {
        MouseEventKind::Wheel => unsafe {
            let dx = input.delta_x.unwrap_or(0.0);
            let dy = input.delta_y.unwrap_or(0.0);
            // Web deltas are pixels; negate Y — HID scroll-up is positive.
            #[allow(clippy::cast_possible_truncation)]
            CGEventCreateScrollWheelEvent(
                std::ptr::null_mut(),
                K_CG_SCROLL_EVENT_UNIT_PIXEL,
                2,
                // clamped to i32 range above the call
                -dy.round().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32,
                -dx.round().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32,
            )
        },
        MouseEventKind::Moved | MouseEventKind::Pressed | MouseEventKind::Released => unsafe {
            let (cg_button, down, up) = match button {
                MouseButtonInput::Left => (
                    K_CG_MOUSE_BUTTON_LEFT,
                    K_CG_EVENT_LEFT_MOUSE_DOWN,
                    K_CG_EVENT_LEFT_MOUSE_UP,
                ),
                MouseButtonInput::Right => (
                    K_CG_MOUSE_BUTTON_RIGHT,
                    K_CG_EVENT_RIGHT_MOUSE_DOWN,
                    K_CG_EVENT_RIGHT_MOUSE_UP,
                ),
                MouseButtonInput::Middle => (
                    K_CG_MOUSE_BUTTON_CENTER,
                    K_CG_EVENT_OTHER_MOUSE_DOWN,
                    K_CG_EVENT_OTHER_MOUSE_UP,
                ),
            };
            let (event_type, event_button) = match input.event_type {
                MouseEventKind::Pressed => (down, cg_button),
                MouseEventKind::Released => (up, cg_button),
                MouseEventKind::Moved => match held {
                    Some(HeldButton::Left) => {
                        (K_CG_EVENT_LEFT_MOUSE_DRAGGED, K_CG_MOUSE_BUTTON_LEFT)
                    }
                    Some(HeldButton::Right) => {
                        (K_CG_EVENT_RIGHT_MOUSE_DRAGGED, K_CG_MOUSE_BUTTON_RIGHT)
                    }
                    Some(HeldButton::Middle) => {
                        (K_CG_EVENT_OTHER_MOUSE_DRAGGED, K_CG_MOUSE_BUTTON_CENTER)
                    }
                    None => (K_CG_EVENT_MOUSE_MOVED, cg_button),
                },
                MouseEventKind::Wheel => unreachable!(),
            };
            let event =
                CGEventCreateMouseEvent(std::ptr::null_mut(), event_type, point, event_button);
            if !event.is_null()
                && let Some(count) = input.click_count.filter(|count| *count > 1)
            {
                CGEventSetIntegerValueField(event, K_CG_MOUSE_EVENT_CLICK_STATE, i64::from(count));
            }
            event
        },
    };

    post(event, flags)
}

fn post_keyboard(input: &KeyboardInput) -> Result<(), String> {
    let down = matches!(input.event_type, KeyboardEventKind::KeyDown);
    let flags = cg_flags(input.modifiers.unwrap_or(0));

    // A printable character goes through the unicode path — no keycode
    // table entry needed and IME/layout agnostic.
    if down && let Some(text) = input.text.as_deref().filter(|t| !t.is_empty()) {
        let utf16: Vec<u16> = text.encode_utf16().collect();
        let event = unsafe {
            let event = CGEventCreateKeyboardEvent(std::ptr::null_mut(), 0, true);
            if !event.is_null() {
                CGEventKeyboardSetUnicodeString(event, utf16.len(), utf16.as_ptr());
            }
            event
        };
        return post(event, flags);
    }

    let Some(code) = input.code.as_deref() else {
        return Err("keyboard input without a code or text".to_owned());
    };
    let Some(keycode) = macos_keycode(code) else {
        return Err(format!("unmapped key code {code}"));
    };
    let event = unsafe { CGEventCreateKeyboardEvent(std::ptr::null_mut(), keycode, down) };
    post(event, flags)
}

fn post(event: CGEventRef, flags: u64) -> Result<(), String> {
    if event.is_null() {
        return Err("CoreGraphics refused the event".to_owned());
    }
    unsafe {
        if flags != 0 {
            CGEventSetFlags(event, flags);
        }
        CGEventPost(K_CG_HID_EVENT_TAP, event);
        CFRelease(event);
    }
    Ok(())
}

/// CDP-order modifier bitmap → `CGEventFlags`.
const fn cg_flags(modifiers: u64) -> u64 {
    let mut flags = 0;
    if modifiers & 1 != 0 {
        flags |= K_CG_EVENT_FLAG_MASK_ALTERNATE;
    }
    if modifiers & 2 != 0 {
        flags |= K_CG_EVENT_FLAG_MASK_CONTROL;
    }
    if modifiers & 4 != 0 {
        flags |= K_CG_EVENT_FLAG_MASK_COMMAND;
    }
    if modifiers & 8 != 0 {
        flags |= K_CG_EVENT_FLAG_MASK_SHIFT;
    }
    flags
}

/// Web `KeyboardEvent.code` → macOS virtual keycode (ANSI layout codes
/// for everything that needs one — printables take the unicode path).
fn macos_keycode(code: &str) -> Option<u16> {
    Some(match code {
        "KeyA" => 0,
        "KeyS" => 1,
        "KeyD" => 2,
        "KeyF" => 3,
        "KeyH" => 4,
        "KeyG" => 5,
        "KeyZ" => 6,
        "KeyX" => 7,
        "KeyC" => 8,
        "KeyV" => 9,
        "KeyB" => 11,
        "KeyQ" => 12,
        "KeyW" => 13,
        "KeyE" => 14,
        "KeyR" => 15,
        "KeyY" => 16,
        "KeyT" => 17,
        "Digit1" => 18,
        "Digit2" => 19,
        "Digit3" => 20,
        "Digit4" => 21,
        "Digit6" => 22,
        "Digit5" => 23,
        "Equal" => 24,
        "Digit9" => 25,
        "Digit7" => 26,
        "Minus" => 27,
        "Digit8" => 28,
        "Digit0" => 29,
        "BracketRight" => 30,
        "KeyO" => 31,
        "KeyU" => 32,
        "BracketLeft" => 33,
        "KeyI" => 34,
        "KeyP" => 35,
        "Enter" => 36,
        "KeyL" => 37,
        "KeyJ" => 38,
        "Quote" => 39,
        "KeyK" => 40,
        "Semicolon" => 41,
        "Backslash" => 42,
        "Comma" => 43,
        "Slash" => 44,
        "KeyN" => 45,
        "KeyM" => 46,
        "Period" => 47,
        "Tab" => 48,
        "Space" => 49,
        "Backquote" => 50,
        "Backspace" => 51,
        "Escape" => 53,
        "MetaRight" => 54,
        "MetaLeft" => 55,
        "ShiftLeft" => 56,
        "CapsLock" => 57,
        "AltLeft" => 58,
        "ControlLeft" => 59,
        "ShiftRight" => 60,
        "AltRight" => 61,
        "ControlRight" => 62,
        "Fn" => 63,
        "F17" => 64,
        "NumpadDecimal" => 65,
        "NumpadMultiply" => 67,
        "NumpadAdd" => 69,
        "NumLock" => 71,
        "NumpadDivide" => 75,
        "NumpadEnter" => 76,
        "NumpadSubtract" => 78,
        "F18" => 79,
        "F19" => 80,
        "NumpadEqual" => 81,
        "Numpad0" => 82,
        "Numpad1" => 83,
        "Numpad2" => 84,
        "Numpad3" => 85,
        "Numpad4" => 86,
        "Numpad5" => 87,
        "Numpad6" => 88,
        "Numpad7" => 89,
        "F20" => 90,
        "Numpad8" => 91,
        "Numpad9" => 92,
        "F5" => 96,
        "F6" => 97,
        "F7" => 98,
        "F3" => 99,
        "F8" => 100,
        "F9" => 101,
        "F11" => 103,
        "F13" => 105,
        "F16" => 106,
        "F14" => 107,
        "F10" => 109,
        "F12" => 111,
        "F15" => 113,
        "Home" => 115,
        "PageUp" => 116,
        "Delete" => 117,
        "F4" => 118,
        "End" => 119,
        "F2" => 120,
        "PageDown" => 121,
        "F1" => 122,
        "ArrowLeft" => 123,
        "ArrowRight" => 124,
        "ArrowDown" => 125,
        "ArrowUp" => 126,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_codes_map_to_macos_keycodes() {
        assert_eq!(macos_keycode("Enter"), Some(36));
        assert_eq!(macos_keycode("Space"), Some(49));
        assert_eq!(macos_keycode("ArrowLeft"), Some(123));
        assert_eq!(macos_keycode("KeyA"), Some(0));
        assert_eq!(macos_keycode("Nonsense"), None);
    }

    #[test]
    fn cdp_modifier_bits_map_to_cg_flags() {
        assert_eq!(cg_flags(0), 0);
        assert_eq!(cg_flags(1), K_CG_EVENT_FLAG_MASK_ALTERNATE);
        assert_eq!(cg_flags(2), K_CG_EVENT_FLAG_MASK_CONTROL);
        assert_eq!(cg_flags(4), K_CG_EVENT_FLAG_MASK_COMMAND);
        assert_eq!(cg_flags(8), K_CG_EVENT_FLAG_MASK_SHIFT);
        assert_eq!(
            cg_flags(15),
            K_CG_EVENT_FLAG_MASK_ALTERNATE
                | K_CG_EVENT_FLAG_MASK_CONTROL
                | K_CG_EVENT_FLAG_MASK_COMMAND
                | K_CG_EVENT_FLAG_MASK_SHIFT
        );
    }

    #[test]
    fn mouse_messages_report_their_phase() {
        let parse = |body: &str| serde_json::from_str::<InputMessage>(body).unwrap();
        let InputMessage::Mouse(mouse) = parse(
            r#"{"type":"mouse","eventType":"mousePressed","x":1.0,"y":2.0,"button":"right"}"#,
        ) else {
            panic!("expected mouse input");
        };
        assert!(matches!(mouse.event_kind(), MousePhase::Pressed));
        assert!(matches!(mouse.held(), Some(HeldButton::Right)));
    }
}
