use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use askama::Template;
use futures_lite::StreamExt;
use notify::{
    Config as NotifyConfig, Event as NotifyEvent, RecommendedWatcher, RecursiveMode, Watcher,
};
use skyzen::extract::Query;
use skyzen::responder::Sse;
use skyzen::routing::Params;
use skyzen::utils::{Json, State};
use skyzen::{Result as SkyResult, StatusCode};
use tokio::process::Command;

use super::chat::spawn_follow_up_runs;
use super::error::{bad_request_error, internal_error};
use super::helpers::{load_session_paths, parse_safe_relative_path, send_event};
use super::local_url_mapper::{
    LocalUrlProxyKey, LocalUrlProxyUpstream, mapped_url_from_target, parse_local_proxy_target,
    target_host_header,
};
use super::state::UiServerState;
use super::types::SseHeartbeat;
use super::types::{
    ArtifactDiagnosticsRequest, ArtifactDiagnosticsResponse, ArtifactEntry, ArtifactPreviewQuery,
    ArtifactRuntimePreviewQuery, ArtifactStreamPayload, IfNoneMatchHeader, LocalUrlMapRequest,
    LocalUrlMapResponse, SessionArtifactsResponse, SessionRuntimeResponse, SlidevPreviewResponse,
};

const PREVIEW_CACHE_CONTROL: &str = "private, no-cache";
const SLIDEV_CACHE_CONTROL: &str = "private, max-age=0, must-revalidate";
const ARTIFACT_RUNTIME_BRIDGE_SRC: &str = "/api/artifact-runtime.js";
const ARTIFACT_RUNTIME_BRIDGE: &str = include_str!("../texts/artifact_runtime_bridge.js");
const ARTIFACT_RUNTIME_BRIDGE_TEMPLATE: &str =
    include_str!("../../templates/artifact_runtime_bridge.html");
const SLIDEV_TOOLCHAIN_PACKAGE_JSON: &str =
    include_str!("../../skills/slide/assets/template/package.json");
const GENERIC_VALIDATION_PROFILE: &str = "generic";
const SLIDEV_VALIDATION_PROFILE: &str = "slidev";

#[derive(Template)]
#[template(path = "artifact_runtime_bridge.html")]
struct ArtifactRuntimeBridgeTemplate<'a> {
    script_src: &'a str,
    artifact_type: &'a str,
    validation_profile: &'a str,
}

struct ResolvedSessionFile {
    canonical_path: PathBuf,
    relative_path: PathBuf,
    metadata: std::fs::Metadata,
}

pub(super) async fn get_session_runtime(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<SessionRuntimeResponse>> {
    let session_id = params.get("id").map_err(bad_request_error)?.to_string();
    if session_id.is_empty() {
        return Err(bad_request_error("session id is required"));
    }

    let runtime = state
        .runtime_service
        .get(&session_id)
        .await
        .map_err(internal_error)?;

    Ok(Json(runtime.into()))
}

async fn collect_session_artifacts(
    state: &UiServerState,
    session_id: &str,
) -> SkyResult<Vec<ArtifactEntry>> {
    let artifacts_dir = load_session_paths(state, session_id).await?.artifacts_dir();

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
                    .map_or(0, |d| d.as_nanos());

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

async fn resolve_scoped_file(
    scope_root: &Path,
    relative_path: PathBuf,
    not_found_message: &str,
    traversal_message: &str,
) -> Result<ResolvedSessionFile, skyzen::Error> {
    let full_path = scope_root.join(&relative_path);
    let canonical_scope_root = async_fs::canonicalize(scope_root)
        .await
        .map_err(|_| bad_request_error(not_found_message))?;
    let canonical_path = async_fs::canonicalize(&full_path)
        .await
        .map_err(|_| bad_request_error(not_found_message))?;
    if !canonical_path.starts_with(&canonical_scope_root) {
        return Err(bad_request_error(traversal_message));
    }

    let metadata = async_fs::metadata(&canonical_path)
        .await
        .map_err(internal_error)?;
    if !metadata.is_file() {
        return Err(bad_request_error(not_found_message));
    }

    Ok(ResolvedSessionFile {
        canonical_path,
        relative_path,
        metadata,
    })
}

fn output_relative_path(raw: &str) -> Result<PathBuf, skyzen::Error> {
    let relative_path = parse_safe_relative_path(raw, "output path")?;
    let outputs_prefix = Path::new("outputs");
    if !relative_path.starts_with(outputs_prefix) {
        return Err(bad_request_error("output path must be under outputs/"));
    }

    let scoped = relative_path
        .strip_prefix(outputs_prefix)
        .map_err(internal_error)?;
    if scoped.as_os_str().is_empty() {
        return Err(bad_request_error("output path must point to a file"));
    }

    Ok(scoped.to_path_buf())
}

fn metadata_modified_nanos(metadata: &std::fs::Metadata) -> u128 {
    metadata
        .modified()
        .ok()
        .and_then(|timestamp| timestamp.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos())
}

fn etag_value(metadata: &std::fs::Metadata) -> String {
    let mut value = String::new();
    value.push('"');
    value.push_str(metadata.len().to_string().as_str());
    value.push('-');
    value.push_str(metadata_modified_nanos(metadata).to_string().as_str());
    value.push('"');
    value
}

fn etag_matches(if_none_match: &IfNoneMatchHeader, etag: &str) -> bool {
    let current = etag.trim_start_matches("W/").trim_matches('"');
    if_none_match
        .0
        .as_deref()
        .map(str::trim)
        .map(|value| value.trim_start_matches("W/").trim_matches('"'))
        .is_some_and(|value| value == "*" || value == current)
}

fn insert_header(response: &mut skyzen::Response, header: skyzen::header::HeaderName, value: &str) {
    if let Ok(header_value) = skyzen::header::HeaderValue::from_str(value) {
        response.headers_mut().insert(header, header_value);
    }
}

fn not_modified_response(etag: &str, cache_control: &str) -> skyzen::Response {
    let mut response = skyzen::Response::new(skyzen::Body::empty());
    *response.status_mut() = StatusCode::NOT_MODIFIED;
    insert_header(&mut response, skyzen::header::ETAG, etag);
    insert_header(&mut response, skyzen::header::CACHE_CONTROL, cache_control);
    response
}

fn file_response(
    bytes: Vec<u8>,
    content_type: &str,
    etag: &str,
    cache_control: &str,
) -> skyzen::Response {
    let mut response = skyzen::Response::new(skyzen::Body::from(bytes));
    insert_header(&mut response, skyzen::header::CONTENT_TYPE, content_type);
    insert_header(&mut response, skyzen::header::ETAG, etag);
    insert_header(&mut response, skyzen::header::CACHE_CONTROL, cache_control);
    response
}

fn mime_requires_utf8(mime: &str) -> bool {
    mime.starts_with("text/")
        || matches!(
            mime,
            "application/json" | "application/javascript" | "application/xml" | "image/svg+xml"
        )
}

fn content_type_for_path(path: &Path) -> String {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let essence = mime.essence_str();
    if mime_requires_utf8(essence) {
        let mut content_type = essence.to_string();
        content_type.push_str("; charset=utf-8");
        content_type
    } else {
        essence.to_string()
    }
}

fn is_slidev_markdown_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|value| value.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("md" | "markdown")
    )
}

