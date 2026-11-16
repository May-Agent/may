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

#[cfg(test)]
mod tests {
    use super::*;
    use aither_sandbox::{
        Interpreter, PreparedSsh, RemoteLaunch, SandboxMode, ScriptDir, ScriptOrigin, SshServer,
    };

    #[tokio::test]
    async fn server_approval_routes_location_in_default_and_unsafe_modes() {
        let registry = ShellSessionRegistry::default();
        registry
            .add_ssh_server(
                SshServer {
                    name: "prod".to_string(),
                    target: "ops@prod.example.com".to_string(),
                },
                PreparedSsh::new(
                    Some("/usr/local/bin/heel".to_string()),
                    RemoteLaunch::new("/usr/bin", "/bin/sh"),
                ),
            )
            .expect("add server");
        let policy = ServerApproval::new(registry);
        let start = ScriptDir::Remote {
            server: "prod".to_string(),
        };
        let home_start = ScriptDir::Host(std::path::PathBuf::from("/tmp"));

        for mode in [SandboxMode::Sandboxed, SandboxMode::Unsandboxed] {
            let remote = policy
                .check(
                    &ScriptCheck {
                        task_id: "task-1",
                        script: "uname -a",
                        interpreter: Interpreter::Shell,
                        shell: None,
                        origin: ScriptOrigin::Foreground,
                        sandbox: mode,
                        location: Some("prod"),
                        start: &start,
                        call: None,
                    },
                    &(),
                )
                .await;
            let ScriptDecision::Ask(request) = remote else {
                panic!("expected Ask for {mode:?}");
            };
            assert_eq!(request.kind().as_str(), SSH_KIND);
            assert_eq!(request.subject(), "uname -a");
            assert_eq!(request.reason(), Some("ops@prod.example.com"));

            let local = policy
                .check(
                    &ScriptCheck {
                        task_id: "task-2",
                        script: "echo ok",
                        interpreter: Interpreter::Shell,
                        shell: None,
                        origin: ScriptOrigin::Foreground,
                        sandbox: mode,
                        location: None,
                        start: &home_start,
                        call: None,
                    },
                    &(),
                )
                .await;
            assert!(matches!(local, ScriptDecision::Pass));
        }
    }
}
