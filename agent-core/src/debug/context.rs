//! Live-resource provider plugging active debug sessions into May's unified
//! [`crate::agent::LiveResourcesContext`] (post-compaction recall and turn-end
//! unattended-resource checks).

use aither_dap::{DebugSessionState, DebugStatus};
use askama::Template;

use crate::agent::{LiveResourceProvider, LiveResourceSection, UnattendedResource};
use crate::debug::DebugBroker;

#[derive(Template)]
#[template(path = "debug_live.txt", escape = "none")]
struct DebugLiveText<'a> {
    adapter: &'a str,
    target_pid: Option<u32>,
    target: Option<&'a str>,
    state: &'a str,
    stop_location: Option<String>,
}

#[derive(Template)]
#[template(path = "debug_unattended.txt", escape = "none")]
struct DebugUnattendedText<'a> {
    adapter: &'a str,
    target_pid: Option<u32>,
    state: &'a str,
}

/// [`LiveResourceProvider`] for the main session's active debug session.
///
/// Surfaces active debug state inside the unified `<live-resources>` inventory
/// on context reassembly and participates in `LiveResourcesContext`'s
/// at-most-once `Moment::FinalTurn` unattended-resource check.
#[derive(Debug, Clone)]
pub struct DebugLiveResourceProvider {
    broker: DebugBroker,
    session_id: String,
}

impl DebugLiveResourceProvider {
    /// Creates a live-resource provider bound to `session_id` on `broker`.
    #[must_use]
    pub const fn new(broker: DebugBroker, session_id: String) -> Self {
        Self { broker, session_id }
    }

    const fn is_live(status: &DebugStatus) -> bool {
        !matches!(
            status.state,
            DebugSessionState::Inactive | DebugSessionState::Terminated
        )
    }

    fn render_inventory(status: &DebugStatus) -> String {
        let adapter = status.adapter.as_deref().unwrap_or("unknown");
        let stop_location = status.stop_location.as_ref().map(ToString::to_string);
        DebugLiveText {
            adapter,
            target_pid: status.target_pid,
            target: status.target.as_deref(),
            state: status.state.as_str(),
            stop_location,
        }
        .render()
        .expect("debug live inventory template must render")
    }

    fn render_unattended(status: &DebugStatus) -> String {
        let adapter = status.adapter.as_deref().unwrap_or("unknown");
        DebugUnattendedText {
            adapter,
            target_pid: status.target_pid,
            state: status.state.as_str(),
        }
        .render()
        .expect("debug unattended template must render")
    }
}

impl LiveResourceProvider for DebugLiveResourceProvider {
    async fn snapshot(&self) -> Vec<LiveResourceSection> {
        let Some((_, status)) = self.broker.active_session_status(&self.session_id).await else {
            return Vec::new();
        };
        if !Self::is_live(&status) {
            return Vec::new();
        }
        vec![LiveResourceSection {
            heading: "Active debug sessions".to_owned(),
            items: vec![Self::render_inventory(&status)],
        }]
    }

