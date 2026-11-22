//! Per-session `memoryOff` switch and one-shot memory isolation in the daemon.

use std::sync::Arc;

use super::chat::{resolve_session_context, schedule_profile_memory_processing};
use super::state::DaemonState;
use super::types::ChatRequest;
use crate::config::{MayConfig, ProviderInstance, ProviderType};
use crate::db::{CreateProfileParams, ProfileEnvironmentKind, RunFence};
use crate::memory::MemoryProcessor;
use crate::model_selector::ModelSelector;
use crate::session::SessionId;

fn test_config() -> MayConfig {
    let mut config = MayConfig::default();
    config.provider.insert(
        "test".to_string(),
        ProviderInstance::new(ProviderType::Custom)
            .with_api_key("test-key")
            .with_base_url("http://127.0.0.1:1/v1"),
    );
    config.models.advanced = vec![ModelSelector::new("test", "test-model")];
    config.last_model = Some(ModelSelector::new("test", "test-model"));
    config
}

async fn seeded_daemon(dir: &std::path::Path) -> Arc<DaemonState> {
    let mut state = DaemonState::for_test(dir, test_config())
        .await
        .expect("daemon state");
    state.memory_processor = Some(MemoryProcessor::for_test(state.db()));
    let state = Arc::new(state);
    let db = state.db();
    db.create_profile(CreateProfileParams {
        id: "p1",
        name: "Default",
        environment: ProfileEnvironmentKind::Local,
        slug: None,
        icon: None,
        persona: None,
        preferences: None,
        location_aware: false,
    })
    .await
    .expect("create profile");
    db.upsert_profile_memory("p1", "preference", "likes Rust")
        .await
        .expect("upsert preference");
    db.upsert_profile_memory("p1", "recency", "working on #1013")
        .await
        .expect("upsert recency");
    state
}

fn chat_request(session_id: &str) -> ChatRequest {
    ChatRequest {
        session_id: session_id.to_string(),
        content: "hello".to_string(),
        attachments: Vec::new(),
        hidden_context_xml: None,
        delivery_mode: crate::session::PendingUserMessageMode::default(),
        pending_entry_id: None,
    }
}

#[tokio::test]
async fn resolve_context_and_extraction_skip_memory_off_and_oneshot_sessions() {
    let dir = tempfile::tempdir().expect("temp dir");
    let state = seeded_daemon(dir.path()).await;
    let db = state.db();

    db.create_session_in_profile(
        "normal",
        Some("Normal"),
        Some("test::test-model"),
        Some("p1"),
    )
    .await
    .expect("normal session");
    db.create_session_in_profile(
        "mem-off",
        Some("Memory off"),
        Some("test::test-model"),
        Some("p1"),
    )
    .await
    .expect("mem-off session");
    db.create_session_in_profile(
        "oneshot",
        Some("One-shot"),
        Some("test::test-model"),
        Some("p1"),
    )
    .await
    .expect("oneshot session");

    state
        .runtime_service
        .set_memory_off("mem-off")
        .await
        .expect("set memory off");
    db.set_session_non_interactive("oneshot")
        .await
        .expect("set non_interactive");

    let fence = |id: &str| RunFence {
        session_id: id.to_string(),
        run_id: "r1".to_string(),
    };

    let mem_off_ctx = resolve_session_context(
        &state,
        &chat_request("mem-off"),
        &fence("mem-off"),
        false,
        false,
        None,
    )
    .await
    .expect("mem-off context");
    assert!(mem_off_ctx.memory_off);
    assert!(mem_off_ctx.profile_memories.is_none());

    let oneshot_ctx = resolve_session_context(
        &state,
        &chat_request("oneshot"),
        &fence("oneshot"),
        false,
        false,
        None,
    )
    .await
    .expect("oneshot context");
    assert!(oneshot_ctx.memory_off);
    assert!(oneshot_ctx.profile_memories.is_none());

    let normal_ctx = resolve_session_context(
        &state,
        &chat_request("normal"),
        &fence("normal"),
        false,
        false,
        None,
    )
    .await
    .expect("normal context");
    assert!(!normal_ctx.memory_off);
    assert!(normal_ctx.profile_memories.is_some());

    // Neither a memory-off session nor a one-shot run dirties the profile for extraction.
    schedule_profile_memory_processing(&state, &SessionId::from_string("mem-off"), Some("p1"))
        .await;
    schedule_profile_memory_processing(&state, &SessionId::from_string("oneshot"), Some("p1"))
        .await;
    let due_after_skip = db
        .list_due_profile_memory_jobs("9999-12-31T23:59:59Z", "9999-12-31T23:59:59Z")
        .await
        .expect("due jobs");
    assert_eq!(due_after_skip, Vec::new());

    // An ordinary session does enqueue extraction.
    schedule_profile_memory_processing(&state, &SessionId::from_string("normal"), Some("p1")).await;
    let dirty = db
        .list_due_profile_memory_jobs("9999-12-31T23:59:59Z", "9999-12-31T23:59:59Z")
        .await
        .expect("due jobs");
    assert_eq!(dirty.len(), 1);
    assert_eq!(dirty[0].profile_id, "p1");

    // Deleting a session also clears its `session_memory` row.
    assert!(db.session_memory_off("mem-off").await.expect("flag set"));
    db.delete_session("mem-off").await.expect("delete session");
    assert!(
        !db.session_memory_off("mem-off")
            .await
            .expect("flag cleared")
    );
}