fn slidev_cache_key(path: &Path, meta: &std::fs::Metadata) -> String {
    let source = path.to_string_lossy();
    let mut normalized = String::with_capacity(source.len());
    for ch in source.chars() {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch.to_ascii_lowercase());
        } else {
            normalized.push('-');
        }
    }
    let normalized = normalized.trim_matches('-');
    let base = if normalized.is_empty() {
        "slides"
    } else if normalized.len() > 64 {
        &normalized[..64]
    } else {
        normalized
    };
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    let mut bridge_hasher = std::hash::DefaultHasher::new();
    ARTIFACT_RUNTIME_BRIDGE.hash(&mut bridge_hasher);
    ARTIFACT_RUNTIME_BRIDGE_TEMPLATE.hash(&mut bridge_hasher);
    ARTIFACT_RUNTIME_BRIDGE_SRC.hash(&mut bridge_hasher);
    SLIDEV_VALIDATION_PROFILE.hash(&mut bridge_hasher);
    format!(
        "{base}-{}-{modified}-{:016x}",
        meta.len(),
        bridge_hasher.finish()
    )
}

fn validate_slidev_cache_key(value: &str) -> Result<(), skyzen::Error> {
    if value.is_empty() {
        return Err(bad_request_error("slidev cache key is required"));
    }
    if value.len() > 160 {
        return Err(bad_request_error("slidev cache key is too long"));
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        return Err(bad_request_error(
            "slidev cache key contains invalid characters",
        ));
    }
    Ok(())
}

const SLIDEV_BUILD_TIMEOUT: Duration = Duration::from_secs(180);
const SLIDEV_DEP_INSTALL_TIMEOUT: Duration = Duration::from_secs(120);
const SLIDEV_BUILD_LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(240);
/// A live builder refreshes the lock file mtime at this cadence.
const SLIDEV_BUILD_LOCK_REFRESH_INTERVAL: Duration = Duration::from_secs(10);
/// A lock whose mtime is older than this belongs to a crashed builder and is broken.
const SLIDEV_BUILD_LOCK_STALE_TIMEOUT: Duration = Duration::from_secs(60);
const SLIDEV_MAX_AUTOINSTALL_ROUNDS: usize = 8;
const MAX_ARTIFACT_DIAGNOSTICS: usize = 16;
const MAX_ARTIFACT_DIAGNOSTIC_CHARS: usize = 2_000;
const ARTIFACT_DIAGNOSTIC_TRUNCATION_SUFFIX: &str = " ... [truncated]";

fn is_slidev_separator(line: &str) -> bool {
    line.trim() == "---"
}

fn valid_slidev_frontmatter(lines: &[&str]) -> bool {
    let source = lines.join("\n");
    let Ok(serde_yml::Value::Mapping(mapping)) = serde_yml::from_str(&source) else {
        return false;
    };
    !mapping.is_empty()
        && mapping.keys().all(|key| {
            let serde_yml::Value::String(key) = key else {
                return false;
            };
            !key.is_empty()
                && key
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || "_-".contains(character))
        })
}

fn slide_has_visible_content(lines: &[&str]) -> bool {
    let mut inside_style = false;
    for line in lines {
        let trimmed = line.trim();
        if inside_style {
            if trimmed.contains("</style>") {
                inside_style = false;
            }
            continue;
        }
        if trimmed.starts_with("<style") {
            inside_style = !trimmed.contains("</style>");
            continue;
        }
        if trimmed.is_empty()
            || trimmed.starts_with("<!--")
            || trimmed.starts_with("-->")
            || trimmed.starts_with("<script")
            || trimmed == "</script>"
        {
            continue;
        }
        return true;
    }
    false
}

fn slidev_source_diagnostics(source: &str) -> Vec<String> {
    let lines = source.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return vec!["Slidev source is empty.".to_string()];
    }

    let mut cursor = 0usize;
    if lines.first().is_some_and(|line| is_slidev_separator(line)) {
        let Some(headmatter_end) = lines
            .iter()
            .enumerate()
            .skip(1)
            .find_map(|(index, line)| is_slidev_separator(line).then_some(index))
        else {
            return vec!["Slidev headmatter is not closed with `---`.".to_string()];
        };
        cursor = headmatter_end + 1;
    }

    let mut diagnostics = Vec::new();
    let mut slides = Vec::new();
    let mut content_start = cursor;
    while cursor < lines.len() {
        if !is_slidev_separator(lines[cursor]) {
            cursor += 1;
            continue;
        }

        slides.push(&lines[content_start..cursor]);
        let next_line = cursor + 1;
        if next_line >= lines.len() || lines[next_line].trim().is_empty() {
            content_start = next_line;
            cursor = next_line;
            continue;
        }

        let frontmatter_end = lines
            .iter()
            .enumerate()
            .skip(next_line)
            .find_map(|(index, line)| is_slidev_separator(line).then_some(index));
        if let Some(frontmatter_end) = frontmatter_end
            && valid_slidev_frontmatter(&lines[next_line..frontmatter_end])
        {
            content_start = frontmatter_end + 1;
            cursor = content_start;
            continue;
        }

        diagnostics.push(format!(
            "Ambiguous Slidev separator at line {}: add a blank line after `---`, or provide a valid YAML frontmatter block closed by `---`.",
            cursor + 1
        ));
        content_start = next_line;
        cursor = next_line;
    }
    slides.push(&lines[content_start..]);

    for (index, slide) in slides.into_iter().enumerate() {
        if !slide_has_visible_content(slide) {
            diagnostics.push(format!(
                "Slide {} has no visible content after style and script blocks are removed.",
                index + 1
            ));
        }
    }
    diagnostics
}

fn slidev_failure_detail(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        format!("exit status {}", output.status)
    }
}

fn is_valid_npm_package_spec(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "@/_-+.".contains(ch))
}

fn is_valid_iconify_collection(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
}

fn is_valid_icon_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct MissingSlidevIcon {
    collection: String,
    name: String,
}

fn extract_missing_slidev_package(detail: &str) -> Option<String> {
    const MARKERS: [&str; 3] = ["The theme \"", "The addon \"", "The package \""];
    for marker in MARKERS {
        let Some(start) = detail.find(marker) else {
            continue;
        };
        let rest = &detail[start + marker.len()..];
        let Some(end) = rest.find('"') else {
            continue;
        };
        let pkg = rest[..end].trim();
        if is_valid_npm_package_spec(pkg) {
            return Some(pkg.to_string());
        }
    }
    None
}

fn extract_missing_slidev_icon(detail: &str) -> Option<MissingSlidevIcon> {
    const ICON_MARKER: &str = "Icon `";
    let Some(start) = detail.find(ICON_MARKER) else {
        return None;
    };
    let rest = &detail[start + ICON_MARKER.len()..];
    let end = rest.find('`')?;
    let icon = rest[..end].trim();
    let (collection, name) = icon.split_once('/')?;
    if !is_valid_iconify_collection(collection) {
        return None;
    }
    if is_valid_icon_name(name) {
        Some(MissingSlidevIcon {
            collection: collection.to_string(),
            name: name.to_string(),
        })
    } else {
        None
    }
}

