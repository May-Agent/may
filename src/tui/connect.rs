//! `/connect`: the picker that connects an installed ACP agent as a subagent
//! type.
//!
//! Detection and connection run in the daemon; this module holds what the
//! picker shows and the requests it sends, all off the event loop.

use std::path::Path;

use crossterm::event::KeyCode;

use crate::client::DaemonClientError;
use crate::client::acp::{
    AcpAgentsResponse, AcpChoice, AcpChoiceBody, ConnectAcpAgentRequest, ConnectedAcpAgent,
    DetectedAcpAgent,
};

use super::app::App;
use super::error::TuiError;
use super::state::AppMode;

/// What the picker is waiting for or showing.
#[derive(Debug)]
pub enum ConnectStage {
    /// The daemon is probing the installed agents.
    Detecting,
    /// The agents to pick from.
    Agents {
        /// Installed agents that speak ACP.
        detected: Vec<DetectedAcpAgent>,
        /// Agents already connected.
        connected: Vec<ConnectedAcpAgent>,
    },
    /// The daemon is running the trial session.
    Connecting(ConnectAcpAgentRequest),
    /// The agent advertises ids the user must choose from.
    Choose {
        /// The request to retry with the choice.
        request: ConnectAcpAgentRequest,
        /// What is being chosen.
        choice: AcpChoice,
        /// The advertised ids.
        valid: Vec<String>,
    },
}

/// State of the `/connect` picker.
#[derive(Debug)]
pub struct ConnectState {
    /// What the picker shows.
    pub stage: ConnectStage,
    /// Cursor in the stage's list.
    pub selected: usize,
}

impl ConnectState {
    /// The picker while detection runs.
    #[must_use]
    pub const fn detecting() -> Self {
        Self {
            stage: ConnectStage::Detecting,
            selected: 0,
        }
    }
}

/// How a connection attempt ended.
#[derive(Debug)]
pub enum ConnectOutcome {
    /// The agent is connected as `name`.
    Connected(String),
    /// The daemon needs a model or mode chosen; retry `request` with it.
    Choose {
        /// The request that was refused.
        request: ConnectAcpAgentRequest,
        /// What the daemon asks for.
        body: AcpChoiceBody,
    },
}

/// The subagent type name for `command`: its file name, unless another
/// command is connected under it, then the first free `-2`, `-3`, ….
///
/// # Panics
/// Never: among `connected.len() + 1` suffixes one is free.
#[must_use]
pub fn connection_name(command: &str, connected: &[ConnectedAcpAgent]) -> String {
    let base = Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command)
        .to_string();
    let free = |name: &str| {
        connected
            .iter()
            .all(|agent| agent.name != name || agent.command == command)
    };
    if free(&base) {
        return base;
    }
    (2..=connected.len() + 2)
        .map(|n| format!("{base}-{n}"))
        .find(|name| free(name))
        .expect("an unbounded range has a free name")
}

/// Splits the argument of `/connect <command> [args…]` the way a shell would.
///
/// # Errors
/// Returns a message when the quoting is unbalanced or no command is given.
pub fn parse_custom(rest: &str) -> Result<(String, Vec<String>), String> {
    let mut words = shlex::split(rest).ok_or("the command line has unbalanced quotes")?;
    if words.is_empty() {
        return Err("give the command that starts the agent".to_string());
    }
    let command = words.remove(0);
    Ok((command, words))
}

impl App {
    /// Opens the picker and has the daemon detect installed agents.
    pub fn open_connect_picker(&mut self) {
        self.completion.cancel();
        let Some(client) = self.btw_daemon.clone() else {
            self.chat.push_error("The May daemon is not connected");
            return;
        };
        self.mode = AppMode::Connect(ConnectState::detecting());
        let tx = self.event_tx.clone();
        spawn_detached(async move {
            let result = client
                .get::<AcpAgentsResponse>("/api/acp-agents")
                .await
                .map_err(TuiError::from);
            let _ = tx.send(super::AppEvent::AcpAgentsLoaded(result)).await;
        });
    }

    /// Connects `command args…` under a name derived from the command.
    pub fn connect_custom(&mut self, command: String, args: Vec<String>) {
        let Some(client) = self.btw_daemon.clone() else {
            self.chat.push_error("The May daemon is not connected");
            return;
        };
        let placeholder = ConnectAcpAgentRequest {
            name: String::new(),
            command: command.clone(),
            args: args.clone(),
            model: None,
            mode: None,
        };
        self.mode = AppMode::Connect(ConnectState {
            stage: ConnectStage::Connecting(placeholder),
            selected: 0,
        });
        let tx = self.event_tx.clone();
        spawn_detached(async move {
            let result = async {
                let listed = client.get::<AcpConfig>("/api/config").await?;
                let name = connection_name(&command, &listed.acp_agents);
                send_connection(
                    &client,
                    ConnectAcpAgentRequest {
                        name,
                        command,
                        args,
                        model: None,
                        mode: None,
                    },
                )
                .await
            }
            .await
            .map_err(TuiError::from);
            let _ = tx.send(super::AppEvent::AcpConnectFinished(result)).await;
        });
    }

    /// Sends `request` to the daemon.
    fn connect_request(&mut self, request: ConnectAcpAgentRequest) {
        let Some(client) = self.btw_daemon.clone() else {
            self.chat.push_error("The May daemon is not connected");
            self.mode = AppMode::Chat;
            return;
        };
        self.mode = AppMode::Connect(ConnectState {
            stage: ConnectStage::Connecting(request.clone()),
            selected: 0,
        });
        let tx = self.event_tx.clone();
        spawn_detached(async move {
            let result = send_connection(&client, request)
                .await
                .map_err(TuiError::from);
            let _ = tx.send(super::AppEvent::AcpConnectFinished(result)).await;
        });
    }

