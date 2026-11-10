use std::sync::Arc;

use aither_sandbox::permission::{NoopPermissionHandler, PermissionError, PermissionHandler};
use aither_sandbox::{
    CommandLocation, CommandRole, LocationApproval, ScriptLanguage, SshServer, TerminalMode,
};

use crate::agent::PermissionApprover;
use crate::agent::ssh_authorizer::MaySshAuthorizer;
use crate::db::{NetworkAccessLogParams, NetworkAccessMode, NetworkAccessSource};
use crate::network_policy::CompiledNetworkPolicy;
use crate::session::SessionRuntimeService;
use crate::ui_event_bus::UiEventTopics;

/// How the first command on an SSH server is decided.
#[derive(Debug, Clone)]
enum SshLocations {
    /// No server is approved.
    Deny,
    /// Every server is approved.
    Allow,
    /// The session's user is asked; a subagent's request goes through the
    /// same session, so the main agent's user answers for it.
    Ask(Arc<MaySshAuthorizer>),
}

/// Permission handler for terminal commands: routes approval requests to
/// the interactive approver and enforces the session's network policy.
#[derive(Debug, Clone)]
pub struct TerminalPermissionHandler {
    ssh: SshLocations,
    approver: Option<PermissionApprover>,
    runtime_service: Option<Arc<SessionRuntimeService>>,
    session_id: Option<String>,
    network_policy: Option<CompiledNetworkPolicy>,
    profile_id: Option<String>,
    source: NetworkAccessSource,
}

impl TerminalPermissionHandler {
    /// Create the handler used inside an agent session.
    #[must_use]
    pub fn for_agent(
        approver: Option<PermissionApprover>,
        network_policy: Option<CompiledNetworkPolicy>,
        profile_id: Option<String>,
    ) -> Self {
        let runtime_service = approver
            .as_ref()
            .and_then(PermissionApprover::runtime_service);
        let session_id = approver
            .as_ref()
            .and_then(PermissionApprover::session_id)
            .map(str::to_string);
        Self {
            ssh: SshLocations::Deny,
            approver,
            runtime_service,
            session_id,
            network_policy,
            profile_id,
            source: NetworkAccessSource::Chat,
        }
    }

    /// Ask the user through `authorizer` before the first command on an SSH
    /// server.
    #[must_use]
    pub fn with_ssh_authorizer(mut self, authorizer: Arc<MaySshAuthorizer>) -> Self {
        self.ssh = SshLocations::Ask(authorizer);
        self
    }

    /// Create the handler used for one-shot `may exec` runs.
    #[must_use]
    pub const fn for_exec(
        session_id: String,
        runtime_service: Arc<SessionRuntimeService>,
        network_policy: Option<CompiledNetworkPolicy>,
        profile_id: Option<String>,
    ) -> Self {
        Self {
            ssh: SshLocations::Allow,
            approver: None,
            runtime_service: Some(runtime_service),
            session_id: Some(session_id),
            network_policy,
            profile_id,
            source: NetworkAccessSource::Exec,
        }
    }

    async fn append_network_audit_log(
        &self,
        domain: &str,
        port: u16,
        allowed: bool,
        reason: Option<&str>,
    ) {
        let Some(runtime_service) = self.runtime_service.clone() else {
            tracing::warn!(
                domain,
                port,
                allowed,
                source = %self.source,
                "skipping network audit append because runtime service is missing"
            );
            return;
        };
        match runtime_service
            .db()
            .append_network_access_log(NetworkAccessLogParams {
                session_id: self.session_id.as_deref(),
                profile_id: self.profile_id.as_deref(),
                source: self.source,
                mode: NetworkAccessMode::Network,
                domain,
                port,
                allowed,
                reason,
            })
            .await
        {
            Ok(_) => {
                runtime_service.publish_ui_events(UiEventTopics::NETWORK_ACCESS_LOGS);
                tracing::debug!(
                    domain,
                    port,
                    allowed,
                    source = %self.source,
                    "appended network audit log"
                );
            }
            Err(error) => {
                tracing::error!(
                    domain,
                    port,
                    allowed,
                    source = %self.source,
                    error = %error,
                    "failed to append network audit log"
                );
            }
        }
    }
}

impl PermissionHandler for TerminalPermissionHandler {
    async fn check(
        &self,
        mode: TerminalMode,
        script: &str,
        language: ScriptLanguage,
        location: CommandLocation<'_>,
    ) -> Result<bool, PermissionError> {
        match &self.approver {
            Some(approver) => approver.check(mode, script, language, location).await,
            None => {
                NoopPermissionHandler
                    .check(mode, script, language, location)
                    .await
            }
        }
    }

    async fn check_domain(&self, domain: &str, port: u16) -> bool {
        if let Some(policy) = &self.network_policy {
            let allowed_by_policy = policy.allows_domain(domain);
            tracing::info!(
                domain,
                port,
                mode = %policy.mode().as_str(),
                allowed_by_policy,
                "network policy evaluation for outbound domain"
            );
            self.append_network_audit_log(
                domain,
                port,
                allowed_by_policy,
                Some(policy.mode().as_str()),
            )
            .await;
            if !allowed_by_policy {
                return false;
            }
        }

        let allowed = match &self.approver {
            Some(approver) => approver.check_domain(domain, port).await,
            None => NoopPermissionHandler.check_domain(domain, port).await,
        };
        self.append_network_audit_log(domain, port, allowed, None)
            .await;
        allowed
    }

    fn for_subagent(&self) -> Option<Self> {
        self.approver.as_ref().map(|approver| Self {
            ssh: self.ssh.clone(),
            approver: Some(approver.clone().for_subagent()),
            runtime_service: self.runtime_service.clone(),
            session_id: self.session_id.clone(),
            network_policy: self.network_policy.clone(),
            profile_id: self.profile_id.clone(),
            source: self.source,
        })
    }

    async fn check_location(
        &self,
        server: &SshServer,
        _role: CommandRole,
    ) -> Result<LocationApproval, PermissionError> {
        let approved = match &self.ssh {
            SshLocations::Deny => false,
            SshLocations::Allow => true,
            SshLocations::Ask(authorizer) => authorizer.authorize_connect(&server.target).await?,
        };
        Ok(if approved {
            LocationApproval::Session
        } else {
            LocationApproval::Refused
        })
    }

    async fn will_wait_for_location(&self, _server: &SshServer, _role: CommandRole) -> bool {
        match &self.ssh {
            SshLocations::Deny | SshLocations::Allow => false,
            SshLocations::Ask(authorizer) => authorizer.will_ask().await.unwrap_or_else(|error| {
                tracing::warn!(%error, "failed to read the session yolo switch");
                false
            }),
        }
    }

    async fn will_wait_for_approval(
        &self,
        mode: TerminalMode,
        script: &str,
        language: ScriptLanguage,
        location: CommandLocation<'_>,
    ) -> bool {
        match &self.approver {
            Some(approver) => {
                approver
                    .will_wait_for_approval(mode, script, language, location)
                    .await
            }
            None => {
                NoopPermissionHandler
                    .will_wait_for_approval(mode, script, language, location)
                    .await
            }
        }
    }
}
