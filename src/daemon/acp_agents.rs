//! `/api/acp-agents`: the ACP agents connected as subagent types and the
//! installed ones that could be.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use skyzen::routing::Params;
use skyzen::utils::{Json, State};
use skyzen::{Result as SkyResult, StatusCode};

use crate::agent::acp_known::{KNOWN_ACP_AGENTS, KnownAcpAgent};
use crate::agent::acp_subagent::{
    AgentIdentity, ProbeError, TrialError, describe, probe_agent, try_connection,
};
use crate::client::acp::{
    AcpAgentsResponse, AcpChoice, AcpChoiceBody, ConnectAcpAgentRequest, ConnectedAcpAgent,
    DetectedAcpAgent,
};
use crate::config::{AcpAgentConfig, ConfigError, MayConfig, check_acp_agent_name};
use crate::ui_event_bus::UiEventTopics;

use super::error::{bad_request_error, internal_error, json_ok_response};
use super::state::DaemonState;
use super::types::ApiHealth;

/// How long a candidate command gets to answer ACP `initialize`.
const PROBE_LIMIT: Duration = Duration::from_secs(10);

/// Why a connection was not made.
#[derive(Debug, thiserror::Error)]
pub(super) enum ConnectError {
    #[error(transparent)]
    Name(#[from] ConfigError),
    #[error("the command must not be empty")]
    EmptyCommand,
    #[error("`{command}` is not an ACP agent: {source}")]
    NotAcp {
        command: String,
        source: Box<ProbeError>,
    },
    #[error("{message}")]
    Choice {
        choice: AcpChoice,
        valid: Vec<String>,
        message: String,
    },
    #[error("the agent refused a trial session: {0}")]
    Trial(acpsub::Error),
    #[error("the agent answered `initialize` but did not open a trial session within {0:?}")]
    TrialTimedOut(Duration),
    #[error("the trial session could not be prepared: {0}")]
    Scratch(std::io::Error),
    #[error("the sandbox's heel could not be prepared: {0}")]
    Heel(crate::tools::ToolRuntimeError),
    #[error("the connection could not be saved: {0}")]
    Save(ConfigError),
}

pub fn connected_dto(config: &MayConfig) -> Vec<ConnectedAcpAgent> {
    config
        .acp_agents
        .iter()
        .map(|(name, agent)| ConnectedAcpAgent {
            name: name.clone(),
            command: agent.command.clone(),
            args: agent.args.clone(),
            model: agent.model.clone(),
            mode: agent.mode.clone(),
            description: agent.description.clone(),
        })
        .collect()
}

/// The known agents installed on `PATH` that answer ACP `initialize`, each
/// probe running concurrently under [`PROBE_LIMIT`].
async fn detect(config: &MayConfig, heel: &heel::HeelBinary) -> Vec<DetectedAcpAgent> {
    let installed: Vec<KnownAcpAgent> = blocking::unblock(|| {
        KNOWN_ACP_AGENTS
            .iter()
            .copied()
            .filter(|known| which::which(known.command).is_ok())
            .collect()
    })
    .await;
    futures_util::stream::iter(installed)
        .map(|known: KnownAcpAgent| async move {
            let args: Vec<String> = known.args.iter().map(ToString::to_string).collect();
            match probe_agent(known.command, &args, PROBE_LIMIT, heel.clone()).await {
                Ok(AgentIdentity { name, version }) => {
                    let connected_as = config
                        .acp_agents
                        .iter()
                        .find(|(_, agent)| agent.command == known.command && agent.args == args)
                        .map(|(connected, _)| connected.clone());
                    Some(DetectedAcpAgent {
                        command: known.command.to_string(),
                        args,
                        name,
                        version,
                        connected_as,
                    })
                }
                Err(error) => {
                    tracing::debug!(command = known.command, %error, "installed command is not a usable ACP agent");
                    None
                }
            }
        })
        // Every known command probes at once.
        .buffered(KNOWN_ACP_AGENTS.len())
        .filter_map(std::future::ready)
        .collect()
        .await
}

pub(super) async fn list_acp_agents(
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<AcpAgentsResponse>> {
    let config = state.config_snapshot();
    let heel = state.heel().await.map_err(internal_error)?;
    let detected = detect(&config, &heel).await;
    Ok(Json(AcpAgentsResponse {
        connected: connected_dto(&config),
        detected,
    }))
}

/// Proves `request` works and connects it. Nothing is configured when it does
/// not.
pub(super) async fn connect(
    state: &DaemonState,
    request: ConnectAcpAgentRequest,
) -> Result<(), ConnectError> {
    check_acp_agent_name(&request.name)?;
    let command = request.command.trim().to_string();
    if command.is_empty() {
        return Err(ConnectError::EmptyCommand);
    }
    let heel = state.heel().await.map_err(ConnectError::Heel)?;
    let identity = probe_agent(&command, &request.args, PROBE_LIMIT, heel.clone())
        .await
        .map_err(|source| ConnectError::NotAcp {
            command: command.clone(),
            source: Box::new(source),
        })?;
    let agent = AcpAgentConfig {
        command,
        args: request.args,
        env: BTreeMap::new(),
        model: request.model,
        mode: request.mode,
        description: describe(&identity),
        grants: Vec::new(),
    };
    let trial_limit = state.config_snapshot().acp.trial_limit;
    match try_connection(
        &state.tokio_handle,
        &request.name,
        &agent,
        trial_limit,
        heel,
    )
    .await
    {
        Ok(()) => {}
        Err(TrialError::TimedOut(limit)) => return Err(ConnectError::TrialTimedOut(limit)),
        Err(TrialError::Scratch(error)) => return Err(ConnectError::Scratch(error)),
        Err(TrialError::Acpsub(error)) => {
            return Err(match error {
                acpsub::Error::ModelRequired { ref valid, .. } => ConnectError::Choice {
                    choice: AcpChoice::ModelRequired,
                    valid: valid.clone(),
                    message: error.to_string(),
                },
                acpsub::Error::ModeRequired { ref valid, .. } => ConnectError::Choice {
                    choice: AcpChoice::ModeRequired,
                    valid: valid.clone(),
                    message: error.to_string(),
                },
                other => ConnectError::Trial(other),
            });
        }
    }
    let updated = state.update_config(|config| {
        config
            .connect_acp_agent(request.name, agent)
            .expect("the name was checked above");
    });
    updated
        .save_to(&state.may_dir.join("config.toml"))
        .await
        .map_err(ConnectError::Save)?;
    state.notify_ui_events(UiEventTopics::CONFIG);
    Ok(())
}

pub(super) async fn connect_acp_agent(
    State(state): State<Arc<DaemonState>>,
    Json(request): Json<ConnectAcpAgentRequest>,
) -> SkyResult<skyzen::Response> {
    match connect(&state, request).await {
        Ok(()) => json_ok_response(&ApiHealth { ok: true }),
        Err(ConnectError::Choice {
            choice,
            valid,
            message,
        }) => {
            let body = AcpChoiceBody {
                code: choice,
                error: message,
                valid,
            };
            let mut response = json_ok_response(&body)?;
            *response.status_mut() = StatusCode::UNPROCESSABLE_ENTITY;
            Ok(response)
        }
        Err(error @ (ConnectError::Scratch(_) | ConnectError::Heel(_) | ConnectError::Save(_))) => {
            Err(super::error::internal_error_message(error.to_string()))
        }
        Err(error) => Err(bad_request_error(error)),
    }
}

pub(super) async fn disconnect_acp_agent(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiHealth>> {
    let name = params.get("name").map_err(bad_request_error)?;
    let updated = state.update_config(|config| {
        let _ = config.disconnect_acp_agent(name);
    });
    updated
        .save_to(&state.may_dir.join("config.toml"))
        .await
        .map_err(internal_error)?;
    state.notify_ui_events(UiEventTopics::CONFIG);
    Ok(Json(ApiHealth { ok: true }))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// The fixture started as a program: the probe and trial sandboxes grant
    /// only the program they start, never a script its arguments name.
    fn fake_agent(model: Option<&str>, mode: Option<&str>) -> ConnectAcpAgentRequest {
        ConnectAcpAgentRequest {
            name: "fake".to_string(),
            command: Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/fake_acp_agent.py")
                .to_string_lossy()
                .into_owned(),
            args: Vec::new(),
            model: model.map(str::to_string),
            mode: mode.map(str::to_string),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn connecting_an_agent_that_goes_silent_after_initialize_times_out() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut config = MayConfig::default();
        config.acp.trial_limit = Duration::from_millis(1500);
        let state = DaemonState::for_test(dir.path(), config)
            .await
            .expect("daemon state");

        let mut silent = fake_agent(Some("a"), Some("default"));
        silent.args = vec!["--silent-after-initialize".to_string()];
        let error = connect(&state, silent).await.expect_err("times out");
        assert!(
            matches!(error, ConnectError::TrialTimedOut(limit) if limit == Duration::from_millis(1500)),
            "{error}"
        );
        assert!(state.config_snapshot().acp_agents.is_empty());
    }

    /// The trial returns at its limit plus the time to kill the agent's tree
    /// and remove the scratch directory (measured at a few milliseconds). The
    /// margin leaves room for a loaded test run and stays far below the
    /// seconds a probe or sandbox start can take, which `connect` spends
    /// before the trial and the limit does not cover.
    const TRIAL_MARGIN: Duration = Duration::from_millis(750);

    #[tokio::test(flavor = "multi_thread")]
    async fn a_trial_with_a_silent_agent_returns_within_its_limit_and_margin() {
        crate::test_support::install_global_executor();
        let limit = Duration::from_millis(1500);
        let silent = fake_agent(Some("a"), Some("default"));
        let agent = AcpAgentConfig {
            command: silent.command,
            args: vec!["--silent-after-initialize".to_string()],
            env: BTreeMap::new(),
            model: silent.model,
            mode: silent.mode,
            description: String::new(),
            grants: Vec::new(),
        };
        let started = std::time::Instant::now();
        let error = try_connection(
            &tokio::runtime::Handle::current(),
            "fake",
            &agent,
            limit,
            crate::tools::test_heel().await,
        )
        .await
        .expect_err("times out");
        let elapsed = started.elapsed();
        assert!(
            matches!(error, TrialError::TimedOut(l) if l == limit),
            "{error}"
        );
        assert!(elapsed >= limit, "{elapsed:?}");
        assert!(elapsed < limit + TRIAL_MARGIN, "{elapsed:?}");
    }

    fn choice(error: ConnectError) -> (AcpChoice, Vec<String>) {
        match error {
            ConnectError::Choice { choice, valid, .. } => (choice, valid),
            other => panic!("expected a choice to be asked, got {other}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn connecting_asks_for_the_model_and_mode_then_configures_the_agent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let state = DaemonState::for_test(dir.path(), MayConfig::default())
            .await
            .expect("daemon state");

        // acpsub checks the mode first, then the model.
        let (kind, valid) = choice(
            connect(&state, fake_agent(None, None))
                .await
                .expect_err("mode"),
        );
        assert_eq!(kind, AcpChoice::ModeRequired);
        assert_eq!(valid, ["default", "bypass"]);

        let (kind, valid) = choice(
            connect(&state, fake_agent(None, Some("default")))
                .await
                .expect_err("model"),
        );
        assert_eq!(kind, AcpChoice::ModelRequired);
        assert_eq!(valid, ["a", "b"]);
        assert!(
            state.config_snapshot().acp_agents.is_empty(),
            "a refused trial configures nothing"
        );

        connect(&state, fake_agent(Some("a"), Some("default")))
            .await
            .expect("the agent connects");
        let connected = state.config_snapshot();
        let fake = &connected.acp_agents["fake"];
        assert_eq!(
            (fake.model.as_deref(), fake.mode.as_deref()),
            (Some("a"), Some("default"))
        );
        assert!(
            !fake.description.is_empty(),
            "the description comes from agentInfo"
        );
        let saved = MayConfig::load_from(&dir.path().join("config.toml"))
            .await
            .expect("the connection was written to the config file");
        assert_eq!(saved.acp_agents["fake"], *fake);

        let mut taken = fake_agent(Some("a"), Some("default"));
        taken.name = "plan".to_string();
        assert!(matches!(
            connect(&state, taken).await.expect_err("built-in name"),
            ConnectError::Name(_)
        ));
    }
}