    /// The daemon's answer to the detection request.
    pub fn acp_agents_loaded(&mut self, result: Result<AcpAgentsResponse, TuiError>) {
        if !matches!(&self.mode, AppMode::Connect(state) if matches!(state.stage, ConnectStage::Detecting))
        {
            return;
        }
        match result {
            Ok(listed) => {
                self.mode = AppMode::Connect(ConnectState {
                    stage: ConnectStage::Agents {
                        detected: listed.detected,
                        connected: listed.connected,
                    },
                    selected: 0,
                });
            }
            Err(error) => {
                self.mode = AppMode::Chat;
                self.chat
                    .push_error(&format!("Failed to detect ACP agents: {error}"));
            }
        }
    }

    /// The daemon's answer to a connection request.
    pub fn acp_connect_finished(&mut self, result: Result<ConnectOutcome, TuiError>) {
        if !matches!(self.mode, AppMode::Connect(_)) {
            return;
        }
        match result {
            Ok(ConnectOutcome::Connected(name)) => {
                self.mode = AppMode::Chat;
                self.chat.push_notice(&format!(
                    "Connected `{name}`: the main agent can now start it with `subagent external --agent {name}`"
                ));
            }
            Ok(ConnectOutcome::Choose { request, body }) => {
                self.mode = AppMode::Connect(ConnectState {
                    stage: ConnectStage::Choose {
                        request,
                        choice: body.code,
                        valid: body.valid,
                    },
                    selected: 0,
                });
            }
            Err(error) => {
                self.mode = AppMode::Chat;
                self.chat
                    .push_error(&format!("Failed to connect the ACP agent: {error}"));
            }
        }
    }
}

/// Key handling for the `/connect` picker.
pub fn handle_connect_key(app: &mut App, code: KeyCode) {
    let AppMode::Connect(state) = &mut app.mode else {
        return;
    };
    let rows = match &state.stage {
        ConnectStage::Agents { detected, .. } => detected.len(),
        ConnectStage::Choose { valid, .. } => valid.len(),
        ConnectStage::Detecting | ConnectStage::Connecting(_) => 0,
    };
    match code {
        KeyCode::Esc => app.mode = AppMode::Chat,
        KeyCode::Up | KeyCode::Char('k') => state.selected = state.selected.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => {
            if state.selected + 1 < rows {
                state.selected += 1;
            }
        }
        KeyCode::Enter => match &mut state.stage {
            ConnectStage::Agents {
                detected,
                connected,
            } => {
                let Some(agent) = detected.get(state.selected) else {
                    return;
                };
                if let Some(name) = &agent.connected_as {
                    let text = format!("`{name}` is already connected");
                    app.chat.push_notice(&text);
                    app.mode = AppMode::Chat;
                    return;
                }
                let request = ConnectAcpAgentRequest {
                    name: connection_name(&agent.command, connected),
                    command: agent.command.clone(),
                    args: agent.args.clone(),
                    model: None,
                    mode: None,
                };
                app.connect_request(request);
            }
            ConnectStage::Choose {
                request,
                choice,
                valid,
            } => {
                let Some(picked) = valid.get(state.selected).cloned() else {
                    return;
                };
                let mut request = request.clone();
                match choice {
                    AcpChoice::ModelRequired => request.model = Some(picked),
                    AcpChoice::ModeRequired => request.mode = Some(picked),
                }
                app.connect_request(request);
            }
            ConnectStage::Detecting | ConnectStage::Connecting(_) => {}
        },
        _ => {}
    }
}

/// The part of `/api/config` that names the connected agents.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AcpConfig {
    acp_agents: Vec<ConnectedAcpAgent>,
}

/// Posts `request`; a `422` from the daemon asks for a choice.
async fn send_connection(
    client: &crate::client::DaemonClient,
    request: ConnectAcpAgentRequest,
) -> Result<ConnectOutcome, DaemonClientError> {
    match client
        .post::<_, serde_json::Value>("/api/acp-agents", &request)
        .await
    {
        Ok(_) => Ok(ConnectOutcome::Connected(request.name)),
        Err(DaemonClientError::Status {
            status, message, ..
        }) if status == hyper::StatusCode::UNPROCESSABLE_ENTITY => {
            let body = serde_json::from_str::<AcpChoiceBody>(&message)?;
            Ok(ConnectOutcome::Choose { request, body })
        }
        Err(error) => Err(error),
    }
}

fn spawn_detached(future: impl std::future::Future<Output = ()> + Send + 'static) {
    executor_core::spawn(future).detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connected(name: &str, command: &str) -> ConnectedAcpAgent {
        ConnectedAcpAgent {
            name: name.to_string(),
            command: command.to_string(),
            args: Vec::new(),
            model: None,
            mode: None,
            description: String::new(),
        }
    }

    #[test]
    fn a_command_is_named_by_its_file_name_unless_another_command_holds_it() {
        assert_eq!(connection_name("/opt/bin/devin", &[]), "devin");
        let taken = [connected("devin", "/usr/bin/devin")];
        assert_eq!(connection_name("/opt/bin/devin", &taken), "devin-2");
        assert_eq!(connection_name("/usr/bin/devin", &taken), "devin");
    }

    #[test]
    fn a_custom_command_line_splits_like_a_shell() {
        let (command, args) = parse_custom(r#"node "my agent.js" --acp"#).expect("parses");
        assert_eq!(command, "node");
        assert_eq!(args, ["my agent.js", "--acp"]);
        parse_custom("").expect_err("no command");
        parse_custom("node 'unclosed").expect_err("unbalanced quote");
    }
}
