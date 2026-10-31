//! `computer` command: the agent sees and drives its session's virtual
//! desktop — screenshots/observe, clicks, drags, scrolls, typing.

use std::borrow::Cow;
use std::sync::Arc;

use aither_core::llm::tool::ToolResultPart;
use aither_core::llm::{Tool, ToolResult};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::computer::input::{
    self, InputMessage, KeyboardEventKind, KeyboardInput, MouseButtonInput, MouseEventKind,
    MouseInput,
};
use crate::computer::{SessionDisplay, SessionDisplays, SessionStreams};

/// Handles the daemon hands the tool so commands run against this
/// session's desktop in-process.
#[derive(Clone)]
pub struct ComputerHandles {
    /// The daemon's per-session displays.
    pub displays: Arc<Mutex<SessionDisplays>>,
    /// The daemon's per-session desktop streams (checked for takeover).
    pub streams: Arc<Mutex<SessionStreams>>,
}

/// `computer` tool: see and drive the session's virtual desktop.
#[derive(Debug, Clone)]
pub struct ComputerTool {
    session_id: String,
    handles: ComputerHandles,
}

impl std::fmt::Debug for ComputerHandles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComputerHandles").finish_non_exhaustive()
    }
}

/// `computer <action> ...` — the session's virtual desktop.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ComputerArgs {
    /// Desktop action. One of:
    /// - `observe` — the desktop's current screen plus a description
    /// - `screenshot` — just the screen image
    /// - `click X Y` / `double-click X Y` / `right-click X Y`
    /// - `drag X1 Y1 X2 Y2` — press at the start, drag to the end
    /// - `scroll DX DY` — pixel deltas at the pointer
    /// - `type TEXT` — type the string at the focused element
    /// - `key CODE` — press a named key (`Enter`, `Tab`, `ArrowLeft`, `F5`, ...)
    ///
    /// `X`/`Y` are points inside the desktop (its size is in `observe`).
    /// While the user is taking control the actions are refused — the
    /// agent waits until takeover ends.
    pub command: String,
}

impl ComputerTool {
    /// Bind the tool to its session.
    #[must_use]
    pub const fn new(session_id: String, handles: ComputerHandles) -> Self {
        Self {
            session_id,
            handles,
        }
    }

    /// The session's display, creating it on first use — provisioning is
    /// the point: an agent asking for its desktop gets one.
    async fn display_id(&self) -> Result<u32, crate::computer::DisplayError> {
        let display = {
            let mut displays = self.handles.displays.lock().await;
            displays.acquire(&self.session_id)?
        };
        Ok(display.display_id())
    }

    async fn check_agent_owns_desktop(&self) -> Result<(), String> {
        let user_is_driving = {
            let streams = self.handles.streams.lock().await;
            streams
                .get(&self.session_id)
                .is_some_and(|stream| stream.takeover_active())
        };
        if user_is_driving {
            return Err(
                "the user is currently driving this desktop; wait until takeover ends".to_owned(),
            );
        }
        Ok(())
    }
}

impl Tool for ComputerTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("computer")
    }

    type Arguments = ComputerArgs;
    type Res = ToolResult;

    async fn call(
        &self,
        arguments: Self::Arguments,
        _cx: aither_core::llm::ToolContext,
    ) -> aither_core::Result<Self::Res> {
        match self.run(&arguments.command).await {
            Ok(result) => Ok(result),
            Err(error) => Ok(ToolResult::error(error)),
        }
    }
}

impl ComputerTool {
    async fn run(&self, command: &str) -> Result<ToolResult, String> {
        let words = shell_words::split(command).map_err(|e| format!("bad command quoting: {e}"))?;
        let (action, rest) = words.split_first().ok_or_else(|| usage().to_owned())?;
        match action.as_str() {
            "screenshot" | "observe" => self.capture(action == "observe").await,
            "click" | "double-click" | "right-click" | "drag" | "scroll" | "type" | "key" => {
                self.drive(action, rest).await
            }
            _ => Err(usage().to_owned()),
        }
    }

