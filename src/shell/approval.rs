//! The approval of a script that runs on an SSH server.

use std::future::Future;

use aither_core::llm::{ApprovalKind, ApprovalRequest};
use aither_sandbox::{ScriptCheck, ScriptDecision, ScriptPolicy, ShellSessionRegistry};

use crate::agent::permission::SSH_KIND;

/// Asks before the first script runs on a server the session's user has not
/// approved: the request carries the server's target as its reason, which the
/// session's approver reads.
///
/// A script that runs outside the sandbox on this machine needs no policy:
/// the shell asks its approver about it when no policy has an opinion.
#[derive(Debug, Clone)]
pub struct ServerApproval {
    servers: ShellSessionRegistry,
}

impl ServerApproval {
    /// A policy over the servers `servers` holds now.
    #[must_use]
    pub const fn new(servers: ShellSessionRegistry) -> Self {
        Self { servers }
    }
}

impl ScriptPolicy for ServerApproval {
    type State = ();

    fn check(
        &self,
        script: &ScriptCheck<'_>,
        (): &(),
    ) -> impl Future<Output = ScriptDecision> + Send {
        let decision = script
            .location
            .and_then(|name| {
                self.servers
                    .ssh_servers()
                    .into_iter()
                    .find(|server| server.name == name)
            })
            .map_or(ScriptDecision::Pass, |server| {
                ScriptDecision::Ask(
                    ApprovalRequest::new(ApprovalKind::new(SSH_KIND), script.script)
                        .with_reason(server.target),
                )
            });
        std::future::ready(decision)
    }
}
