//! `computer` command: the agent sees and drives its session's virtual
//! desktop — screenshots/observe, clicks, drags, scrolls, typing.

use std::borrow::Cow;
use std::sync::Arc;

use aither_core::llm::tool::ToolResultPart;
use aither_core::llm::{Tool, ToolResult};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::computer::proto::{
    InputMessage, KeyboardEventKind, KeyboardInput, MouseButtonInput, MouseEventKind, MouseInput,
};
use crate::computer::{
    ComputerAgent,
    proto::{AgentRequest, AgentResponse},
};

/// What the daemon hands the tool: a client to `computer-agent`, the
/// helper that owns every session's desktop.
#[derive(Clone)]
pub struct ComputerHandles {
    /// The daemon's computer-agent client.
    pub agent: Arc<ComputerAgent>,
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
    /// Usage, one line per action:
    /// - `observe`: the screen plus a description
    /// - `screenshot`: just the screen image
    /// - `click X Y`, `double-click X Y`, `right-click X Y`
    /// - `drag X1 Y1 X2 Y2`: press at the start, release at the end
    /// - `scroll DX DY`: pixel deltas at the pointer
    /// - `type TEXT`: type at the focused element
    /// - `key KEY` or `key MOD+MOD+KEY`: `Enter`, `Tab`, `ArrowLeft`, `F5`,
    ///   a letter or digit, `cmd+l`, `cmd+shift+4`
    ///
    /// Modifiers: `cmd` (`command`, `meta`), `ctrl` (`control`), `alt`
    /// (`option`), `shift`. `X`/`Y` are points on the desktop (its size is in
    /// `observe`). While the user has taken control, actions are refused.
    pub action: String,
    /// The action's operands, as separate arguments.
    #[serde(default)]
    #[schemars(extend("x-positional" = true))]
    pub operands: Vec<String>,
}

/// A key press: the key code and the modifiers held (the wire bitmask).
#[derive(Debug, PartialEq, Eq)]
struct KeyPress {
    code: String,
    modifiers: u64,
}

impl KeyPress {
    /// Parses `KEY` or `MOD+MOD+KEY`; modifier names are case-insensitive.
    fn parse(spec: &str) -> Result<Self, String> {
        let mut parts: Vec<&str> = spec.split('+').collect();
        let key = parts.pop().filter(|key| !key.is_empty()).ok_or_else(|| {
            format!("`computer key {spec}` names no key; expected `KEY` or `MOD+KEY`")
        })?;
        let mut modifiers = 0;
        for name in parts {
            modifiers |= modifier_bit(name).ok_or_else(|| {
                format!(
                    "unknown modifier `{name}`; valid: cmd (command, meta), ctrl (control), alt (option), shift"
                )
            })?;
        }
        Ok(Self {
            code: key_code(key),
            modifiers,
        })
    }
}

/// The wire bit of a modifier name.
fn modifier_bit(name: &str) -> Option<u64> {
    use crate::computer::proto::modifier_bits::{ALT, CTRL, META, SHIFT};
    match name.to_ascii_lowercase().as_str() {
        "cmd" | "command" | "meta" => Some(META),
        "ctrl" | "control" => Some(CTRL),
        "alt" | "option" => Some(ALT),
        "shift" => Some(SHIFT),
        _ => None,
    }
}

/// A single letter or digit names its physical key (`l` is `KeyL`, `1` is
/// `Digit1`); anything else is already a key code.
fn key_code(key: &str) -> String {
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => format!("Key{}", c.to_ascii_uppercase()),
        (Some(c), None) if c.is_ascii_digit() => format!("Digit{c}"),
        _ => key.to_owned(),
    }
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
    async fn ensure_desktop(&self) -> Result<(), String> {
        match self
            .handles
            .agent
            .request(AgentRequest::EnsureDisplay {
                session_id: self.session_id.clone(),
            })
            .await?
        {
            AgentResponse::Display { .. } => Ok(()),
            AgentResponse::Error { error } => Err(error),
            _ => Err("unexpected reply from computer-agent".to_owned()),
        }
    }

    async fn check_agent_owns_desktop(&self) -> Result<(), String> {
        match self
            .handles
            .agent
            .request(AgentRequest::TakeoverActive {
                session_id: self.session_id.clone(),
            })
            .await?
        {
            AgentResponse::Flag { value: true } => Err(
                "the user is currently driving this desktop; wait until takeover ends".to_owned(),
            ),
            _ => Ok(()),
        }
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
        match self.run(&arguments.action, &arguments.operands).await {
            Ok(result) => Ok(result),
            Err(error) => Ok(ToolResult::error(error)),
        }
    }
}

