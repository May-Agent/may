//! Viewer and agent input into a session desktop: May's wire messages
//! ([`InputMessage`]) translated into `waterkit_automation` events.
//!
//! waterkit delivers them: it resolves the window under each point, stamps
//! window-local locations and posts straight to the owning process, never
//! through the HID tap, so the user's cursor and focus stay put. This module
//! only maps the protocol.
//!
//! The display's stream pixels are its points, so an incoming `(x, y)` is a
//! point on the display. Modifier bits arrive in the web client's CDP-order
//! bitmap (Alt=1, Ctrl=2, Meta=4, Shift=8).

use std::num::NonZeroU32;
use std::str::FromStr;

use waterkit_automation::{
    Code, Input, KeyDown, KeyUp, Modifiers, MouseButton, MouseDown, MouseMove, MouseUp, Point,
    Scroll, Text,
};

use super::proto::{
    InputMessage, KeyboardEventKind, KeyboardInput, MouseButtonInput, MouseEventKind, MouseInput,
};

/// Why a wire message was not delivered.
#[derive(Debug, thiserror::Error)]
pub enum InputError {
    /// The keyboard message names no key and carries no text.
    #[error("keyboard input without a code or text")]
    NoKey,
    /// The code is not a W3C `KeyboardEvent.code` value.
    #[error("unknown key code {0:?}")]
    UnknownCode(String),
    /// The modifier bitmap has bits beyond Alt, Ctrl, Meta and Shift.
    #[error("unknown modifier bits {0:#x}")]
    UnknownModifiers(u64),
    /// A click count that is not a positive number.
    #[error("invalid click count {0}")]
    ClickCount(i32),
    /// The task that owns the display's input has ended.
    #[error("the display's input owner is gone")]
    OwnerGone,
    /// waterkit could not deliver the event, or could not map the key.
    #[error(transparent)]
    Deliver(#[from] waterkit_automation::Error),
}

/// A mouse message as a waterkit event.
#[derive(Debug, Clone, Copy, PartialEq)]
enum MouseEvent {
    Down(MouseDown),
    Up(MouseUp),
    Move(MouseMove),
    Scroll(Scroll),
}

/// A keyboard message as a waterkit event.
#[derive(Debug, Clone, PartialEq)]
enum KeyEvent {
    Down(KeyDown),
    Up(KeyUp),
    Text(Text),
}

/// A wire message for a display's input owner, with where to report how
/// its delivery went, if anyone waits.
#[derive(Debug)]
pub struct Posting {
    /// The message to deliver.
    pub message: InputMessage,
    /// Receives the delivery result; `None` logs a failure instead.
    pub reply: Option<async_channel::Sender<Result<(), InputError>>>,
}

/// The one owner of a display's [`Input`], run on a thread of its own:
/// delivers each posting in order and ends when every sender is gone.
///
/// A post holds its thread until the target confirms the input arrived,
/// serving that thread's run loop meanwhile, so it never runs on an
/// executor thread.
pub fn own(mut input: Input, postings: &async_channel::Receiver<Posting>) {
    while let Ok(Posting { message, reply }) = postings.recv_blocking() {
        let result = post(&mut input, message);
        match reply {
            Some(reply) => {
                // The asker may have stopped waiting.
                let _ = reply.send_blocking(result);
            }
            None => {
                if let Err(error) = result {
                    tracing::warn!(%error, "computer input not delivered");
                }
            }
        }
    }
}

/// Posts one wire message into the display `input` drives.
///
/// # Errors
/// Returns [`InputError`] when the message cannot be mapped (unknown key
/// code, modifier bit or click count) or waterkit cannot deliver it.
pub fn post(input: &mut Input, message: InputMessage) -> Result<(), InputError> {
    match message {
        InputMessage::Mouse(mouse) => match mouse_event(&mouse)? {
            MouseEvent::Down(event) => input.post(event)?,
            MouseEvent::Up(event) => input.post(event)?,
            MouseEvent::Move(event) => input.post(event)?,
            MouseEvent::Scroll(event) => input.post(event)?,
        },
        InputMessage::Keyboard(keyboard) => match key_event(&keyboard)? {
            KeyEvent::Down(event) => input.post(event)?,
            KeyEvent::Up(event) => input.post(event)?,
            KeyEvent::Text(event) => input.post(event)?,
        },
    }
    Ok(())
}

fn modifiers(bits: Option<u64>) -> Result<Modifiers, InputError> {
    const ALT: u64 = 1;
    const CTRL: u64 = 2;
    const META: u64 = 4;
    const SHIFT: u64 = 8;
    let bits = bits.unwrap_or(0);
    if bits & !(ALT | CTRL | META | SHIFT) != 0 {
        return Err(InputError::UnknownModifiers(bits));
    }
    let mut modifiers = Modifiers::empty();
    for (bit, modifier) in [
        (ALT, Modifiers::ALT),
        (CTRL, Modifiers::CONTROL),
        (META, Modifiers::META),
        (SHIFT, Modifiers::SHIFT),
    ] {
        if bits & bit != 0 {
            modifiers |= modifier;
        }
    }
    Ok(modifiers)
}

const fn button(button: Option<MouseButtonInput>) -> MouseButton {
    match button {
        Some(MouseButtonInput::Left) | None => MouseButton::Left,
        Some(MouseButtonInput::Right) => MouseButton::Right,
        Some(MouseButtonInput::Middle) => MouseButton::Middle,
    }
}

fn clicks(count: Option<i32>) -> Result<NonZeroU32, InputError> {
    count.map_or(Ok(NonZeroU32::MIN), |count| {
        u32::try_from(count)
            .ok()
            .and_then(NonZeroU32::new)
            .ok_or(InputError::ClickCount(count))
    })
}

/// A web wheel delta in pixels, rounded and clamped to what an event holds.
#[expect(
    clippy::cast_possible_truncation,
    reason = "clamped to the i32 range first"
)]
fn pixels(delta: Option<f64>) -> i32 {
    delta
        .unwrap_or(0.0)
        .round()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

fn mouse_event(input: &MouseInput) -> Result<MouseEvent, InputError> {
    let at = Point::new(input.x, input.y);
    let modifiers = modifiers(input.modifiers)?;
    Ok(match input.event_type {
        MouseEventKind::Pressed => MouseEvent::Down(
            MouseDown::new(at, button(input.button))
                .clicks(clicks(input.click_count)?)
                .modifiers(modifiers),
        ),
        MouseEventKind::Released => MouseEvent::Up(
            MouseUp::new(at, button(input.button))
                .clicks(clicks(input.click_count)?)
                .modifiers(modifiers),
        ),
        MouseEventKind::Moved => MouseEvent::Move(MouseMove::new(at).modifiers(modifiers)),
        MouseEventKind::Wheel => MouseEvent::Scroll(
            Scroll::new(at, pixels(input.delta_x), pixels(input.delta_y)).modifiers(modifiers),
        ),
    })
}

fn key_event(input: &KeyboardInput) -> Result<KeyEvent, InputError> {
    let down = matches!(input.event_type, KeyboardEventKind::KeyDown);
    // A printable character goes through the text path: layout and IME
    // agnostic, no key code needed. The web client sends text on key-down
    // only, and never with Ctrl or Meta held.
    if down && let Some(text) = input.text.as_deref().filter(|text| !text.is_empty()) {
        return Ok(KeyEvent::Text(Text::new(text)));
    }
    let name = input.code.as_deref().ok_or(InputError::NoKey)?;
    let code = Code::from_str(name).map_err(|_| InputError::UnknownCode(name.to_owned()))?;
    let modifiers = modifiers(input.modifiers)?;
    Ok(if down {
        KeyEvent::Down(KeyDown::new(code).modifiers(modifiers))
    } else {
        KeyEvent::Up(KeyUp::new(code).modifiers(modifiers))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> InputMessage {
        serde_json::from_str(body).expect("a wire message")
    }

    fn mouse(body: &str) -> Result<MouseEvent, InputError> {
        let InputMessage::Mouse(input) = parse(body) else {
            panic!("expected mouse input");
        };
        mouse_event(&input)
    }

    fn keyboard(body: &str) -> Result<KeyEvent, InputError> {
        let InputMessage::Keyboard(input) = parse(body) else {
            panic!("expected keyboard input");
        };
        key_event(&input)
    }

    #[test]
    fn cdp_modifier_bits_map_to_modifiers() {
        assert_eq!(modifiers(None).unwrap(), Modifiers::empty());
        assert_eq!(modifiers(Some(1)).unwrap(), Modifiers::ALT);
        assert_eq!(modifiers(Some(2)).unwrap(), Modifiers::CONTROL);
        assert_eq!(modifiers(Some(4)).unwrap(), Modifiers::META);
        assert_eq!(modifiers(Some(8)).unwrap(), Modifiers::SHIFT);
        assert_eq!(
            modifiers(Some(15)).unwrap(),
            Modifiers::ALT | Modifiers::CONTROL | Modifiers::META | Modifiers::SHIFT
        );
    }

    #[test]
    fn a_modifier_bit_beyond_the_four_is_an_error() {
        assert!(matches!(
            modifiers(Some(16)),
            Err(InputError::UnknownModifiers(16))
        ));
    }

    #[test]
    fn a_press_carries_its_button_clicks_and_modifiers() {
        let event = mouse(
            r#"{"type":"mouse","eventType":"mousePressed","x":1.5,"y":2.0,"button":"right","clickCount":2,"modifiers":8}"#,
        )
        .unwrap();
        assert_eq!(
            event,
            MouseEvent::Down(
                MouseDown::new(Point::new(1.5, 2.0), MouseButton::Right)
                    .clicks(NonZeroU32::new(2).unwrap())
                    .modifiers(Modifiers::SHIFT)
            )
        );
    }

    #[test]
    fn a_wheel_passes_web_deltas_rounded() {
        let event = mouse(
            r#"{"type":"mouse","eventType":"mouseWheel","x":3.0,"y":4.0,"deltaX":-1.4,"deltaY":120.6}"#,
        )
        .unwrap();
        assert_eq!(
            event,
            MouseEvent::Scroll(Scroll::new(Point::new(3.0, 4.0), -1, 121))
        );
    }

    #[test]
    fn a_non_positive_click_count_is_an_error() {
        assert!(matches!(
            mouse(r#"{"type":"mouse","eventType":"mousePressed","x":0,"y":0,"clickCount":0}"#),
            Err(InputError::ClickCount(0))
        ));
    }

    #[test]
    fn a_key_goes_by_its_web_code() {
        let event =
            keyboard(r#"{"type":"keyboard","eventType":"keyDown","code":"ArrowLeft"}"#).unwrap();
        assert_eq!(event, KeyEvent::Down(KeyDown::new(Code::ArrowLeft)));
        let event = keyboard(r#"{"type":"keyboard","eventType":"keyUp","code":"Enter"}"#).unwrap();
        assert_eq!(event, KeyEvent::Up(KeyUp::new(Code::Enter)));
    }

    #[test]
    fn typed_text_wins_over_the_code_on_key_down_only() {
        let event =
            keyboard(r#"{"type":"keyboard","eventType":"keyDown","code":"KeyA","text":"a"}"#)
                .unwrap();
        assert_eq!(event, KeyEvent::Text(Text::new("a")));
        let event = keyboard(r#"{"type":"keyboard","eventType":"keyUp","code":"KeyA","text":"a"}"#)
            .unwrap();
        assert_eq!(event, KeyEvent::Up(KeyUp::new(Code::KeyA)));
    }

    #[test]
    fn unmappable_keyboard_input_is_an_error() {
        assert!(matches!(
            keyboard(r#"{"type":"keyboard","eventType":"keyDown","code":"Nonsense"}"#),
            Err(InputError::UnknownCode(_))
        ));
        assert!(matches!(
            keyboard(r#"{"type":"keyboard","eventType":"keyDown"}"#),
            Err(InputError::NoKey)
        ));
    }
}
