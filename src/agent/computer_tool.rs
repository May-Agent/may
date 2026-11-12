//! `computer` command: the agent sees and drives its session's virtual
//! desktop — screenshots/observe, clicks, drags, scrolls, typing.

use std::borrow::Cow;
use std::sync::Arc;

use aither_core::llm::tool::ToolResultPart;
use aither_core::llm::{Tool, ToolResult};
use askama::Template;
use schemars::JsonSchema;
use serde::Deserialize;

use waterkit_automation::ElementRef;

use crate::computer::proto::{
    ElementListing, ElementPoint, InputMessage, KeyboardEventKind, KeyboardInput, LaunchedWindow,
    ListedElement, MouseButtonInput, MouseEventKind, MouseInput,
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
    /// - `observe`: the screen, its size and the actionable elements, each
    ///   with a reference such as `0a3f91c2:12`, valid until the next observe
    /// - `screenshot`: just the screen image
    /// - `click REF`: press the element a reference from `observe` names
    /// - `click X Y`, `double-click X Y`, `right-click X Y`
    /// - `drag X1 Y1 X2 Y2`: press at the start, release at the end
    /// - `scroll DX DY`: pixel deltas at the pointer
    /// - `type REF TEXT`: focus the element REF names, then type TEXT
    /// - `type TEXT`: type at the focused element
    /// - `key KEY` or `key MOD+MOD+KEY`: KEY is a W3C `KeyboardEvent.code`
    ///   name, exactly as written there (`Enter`, `Tab`, `Escape`,
    ///   `Backspace`, `Delete`, `ArrowLeft`/`ArrowRight`/`ArrowUp`/`ArrowDown`,
    ///   `Home`, `End`, `PageUp`, `PageDown`, `F5`), or a single letter or
    ///   digit; `cmd+l`, `cmd+shift+4`
    /// - `launch APP [URL ...]`: open an application on the desktop without
    ///   taking the user's focus; APP is its name, bundle id or `.app` path
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

/// The actions `computer` accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Screenshot,
    Observe,
    Click,
    DoubleClick,
    RightClick,
    Drag,
    Scroll,
    Type,
    Key,
    Launch,
}

impl Action {
    /// Every action, in the order `usage` lists them.
    const ALL: [Self; 10] = [
        Self::Screenshot,
        Self::Observe,
        Self::Click,
        Self::DoubleClick,
        Self::RightClick,
        Self::Drag,
        Self::Scroll,
        Self::Type,
        Self::Key,
        Self::Launch,
    ];

    /// The word that names the action on the command line.
    const fn name(self) -> &'static str {
        match self {
            Self::Screenshot => "screenshot",
            Self::Observe => "observe",
            Self::Click => "click",
            Self::DoubleClick => "double-click",
            Self::RightClick => "right-click",
            Self::Drag => "drag",
            Self::Scroll => "scroll",
            Self::Type => "type",
            Self::Key => "key",
            Self::Launch => "launch",
        }
    }

    fn parse(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.name() == word)
    }
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

/// An action aimed at an element by its reference.
#[derive(Debug, PartialEq, Eq)]
enum ElementAction {
    /// `click REF`.
    Press(ElementRef),
    /// `type REF TEXT`.
    TypeInto(ElementRef, String),
}