impl ComputerTool {
    async fn run(&self, action: &str, rest: &[String]) -> Result<ToolResult, String> {
        match action {
            "screenshot" | "observe" => self.capture(action == "observe").await,
            "click" | "double-click" | "right-click" | "drag" | "scroll" | "type" | "key" => {
                self.drive(action, rest).await
            }
            _ => Err(usage().to_owned()),
        }
    }

    async fn capture(&self, with_text: bool) -> Result<ToolResult, String> {
        self.ensure_desktop().await?;
        let png = match self
            .handles
            .agent
            .request(AgentRequest::Screenshot {
                session_id: self.session_id.clone(),
            })
            .await?
        {
            AgentResponse::Png { data } => data,
            AgentResponse::Error { error } => return Err(error),
            _ => return Err("unexpected reply from computer-agent".to_owned()),
        };
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
        self.ensure_desktop().await?;
        let form = match action {
            "click" | "double-click" | "right-click" => Some(format!("{action} X Y")),
            "drag" => Some("drag X1 Y1 X2 Y2".to_owned()),
            "scroll" => Some("scroll DX DY".to_owned()),
            _ => None,
        };
        let numbers = match &form {
            Some(form) => numbers(form, rest)?,
            None => Vec::new(),
        };
        let at = |i: usize| numbers[i];
        let messages: Vec<InputMessage> = match action {
            "click" => vec![
                mouse(
                    MouseEventKind::Pressed,
                    at(0),
                    at(1),
                    MouseButtonInput::Left,
                ),
                mouse(
                    MouseEventKind::Released,
                    at(0),
                    at(1),
                    MouseButtonInput::Left,
                ),
            ],
            "double-click" => {
                let (x, y) = (at(0), at(1));
                let mut press = mouse(MouseEventKind::Pressed, x, y, MouseButtonInput::Left);
                press = with_click_count(press, 2);
                let mut release = mouse(MouseEventKind::Released, x, y, MouseButtonInput::Left);
                release = with_click_count(release, 2);
                vec![press, release]
            }
            "right-click" => vec![
                mouse(
                    MouseEventKind::Pressed,
                    at(0),
                    at(1),
                    MouseButtonInput::Right,
                ),
                mouse(
                    MouseEventKind::Released,
                    at(0),
                    at(1),
                    MouseButtonInput::Right,
                ),
            ],
            "drag" => {
                let (x1, y1, x2, y2) = (at(0), at(1), at(2), at(3));
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
                let (dx, dy) = (at(0), at(1));
                let wheel = MouseInput {
                    event_type: MouseEventKind::Wheel,
                    x: 0.0,
                    y: 0.0,
                    button: None,
                    click_count: None,
                    delta_x: Some(dx),
                    delta_y: Some(dy),
                    modifiers: None,
                };
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
                let [spec] = rest else {
                    return Err(
                        "expected `computer key KEY` or `computer key MOD+MOD+KEY`".to_owned()
                    );
                };
                let press = KeyPress::parse(spec)?;
                let modifiers = (press.modifiers != 0).then_some(press.modifiers);
                [KeyboardEventKind::KeyDown, KeyboardEventKind::KeyUp]
                    .into_iter()
                    .map(|event_type| {
                        InputMessage::Keyboard(KeyboardInput {
                            event_type,
                            code: Some(press.code.clone()),
                            text: None,
                            modifiers,
                        })
                    })
                    .collect()
            }
            _ => return Err(usage().to_owned()),
        };

        for message in messages {
            match self
                .handles
                .agent
                .request(AgentRequest::Input {
                    session_id: self.session_id.clone(),
                    message,
                })
                .await?
            {
                AgentResponse::Ok => {}
                AgentResponse::Error { error } => return Err(error),
                _ => return Err("unexpected reply from computer-agent".to_owned()),
            }
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

/// Parses `rest` as the numeric operands of `form` (e.g. `click X Y`): one
/// number per uppercase placeholder word.
fn numbers(form: &str, rest: &[String]) -> Result<Vec<f64>, String> {
    let expected = form.split(' ').count() - 1;
    if rest.len() != expected {
        return Err(format!(
            "expected `computer {form}` ({expected} numbers), got {}",
            rest.len()
        ));
    }
    rest.iter()
        .map(|word| {
            word.parse::<f64>()
                .map_err(|_| format!("expected `computer {form}`, `{word}` is not a number"))
        })
        .collect()
}

const fn usage() -> &'static str {
    "usage: computer <screenshot|observe|click X Y|double-click X Y|right-click X Y|drag X1 Y1 X2 Y2|scroll DX DY|type TEXT|key KEY|key MOD+KEY>"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::proto::modifier_bits::{ALT, CTRL, META, SHIFT};

    /// Parses `words` the way the terminal bridge does.
    fn parse(words: &[&str]) -> anyhow::Result<ComputerArgs> {
        let schema = serde_json::to_value(schemars::schema_for!(ComputerArgs)).unwrap();
        let words: Vec<String> = words.iter().map(|w| (*w).to_owned()).collect();
        Ok(serde_json::from_value(crate::ipc::cli_to_json(
            &schema, &words,
        )?)?)
    }

    fn parsed(words: &[&str]) -> (String, Vec<String>) {
        let args = parse(words).unwrap();
        (args.action, args.operands)
    }

    fn strings(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    #[test]
    fn documented_forms_parse_as_separate_arguments() {
        assert_eq!(
            parsed(&["click", "10", "20"]),
            ("click".into(), strings(&["10", "20"]))
        );
        assert_eq!(
            parsed(&["type", "hello", "world"]),
            ("type".into(), strings(&["hello", "world"]))
        );
        assert_eq!(
            parsed(&["key", "Enter"]),
            ("key".into(), strings(&["Enter"]))
        );
        assert_eq!(
            parsed(&["key", "cmd+l"]),
            ("key".into(), strings(&["cmd+l"]))
        );
        assert_eq!(parsed(&["observe"]), ("observe".into(), Vec::new()));
        assert_eq!(
            parsed(&["drag", "1", "2", "3", "4"]).1,
            strings(&["1", "2", "3", "4"])
        );
    }

    #[test]
    fn a_negative_number_is_an_operand() {
        assert_eq!(parsed(&["scroll", "0", "-300"]).1, strings(&["0", "-300"]));
    }

    #[test]
    fn key_combinations_parse_from_the_cli() {
        let (_, operands) = parsed(&["key", "cmd+shift+4"]);
        let press = KeyPress::parse(&operands[0]).unwrap();
        assert_eq!(press.code, "Digit4");
        assert_eq!(press.modifiers, META | SHIFT);
    }

    #[test]
    fn numeric_operands_are_checked() {
        assert_eq!(
            numbers("scroll DX DY", &strings(&["0", "-300"])),
            Ok(vec![0.0, -300.0])
        );
        let count = numbers("click X Y", &strings(&["1"])).unwrap_err();
        assert!(count.contains("`computer click X Y`"), "{count}");
        let word = numbers("click X Y", &strings(&["1", "a"])).unwrap_err();
        assert!(word.contains("`a` is not a number"), "{word}");
    }

    #[test]
    fn modifiers_map_to_the_wire_bits() {
        let press = |spec| KeyPress::parse(spec).unwrap();
        assert_eq!(
            press("Enter"),
            KeyPress {
                code: "Enter".into(),
                modifiers: 0
            }
        );
        assert_eq!(
            press("cmd+l"),
            KeyPress {
                code: "KeyL".into(),
                modifiers: META
            }
        );
        assert_eq!(press("Command+Space").modifiers, META);
        assert_eq!(press("META+1").code, "Digit1");
        assert_eq!(press("control+c").modifiers, CTRL);
        assert_eq!(press("Option+Tab").modifiers, ALT);
        assert_eq!(
            press("ctrl+shift+alt+cmd+Tab").modifiers,
            CTRL | SHIFT | ALT | META
        );
    }

    #[test]
    fn bad_key_specs_are_rejected() {
        let error = KeyPress::parse("hyper+l").unwrap_err();
        assert!(error.contains("`hyper`"), "{error}");
        assert!(error.contains("cmd") && error.contains("shift"), "{error}");
        assert!(KeyPress::parse("cmd+").is_err());
        assert!(KeyPress::parse("").is_err());
    }
}