async fn ensure_slidev_toolchain_dir(toolchain_dir: &Path) -> Result<(), skyzen::Error> {
    async_fs::create_dir_all(toolchain_dir)
        .await
        .map_err(internal_error)?;
    let package_json = toolchain_dir.join("package.json");
    let needs_manifest = match async_fs::read_to_string(&package_json).await {
        Ok(existing) => {
            let manifest: serde_json::Value =
                serde_json::from_str(existing.as_str()).map_err(internal_error)?;
            !manifest
                .get("dependencies")
                .and_then(serde_json::Value::as_object)
                .is_some_and(|dependencies| dependencies.contains_key("@slidev/cli"))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => true,
        Err(error) => return Err(internal_error(error)),
    };
    if needs_manifest {
        async_fs::write(&package_json, SLIDEV_TOOLCHAIN_PACKAGE_JSON)
            .await
            .map_err(internal_error)?;
    }

    let slidev_cli = toolchain_dir.join("node_modules/@slidev/cli/bin/slidev.mjs");
    if async_fs::metadata(slidev_cli).await.is_err() {
        install_slidev_toolchain(toolchain_dir).await?;
    }
    Ok(())
}

fn configure_internal_bun_command(command: &mut Command) {
    for variable in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        command.env_remove(variable);
    }
}

async fn install_slidev_toolchain(toolchain_dir: &Path) -> Result<(), skyzen::Error> {
    let mut command = Command::new("bun");
    command
        .arg("install")
        .arg("--silent")
        .arg("--no-progress")
        .current_dir(toolchain_dir)
        .env("CI", "1")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    configure_internal_bun_command(&mut command);

    let output = tokio::time::timeout(SLIDEV_DEP_INSTALL_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            internal_error(format!(
                "slidev toolchain install timed out after {}s",
                SLIDEV_DEP_INSTALL_TIMEOUT.as_secs()
            ))
        })?
        .map_err(internal_error)?;

    if !output.status.success() {
        let detail = slidev_failure_detail(&output);
        return Err(internal_error(format!(
            "slidev toolchain install failed: {detail}"
        )));
    }

    Ok(())
}

async fn install_slidev_dependency(
    toolchain_dir: &Path,
    package: &str,
) -> Result<(), skyzen::Error> {
    let mut command = Command::new("bun");
    command
        .arg("add")
        .arg(package)
        .arg("--silent")
        .arg("--no-progress")
        .current_dir(toolchain_dir)
        .env("CI", "1")
        .stdin(Stdio::null())
        // Reap the child when the timeout drops the output future; otherwise
        // an orphaned bun keeps holding its global install lock.
        .kill_on_drop(true);
    configure_internal_bun_command(&mut command);

    let output = tokio::time::timeout(SLIDEV_DEP_INSTALL_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            internal_error(format!(
                "slidev dependency install timed out after {}s: {package}",
                SLIDEV_DEP_INSTALL_TIMEOUT.as_secs()
            ))
        })?
        .map_err(internal_error)?;

    if !output.status.success() {
        let detail = slidev_failure_detail(&output);
        return Err(internal_error(format!(
            "slidev dependency install failed for {package}: {detail}"
        )));
    }

    Ok(())
}

fn replacement_icon_name(name: &str) -> Option<&'static str> {
    match name {
        "prev" => Some("arrow-left"),
        "next" => Some("arrow-right"),
        "left" => Some("arrow-left"),
        "right" => Some("arrow-right"),
        _ => None,
    }
}

fn rewrite_icon_references(
    content: &str,
    missing_icon: &MissingSlidevIcon,
    replacement_name: &str,
) -> String {
    let tag_from = format!("<{}:{}", missing_icon.collection, missing_icon.name);
    let tag_to = format!("<{}:{replacement_name}", missing_icon.collection);
    let import_from = format!("~icons/{}/{}", missing_icon.collection, missing_icon.name);
    let import_to = format!("~icons/{}/{}", missing_icon.collection, replacement_name);

    content
        .replace(tag_from.as_str(), tag_to.as_str())
        .replace(import_from.as_str(), import_to.as_str())
}

async fn replace_missing_slidev_icon_in_entry(
    entry_path: &Path,
    missing_icon: &MissingSlidevIcon,
    replacement_name: &str,
) -> Result<(), skyzen::Error> {
    let original = async_fs::read_to_string(entry_path)
        .await
        .map_err(internal_error)?;
    let rewritten = rewrite_icon_references(original.as_str(), missing_icon, replacement_name);
    if rewritten == original {
        return Err(internal_error(format!(
            "slidev icon replacement could not find {}:{} in {}",
            missing_icon.collection,
            missing_icon.name,
            entry_path.display()
        )));
    }
    async_fs::write(entry_path, rewritten)
        .await
        .map_err(internal_error)?;
    Ok(())
}

async fn build_slidev_with_autoinstall(
    entry_path: &Path,
    output_dir: &Path,
    base_url: &str,
    toolchain_dir: &Path,
) -> Result<(), skyzen::Error> {
    ensure_slidev_toolchain_dir(toolchain_dir).await?;

    let mut installed_packages = BTreeSet::new();
    let mut icon_rewrites = BTreeSet::new();
    for _ in 0..SLIDEV_MAX_AUTOINSTALL_ROUNDS {
        let mut command = Command::new("node");
        command
            .arg(toolchain_dir.join("node_modules/@slidev/cli/bin/slidev.mjs"))
            .arg("build")
            .arg(entry_path.as_os_str())
            .arg("--out")
            .arg(output_dir.as_os_str())
            .arg("--base")
            .arg(base_url)
            .current_dir(toolchain_dir)
            .env("CI", "1")
            .stdin(Stdio::null())
            // Reap the child when the timeout drops the output future.
            .kill_on_drop(true);

        let output = tokio::time::timeout(SLIDEV_BUILD_TIMEOUT, command.output())
            .await
            .map_err(|_| {
                internal_error(format!(
                    "slidev build timed out after {}s",
                    SLIDEV_BUILD_TIMEOUT.as_secs()
                ))
            })?
            .map_err(internal_error)?;

        if output.status.success() {
            return Ok(());
        }

        let detail = slidev_failure_detail(&output);
        if let Some(package) = extract_missing_slidev_package(&detail) {
            if !installed_packages.insert(package.clone()) {
                return Err(internal_error(format!(
                    "slidev build repeatedly requested missing dependency {package}"
                )));
            }
            install_slidev_dependency(toolchain_dir, package.as_str()).await?;
            continue;
        }

        if let Some(missing_icon) = extract_missing_slidev_icon(&detail) {
            let icon_package = format!("@iconify-json/{}", missing_icon.collection);
            if installed_packages.insert(icon_package.clone()) {
                install_slidev_dependency(toolchain_dir, icon_package.as_str()).await?;
                continue;
            }

            let Some(replacement_name) = replacement_icon_name(missing_icon.name.as_str()) else {
                return Err(internal_error(format!(
                    "slidev icon {}:{} is unavailable and has no known replacement",
                    missing_icon.collection, missing_icon.name
                )));
            };

            let rewrite_key = format!(
                "{}:{}=>{}",
                missing_icon.collection, missing_icon.name, replacement_name
            );
            if !icon_rewrites.insert(rewrite_key.clone()) {
                return Err(internal_error(format!(
                    "slidev icon rewrite loop detected for {rewrite_key}"
                )));
            }

            replace_missing_slidev_icon_in_entry(entry_path, &missing_icon, replacement_name)
                .await?;
            continue;
        }

        return Err(internal_error(format!("slidev build failed: {detail}")));
    }

    Err(internal_error(format!(
        "slidev build exceeded automatic dependency install limit ({SLIDEV_MAX_AUTOINSTALL_ROUNDS})"
    )))
}

