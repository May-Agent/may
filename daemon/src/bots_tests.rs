//! Integration tests for `/api/bots/*` REST handlers and `/api/events?topics=bots`
//! SSE multiplexing on the single daemon event stream.

use std::sync::Arc;
use std::time::Duration;

use futures_util::TryStreamExt as _;
use http_body_util::{BodyDataStream, BodyExt as _};
use may_protocol::bot::{
    BotApprovalPolicy, BotChatMessage, BotMemoryCommit, BotMemoryNode, BotSummary,
    CreateBotRequest, GatePolicy, SendBotMessageRequest, UpdateBotRequest,
};
use skyzen::{Request, StatusCode};

use super::auth::{RequireIdentity, SocketOwner, TailscaleServeIdentitySource};
use super::server::{DaemonMode, build_router};
use super::state::DaemonState;
use crate::config::MayConfig;

async fn test_state() -> (tempfile::TempDir, Arc<DaemonState>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = DaemonState::for_test(dir.path(), MayConfig::default())
        .await
        .expect("daemon state");
    (dir, Arc::new(state))
}

fn json_request<T: serde::Serialize>(method: &str, uri: &str, body: &T) -> Request {
    let bytes = serde_json::to_vec(body).expect("serialize request body");
    hyper::Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(skyzen::Body::from(bytes))
        .expect("request builds")
}

fn empty_request(method: &str, uri: &str) -> Request {
    hyper::Request::builder()
        .method(method)
        .uri(uri)
        .body(skyzen::Body::empty())
        .expect("request builds")
}

async fn read_json<T: serde::de::DeserializeOwned>(
    router: &skyzen::routing::Router,
    req: Request,
    expected_status: StatusCode,
) -> T {
    let resp = router.go(req).await.expect("router answers");
    assert_eq!(resp.status(), expected_status);
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("decode json response")
}

struct SseReader {
    body: BodyDataStream<skyzen::Body>,
    buf: Vec<u8>,
}

impl SseReader {
    async fn open(router: &skyzen::routing::Router, uri: &str) -> Self {
        let resp = router
            .go(empty_request("GET", uri))
            .await
            .expect("sse router answers");
        assert_eq!(resp.status(), StatusCode::OK);
        Self {
            body: BodyDataStream::new(resp.into_body()),
            buf: Vec::new(),
        }
    }

    async fn next_event(&mut self) -> (String, String) {
        tokio::time::timeout(Duration::from_secs(10), self.read_event())
            .await
            .expect("sse event within 10s")
    }

    async fn read_event(&mut self) -> (String, String) {
        loop {
            if let Some(ev) = self.take_buffered() {
                return ev;
            }
            let chunk = self
                .body
                .try_next()
                .await
                .expect("sse chunk ok")
                .expect("sse stream still open");
            self.buf.extend_from_slice(&chunk);
        }
    }

    fn take_buffered(&mut self) -> Option<(String, String)> {
        let sep = self.buf.windows(2).position(|w| w == b"\n\n")?;
        let raw = self.buf.drain(..sep + 2).collect::<Vec<_>>();
        let text = String::from_utf8(raw).expect("valid utf8 sse frame");
        let mut kind = String::new();
        let mut data = Vec::new();
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("event:") {
                kind = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("data:") {
                data.push(v.trim_start().to_string());
            }
        }
        Some((kind, data.join("\n")))
    }
}

