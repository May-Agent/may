//! Integration tests for host-brokered DAP debugging (`debug` command, `DebugBroker`,
//! real-PID `JobRegistry` lifecycle, async `SessionPrompts` wakeups, `DebugLiveContext`,
//! and `read_only` subagent refusal).

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aither_agent::{ContextSource, Moment};
use aither_core::llm::{
    AgentIdentity, CallIdentity, IntoToolResult, SubagentIdentity, Tool, ToolCallId, ToolContext,
};
use aither_dap::{
    ConnectMode, DapEvent, DapRequest, DapResponse, DebugArgs, DebugResult, DebugSessionState,
    FrameReader, ProtocolMessage, write_message,
};
use aither_sandbox::{LaunchId, LaunchInfo, SandboxMode};
use async_net::{TcpListener, TcpStream};
use futures_lite::io::BufReader;
use may_agent_core::db::ChatDb;
use may_agent_core::debug::{
    AdapterError, AdapterKind, DebugBroker, DebugCallContext, DebugLiveContext, DebugOwner,
    MayDebugTool, adapter,
};
use may_agent_core::ipc::Binding;
use may_agent_core::session::{MayHomePaths, SessionRuntimeService};
use may_agent_core::task_wake::SessionPrompts;
use may_agent_core::ui_event_bus::UiEventBus;
use serde_json::json;

#[derive(Default)]
struct MockDapBehavior {
    continue_delay: Duration,
    detached: bool,
    terminated: bool,
}

async fn spawn_mock_dap_server(behavior: Arc<Mutex<MockDapBehavior>>) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        serve_mock_dap(stream, behavior).await;
    });
    addr
}

async fn serve_mock_dap(stream: TcpStream, behavior: Arc<Mutex<MockDapBehavior>>) {
    let mut reader = BufReader::new(stream.clone());
    let mut writer = stream;
    let mut framer = FrameReader::new();
    let mut seq = 1_i64;
    let mut next_seq = || {
        let cur = seq;
        seq += 1;
        cur
    };

    while let Ok(Some(ProtocolMessage::Request(DapRequest {
        seq: req_seq,
        command,
        ..
    }))) = framer.read_message(&mut reader).await
    {
        match command.as_str() {
            "initialize" => {
                write_message(
                    &mut writer,
                    &ProtocolMessage::Response(DapResponse {
                        seq: next_seq(),
                        request_seq: req_seq,
                        success: true,
                        command,
                        message: None,
                        body: Some(json!({
                            "supportsConfigurationDoneRequest": true,
                            "supportsTerminateRequest": true
                        })),
                    }),
                )
                .await
                .unwrap();
            }
            "attach" | "launch" => {
                write_message(
                    &mut writer,
                    &ProtocolMessage::Event(DapEvent {
                        seq: next_seq(),
                        event: "initialized".to_owned(),
                        body: None,
                    }),
                )
                .await
                .unwrap();
                write_message(
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
                .await
                .unwrap();
            }
            "configurationDone" => {
                write_message(
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
                .await
                .unwrap();
                write_message(
                    &mut writer,
                    &ProtocolMessage::Event(DapEvent {
                        seq: next_seq(),
                        event: "stopped".to_owned(),
                        body: Some(json!({ "reason": "entry", "threadId": 1 })),
                    }),
                )
                .await
                .unwrap();
            }
            "threads" => {
                write_message(
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
                .await
                .unwrap();
            }
            "stackTrace" => {
                write_message(
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
                                "name": "work",
                                "source": { "path": "src/main.rs" },
                                "line": 42,
                                "column": 1
                            }]
                        })),
                    }),
                )
                .await
                .unwrap();
            }
            "continue" => {
                let delay = behavior.lock().unwrap().continue_delay;
                write_message(
                    &mut writer,
                    &ProtocolMessage::Response(DapResponse {
                        seq: next_seq(),
                        request_seq: req_seq,
                        success: true,
                        command,
                        message: None,
                        body: Some(json!({ "allThreadsContinued": true })),
                    }),
                )
                .await
                .unwrap();
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                write_message(
                    &mut writer,
                    &ProtocolMessage::Event(DapEvent {
                        seq: next_seq(),
                        event: "stopped".to_owned(),
                        body: Some(json!({
                            "reason": "breakpoint",
                            "threadId": 1
                        })),
                    }),
                )
                .await
                .unwrap();
            }
            "terminate" => {
                behavior.lock().unwrap().terminated = true;
                write_message(
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
                .await
                .unwrap();
            }
            "disconnect" => {
                behavior.lock().unwrap().detached = true;
                write_message(
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
                .await
                .unwrap();
                break;
            }
            other => {
                write_message(
                    &mut writer,
                    &ProtocolMessage::Response(DapResponse {
                        seq: next_seq(),
                        request_seq: req_seq,
                        success: false,
                        command: other.to_owned(),
                        message: Some("unsupported".to_owned()),
                        body: None,
                    }),
                )
                .await
                .unwrap();
            }
        }
    }
}