async fn copy_directory_recursive(
    source_root: &Path,
    target_root: &Path,
) -> Result<(), skyzen::Error> {
    let mut stack = vec![(source_root.to_path_buf(), target_root.to_path_buf())];

    while let Some((source_dir, target_dir)) = stack.pop() {
        async_fs::create_dir_all(&target_dir)
            .await
            .map_err(internal_error)?;

        let mut rd = async_fs::read_dir(&source_dir)
            .await
            .map_err(internal_error)?;
        while let Some(entry) = rd.next().await {
            let entry = entry.map_err(internal_error)?;
            let source_path = entry.path();
            let target_path = target_dir.join(entry.file_name());
            let meta = entry.metadata().await.map_err(internal_error)?;

            if meta.is_dir() {
                stack.push((source_path, target_path));
                continue;
            }
            if !meta.is_file() {
                continue;
            }

            async_fs::copy(&source_path, &target_path)
                .await
                .map_err(internal_error)?;
        }
    }

    Ok(())
}

fn instrument_artifact_html(
    html: &str,
    artifact_type: &str,
    validation_profile: &str,
) -> Result<String, skyzen::Error> {
    let bridge = ArtifactRuntimeBridgeTemplate {
        script_src: ARTIFACT_RUNTIME_BRIDGE_SRC,
        artifact_type,
        validation_profile,
    }
    .render()
    .map_err(internal_error)?;
    let Some(bridge_tag) = bridge.strip_suffix("</head>") else {
        return Err(internal_error(
            "artifact runtime bridge template must end with a closing head tag",
        ));
    };
    if html.contains(bridge_tag) {
        return Ok(html.to_string());
    }
    if html.contains(ARTIFACT_RUNTIME_BRIDGE_SRC) {
        return Err(internal_error(
            "artifact HTML contains a mismatched runtime bridge",
        ));
    }
    if !html.contains("</head>") {
        return Err(internal_error(
            "artifact HTML does not contain a closing head tag",
        ));
    }
    Ok(html.replacen("</head>", bridge.as_str(), 1))
}

async fn ensure_slidev_runtime_bridge(index_file: &Path) -> Result<(), skyzen::Error> {
    let index = async_fs::read_to_string(index_file)
        .await
        .map_err(internal_error)?;
    let instrumented =
        instrument_artifact_html(index.as_str(), "slidev", SLIDEV_VALIDATION_PROFILE)?;
    if instrumented == index {
        return Ok(());
    }
    async_fs::write(index_file, instrumented)
        .await
        .map_err(internal_error)?;
    Ok(())
}

async fn ensure_slidev_source_root(
    toolchain_dir: &Path,
    cache_key: &str,
    artifacts_root: &Path,
) -> Result<PathBuf, skyzen::Error> {
    let source_parent = toolchain_dir.join("sources");
    async_fs::create_dir_all(&source_parent)
        .await
        .map_err(internal_error)?;

    let source_root = source_parent.join(cache_key);
    if let Ok(meta) = async_fs::symlink_metadata(&source_root).await {
        let file_type = meta.file_type();
        if file_type.is_symlink() || file_type.is_file() {
            async_fs::remove_file(&source_root)
                .await
                .map_err(internal_error)?;
        } else if file_type.is_dir() {
            async_fs::remove_dir_all(&source_root)
                .await
                .map_err(internal_error)?;
        } else {
            return Err(internal_error("unsupported slidev source entry type"));
        }
    }

    copy_directory_recursive(artifacts_root, &source_root).await?;
    Ok(source_root)
}

#[derive(Debug)]
struct SlidevBuildLock {
    path: PathBuf,
    refresher: tokio::task::JoinHandle<()>,
}

impl Drop for SlidevBuildLock {
    fn drop(&mut self) {
        self.refresher.abort();
    }
}

/// Whether the lock file's mtime shows no recent heartbeat from a live builder.
async fn slidev_lock_is_stale(lock_path: &Path) -> bool {
    let Ok(meta) = async_fs::metadata(lock_path).await else {
        // Lock vanished between the failed create and this check; retry.
        return true;
    };
    meta.modified()
        .ok()
        .and_then(|mtime| mtime.elapsed().ok())
        .is_some_and(|age| age > SLIDEV_BUILD_LOCK_STALE_TIMEOUT)
}

/// Rewrites the lock file in place to bump its mtime; exits once the lock is gone.
async fn refresh_slidev_lock(lock_path: PathBuf) {
    use futures_lite::AsyncWriteExt;

    loop {
        tokio::time::sleep(SLIDEV_BUILD_LOCK_REFRESH_INTERVAL).await;
        // `create(false)` semantics: never resurrect a lock another request broke.
        let opened = async_fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&lock_path)
            .await;
        let Ok(mut file) = opened else {
            return;
        };
        if file.write_all(b"x").await.is_err() || file.flush().await.is_err() {
            return;
        }
    }
}

async fn acquire_slidev_build_lock(
    lock_path: &Path,
    index_file: &Path,
    diagnostics_file: &Path,
) -> Result<Option<SlidevBuildLock>, skyzen::Error> {
    let started_at = tokio::time::Instant::now();

    loop {
        if async_fs::metadata(index_file).await.is_ok()
            || async_fs::metadata(diagnostics_file).await.is_ok()
        {
            return Ok(None);
        }

        match async_fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock_path)
            .await
        {
            Ok(_) => {
                let refresher = tokio::spawn(refresh_slidev_lock(lock_path.to_path_buf()));
                return Ok(Some(SlidevBuildLock {
                    path: lock_path.to_path_buf(),
                    refresher,
                }));
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                if slidev_lock_is_stale(lock_path).await {
                    tracing::warn!(
                        path = %lock_path.display(),
                        "breaking stale slidev build lock left by a crashed build"
                    );
                    let _ = async_fs::remove_file(lock_path).await;
                    continue;
                }
                if started_at.elapsed() >= SLIDEV_BUILD_LOCK_WAIT_TIMEOUT {
                    return Err(internal_error(format!(
                        "slidev build lock wait timed out after {}s",
                        SLIDEV_BUILD_LOCK_WAIT_TIMEOUT.as_secs()
                    )));
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(error) => return Err(internal_error(error)),
        }
    }
}

async fn read_slidev_build_diagnostics(
    diagnostics_file: &Path,
) -> Result<Option<Vec<String>>, skyzen::Error> {
    let bytes = match async_fs::read(diagnostics_file).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(internal_error(error)),
    };
    serde_json::from_slice(bytes.as_slice())
        .map(Some)
        .map_err(internal_error)
}

