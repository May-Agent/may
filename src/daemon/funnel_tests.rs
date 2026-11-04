//! Funnel end to end against a fake `tailscale`: turning it on binds a public
//! listener and installs the Funnel handler, the listener serves webhook
//! deliveries and no other route, and turning it off removes both.

use std::path::Path;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::remote_access::RemoteAccessManager;
use super::server::build_webhook_router;
use super::state::DaemonState;
use crate::config::{MayConfig, RemoteAccessConfig};
use crate::db::CreateScheduledTaskParams;
use crate::task_trigger::{PersistedTaskTrigger, WebhookSignature, new_webhook_token};
use crate::ui_event_bus::UiEventBus;

const FAKE_TAILSCALE: &str = include_str!("../../tests/fixtures/fake_tailscale.sh");

fn install_fake_tailscale(dir: &Path) -> String {
    use std::os::unix::fs::PermissionsExt as _;
    let script = dir.join("tailscale");
    std::fs::write(&script, FAKE_TAILSCALE).expect("script written");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("script executable");
    script.to_string_lossy().into_owned()
}

fn calls(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("calls.log"))
        .unwrap_or_default()
        .lines()
        .map(ToOwned::to_owned)
        .collect()
}

/// The proxy target the fake received on its `funnel --bg` call.
fn funnel_target(dir: &Path) -> String {
    calls(dir)
        .iter()
        .find(|call| call.starts_with("funnel --bg"))
        .and_then(|call| call.split(' ').next_back())
        .expect("a funnel --bg call")
        .to_owned()
}

/// Sends one request to `target` (`http://127.0.0.1:<port>`) and returns the
/// response status.
async fn http_status(target: &str, method: &str, path: &str) -> u16 {
    let addr = target.trim_start_matches("http://");
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("listener accepts");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: mac.ts.net\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
    );
    stream.write_all(request.as_bytes()).await.expect("sent");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    response
        .split(' ')
        .nth(1)
        .and_then(|status| status.parse().ok())
        .expect("status line")
}

async fn listener_is_closed(target: &str) -> bool {
    let addr = target.trim_start_matches("http://");
    tokio::net::TcpStream::connect(addr).await.is_err()
}