#[tokio::test]
async fn async_stop_wakes_session_prompts_and_live_context_recalls_and_tears_down() {
    let temp = tempfile::tempdir().unwrap();
    let db = ChatDb::open(&temp.path().join("history.db")).await.unwrap();
    db.create_session("debug-session", Some("Debug"), None)
        .await
        .unwrap();
    let runtime = Arc::new(SessionRuntimeService::new(
        db.clone(),
        UiEventBus::new(),
        MayHomePaths::new(temp.path().join(".may")),
    ));
    let (wake_tx, wake_rx) = async_channel::unbounded();
    let prompts = SessionPrompts::new(runtime.clone(), wake_tx);

    let behavior = Arc::new(Mutex::new(MockDapBehavior {
        continue_delay: Duration::from_millis(160),
        ..MockDapBehavior::default()
    }));
    let addr = spawn_mock_dap_server(Arc::clone(&behavior)).await;

    let broker = DebugBroker::new();
    let ctx = DebugCallContext {
        owner: DebugOwner::MainSession("debug-session".to_owned()),
        shell: None,
        working_dir: None,
        launch_mode: SandboxMode::Sandboxed,
        session_prompts: Some(prompts),
        prior_jobs: None,
    };

    // 1. Connect to loopback mock DAP server.
    let connected = broker
        .handle(
            ctx.clone(),
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
        .unwrap();
    assert!(matches!(connected, DebugResult::Stopped(_)));

    // 2. Post-compaction DebugLiveContext recalls active session on reassembled=true,
    // emits nothing on reassembled=false, and raises Moment::FinalTurn at most once.
    let mut live_ctx = DebugLiveContext::new(broker.clone(), "debug-session".to_owned());
    assert_eq!(
        live_ctx
            .blocks(Moment::Request { reassembled: false })
            .await,
        [] as [aither_agent::ContextBlock; 0]
    );
    let reassembled_blocks = live_ctx.blocks(Moment::Request { reassembled: true }).await;
    assert_eq!(reassembled_blocks.len(), 1);
    assert!(reassembled_blocks[0].block().content().contains("dap-tcp"));
    assert!(
        reassembled_blocks[0]
            .block()
            .content()
            .contains("src/main.rs:42:1")
    );

    let final_first = live_ctx.blocks(Moment::FinalTurn).await;
    assert_eq!(final_first.len(), 1);
    assert!(final_first[0].block().content().contains("debug detach"));
    let final_second = live_ctx.blocks(Moment::FinalTurn).await;
    assert_eq!(final_second, [] as [aither_agent::ContextBlock; 0]);

    // 3. Continue with short timeout returns Running, then delayed breakpoint hit
    // wakes SessionPrompts once with hidden context!
    let cont = broker
        .handle(
            ctx.clone(),
            DebugArgs::Continue {
                thread_id: None,
                timeout_ms: Some(30),
            },
        )
        .await
        .unwrap();
    assert!(matches!(cont, DebugResult::Running { .. }));

    let woken_session = tokio::time::timeout(Duration::from_secs(2), wake_rx.recv())
        .await
        .expect("expected SessionPrompts wake on async breakpoint hit")
        .unwrap();
    assert_eq!(woken_session, "debug-session");

    let pending = runtime.get("debug-session").await.unwrap().queued_messages;
    assert_eq!(pending.len(), 1);
    let xml = pending[0]
        .hidden_context_xml
        .as_ref()
        .expect("hidden context xml");
    assert!(xml.contains("Debug target stopped (breakpoint)"));

    // 4. Session archive/delete teardown cleanly removes active debug session.
    broker.teardown_session("debug-session").await;
    let status = broker.handle(ctx, DebugArgs::Status).await.unwrap();
    let DebugResult::Status(s) = status else {
        panic!("expected status");
    };
    assert_eq!(s.state, DebugSessionState::Inactive);
    assert_eq!(
        live_ctx.blocks(Moment::Request { reassembled: true }).await,
        [] as [aither_agent::ContextBlock; 0]
    );
}

#[tokio::test]
async fn read_only_subagents_are_refused_while_writable_subagent_is_isolated() {
    let broker = DebugBroker::new();
    let read_only: HashSet<String> = ["explore", "review", "audit"]
        .into_iter()
        .map(str::to_owned)
        .collect();

    for name in ["explore", "review", "audit"] {
        let launch = LaunchInfo::new(
            LaunchId::next(),
            Some(CallIdentity::new(
                ToolCallId::new("call-1"),
                AgentIdentity::Subagent(SubagentIdentity::new(name, None)),
            )),
            SandboxMode::Sandboxed,
        );
        let binding = Binding {
            launch: &launch,
            shell: None,
        };
        let tool = MayDebugTool::for_binding(
            broker.clone(),
            "s1".to_owned(),
            &binding,
            &read_only,
            None,
            None,
        );
        let res = tool
            .call(DebugArgs::Status, ToolContext::new())
            .await
            .unwrap()
            .into_tool_result();
        assert!(res.is_error(), "{name} must be refused");
        assert!(res.render_for_model().contains("read-only subagent"));
    }

    // Writable subagent `general` is allowed and isolated from main session.
    let addr = spawn_mock_dap_server(Arc::new(Mutex::new(MockDapBehavior::default()))).await;
    let sub_origin = CallIdentity::new(ToolCallId::new("sub-run-1"), AgentIdentity::Main);
    let sub_launch = LaunchInfo::new(
        LaunchId::next(),
        Some(CallIdentity::new(
            ToolCallId::new("call-2"),
            AgentIdentity::Subagent(SubagentIdentity::new("general", Some(sub_origin))),
        )),
        SandboxMode::Sandboxed,
    );
    let sub_binding = Binding {
        launch: &sub_launch,
        shell: None,
    };
    let sub_tool = MayDebugTool::for_binding(
        broker.clone(),
        "s1".to_owned(),
        &sub_binding,
        &read_only,
        None,
        None,
    );
    let res = sub_tool
        .call(
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
            ToolContext::new(),
        )
        .await
        .unwrap()
        .into_tool_result();
    assert!(!res.is_error());
    // Main session inventory does not see the subagent's debug session.
    assert!(broker.active_session_status("s1").await.is_none());

    // Tearing down the subagent owner closes its debug session cleanly.
    let sub_owner = DebugOwner::Subagent {
        session_id: "s1".to_owned(),
        subagent_key: "general-sub-run-1".to_owned(),
    };
    broker.teardown_owner(&sub_owner).await;
    let status = sub_tool
        .call(DebugArgs::Status, ToolContext::new())
        .await
        .unwrap()
        .into_tool_result();
    assert!(status.render_for_model().contains("inactive"));
}

#[test]
fn unsupported_runtimes_and_bundled_debug_skill_are_present() {
    assert!(matches!(
        adapter::select_adapter_for_launch("index.ts", None),
        Err(AdapterError::UnsupportedRuntime { .. })
    ));
    assert!(matches!(
        adapter::select_adapter_for_launch("app.jar", None),
        Err(AdapterError::UnsupportedRuntime { .. })
    ));
    assert_eq!(
        adapter::select_adapter_for_launch("script.py", None).unwrap(),
        AdapterKind::Debugpy
    );

    let debug_skill = may_agent_core::bundle::SKILLS_DIR
        .get_file("debug/SKILL.md")
        .expect("bundled debug skill must exist")
        .contents_utf8()
        .expect("bundled debug skill must be valid UTF-8");
    assert!(debug_skill.contains("requires-command: debug"));
    assert!(debug_skill.contains("lldb-dap"));
    assert!(debug_skill.contains("debugpy"));
    assert!(debug_skill.contains("dlv"));
}

#[cfg(unix)]
#[tokio::test]
async fn resolves_wrapper_shell_child_pid() {
    let mut child = async_process::Command::new("/bin/sh")
        .args(["-c", "sleep 30; true"])
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let shell_pid = child.id();
    // Allow `/bin/sh` a brief moment to fork `sleep`.
    let mut resolved = shell_pid;
    for _ in 0..20 {
        resolved = may_agent_core::debug::process::resolve_debuggee_pid(shell_pid).await;
        if resolved != shell_pid {
            break;
        }
        async_io::Timer::after(Duration::from_millis(25)).await;
    }
    assert_ne!(
        resolved, shell_pid,
        "wrapper `/bin/sh` should resolve to its `sleep` child process"
    );
    let _ = async_process::Command::new("kill")
        .args(["-9", &resolved.to_string()])
        .status()
        .await;
    let _ = child.kill();
    let _ = child.status().await;
}