    async fn capture(&self, with_text: bool) -> Result<ToolResult, String> {
        let display_id = self.display_id().await.map_err(|e| e.to_string())?;
        let png = tokio::task::spawn_blocking(move || {
            crate::computer::screenshot::screenshot_png(display_id)
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
        if with_text {
            Ok(ToolResult::parts(vec![
                ToolResultPart::text("The attached image is the session desktop's current screen."),
                ToolResultPart::image(png, "image/png"),
            ]))
        } else {
            Ok(ToolResult::image(png, "image/png"))
        }
    }

    async fn drive(&self, action: &str, rest: &[String]) -> Result<ToolResult, String> {
        self.check_agent_owns_desktop().await?;
        let display_id = self.display_id().await.map_err(|e| e.to_string())?;
        let at = |i: usize| -> Result<f64, String> {
            rest.get(i)
                .and_then(|s| s.parse::<f64>().ok())
                .ok_or_else(|| format!("`computer {action}` needs a number at position {}", i + 1))
        };
        let messages: Vec<InputMessage> = match action {
            "click" => vec![
                mouse(
                    MouseEventKind::Pressed,
                    at(0)?,
                    at(1)?,
                    MouseButtonInput::Left,
                ),
                mouse(
                    MouseEventKind::Released,
                    at(0)?,
                    at(1)?,
                    MouseButtonInput::Left,
                ),
            ],
            "double-click" => {
                let (x, y) = (at(0)?, at(1)?);
                let mut press = mouse(MouseEventKind::Pressed, x, y, MouseButtonInput::Left);
                press = with_click_count(press, 2);
                let mut release = mouse(MouseEventKind::Released, x, y, MouseButtonInput::Left);
                release = with_click_count(release, 2);
                vec![press, release]
            }
            "right-click" => vec![
                mouse(
                    MouseEventKind::Pressed,
                    at(0)?,
                    at(1)?,
                    MouseButtonInput::Right,
                ),
                mouse(
                    MouseEventKind::Released,
                    at(0)?,
                    at(1)?,
                    MouseButtonInput::Right,
                ),
            ],
            "drag" => {
                let (x1, y1, x2, y2) = (at(0)?, at(1)?, at(2)?, at(3)?);
                vec![
                    mouse(MouseEventKind::Pressed, x1, y1, MouseButtonInput::Left),
                    mouse(
                        MouseEventKind::Moved,
                        x1.midpoint(x2),
                        y1.midpoint(y2),
                        MouseButtonInput::Left,
                    ),
                    mouse(MouseEventKind::Moved, x2, y2, MouseButtonInput::Left),
                    mouse(MouseEventKind::Released, x2, y2, MouseButtonInput::Left),
                ]
            }
            "scroll" => {
                let (dx, dy) = (at(0)?, at(1)?);
                let mut wheel = MouseInput {
                    event_type: MouseEventKind::Wheel,
                    x: at(2).unwrap_or(0.0),
                    y: at(3).unwrap_or(0.0),
                    button: None,
                    click_count: None,
                    delta_x: Some(dx),
                    delta_y: Some(dy),
                    modifiers: None,
                };
                wheel.delta_x = Some(dx);
                vec![InputMessage::Mouse(wheel)]
            }
            "type" => {
                let text = rest.join(" ");
                if text.is_empty() {
                    return Err("`computer type` needs the text to type".to_owned());
                }
                vec![InputMessage::Keyboard(KeyboardInput {
                    event_type: KeyboardEventKind::KeyDown,
                    code: None,
                    text: Some(text),
                    modifiers: None,
                })]
            }
            "key" => {
                let code = rest
                    .first()
                    .ok_or_else(|| "`computer key` needs a key code like `Enter`".to_owned())?;
                vec![
                    InputMessage::Keyboard(KeyboardInput {
                        event_type: KeyboardEventKind::KeyDown,
                        code: Some(code.clone()),
                        text: None,
                        modifiers: None,
                    }),
                    InputMessage::Keyboard(KeyboardInput {
                        event_type: KeyboardEventKind::KeyUp,
                        code: Some(code.clone()),
                        text: None,
                        modifiers: None,
                    }),
                ]
            }
            _ => return Err(usage().to_owned()),
        };

        let mut held = None;
        for message in messages {
            if let InputMessage::Mouse(m) = &message {
                match m.event_kind() {
                    input::MousePhase::Pressed => held = m.held(),
                    input::MousePhase::Released => held = None,
                    _ => {}
                }
            }
            input::post_input(display_id, message, held)?;
        }
        Ok(ToolResult::text(format!("done: {action}")))
    }
}

const fn with_click_count(mut message: InputMessage, count: i32) -> InputMessage {
    if let InputMessage::Mouse(mouse) = &mut message {
        mouse.click_count = Some(count);
    }
    message
}

const fn mouse(
    event_type: MouseEventKind,
    x: f64,
    y: f64,
    button: MouseButtonInput,
) -> InputMessage {
    InputMessage::Mouse(MouseInput {
        event_type,
        x,
        y,
        button: Some(button),
        click_count: None,
        delta_x: None,
        delta_y: None,
        modifiers: None,
    })
}

const fn usage() -> &'static str {
    "usage: computer <screenshot|observe|click X Y|double-click X Y|right-click X Y|drag X1 Y1 X2 Y2|scroll DX DY|type TEXT|key CODE>"
}