#[tokio::test]
async fn bots_rest_crud_live_policy_memory_and_single_sse_stream() {
    let (_dir, state) = test_state().await;
    let router = build_router(state.clone(), DaemonMode::Rest, SocketOwner);

    // 1. Open single `/api/events?topics=bots` SSE stream and receive initial `bot_state` snapshot.
    let mut sse = SseReader::open(&router, "/api/events?topics=bots").await;
    let (init_kind, init_data) = sse.next_event().await;
    assert_eq!(init_kind, "bot_state");
    assert!(init_data.contains("\"bots\":[]"));

    // 2. Create a bot over REST (with CJK persona covering Chinese, Japanese, Korean).
    let created: BotSummary = read_json(
        &router,
        json_request(
            "POST",
            "/api/bots",
            &CreateBotRequest {
                name: "bridge-bot".to_string(),
                persona: Some("Multilingual bridge (中文 / 日本語 / 한국어)".to_string()),
                model: None,
                profile_id: None,
                enabled: false,
                approval_policy: BotApprovalPolicy::default(),
            },
        ),
        StatusCode::OK,
    )
    .await;
    assert_eq!(created.name, "bridge-bot");
    assert_eq!(created.agent_id.split('-').count(), 3);

    // Verify `bot_state` arrived on `/api/events` carrying the created bot.
    let mut saw_created = false;
    for _ in 0..4 {
        let (kind, data) = sse.next_event().await;
        if kind == "bot_state" && data.contains(&created.id) {
            saw_created = true;
            break;
        }
    }
    assert!(
        saw_created,
        "creating a bot must push bot_state on /api/events"
    );

    // 3. List & get bot.
    let listed: Vec<BotSummary> =
        read_json(&router, empty_request("GET", "/api/bots"), StatusCode::OK).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, created.id);

    let fetched: BotSummary = read_json(
        &router,
        empty_request("GET", &format!("/api/bots/{}", created.id)),
        StatusCode::OK,
    )
    .await;
    assert_eq!(fetched.agent_id, created.agent_id);

    // 4. Live policy update via PATCH (no restart required).
    let updated: BotSummary = read_json(
        &router,
        json_request(
            "PATCH",
            &format!("/api/bots/{}", created.id),
            &UpdateBotRequest {
                name: None,
                persona: None,
                model: None,
                enabled: None,
                approval_policy: Some(BotApprovalPolicy {
                    unsafe_cmd: GatePolicy::AutoApprove,
                    ssh: GatePolicy::AlwaysEscalate,
                    publish: GatePolicy::BotTriage,
                    workspace: GatePolicy::AutoApprove,
                }),
            },
        ),
        StatusCode::OK,
    )
    .await;
    assert_eq!(updated.approval_policy.unsafe_cmd, GatePolicy::AutoApprove);
    assert_eq!(updated.approval_policy.ssh, GatePolicy::AlwaysEscalate);

    // 5. Post a user message via REST and verify `bot_chat` multiplexes onto `/api/events`.
    let posted: BotChatMessage = read_json(
        &router,
        json_request(
            "POST",
            &format!("/api/bots/{}/messages", created.id),
            &SendBotMessageRequest {
                content: "你好 / こんにちは / 안녕하세요".to_string(),
            },
        ),
        StatusCode::OK,
    )
    .await;
    assert_eq!(posted.role, "user");
    assert_eq!(posted.content, "你好 / こんにちは / 안녕하세요");

    let mut saw_chat = false;
    for _ in 0..6 {
        let (kind, data) = sse.next_event().await;
        if kind == "bot_chat" && data.contains("こんにちは") {
            saw_chat = true;
            break;
        }
    }
    assert!(
        saw_chat,
        "posting a bot message must push bot_chat on /api/events"
    );

    let msgs: Vec<BotChatMessage> = read_json(
        &router,
        empty_request(
            "GET",
            &format!("/api/bots/{}/messages?limit=50", created.id),
        ),
        StatusCode::OK,
    )
    .await;
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].content, "你好 / こんにちは / 안녕하세요");

    // 6. Inspect memory tree & git history endpoints.
    let tree: BotMemoryNode = read_json(
        &router,
        empty_request("GET", &format!("/api/bots/{}/memory", created.id)),
        StatusCode::OK,
    )
    .await;
    assert!(tree.is_dir);

    let history: Vec<BotMemoryCommit> = read_json(
        &router,
        empty_request(
            "GET",
            &format!("/api/bots/{}/memory/history?limit=10", created.id),
        ),
        StatusCode::OK,
    )
    .await;
    assert_eq!(history, [] as [may_protocol::bot::BotMemoryCommit; 0]);

    // 7. Remote listener rejects unauthenticated `/api/bots` requests with 401.
    let remote_router = build_router(
        state.clone(),
        DaemonMode::Rest,
        RequireIdentity::new(vec![Box::new(TailscaleServeIdentitySource::new(
            std::collections::HashSet::from(["owner@example.com".to_string()]),
        ))]),
    );
    let remote_status = match remote_router.go(empty_request("GET", "/api/bots")).await {
        Ok(r) => r.status(),
        Err(e) => skyzen::error_response(&*e).status(),
    };
    assert_eq!(remote_status, StatusCode::UNAUTHORIZED);

    // 8. Delete bot over REST.
    let resp = router
        .go(empty_request(
            "DELETE",
            &format!("/api/bots/{}", created.id),
        ))
        .await
        .expect("delete answers");
    assert_eq!(resp.status(), StatusCode::OK);

    let listed_after: Vec<BotSummary> =
        read_json(&router, empty_request("GET", "/api/bots"), StatusCode::OK).await;
    assert_eq!(listed_after, [] as [may_protocol::bot::BotSummary; 0]);
}
