use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_lite::StreamExt;
use skyzen::extract::Query;
use skyzen::responder::Sse;
use skyzen::routing::Params;
use skyzen::utils::{Json, State};
use skyzen::{Result as SkyResult, StatusCode};

use super::helpers::{bad_request_error, internal_error, send_event};
use super::state::UiServerState;
use super::types::{
    ArtifactEntry, ArtifactPreviewQuery, ArtifactStreamPayload, IfNoneMatchHeader,
    SessionArtifactsResponse, SessionRuntimeResponse,
};

pub(super) async fn get_session_runtime(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<SessionRuntimeResponse>> {
    let session_id = params.get("id").map_err(bad_request_error)?.to_string();
    if session_id.is_empty() {
        return Err(bad_request_error("session id is required"));
    }

    if state
        .permission_approver
        .snapshot_session_runtime(Some(session_id.as_str()))
        .is_some()
    {
        return Ok(Json(SessionRuntimeResponse {
            state: "waiting_permission",
        }));
    }

    let is_streaming = state
        .active_streaming_sessions
        .lock()
        .await
        .contains(session_id.as_str());

    Ok(Json(SessionRuntimeResponse {
        state: if is_streaming { "streaming" } else { "idle" },
    }))
}

async fn collect_session_artifacts(
    state: &UiServerState,
    session_id: &str,
) -> SkyResult<Vec<ArtifactEntry>> {
    let artifacts_dir = state
        .may_dir
        .join("sessions")
        .join(session_id)
        .join("workdir")
        .join("artifacts");

    let mut entries = Vec::new();

    if async_fs::metadata(&artifacts_dir).await.is_ok() {
        let mut stack = vec![artifacts_dir.clone()];

        while let Some(dir) = stack.pop() {
            let mut rd = async_fs::read_dir(&dir).await.map_err(internal_error)?;
            while let Some(entry) = rd.next().await {
                let entry = entry.map_err(internal_error)?;
                let path = entry.path();
                let meta = entry.metadata().await.map_err(internal_error)?;

                if meta.is_dir() {
                    stack.push(path);
                    continue;
                }

                if !meta.is_file() {
                    continue;
                }

                let rel = path.strip_prefix(&artifacts_dir).map_err(internal_error)?;
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                let name = rel
                    .file_name()
                    .and_then(|v| v.to_str())
                    .unwrap_or("artifact")
                    .to_string();
                let lower = name.to_lowercase();
                let is_html = lower.ends_with(".html") || lower.ends_with(".htm");

                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);

                entries.push(ArtifactEntry {
                    path: rel_str,
                    name,
                    size: meta.len(),
                    is_html,
                    fingerprint: format!("{}-{}", meta.len(), modified),
                });
            }
        }
    }

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

fn artifacts_signature(entries: &[ArtifactEntry]) -> String {
    entries
        .iter()
        .map(|e| format!("{}:{}:{}", e.path, e.fingerprint, e.is_html))
        .collect::<Vec<_>>()
        .join("|")
}

pub(super) async fn list_session_artifacts(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<SessionArtifactsResponse>> {
    let session_id = params.get("id").map_err(bad_request_error)?.to_string();
    if session_id.is_empty() {
        return Err(bad_request_error("session id is required"));
    }

    let entries = collect_session_artifacts(&state, &session_id).await?;
    Ok(Json(SessionArtifactsResponse { artifacts: entries }))
}

pub(super) async fn stream_session_artifacts(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Sse> {
    let session_id = params.get("id").map_err(bad_request_error)?.to_string();
    if session_id.is_empty() {
        return Err(bad_request_error("session id is required"));
    }

    let (sender, sse) = Sse::channel();
    let state_cloned = state.clone();

    tokio::spawn(async move {
        let mut last_sig = String::new();
        loop {
            if let Ok(entries) = collect_session_artifacts(&state_cloned, &session_id).await {
                let sig = artifacts_signature(&entries);
                if sig != last_sig {
                    send_event(
                        &sender,
                        "artifacts",
                        &ArtifactStreamPayload {
                            count: entries.len(),
                            signature: sig.clone(),
                        },
                    )
                    .await;
                    last_sig = sig;
                }
            }

            send_event(&sender, "heartbeat", &serde_json::json!({ "ok": true })).await;
            tokio::time::sleep(Duration::from_millis(800)).await;
        }
    });

    Ok(sse)
}

pub(super) async fn get_artifact_preview(
    params: Params,
    Query(query): Query<ArtifactPreviewQuery>,
    if_none_match: IfNoneMatchHeader,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<skyzen::Response> {
    let session_id = params.get("id").map_err(bad_request_error)?.to_string();
    if session_id.is_empty() {
        return Err(bad_request_error("session id is required"));
    }

    let requested = query.path.trim();
    if requested.is_empty() {
        return Err(bad_request_error("artifact path is required"));
    }

    let rel = PathBuf::from(requested);
    if rel.is_absolute() {
        return Err(bad_request_error("artifact path must be relative"));
    }
    for comp in rel.components() {
        if matches!(comp, std::path::Component::ParentDir) {
            return Err(bad_request_error("artifact path traversal is not allowed"));
        }
    }

    let artifacts_dir = state
        .may_dir
        .join("sessions")
        .join(&session_id)
        .join("workdir")
        .join("artifacts");
    let full = artifacts_dir.join(&rel);

    let canonical_artifacts = async_fs::canonicalize(&artifacts_dir)
        .await
        .map_err(internal_error)?;
    let canonical_full = async_fs::canonicalize(&full)
        .await
        .map_err(|_| bad_request_error("artifact not found"))?;
    if !canonical_full.starts_with(&canonical_artifacts) {
        return Err(bad_request_error("artifact path traversal is not allowed"));
    }

    let meta = async_fs::metadata(&canonical_full)
        .await
        .map_err(internal_error)?;
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let etag_value = format!("\"{}-{}\"", meta.len(), modified);

    let incoming_etag = if_none_match
        .0
        .as_deref()
        .map(str::trim)
        .map(|v| v.trim_start_matches("W/").trim_matches('"'));
    let current_etag = etag_value.trim_start_matches("W/").trim_matches('"');

    if incoming_etag.is_some_and(|v| v == "*" || v == current_etag) {
        let mut not_modified = skyzen::Response::new(skyzen::Body::empty());
        *not_modified.status_mut() = StatusCode::NOT_MODIFIED;
        if let Ok(etag_header) = skyzen::header::HeaderValue::from_str(&etag_value) {
            not_modified
                .headers_mut()
                .insert(skyzen::header::ETAG, etag_header);
        }
        if let Ok(cache_control) = skyzen::header::HeaderValue::from_str("private, no-cache") {
            not_modified
                .headers_mut()
                .insert(skyzen::header::CACHE_CONTROL, cache_control);
        }
        return Ok(not_modified);
    }

    let bytes = async_fs::read(&canonical_full)
        .await
        .map_err(internal_error)?;

    let ext = rel
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or("")
        .to_lowercase();

    let content_type = match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "md" | "markdown" => "text/markdown; charset=utf-8",
        "txt" | "text" | "log" | "csv" | "json" | "xml" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "pdf" => "application/pdf",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "m4a" => "audio/mp4",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        _ => "application/octet-stream",
    }
    .to_string();

    let mut response = skyzen::Response::new(skyzen::Body::from(bytes));
    if let Ok(content_type_header) = skyzen::header::HeaderValue::from_str(&content_type) {
        response
            .headers_mut()
            .insert(skyzen::header::CONTENT_TYPE, content_type_header);
    }
    if let Ok(etag_header) = skyzen::header::HeaderValue::from_str(&etag_value) {
        response
            .headers_mut()
            .insert(skyzen::header::ETAG, etag_header);
    }
    if let Ok(cache_control) = skyzen::header::HeaderValue::from_str("private, no-cache") {
        response
            .headers_mut()
            .insert(skyzen::header::CACHE_CONTROL, cache_control);
    }

    Ok(response)
}