    async fn unattended_final_turn(&self) -> Vec<UnattendedResource> {
        let Some((generation, status)) = self.broker.active_session_status(&self.session_id).await
        else {
            return Vec::new();
        };
        if !Self::is_live(&status) {
            return Vec::new();
        }
        vec![UnattendedResource {
            key: format!("debug:{generation}"),
            description: Self::render_unattended(&status),
            cleanup_hint: "Close it with `debug detach` (to leave an attached background job running) or `debug terminate` (to stop the target).".to_owned(),
        }]
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use aither_agent::{ContextSource, Moment, Placement};
    use aither_dap::{
        ConnectMode, DapEvent, DapRequest, DapResponse, DebugArgs, FrameReader, ProtocolMessage,
        write_message,
    };
    use aither_sandbox::SandboxMode;
    use async_net::TcpListener;
    use futures_lite::io::BufReader;
    use serde_json::json;

    use super::*;
    use crate::agent::tui_tool::TuiRegistry;
    use crate::agent::{AnyLiveResourceProvider, LiveResourcesContext};
    use crate::debug::{DebugCallContext, DebugOwner};
    use crate::test_sandbox::TestTerminal;

    #[tokio::test]
    async fn unified_live_resources_recalls_active_debug_session_and_checks_final_turn_once() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let mut reader = BufReader::new(stream.clone());
            let mut writer = stream;
            let mut framer = FrameReader::new();
            let mut seq = 1_i64;
            let mut next_seq = || {
                let s = seq;
                seq += 1;
                s
            };
            while let Ok(Some(ProtocolMessage::Request(DapRequest {
                seq: req_seq,
                command,
                ..
            }))) = framer.read_message(&mut reader).await
            {
                match command.as_str() {
                    "initialize" => {
                        let _ = write_message(
                            &mut writer,
                            &ProtocolMessage::Response(DapResponse {
                                seq: next_seq(),
                                request_seq: req_seq,
                                success: true,
                                command,
                                message: None,
                                body: Some(json!({ "supportsConfigurationDoneRequest": true })),
                            }),
                        )
                        .await;
                    }
                    "attach" => {
                        let _ = write_message(
                            &mut writer,
                            &ProtocolMessage::Event(DapEvent {
                                seq: next_seq(),
                                event: "initialized".to_owned(),
                                body: None,
                            }),
                        )
                        .await;
                        let _ = write_message(
                            &mut writer,
                            &ProtocolMessage::Response(DapResponse {
                                seq: next_seq(),
                                request_seq: req_seq,
                                success: true,
                                command,
                                message: None,
                                body: None,
                            }),
                        )
                        .await;
                    }
                    "configurationDone" => {
                        let _ = write_message(
                            &mut writer,
                            &ProtocolMessage::Response(DapResponse {
                                seq: next_seq(),
                                request_seq: req_seq,
                                success: true,
                                command,
                                message: None,
                                body: None,
                            }),
                        )
                        .await;
                        let _ = write_message(
                            &mut writer,
                            &ProtocolMessage::Event(DapEvent {
                                seq: next_seq(),
                                event: "stopped".to_owned(),
                                body: Some(json!({ "reason": "entry", "threadId": 1 })),
                            }),
                        )
                        .await;
                    }
                    "threads" => {
                        let _ = write_message(
                            &mut writer,
                            &ProtocolMessage::Response(DapResponse {
                                seq: next_seq(),
                                request_seq: req_seq,
                                success: true,
                                command,
                                message: None,
                                body: Some(json!({ "threads": [{ "id": 1, "name": "main" }] })),
                            }),
                        )
                        .await;
                    }
                    "stackTrace" => {
                        let _ = write_message(
                            &mut writer,
                            &ProtocolMessage::Response(DapResponse {
                                seq: next_seq(),
                                request_seq: req_seq,
                                success: true,
                                command,
                                message: None,
                                body: Some(json!({
                                    "stackFrames": [{
                                        "id": 1,
                                        "name": "main",
                                        "source": { "path": "src/app.rs" },
                                        "line": 17,
                                        "column": 1
                                    }]
                                })),
                            }),
                        )
                        .await;
                    }
                    "disconnect" | "terminate" => {
                        let _ = write_message(
                            &mut writer,
                            &ProtocolMessage::Response(DapResponse {
                                seq: next_seq(),
                                request_seq: req_seq,
                                success: true,
                                command,
                                message: None,
                                body: None,
                            }),
                        )
                        .await;
                        break;
                    }
                    _ => {}
                }
            }
        });

        let temp = tempfile::tempdir().expect("tempdir");
        let terminal = TestTerminal::new(temp.path()).await;
        let broker = DebugBroker::new();
        let ctx = DebugCallContext {
            owner: DebugOwner::MainSession("s-unified".to_owned()),
            shell: None,
            working_dir: None,
            launch_mode: SandboxMode::Sandboxed,
            session_prompts: None,
            prior_jobs: None,
        };
        let _ = broker
            .handle(
                ctx,
                DebugArgs::Connect {
                    endpoint: addr.to_string(),
                    adapter: None,
                    mode: ConnectMode::Attach,
                    program: None,
                    cwd: None,
                    stop_on_entry: true,
                    timeout_ms: Some(2000),
                    args: Vec::new(),
                },
            )
            .await
            .expect("connect");

        let mut source = LiveResourcesContext::new(
            terminal.shell().jobs(),
            TuiRegistry::new(),
            None,
            vec![AnyLiveResourceProvider::new(
                DebugLiveResourceProvider::new(broker.clone(), "s-unified".to_owned()),
            )],
        );

        assert_eq!(
            source.blocks(Moment::Request { reassembled: false }).await,
            [] as [aither_agent::ContextBlock; 0]
        );
        let blocks = source.blocks(Moment::Request { reassembled: true }).await;
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].placement(), Placement::Stable);
        assert_eq!(blocks[0].block().tag(), "live-resources");
        assert!(
            blocks[0]
                .block()
                .content()
                .contains("Active debug sessions:")
        );
        assert!(blocks[0].block().content().contains("dap-tcp"));
        assert!(blocks[0].block().content().contains("src/app.rs:17:1"));

        let first = source.blocks(Moment::FinalTurn).await;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].block().tag(), "system-reminder");
        assert!(first[0].block().content().contains("debug detach"));
        assert_eq!(
            source.blocks(Moment::FinalTurn).await,
            [] as [aither_agent::ContextBlock; 0]
        );

        broker.teardown_session("s-unified").await;
        async_io::Timer::after(Duration::from_millis(20)).await;
        assert_eq!(
            source.blocks(Moment::Request { reassembled: true }).await,
            [] as [aither_agent::ContextBlock; 0]
        );
        assert_eq!(
            source.blocks(Moment::FinalTurn).await,
            [] as [aither_agent::ContextBlock; 0]
        );
    }
}