/// The element form of `click` and `type`: `click REF` (one operand), and
/// `type REF TEXT` (a first operand in a reference's text form). `None` is
/// the coordinate or focused-element form.
fn element_action(kind: Action, rest: &[String]) -> Result<Option<ElementAction>, String> {
    match (kind, rest) {
        (Action::Click, [reference]) => reference
            .parse()
            .map(|element| Some(ElementAction::Press(element)))
            .map_err(|e| format!("expected `computer click REF` or `computer click X Y`: {e}")),
        (Action::Type, [first, text @ ..]) => match first.parse() {
            Ok(_) if text.is_empty() => {
                Err("`computer type REF TEXT` needs the text to type".to_owned())
            }
            Ok(element) => Ok(Some(ElementAction::TypeInto(element, text.join(" ")))),
            Err(_) => Ok(None),
        },
        _ => Ok(None),
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
    /// the point: an agent asking for its desktop gets one. Answers its
    /// size in points.
    async fn ensure_desktop(&self) -> Result<(u32, u32), String> {
        match self
            .handles
            .agent
            .request(AgentRequest::EnsureDisplay {
                session_id: self.session_id.clone(),
            })
            .await?
        {
            AgentResponse::Display { width, height, .. } => Ok((width, height)),
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
        let Some(kind) = Action::parse(action) else {
            return Err(usage().to_owned());
        };
        if let Some(element_action) = element_action(kind, rest)? {
            return self.act_on_element(element_action).await;
        }
        match kind {
            Action::Screenshot => self.screenshot().await,
            Action::Observe => self.observe().await,
            Action::Click
            | Action::DoubleClick
            | Action::RightClick
            | Action::Drag
            | Action::Scroll
            | Action::Type
            | Action::Key => self.drive(action, rest).await,
            Action::Launch => self.launch(rest).await,
        }
    }

    async fn launch(&self, rest: &[String]) -> Result<ToolResult, String> {
        let (app, items) = launch_operands(rest)?;
        self.check_agent_owns_desktop().await?;
        self.ensure_desktop().await?;
        match self
            .handles
            .agent
            .request(AgentRequest::Launch {
                session_id: self.session_id.clone(),
                app: app.clone(),
                items: items.to_vec(),
            })
            .await?
        {
            AgentResponse::Launched { app } => LaunchedText {
                bundle_id: &app.bundle_id,
                pid: app.pid,
                windows: &app.windows,
            }
            .render()
            .map(ToolResult::text)
            .map_err(|e| e.to_string()),
            AgentResponse::Error { error } => Err(error),
            _ => Err("unexpected reply from computer-agent".to_owned()),
        }
    }

    async fn screenshot(&self) -> Result<ToolResult, String> {
        self.ensure_desktop().await?;
        Ok(ToolResult::image(self.png().await?, "image/png"))
    }

    /// The screen, its size and its actionable elements. The screenshot and
    /// the listing are taken together.
    async fn observe(&self) -> Result<ToolResult, String> {
        let (width, height) = self.ensure_desktop().await?;
        let (png, listing) = futures_lite::future::zip(self.png(), self.elements()).await;
        let (png, listing) = (png?, listing?);
        let text = ObservedText::new(width, height, &listing)
            .render()
            .map_err(|e| e.to_string())?;
        Ok(ToolResult::parts(vec![
            ToolResultPart::text(text),
            ToolResultPart::image(png, "image/png"),
        ]))
    }

    /// The display's current screen as PNG.
    async fn png(&self) -> Result<Vec<u8>, String> {
        match self
            .handles
            .agent
            .request(AgentRequest::Screenshot {
                session_id: self.session_id.clone(),
            })
            .await?
        {
            AgentResponse::Png { data } => Ok(data),
            AgentResponse::Error { error } => Err(error),
            _ => Err("unexpected reply from computer-agent".to_owned()),
        }
    }

    /// A fresh element listing; the previous one's references stop
    /// resolving.
    async fn elements(&self) -> Result<ElementListing, String> {
        match self
            .handles
            .agent
            .request(AgentRequest::Elements {
                session_id: self.session_id.clone(),
            })
            .await?
        {
            AgentResponse::Elements { listing } => Ok(listing),
            AgentResponse::Error { error } => Err(error),
            _ => Err("unexpected reply from computer-agent".to_owned()),
        }
    }

    async fn act_on_element(&self, action: ElementAction) -> Result<ToolResult, String> {
        self.check_agent_owns_desktop().await?;
        self.ensure_desktop().await?;
        let session_id = self.session_id.clone();
        let (request, done) = match action {
            ElementAction::Press(element) => (
                AgentRequest::Press {
                    session_id,
                    element,
                },
                "done: click",
            ),
            ElementAction::TypeInto(element, text) => (
                AgentRequest::TypeInto {
                    session_id,
                    element,
                    text,
                },
                "done: type",
            ),
        };
        match self.handles.agent.request(request).await? {
            AgentResponse::Ok => Ok(ToolResult::text(done)),
            AgentResponse::StaleElement { error } => Err(StaleElementText { error: &error }
                .render()
                .map_err(|e| e.to_string())?),
            AgentResponse::Error { error } => Err(error),
            _ => Err("unexpected reply from computer-agent".to_owned()),
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

/// Splits `launch`'s operands into the application and what it opens.
fn launch_operands(rest: &[String]) -> Result<(&String, &[String]), String> {
    rest.split_first().ok_or_else(|| {
        "expected `computer launch APP [URL ...]`: APP is an application's name, bundle id or .app path".to_owned()
    })
}

/// What `computer launch` tells the model.
#[derive(Template)]
#[template(path = "computer_launched.txt", escape = "none")]
struct LaunchedText<'a> {
    bundle_id: &'a str,
    pid: i32,
    windows: &'a [LaunchedWindow],
}

/// What `computer observe` tells the model beside the screenshot.
#[derive(Template)]
#[template(path = "computer_observed.txt", escape = "none")]
struct ObservedText<'a> {
    width: u32,
    height: u32,
    elements: Vec<ElementLine<'a>>,
    truncated: bool,
    skipped: usize,
}

impl<'a> ObservedText<'a> {
    fn new(width: u32, height: u32, listing: &'a ElementListing) -> Self {
        Self {
            width,
            height,
            elements: listing.elements.iter().map(ElementLine::new).collect(),
            truncated: listing.truncated,
            skipped: listing.skipped,
        }
    }
}

/// One element's line of the observe listing.
struct ElementLine<'a> {
    reference: ElementRef,
    /// The subrole when it has one, which is the more specific (a secure or
    /// search field), else the role; without the `AX` prefix.
    role: &'a str,
    /// Its assistive label, else its title.
    name: Option<Quoted<'a>>,
    value: Option<Quoted<'a>>,
    placeholder: Option<Quoted<'a>>,
    center: Option<ElementPoint>,
    enabled: bool,
}

impl<'a> ElementLine<'a> {
    fn new(element: &'a ListedElement) -> Self {
        let role = element.subrole.as_deref().unwrap_or(&element.role);
        let text = |text: &'a Option<String>| text.as_deref().filter(|text| !text.is_empty());
        Self {
            reference: element.reference,
            role: role.strip_prefix("AX").unwrap_or(role),
            name: text(&element.label)
                .or_else(|| text(&element.title))
                .map(Quoted),
            value: text(&element.value).map(Quoted),
            placeholder: text(&element.placeholder).map(Quoted),
            center: element.center,
            enabled: element.enabled,
        }
    }
}

/// How many characters of an element's text the listing shows: the model
/// reads it every turn, and a name or value past this is cut with `…`.
const QUOTED_CHARS: usize = 80;

/// An element's text in quotes, escaped, cut at [`QUOTED_CHARS`].
struct Quoted<'a>(&'a str);

impl std::fmt::Display for Quoted<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (shown, cut) = match self.0.char_indices().nth(QUOTED_CHARS) {
            Some((at, _)) => (self.0.split_at(at).0, "…"),
            None => (self.0, ""),
        };
        write!(f, "\"{}{cut}\"", shown.escape_debug())
    }
}

