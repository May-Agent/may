use std::sync::Arc;

use aither_sandbox::TerminalMode;
use aither_sandbox::permission::{NoopPermissionHandler, PermissionError, PermissionHandler};

use crate::agent::PermissionApprover;
use crate::db::{NetworkAccessLogParams, NetworkAccessMode, NetworkAccessSource};
use crate::network_policy::CompiledNetworkPolicy;
use crate::session::SessionRuntimeService;
use crate::ui_event_bus::UiEventTopics;

/// Permission handler for terminal commands: routes approval requests to
/// the interactive approver and enforces the session's network policy.
#[derive(Debug, Clone)]
pub struct TerminalPermissionHandler {
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
            approver,
            runtime_service,
            session_id,
            network_policy,
            profile_id,
            source: NetworkAccessSource::Chat,
        }
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
    async fn check(&self, mode: TerminalMode, script: &str) -> Result<bool, PermissionError> {
        match &self.approver {
            Some(approver) => approver.check(mode, script).await,
            None => NoopPermissionHandler.check(mode, script).await,
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

    async fn will_wait_for_approval(&self, mode: TerminalMode, script: &str) -> bool {
        match &self.approver {
            Some(approver) => approver.will_wait_for_approval(mode, script).await,
            None => {
                NoopPermissionHandler
                    .will_wait_for_approval(mode, script)
                    .await
            }
        }
    }
}