#[tokio::test]
async fn funnel_serves_only_webhooks_and_turning_it_off_removes_everything() {
    let home = tempfile::tempdir().expect("temp dir");
    let fake = tempfile::tempdir().expect("temp dir");
    let binary = install_fake_tailscale(fake.path());
    let state = Arc::new(
        DaemonState::for_test(home.path(), home.path(), MayConfig::default())
            .await
            .expect("daemon state"),
    );

    // One signed webhook task: a delivery without its signature is refused.
    let profile_id = state
        .db()
        .ensure_default_profiles()
        .await
        .expect("profiles");
    let token = new_webhook_token();
    let trigger = PersistedTaskTrigger::Webhook {
        token: token.clone(),
        signature: Some(WebhookSignature {
            secret: "MAY_TEST_FUNNEL_SECRET_526".to_string(),
            header: "x-hub-signature-256".to_string(),
        }),
    };
    state
        .db()
        .create_scheduled_task(CreateScheduledTaskParams {
            id: "signed",
            profile_id: profile_id.as_str(),
            session_id: None,
            name: "signed",
            trigger: &trigger,
            cron_expression: None,
            run_at: None,
            script_path: None,
            script_inline: Some("echo hi"),
            needs_network: false,
            timeout_secs: 60,
        })
        .await
        .expect("task persists");
    state
        .db()
        .approve_scheduled_task("signed")
        .await
        .expect("task approved");

    let manager = RemoteAccessManager::new(UiEventBus::new());
    manager.set_webhook_router(Arc::new(build_webhook_router(state)));
    let on = RemoteAccessConfig {
        funnel: true,
        funnel_port: 8443,
        tailscale_binary: binary,
        ..RemoteAccessConfig::default()
    };

    manager.sync(on.clone(), false).await.expect("funnel on");
    assert_eq!(
        manager.snapshot().funnel_url.as_deref(),
        Some("https://mac.tail1234.ts.net:8443")
    );
    let target = funnel_target(fake.path());
    assert!(calls(fake.path()).contains(&format!(
        "funnel --bg --https=8443 --set-path=/api/tasks/webhook {target}"
    )));

    // A delivery reaches the webhook handler; the signed task refuses it
    // without a signature, an unknown token is not found.
    let hook = format!("/api/tasks/webhook/{token}");
    assert_eq!(http_status(&target, "POST", &hook).await, 401);
    let unknown = format!("/api/tasks/webhook/{}", new_webhook_token());
    assert_eq!(http_status(&target, "POST", &unknown).await, 400);
    // Nothing else is routed on the public listener.
    for (method, path) in [
        ("GET", "/api/health"),
        ("GET", "/api/config"),
        ("GET", "/api/tasks"),
        ("POST", "/api/tasks"),
        ("POST", "/api/chat/enqueue"),
        ("GET", "/api/events"),
        ("GET", "/"),
    ] {
        assert_eq!(
            http_status(&target, method, path).await,
            404,
            "{method} {path}"
        );
    }

    // Syncing the same settings again changes nothing.
    manager.sync(on.clone(), false).await.expect("idempotent");
    let funnel_calls = |dir: &Path| {
        calls(dir)
            .iter()
            .filter(|c| c.starts_with("funnel"))
            .count()
    };
    assert_eq!(funnel_calls(fake.path()), 1);

    // Turning it off removes the handler and the listener at once.
    let off = RemoteAccessConfig {
        funnel: false,
        ..on.clone()
    };
    manager.sync(off, false).await.expect("funnel off");
    assert!(
        calls(fake.path())
            .contains(&"funnel --https=8443 --set-path=/api/tasks/webhook off".to_string())
    );
    assert_eq!(manager.snapshot().funnel_url, None);
    assert_eq!(
        std::fs::read_to_string(fake.path().join("serve.json"))
            .expect("serve.json")
            .trim(),
        "{}"
    );
    assert!(listener_is_closed(&target).await);
}

#[tokio::test]
async fn changing_the_funnel_port_moves_the_handler() {
    let home = tempfile::tempdir().expect("temp dir");
    let fake = tempfile::tempdir().expect("temp dir");
    let binary = install_fake_tailscale(fake.path());
    let state = Arc::new(
        DaemonState::for_test(home.path(), home.path(), MayConfig::default())
            .await
            .expect("daemon state"),
    );
    let manager = RemoteAccessManager::new(UiEventBus::new());
    manager.set_webhook_router(Arc::new(build_webhook_router(state)));
    let config = |port| RemoteAccessConfig {
        funnel: true,
        funnel_port: port,
        tailscale_binary: binary.clone(),
        ..RemoteAccessConfig::default()
    };

    manager.sync(config(443), false).await.expect("on 443");
    assert_eq!(
        manager.snapshot().funnel_url.as_deref(),
        Some("https://mac.tail1234.ts.net")
    );
    let first = funnel_target(fake.path());
    manager.sync(config(10000), false).await.expect("on 10000");
    assert_eq!(
        manager.snapshot().funnel_url.as_deref(),
        Some("https://mac.tail1234.ts.net:10000")
    );
    let recorded = calls(fake.path());
    assert!(recorded.contains(&"funnel --https=443 --set-path=/api/tasks/webhook off".to_string()));
    assert!(listener_is_closed(&first).await);

    // A port Tailscale does not allow tears Funnel down and reports why.
    let error = manager
        .sync(config(9000), false)
        .await
        .expect_err("port refused");
    assert!(error.contains("443, 8443 or 10000"));
    assert_eq!(manager.snapshot().funnel_url, None);
}