/// What a refused action on a stale or unknown reference tells the model.
#[derive(Template)]
#[template(path = "computer_stale_element.txt", escape = "none")]
struct StaleElementText<'a> {
    error: &'a str,
}

const fn usage() -> &'static str {
    "usage: computer <screenshot|observe|click REF|click X Y|double-click X Y|right-click X Y|drag X1 Y1 X2 Y2|scroll DX DY|type REF TEXT|type TEXT|key KEY|key MOD+KEY|launch APP [URL ...]>"
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
    fn every_action_the_skill_names_is_accepted() {
        let skill = include_str!("../../skills/computer/SKILL.md");
        let named: Vec<&str> = skill
            .split('`')
            .skip(1)
            .step_by(2)
            .filter_map(|span| span.strip_prefix("computer "))
            .filter_map(|rest| rest.split_whitespace().next())
            .filter(|word| !word.starts_with('-'))
            .collect();
        assert!(named.len() >= 4, "{named:?}");
        for word in named {
            assert!(
                Action::parse(word).is_some(),
                "the skill names `computer {word}`, which the command does not accept"
            );
        }
    }

    #[test]
    fn usage_lists_every_action() {
        for action in Action::ALL {
            assert!(usage().contains(action.name()), "{}", action.name());
        }
    }

    #[test]
    fn launch_takes_an_app_and_urls() {
        assert_eq!(
            parsed(&["launch", "Google Chrome", "https://example.com"]),
            (
                "launch".into(),
                strings(&["Google Chrome", "https://example.com"])
            )
        );
    }

    #[test]
    fn launch_without_an_app_names_the_form() {
        let (action, operands) = parsed(&["launch"]);
        assert_eq!(action, "launch");
        let error = launch_operands(&operands).unwrap_err();
        assert!(error.contains("`computer launch APP [URL ...]`"), "{error}");
    }

    #[test]
    fn launched_text_reports_app_pid_and_windows() {
        let text = LaunchedText {
            bundle_id: "com.google.Chrome",
            pid: 42,
            windows: &[LaunchedWindow {
                id: 7,
                x: 10.0,
                y: 20.0,
                width: 800.0,
                height: 600.0,
            }],
        }
        .render()
        .unwrap();
        assert!(text.contains("com.google.Chrome (pid 42)"), "{text}");
        assert!(text.contains("Window 7: 800x600 at (10, 20)"), "{text}");
    }

    fn reference(text: &str) -> ElementRef {
        text.parse().unwrap()
    }

    fn act(words: &[&str]) -> Result<Option<ElementAction>, String> {
        let (action, operands) = parsed(words);
        element_action(Action::parse(&action).unwrap(), &operands)
    }

    #[test]
    fn click_takes_a_reference_or_coordinates() {
        assert_eq!(
            act(&["click", "0a3f91c2:12"]),
            Ok(Some(ElementAction::Press(reference("0a3f91c2:12"))))
        );
        assert_eq!(act(&["click", "10", "20"]), Ok(None));
        let error = act(&["click", "10"]).unwrap_err();
        assert!(error.contains("`computer click REF`"), "{error}");
        assert!(error.contains("`computer click X Y`"), "{error}");
        assert!(error.contains("not an element reference"), "{error}");
    }

    #[test]
    fn type_takes_a_reference_before_its_text() {
        assert_eq!(
            act(&["type", "0a3f91c2:3", "山田", "太郎"]),
            Ok(Some(ElementAction::TypeInto(
                reference("0a3f91c2:3"),
                "山田 太郎".to_owned()
            )))
        );
        assert_eq!(
            act(&["type", "0000ffff:0", "こんにちは 안녕하세요"]),
            Ok(Some(ElementAction::TypeInto(
                reference("0000ffff:0"),
                "こんにちは 안녕하세요".to_owned()
            )))
        );
        assert_eq!(act(&["type", "你好", "世界"]), Ok(None));
        assert_eq!(act(&["type", "12:30"]), Ok(None));
        let error = act(&["type", "0a3f91c2:3"]).unwrap_err();
        assert!(error.contains("`computer type REF TEXT`"), "{error}");
    }

    #[test]
    fn other_actions_have_no_element_form() {
        assert_eq!(act(&["double-click", "0a3f91c2:1"]), Ok(None));
        assert_eq!(act(&["observe"]), Ok(None));
    }

    fn listed(
        text: &str,
        role: &str,
        subrole: Option<&str>,
        texts: [Option<&str>; 4],
        center: Option<(f64, f64)>,
        enabled: bool,
    ) -> ListedElement {
        let [title, label, value, placeholder] = texts.map(|t| t.map(str::to_owned));
        ListedElement {
            reference: reference(text),
            role: role.to_owned(),
            subrole: subrole.map(str::to_owned),
            title,
            label,
            value,
            placeholder,
            enabled,
            center: center.map(|(x, y)| ElementPoint { x, y }),
        }
    }

    #[test]
    fn observe_lists_each_element_on_one_line() {
        let listing = ElementListing {
            elements: vec![
                listed(
                    "0a3f91c2:0",
                    "AXTextField",
                    None,
                    [None, Some("名前 / 이름"), Some("山田太郎"), None],
                    Some((640.4, 311.6)),
                    true,
                ),
                listed(
                    "0a3f91c2:1",
                    "AXTextField",
                    Some("AXSecureTextField"),
                    [Some("パスワード"), Some(""), Some(""), Some("8文字以上")],
                    Some((640.0, 360.0)),
                    true,
                ),
                listed(
                    "0a3f91c2:2",
                    "AXButton",
                    None,
                    [Some("送信"), None, None, None],
                    Some((700.0, 420.0)),
                    false,
                ),
                listed(
                    "0a3f91c2:3",
                    "AXLink",
                    None,
                    [Some("帮助"), None, None, None],
                    None,
                    true,
                ),
            ],
            truncated: false,
            skipped: 0,
        };
        let text = ObservedText::new(1920, 1200, &listing).render().unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "The attached image is the session desktop's current screen, 1920x1200 points."
        );
        assert_eq!(
            &lines[2..],
            [
                r#"0a3f91c2:0 TextField "名前 / 이름" = "山田太郎" at (640, 312)"#,
                r#"0a3f91c2:1 SecureTextField "パスワード" empty, hint "8文字以上" at (640, 360)"#,
                r#"0a3f91c2:2 Button "送信" at (700, 420), disabled"#,
                r#"0a3f91c2:3 Link "帮助" with no position"#,
            ]
        );
        assert!(!text.contains("incomplete"), "{text}");
    }

    #[test]
    fn an_incomplete_listing_is_noted_in_one_line() {
        let mut listing = ElementListing {
            elements: Vec::new(),
            truncated: true,
            skipped: 2,
        };
        let text = ObservedText::new(800, 600, &listing).render().unwrap();
        assert!(
            text.contains("No actionable elements were listed"),
            "{text}"
        );
        assert_eq!(text.lines().filter(|l| l.contains("incomplete")).count(), 1);
        assert!(text.contains("and 2 element reads failed"), "{text}");

        listing.truncated = false;
        let text = ObservedText::new(800, 600, &listing).render().unwrap();
        assert!(!text.contains("incomplete"), "{text}");
        assert!(text.contains("2 element reads failed, so the listing may leave elements out"));

        listing.skipped = 0;
        let text = ObservedText::new(800, 600, &listing).render().unwrap();
        assert_eq!(text.lines().count(), 2, "{text}");
    }

    #[test]
    fn element_text_is_escaped_and_cut() {
        assert_eq!(
            Quoted("一行\n二行 \"引用\"").to_string(),
            r#""一行\n二行 \"引用\"""#
        );
        let long = "あ".repeat(QUOTED_CHARS + 5);
        let shown = Quoted(&long).to_string();
        assert_eq!(shown, format!("\"{}…\"", "あ".repeat(QUOTED_CHARS)));
        let exact = "한".repeat(QUOTED_CHARS);
        assert_eq!(Quoted(&exact).to_string(), format!("\"{exact}\""));
    }

    #[test]
    fn a_stale_reference_is_told_to_observe_again() {
        let text = StaleElementText {
            error: "element 0a3f91c2:12 no longer exists in its application",
        }
        .render()
        .unwrap();
        assert!(
            text.contains("element 0a3f91c2:12 no longer exists"),
            "{text}"
        );
        assert!(text.contains("Run `computer observe` again"), "{text}");
    }

    #[test]
    fn element_requests_carry_the_reference_text_form() {
        let request = AgentRequest::TypeInto {
            session_id: "s".to_owned(),
            element: reference("0a3f91c2:12"),
            text: "東京".to_owned(),
        };
        let wire = serde_json::to_string(&request).unwrap();
        assert!(wire.contains(r#""element":"0a3f91c2:12""#), "{wire}");
        let AgentRequest::TypeInto { element, text, .. } = serde_json::from_str(&wire).unwrap()
        else {
            panic!("expected type_into: {wire}");
        };
        assert_eq!((element, text.as_str()), (reference("0a3f91c2:12"), "東京"));
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
