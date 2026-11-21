use std::future::Future;
use std::sync::Arc;

use aither_core::llm::{Approval, ApprovalRequest, Approver, ApproverFailure};
use aither_sandbox::{DomainApprover, DomainRequest};
use arc_swap::ArcSwap;

use crate::agent::PermissionApprover;
use crate::agent::permission::{PermissionError, SSH_KIND, UNSANDBOXED_KIND};
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

/// What decides a session's approvals and its network connections.
///
/// The [`Approver`] answers scripts that leave the sandbox, and the
/// [`DomainApprover`] enforces the network policy and audits every
/// connection.
#[derive(Debug, Clone)]
pub struct TerminalPermissionHandler {
    ssh: SshLocations,
    approver: Option<PermissionApprover>,
    /// The approver a subagent's escalation goes through, derived once: the
    /// subagent shells a session builds share the handler aither hands them,
    /// so the boundary is enforced per request by the call's agent identity.
    subagent_approver: Arc<std::sync::OnceLock<Option<PermissionApprover>>>,
    /// The requests subagents put to the agent that launched them: a child's
    /// unsandboxed escalation waits on its parent's `subagent approve`/`deny`
    /// before it reaches the session's user.
    child_approvals: crate::subagent::ChildApprovals,
    runtime_service: Option<Arc<SessionRuntimeService>>,
    session_id: Option<String>,
    network_policy: Arc<ArcSwap<Option<CompiledNetworkPolicy>>>,
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
            subagent_approver: Arc::new(std::sync::OnceLock::new()),
            child_approvals: crate::subagent::ChildApprovals::new(),
            runtime_service,
            session_id,
            network_policy: Arc::new(ArcSwap::from_pointee(network_policy)),
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
    pub fn for_exec(
        session_id: String,
        runtime_service: Arc<SessionRuntimeService>,
        network_policy: Option<CompiledNetworkPolicy>,
        profile_id: Option<String>,
    ) -> Self {
        Self {
            ssh: SshLocations::Allow,
            approver: None,
            subagent_approver: Arc::new(std::sync::OnceLock::new()),
            child_approvals: crate::subagent::ChildApprovals::new(),
            runtime_service: Some(runtime_service),
            session_id: Some(session_id),
            network_policy: Arc::new(ArcSwap::from_pointee(network_policy)),
            profile_id,
            source: NetworkAccessSource::Exec,
        }
    }

    /// Replaces the live outbound network policy used by subsequent requests.
    pub fn set_network_policy(&self, policy: Option<CompiledNetworkPolicy>) {
        self.network_policy.store(Arc::new(policy));
    }

    /// Shares `approvals` with the session's `subagent` command: the requests
    /// subagents put to their parents park there and this handler waits on
    /// them.
    #[must_use]
    pub fn with_child_approvals(mut self, approvals: crate::subagent::ChildApprovals) -> Self {
        self.child_approvals = approvals;
        self
    }

    /// The registry the session's `subagent` command answers.
    #[must_use]
    pub const fn child_approvals(&self) -> &crate::subagent::ChildApprovals {
        &self.child_approvals
    }

    /// The handler a subagent's approvals go through: the same session's
    /// user answers, under the subagent's own deadline and agent id.
    #[must_use]
    pub fn for_subagent(&self) -> Self {
        Self {
            approver: self.approver.clone().map(PermissionApprover::for_subagent),
            ..self.clone()
        }
    }

    /// Whether a script on an SSH server is asked about now: the session's
    /// user is asked and its yolo switch is off.
    pub async fn will_wait_for_location(&self) -> bool {
        match &self.ssh {
            SshLocations::Deny | SshLocations::Allow => false,
            SshLocations::Ask(authorizer) => authorizer.will_ask().await.unwrap_or_else(|error| {
                tracing::warn!(%error, "failed to read the session yolo switch");
                false
            }),
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
                    error = %error,
                    "failed to append network audit log"
                );
            }
        }
    }

    async fn authorize_server(&self, target: &str) -> Result<bool, PermissionError> {
        match &self.ssh {
            SshLocations::Deny => Ok(false),
            SshLocations::Allow => Ok(true),
            SshLocations::Ask(authorizer) => authorizer.authorize_connect(target).await,
        }
    }
}

impl Approver for TerminalPermissionHandler {
    async fn approve(&self, request: ApprovalRequest) -> Result<Approval, ApproverFailure> {
        // A subagent never reaches the session's approver directly: its
        // escalation waits on the subagent deadline and agent id, and it
        // cannot put a new SSH server to the user at all.
        let subagent = request.call().is_some_and(|identity| {
            matches!(
                identity.agent(),
                aither_core::llm::AgentIdentity::Subagent(_)
            )
        });
        match request.kind().as_str() {
            UNSANDBOXED_KIND if subagent => {
                // The parent agent decides first: the request parks until its
                // `subagent approve`/`deny`, and only an approval reaches the
                // session's user. A parent not watching gets the request as a
                // prompt to its session.
                match self.child_approvals.open(request.clone()) {
                    Ok((approval, decision)) => {
                        if !self.child_approvals.watched(approval.parent()) {
                            self.child_approvals.alert_unwatched(&approval).await;
                        }
                        match decision.recv().await {
                            Ok(Approval::Approved) | Err(_) => {}
                            Ok(denied) => return Ok(denied),
                        }
                    }
                    Err(error) => tracing::warn!(
                        %error,
                        "a subagent request could not be put to its parent"
                    ),
                }
                let approver = self
                    .subagent_approver
                    .get_or_init(|| self.approver.clone().map(PermissionApprover::for_subagent));
                match approver {
                    Some(approver) => approver.approve(request).await,
                    None => Ok(Approval::Approved),
                }
            }
            UNSANDBOXED_KIND => match &self.approver {
                Some(approver) => approver.approve(request).await,
                None => Ok(Approval::Approved),
            },
            SSH_KIND if subagent => Ok(Approval::Denied {
                reason: "a subagent cannot open a new SSH server".to_string(),
            }),
            SSH_KIND => {
                let target = request.reason().unwrap_or_default();
                match self.authorize_server(target).await {
                    Ok(true) => Ok(Approval::Approved),
                    Ok(false) => Ok(Approval::Denied {
                        reason: format!("running on `{target}` was not approved"),
                    }),
                    Err(error) => Err(ApproverFailure::new(error)),
                }
            }
            other => Ok(Approval::Denied {
                reason: format!("nothing approves a `{other}` request here"),
            }),
        }
    }
}

impl DomainApprover for TerminalPermissionHandler {
    fn approve(&self, request: &DomainRequest) -> impl Future<Output = bool> + Send {
        let (domain, port) = (request.host.clone(), request.port);
        async move {
            let network_policy = self.network_policy.load();
            if let Some(policy) = network_policy.as_ref().as_ref() {
                let allowed_by_policy = policy.allows_domain(&domain);
                tracing::info!(
                    domain,
                    port,
                    mode = %policy.mode().as_str(),
                    allowed_by_policy,
                    "network policy evaluation for outbound domain"
                );
                self.append_network_audit_log(
                    &domain,
                    port,
                    allowed_by_policy,
                    Some(policy.mode().as_str()),
                )
                .await;
                if !allowed_by_policy {
                    return false;
                }
            }
            // The approval of the user is not asked per connection.
            self.append_network_audit_log(&domain, port, true, None)
                .await;
            true
        }
    }
}