async fn write_slidev_build_diagnostics(
    diagnostics_file: &Path,
    diagnostics: &[String],
) -> Result<(), skyzen::Error> {
    let bytes = serde_json::to_vec(diagnostics).map_err(internal_error)?;
    async_fs::write(diagnostics_file, bytes)
        .await
        .map_err(internal_error)
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

pub(super) async fn prepare_slidev_preview(
    params: Params,
    Query(query): Query<ArtifactPreviewQuery>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<SlidevPreviewResponse>> {
    let session_id = params.get("id").map_err(bad_request_error)?.to_string();
    if session_id.is_empty() {
        return Err(bad_request_error("session id is required"));
    }

    let raw_rel = parse_safe_relative_path(query.path.as_str(), "artifact path")?;
    if !is_slidev_markdown_path(&raw_rel) {
        return Err(bad_request_error(
            "slidev preview requires a markdown artifact (.md/.markdown)",
        ));
    }

    let session_paths = load_session_paths(&state, &session_id).await?;
    let session_dir = session_paths.root().to_path_buf();
    let resolved = resolve_scoped_file(
        &session_paths.artifacts_dir(),
        raw_rel.clone(),
        "artifact not found",
        "artifact path traversal is not allowed",
    )
    .await?;
    let source = async_fs::read_to_string(&resolved.canonical_path)
        .await
        .map_err(internal_error)?;
    let diagnostics = slidev_source_diagnostics(source.as_str());
    if !diagnostics.is_empty() {
        return Ok(Json(SlidevPreviewResponse {
            preview_url: None,
            diagnostics,
        }));
    }
    let cache_key = slidev_cache_key(&raw_rel, &resolved.metadata);
    let cache_root = session_dir.join(".slidev-cache");
    async_fs::create_dir_all(&cache_root)
        .await
        .map_err(internal_error)?;
    let cache_dir = cache_root.join(&cache_key);
    let index_file = cache_dir.join("index.html");
    let diagnostics_file = cache_dir.join("diagnostics.json");
    let toolchain_dir = session_dir.join(".slidev-toolchain");
    let lock_path = cache_root.join("build.lock");

    let preview_url =
        format!("/api/sessions/{session_id}/artifacts/slidev-static/{cache_key}/index.html");

    if let Some(diagnostics) = read_slidev_build_diagnostics(&diagnostics_file).await? {
        return Ok(Json(SlidevPreviewResponse {
            preview_url: None,
            diagnostics,
        }));
    }

    if async_fs::metadata(&index_file).await.is_err() {
        let artifacts_dir = session_paths.artifacts_dir();
        let base_url = format!("/api/sessions/{session_id}/artifacts/slidev-static/{cache_key}/");
        let build_cache_dir = cache_dir.clone();
        let build_index_file = index_file.clone();
        let build_diagnostics_file = diagnostics_file.clone();
        // The build shells out through `tokio::process`, which needs a Tokio
        // reactor; skyzen handlers run outside one, so hop onto the shared
        // Tokio runtime for the whole build.
        let build = state.tokio_handle.spawn(async move {
            let Some(lock) = acquire_slidev_build_lock(
                lock_path.as_path(),
                build_index_file.as_path(),
                build_diagnostics_file.as_path(),
            )
            .await?
            else {
                // Another request completed the build while we waited for lock acquisition.
                return read_slidev_build_diagnostics(build_diagnostics_file.as_path()).await;
            };

            if async_fs::metadata(&build_index_file).await.is_ok() {
                let _ = async_fs::remove_file(lock.path.as_path()).await;
                return Ok(None);
            }

            let _ = async_fs::remove_dir_all(&build_cache_dir).await;
            async_fs::create_dir_all(&build_cache_dir)
                .await
                .map_err(internal_error)?;

            let source_root = ensure_slidev_source_root(
                toolchain_dir.as_path(),
                cache_key.as_str(),
                &artifacts_dir,
            )
            .await?;
            let build_entry = source_root.join(&raw_rel);
            let build_result = build_slidev_with_autoinstall(
                build_entry.as_path(),
                build_cache_dir.as_path(),
                base_url.as_str(),
                toolchain_dir.as_path(),
            )
            .await;
            let _ = async_fs::remove_file(lock.path.as_path()).await;
            if let Err(error) = build_result {
                let diagnostics = vec![format!("Slidev preview build failed: {error}")];
                write_slidev_build_diagnostics(
                    build_diagnostics_file.as_path(),
                    diagnostics.as_slice(),
                )
                .await?;
                return Ok(Some(diagnostics));
            }

            if async_fs::metadata(&build_index_file).await.is_err() {
                return Err(internal_error("slidev build completed without index.html"));
            }
            Ok(None)
        });
        let build_diagnostics = build
            .await
            .map_err(|error| internal_error(format!("slidev build task failed: {error}")))??;
        if let Some(diagnostics) = build_diagnostics {
            return Ok(Json(SlidevPreviewResponse {
                preview_url: None,
                diagnostics,
            }));
        }
    }
    ensure_slidev_runtime_bridge(index_file.as_path()).await?;

    Ok(Json(SlidevPreviewResponse {
        preview_url: Some(preview_url),
        diagnostics: Vec::new(),
    }))
}

#[derive(serde::Serialize, skyzen::ToSchema)]
struct ArtifactValidationReminder<'a> {
    #[serde(rename = "@type")]
    reminder_type: &'static str,
    validation_id: &'a str,
    artifact_type: &'a str,
    path: &'a str,
    diagnostics: &'a [String],
    instruction: &'static str,
}

fn normalized_artifact_diagnostics(diagnostics: Vec<String>) -> Result<Vec<String>, skyzen::Error> {
    if diagnostics.is_empty() {
        return Err(bad_request_error(
            "artifact diagnostics must contain at least one error",
        ));
    }

    let total = diagnostics.len();
    let retained = total.min(MAX_ARTIFACT_DIAGNOSTICS);
    let mut normalized = diagnostics
        .into_iter()
        .take(retained)
        .map(|diagnostic| {
            let diagnostic = diagnostic.trim();
            if diagnostic.is_empty() {
                return Err(bad_request_error("artifact diagnostic cannot be empty"));
            }

            if diagnostic.chars().count() <= MAX_ARTIFACT_DIAGNOSTIC_CHARS {
                return Ok(diagnostic.to_string());
            }

            let retained_chars = MAX_ARTIFACT_DIAGNOSTIC_CHARS
                .checked_sub(ARTIFACT_DIAGNOSTIC_TRUNCATION_SUFFIX.chars().count())
                .expect("artifact diagnostic truncation suffix must fit the diagnostic limit");
            let mut truncated = diagnostic.chars().take(retained_chars).collect::<String>();
            truncated.push_str(ARTIFACT_DIAGNOSTIC_TRUNCATION_SUFFIX);
            Ok(truncated)
        })
        .collect::<Result<Vec<_>, _>>()?;

    if total > retained {
        normalized[MAX_ARTIFACT_DIAGNOSTICS - 1] = format!(
            "{} additional artifact diagnostics were omitted.",
            total - MAX_ARTIFACT_DIAGNOSTICS + 1
        );
    }
    Ok(normalized)
}

fn artifact_validation_id(
    artifact_type: &str,
    path: &Path,
    metadata: &std::fs::Metadata,
    diagnostics: &[String],
) -> String {
    let mut hasher = std::hash::DefaultHasher::new();
    artifact_type.hash(&mut hasher);
    path.hash(&mut hasher);
    metadata.len().hash(&mut hasher);
    metadata_modified_nanos(metadata).hash(&mut hasher);
    diagnostics.hash(&mut hasher);
    format!("artifact-validation-{:016x}", hasher.finish())
}

pub(super) async fn report_artifact_diagnostics(
    params: Params,
    Json(request): Json<ArtifactDiagnosticsRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ArtifactDiagnosticsResponse>> {
    let session_id = params.get("id").map_err(bad_request_error)?.to_string();
    if session_id.is_empty() {
        return Err(bad_request_error("session id is required"));
    }

    let artifact_type = request.artifact_type.as_str();
    let diagnostics = normalized_artifact_diagnostics(request.diagnostics)?;
    let relative_path = parse_safe_relative_path(request.path.as_str(), "artifact path")?;
    let session_paths = load_session_paths(&state, &session_id).await?;
    let resolved = resolve_scoped_file(
        &session_paths.artifacts_dir(),
        relative_path,
        "artifact not found",
        "artifact path traversal is not allowed",
    )
    .await?;
    let validation_id = artifact_validation_id(
        artifact_type,
        &resolved.relative_path,
        &resolved.metadata,
        diagnostics.as_slice(),
    );

    let messages = state
        .db()
        .get_messages(&session_id)
        .await
        .map_err(internal_error)?;
    if messages
        .iter()
        .any(|message| message.content.contains(validation_id.as_str()))
    {
        return Ok(Json(ArtifactDiagnosticsResponse {
            repair_scheduled: false,
        }));
    }

    let reminder = ArtifactValidationReminder {
        reminder_type: "artifact_validation_failed",
        validation_id: validation_id.as_str(),
        artifact_type,
        path: request.path.as_str(),
        diagnostics: diagnostics.as_slice(),
        instruction: "The declared artifact failed validation. Inspect and repair the source, validate it again, and do not claim completion until it renders without diagnostics. Return the artifact control tag again after the repair.",
    };
    let reminder_xml =
        crate::xml::serialize_xml_block("system-reminder", &reminder).map_err(internal_error)?;
    let hidden_context_xml = crate::XmlBlock::parse(reminder_xml).map_err(internal_error)?;
    let pending = crate::session::PendingUserMessage {
        content: String::new(),
        attachments: Vec::new(),
        hidden_context_xml: Some(hidden_context_xml),
        created_at: jiff::Timestamp::now().to_string(),
        delivery_mode: crate::session::PendingUserMessageMode::Queue,
    };
    let runtime = state
        .runtime_service
        .enqueue(
            &session_id,
            pending,
            crate::session::PendingUserMessageMode::Queue,
        )
        .await
        .map_err(internal_error)?;
    if runtime.can_dispatch_follow_up() {
        spawn_follow_up_runs(state, session_id);
    }

    Ok(Json(ArtifactDiagnosticsResponse {
        repair_scheduled: true,
    }))
}

pub(super) async fn get_slidev_static_asset(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<skyzen::Response> {
    let session_id = params.get("id").map_err(bad_request_error)?.to_string();
    if session_id.is_empty() {
        return Err(bad_request_error("session id is required"));
    }

    let cache_key = params
        .get("buildKey")
        .map_err(bad_request_error)?
        .to_string();
    validate_slidev_cache_key(cache_key.as_str())?;

    let asset_path = params.get("path").unwrap_or("index.html");
    let relative_asset_path = parse_safe_relative_path(asset_path, "slidev static asset path")?;

    let session_paths = load_session_paths(&state, &session_id).await?;
    let cache_root = session_paths.root().join(".slidev-cache").join(&cache_key);
    let resolved = resolve_scoped_file(
        &cache_root,
        relative_asset_path,
        "slidev asset not found",
        "slidev asset path traversal is not allowed",
    )
    .await?;

    let bytes = async_fs::read(&resolved.canonical_path)
        .await
        .map_err(internal_error)?;
    Ok(file_response(
        bytes,
        content_type_for_path(&resolved.relative_path).as_str(),
        etag_value(&resolved.metadata).as_str(),
        SLIDEV_CACHE_CONTROL,
    ))
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
    let session_paths = load_session_paths(&state, &session_id).await?;
    let artifacts_root = session_paths.artifacts_dir();

    async_fs::create_dir_all(&artifacts_root)
        .await
        .map_err(internal_error)?;

    let (watch_tx, mut watch_rx) =
        tokio::sync::mpsc::unbounded_channel::<notify::Result<NotifyEvent>>();
    let mut watcher = RecommendedWatcher::new(
        move |event| {
            let _ = watch_tx.send(event);
        },
        NotifyConfig::default(),
    )
    .map_err(internal_error)?;
    watcher
        .watch(&artifacts_root, RecursiveMode::Recursive)
        .map_err(internal_error)?;

    state.tokio_handle.spawn(async move {
        let _watcher = watcher;
        let mut last_sig = String::new();
        let mut heartbeat_tick = tokio::time::interval(Duration::from_secs(15));

        if !send_event(&sender, "heartbeat", &SseHeartbeat { ok: true }).await {
            return;
        }

        if !emit_artifacts_if_changed(&state_cloned, &sender, &session_id, &mut last_sig).await {
            return;
        }

        loop {
            tokio::select! {
                _ = heartbeat_tick.tick() => {
                    if !send_event(&sender, "heartbeat", &SseHeartbeat { ok: true }).await {
                        break;
                    }
                }
                maybe_event = watch_rx.recv() => {
                    match maybe_event {
                        Some(Ok(event)) => {
                            if !event_touches_artifacts(&event, &artifacts_root) {
                                continue;
                            }
                            if !emit_artifacts_if_changed(&state_cloned, &sender, &session_id, &mut last_sig).await {
                                break;
                            }
                        }
                        Some(Err(error)) => {
                            tracing::warn!(session_id, error = %error, "artifact watcher error");
                        }
                        None => break,
                    }
                }
            }
        }
    });

    Ok(sse)
}

fn event_touches_artifacts(event: &NotifyEvent, artifacts_root: &Path) -> bool {
    event
        .paths
        .iter()
        .any(|path| path.starts_with(artifacts_root))
}

async fn emit_artifacts_if_changed(
    state: &UiServerState,
    sender: &skyzen::responder::sse::Sender,
    session_id: &str,
    last_sig: &mut String,
) -> bool {
    match collect_session_artifacts(state, session_id).await {
        Ok(entries) => {
            let sig = artifacts_signature(&entries);
            if sig != *last_sig {
                if !send_event(
                    sender,
                    "artifacts",
                    &ArtifactStreamPayload {
                        count: entries.len(),
                        signature: sig.clone(),
                    },
                )
                .await
                {
                    return false;
                }
                *last_sig = sig;
            }
        }
        Err(error) => {
            tracing::warn!(session_id, error = %error, "failed to collect session artifacts");
        }
    }
    true
}

fn is_container_local_proxy_host(host: &str) -> bool {
    let normalized = host
        .trim()
        .trim_matches('[')
        .trim_matches(']')
        .to_lowercase();
    if normalized == "localhost"
        || normalized == "0.0.0.0"
        || normalized == "::1"
        || normalized == "host.docker.internal"
        || normalized == "host.containers.internal"
    {
        return true;
    }
    if normalized.starts_with("127.") {
        return true;
    }
    false
}

#[derive(Debug)]
struct ResolvedContainerProxyTarget {
    container_id: String,
    container_ip: String,
}

async fn resolve_container_proxy_target(
    state: &UiServerState,
    session_id: &str,
) -> Result<ResolvedContainerProxyTarget, skyzen::Error> {
    let container_manager = state
        .container_manager
        .as_ref()
        .ok_or_else(|| {
            skyzen::Error::msg("container runtime unavailable")
                .set_status(StatusCode::SERVICE_UNAVAILABLE)
        })?
        .clone();

    let db = state.db();

    let session = db
        .get_session(session_id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| bad_request_error("session not found"))?;
    let profile_id = session
        .profile_id
        .ok_or_else(|| bad_request_error("session has no profile"))?;
    let profile = db
        .get_profile(&profile_id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| bad_request_error("profile not found"))?;
    let environment = crate::db::ProfileEnvironmentKind::from_str(profile.environment.as_str())
        .map_err(internal_error)?;

    if environment != crate::db::ProfileEnvironmentKind::Container {
        return Err(
            skyzen::Error::msg("local-url proxy is only available for container profiles")
                .set_status(StatusCode::BAD_REQUEST),
        );
    }

    let session_id_obj = crate::session::SessionId::from_string(session_id);
    let container_name =
        crate::container::profile::container_name_for_profile_session(&profile, &session_id_obj);
    let container_id = container_manager
        .resolve_running_container_id_by_name(container_name.as_str())
        .await
        .map_err(internal_error)?
        .ok_or_else(|| {
            skyzen::Error::msg("container is not running for this session")
                .set_status(StatusCode::CONFLICT)
        })?;

    let inspect = container_manager
        .runtime()
        .client
        .inspect_container(&container_id, None)
        .await
        .map_err(internal_error)?;

    let container_ip = inspect
        .network_settings
        .and_then(|settings| settings.networks)
        .and_then(|networks| {
            networks
                .into_values()
                .find_map(|network| network.ip_address.filter(|ip| !ip.is_empty()))
        })
        .ok_or_else(|| {
            skyzen::Error::msg("container has no reachable network ip")
                .set_status(StatusCode::BAD_GATEWAY)
        })?;

    container_manager.touch_activity(&container_id).await;

    Ok(ResolvedContainerProxyTarget {
        container_id,
        container_ip,
    })
}

pub(super) async fn map_container_local_url(
    params: Params,
    Json(payload): Json<LocalUrlMapRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<LocalUrlMapResponse>> {
    let session_id = params.get("id").map_err(bad_request_error)?.to_string();
    if session_id.is_empty() {
        return Err(bad_request_error("session id is required"));
    }

    let raw_url = payload.url.trim();
    if raw_url.is_empty() {
        return Err(bad_request_error("local-url map request url is required"));
    }
    let (target_url, scheme, target_port) =
        parse_local_proxy_target(raw_url).map_err(|error| bad_request_error(error.to_string()))?;
    let requested_host = target_url
        .host_str()
        .ok_or_else(|| bad_request_error("local-url map requires an explicit host"))?
        .to_string();
    if !is_container_local_proxy_host(requested_host.as_str()) {
        return Err(bad_request_error(
            "local-url map host must be localhost/loopback",
        ));
    }
    let resolved = resolve_container_proxy_target(&state, session_id.as_str()).await?;
    let key = LocalUrlProxyKey {
        session_id: session_id.clone(),
        scheme,
        requested_host: requested_host.clone(),
        target_port,
    };
    let upstream = LocalUrlProxyUpstream {
        scheme,
        container_ip: resolved.container_ip,
        target_port,
        host_header: target_host_header(&target_url, target_port),
    };
    let mapped_port = state
        .local_url_port_mapper
        .ensure_port(key, upstream)
        .await
        .map_err(internal_error)?;
    if let Some(container_manager) = state.container_manager.as_ref() {
        container_manager
            .touch_activity(&resolved.container_id)
            .await;
    }
    Ok(Json(LocalUrlMapResponse {
        mapped_url: mapped_url_from_target(mapped_port, &target_url),
    }))
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

    let raw_rel = parse_safe_relative_path(requested, "artifact path")?;
    let session_paths = load_session_paths(&state, &session_id).await?;
    let resolved = resolve_scoped_file(
        &session_paths.artifacts_dir(),
        raw_rel,
        "artifact not found",
        "artifact path traversal is not allowed",
    )
    .await?;
    let etag = etag_value(&resolved.metadata);
    if etag_matches(&if_none_match, &etag) {
        return Ok(not_modified_response(&etag, PREVIEW_CACHE_CONTROL));
    }

    let bytes = async_fs::read(&resolved.canonical_path)
        .await
        .map_err(internal_error)?;
    Ok(file_response(
        bytes,
        content_type_for_path(&resolved.relative_path).as_str(),
        &etag,
        PREVIEW_CACHE_CONTROL,
    ))
}

fn artifact_runtime_etag(
    metadata: &std::fs::Metadata,
    artifact_type: &str,
    validation_profile: &str,
) -> String {
    let mut hasher = std::hash::DefaultHasher::new();
    ARTIFACT_RUNTIME_BRIDGE.hash(&mut hasher);
    ARTIFACT_RUNTIME_BRIDGE_TEMPLATE.hash(&mut hasher);
    ARTIFACT_RUNTIME_BRIDGE_SRC.hash(&mut hasher);
    artifact_type.hash(&mut hasher);
    validation_profile.hash(&mut hasher);
    format!(
        "\"{}-{}-{:016x}\"",
        metadata.len(),
        metadata_modified_nanos(metadata),
        hasher.finish()
    )
}

pub(super) async fn get_artifact_runtime_bridge() -> SkyResult<skyzen::Response> {
    let mut hasher = std::hash::DefaultHasher::new();
    ARTIFACT_RUNTIME_BRIDGE.hash(&mut hasher);
    let etag = format!("\"{:016x}\"", hasher.finish());
    Ok(file_response(
        ARTIFACT_RUNTIME_BRIDGE.as_bytes().to_vec(),
        "application/javascript; charset=utf-8",
        etag.as_str(),
        SLIDEV_CACHE_CONTROL,
    ))
}

pub(super) async fn get_artifact_runtime_preview(
    params: Params,
    Query(query): Query<ArtifactRuntimePreviewQuery>,
    if_none_match: IfNoneMatchHeader,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<skyzen::Response> {
    let session_id = params.get("id").map_err(bad_request_error)?.to_string();
    if session_id.is_empty() {
        return Err(bad_request_error("session id is required"));
    }

    let raw_rel = parse_safe_relative_path(query.path.as_str(), "artifact path")?;
    let session_paths = load_session_paths(&state, &session_id).await?;
    let resolved = resolve_scoped_file(
        &session_paths.artifacts_dir(),
        raw_rel,
        "artifact not found",
        "artifact path traversal is not allowed",
    )
    .await?;
    let content_type = content_type_for_path(&resolved.relative_path);
    if !content_type.starts_with("text/html") {
        return Err(bad_request_error(
            "artifact runtime preview requires an HTML artifact",
        ));
    }
    let artifact_type = query.artifact_type.as_str();
    let etag = artifact_runtime_etag(
        &resolved.metadata,
        artifact_type,
        GENERIC_VALIDATION_PROFILE,
    );
    if etag_matches(&if_none_match, etag.as_str()) {
        return Ok(not_modified_response(etag.as_str(), PREVIEW_CACHE_CONTROL));
    }

    let bytes = async_fs::read(&resolved.canonical_path)
        .await
        .map_err(internal_error)?;
    let html = String::from_utf8(bytes)
        .map_err(|_| bad_request_error("artifact runtime preview requires UTF-8 HTML"))?;
    let instrumented =
        instrument_artifact_html(html.as_str(), artifact_type, GENERIC_VALIDATION_PROFILE)?;
    Ok(file_response(
        instrumented.into_bytes(),
        "text/html; charset=utf-8",
        etag.as_str(),
        PREVIEW_CACHE_CONTROL,
    ))
}

fn detect_output_content_type(path: &Path, bytes: &[u8]) -> String {
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        return "image/png".to_string();
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return "image/jpeg".to_string();
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return "image/gif".to_string();
    }
    if bytes.starts_with(b"RIFF")
        && bytes.len() > 12
        && bytes.get(8..12).is_some_and(|marker| marker == b"WEBP")
    {
        return "image/webp".to_string();
    }

    let guessed = content_type_for_path(path);
    if guessed != "application/octet-stream" {
        return guessed;
    }

    match std::str::from_utf8(bytes) {
        Ok(text)
            if text
                .chars()
                .all(|c| c == '\n' || c == '\r' || c == '\t' || (c >= ' ' && c != '\x7f')) =>
        {
            "text/plain; charset=utf-8".to_string()
        }
        _ => "application/octet-stream".to_string(),
    }
}

pub(super) async fn get_output_preview(
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
        return Err(bad_request_error("output path is required"));
    }

    let raw_rel = output_relative_path(requested)?;
    let session_paths = load_session_paths(&state, &session_id).await?;
    let resolved = resolve_scoped_file(
        &session_paths.outputs_dir(),
        raw_rel,
        "output not found",
        "output path traversal is not allowed",
    )
    .await?;
    let etag = etag_value(&resolved.metadata);
    if etag_matches(&if_none_match, &etag) {
        return Ok(not_modified_response(&etag, PREVIEW_CACHE_CONTROL));
    }

    let bytes = async_fs::read(&resolved.canonical_path)
        .await
        .map_err(internal_error)?;
    let content_type = detect_output_content_type(&resolved.relative_path, &bytes);

    Ok(file_response(
        bytes,
        &content_type,
        &etag,
        PREVIEW_CACHE_CONTROL,
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        ARTIFACT_DIAGNOSTIC_TRUNCATION_SUFFIX, GENERIC_VALIDATION_PROFILE,
        MAX_ARTIFACT_DIAGNOSTIC_CHARS, MAX_ARTIFACT_DIAGNOSTICS, acquire_slidev_build_lock,
        instrument_artifact_html, normalized_artifact_diagnostics, read_slidev_build_diagnostics,
        slidev_source_diagnostics, write_slidev_build_diagnostics,
    };
    use crate::ui_server::types::ArtifactDiagnosticsRequest;

    const VALID_SLIDEV: &str = include_str!("../../tests/fixtures/slidev/valid.md");
    const INVALID_SLIDEV: &str =
        include_str!("../../tests/fixtures/slidev/invalid-swallowed-slides.md");

    #[test]
    fn valid_slidev_source_has_no_structural_diagnostics() {
        assert!(slidev_source_diagnostics(VALID_SLIDEV).is_empty());
    }

    #[test]
    fn catches_ambiguous_separators_and_blank_first_slide() {
        let diagnostics = slidev_source_diagnostics(INVALID_SLIDEV);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("Ambiguous Slidev separator"))
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("Slide 1 has no visible content"))
        );
    }

    #[test]
    fn instruments_generic_artifact_html_with_runtime_metadata() {
        let instrumented = instrument_artifact_html(
            "<!doctype html><html><head><title>Artifact</title></head><body><main>artifact</main></body></html>",
            "html",
            GENERIC_VALIDATION_PROFILE,
        )
        .expect("valid HTML should be instrumented");
        assert!(instrumented.contains("src=\"/api/artifact-runtime.js\""));
        assert!(instrumented.contains("data-artifact-type=\"html\""));
        assert!(instrumented.contains("data-validation-profile=\"generic\""));
    }

    #[test]
    fn artifact_type_rejects_non_protocol_identifiers() {
        let request = serde_json::json!({
            "path": "index.html",
            "artifactType": "HTML runtime",
            "diagnostics": ["console.error: failed"]
        });
        assert!(serde_json::from_value::<ArtifactDiagnosticsRequest>(request).is_err());
    }

    #[test]
    fn bounds_runtime_diagnostics_without_dropping_the_report() {
        let mut diagnostics = vec!["x".repeat(MAX_ARTIFACT_DIAGNOSTIC_CHARS + 100)];
        diagnostics.extend((1..=MAX_ARTIFACT_DIAGNOSTICS).map(|index| format!("error {index}")));

        let normalized = normalized_artifact_diagnostics(diagnostics)
            .expect("oversized runtime diagnostics should be bounded");

        assert_eq!(normalized.len(), MAX_ARTIFACT_DIAGNOSTICS);
        assert_eq!(normalized[0].chars().count(), MAX_ARTIFACT_DIAGNOSTIC_CHARS);
        assert!(normalized[0].ends_with(ARTIFACT_DIAGNOSTIC_TRUNCATION_SUFFIX));
        assert_eq!(
            normalized[MAX_ARTIFACT_DIAGNOSTICS - 1],
            "2 additional artifact diagnostics were omitted."
        );
    }

    #[tokio::test]
    async fn cached_slidev_build_diagnostics_complete_waiting_requests() {
        let dir = tempfile::tempdir().expect("temp dir should be created");
        let diagnostics_file = dir.path().join("diagnostics.json");
        let diagnostics =
            vec!["Slidev preview build failed: dependency install timed out".to_string()];
        write_slidev_build_diagnostics(&diagnostics_file, diagnostics.as_slice())
            .await
            .expect("diagnostics should be persisted");

        assert_eq!(
            read_slidev_build_diagnostics(&diagnostics_file)
                .await
                .expect("diagnostics should be readable"),
            Some(diagnostics)
        );
        assert!(
            acquire_slidev_build_lock(
                dir.path().join("build.lock").as_path(),
                dir.path().join("index.html").as_path(),
                diagnostics_file.as_path(),
            )
            .await
            .expect("cached diagnostics should complete lock acquisition")
            .is_none()
        );
    }
}
