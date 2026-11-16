//! User-facing artifact pages.
//!
//! `/profile/{profile}/session/{session}/artifacts[/{path}]` browses the
//! session's `artifacts/` directory: browsers (`Accept: text/html`) get a
//! rendered preview page — markdown rendered to HTML, media in viewers,
//! UTF-8 sources in `<pre>` — while any other client receives the raw
//! file. `?raw` and `?download` force the raw bytes either way.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use askama::Template;
use skyzen::extract::Query;
use skyzen::routing::Params;
use skyzen::utils::State;
use skyzen::{Result as SkyResult, StatusCode};

use super::artifacts::{
    ResolvedSessionFile, content_type_for_path, etag_matches, etag_value, file_response,
    insert_header, not_modified_response, resolve_scoped_path,
};
use super::error::{bad_request_error, internal_error};
use super::helpers::{load_session_paths, parse_safe_relative_path};
use super::state::DaemonState;
use super::types::IfNoneMatchHeader;

const PAGE_CACHE_CONTROL: &str = "private, no-cache";
/// Preview pages render agent-generated content (e.g. markdown HTML), so
/// they carry a CSP sandbox: no scripts, no forms; downloads stay allowed
/// for the page's own Download action.
const PAGE_CSP: &str = "sandbox allow-downloads";
/// UTF-8 bodies larger than this render as a download card instead.
const PREVIEW_MAX_TEXT_BYTES: u64 = 512 * 1024;
/// Markdown sources larger than this render as a download card instead.
const PREVIEW_MAX_MARKDOWN_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
pub(super) struct ArtifactPageQuery {
    #[serde(default)]
    raw: Option<String>,
    #[serde(default)]
    download: Option<String>,
}

/// The request's `Accept` header, for browser/raw content negotiation.
pub(super) struct AcceptHeader(pub Option<String>);

impl skyzen::extract::Extractor for AcceptHeader {
    type Error = std::convert::Infallible;

    fn extract(
        request: &mut skyzen::Request,
    ) -> impl std::future::Future<Output = Result<Self, Self::Error>> + Send {
        let value = request
            .headers()
            .get(skyzen::header::ACCEPT)
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string);
        std::future::ready(Ok(Self(value)))
    }
}

impl AcceptHeader {
    fn wants_html(&self) -> bool {
        self.0
            .as_deref()
            .is_some_and(|accept| accept.contains("text/html"))
    }
}

/// The `artifacts/` directory of the session the URL names.
///
/// The profile segment must equal the session's own profile id, `default`
/// for a session without one, or `-` to match any session by id alone.
async fn resolve_session_artifacts_root(
    state: &DaemonState,
    profile_id: &str,
    session_id: &str,
) -> Result<PathBuf, skyzen::Error> {
    let not_found =
        || skyzen::Error::msg("artifact page not found").set_status(StatusCode::NOT_FOUND);
    let session = state
        .db()
        .get_session(session_id)
        .await
        .map_err(internal_error)?
        .ok_or_else(not_found)?;
    let profile_matches = session.profile_id.as_deref().map_or_else(
        || profile_id == "-" || profile_id == "default",
        |actual| profile_id == "-" || profile_id == actual,
    );
    if !profile_matches {
        return Err(not_found());
    }
    Ok(load_session_paths(state, session_id).await?.artifacts_dir())
}

pub(super) async fn serve_artifact(
    params: Params,
    Query(query): Query<ArtifactPageQuery>,
    accept: AcceptHeader,
    if_none_match: IfNoneMatchHeader,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<skyzen::Response> {
    let profile_id = params.get("profileId").map_err(bad_request_error)?;
    let session_id = params.get("sessionId").map_err(bad_request_error)?;
    let raw_path = params.get("path").unwrap_or("").trim().to_string();
    let artifacts_root = resolve_session_artifacts_root(&state, profile_id, session_id).await?;
    let page_base = format!("/profile/{profile_id}/session/{session_id}/artifacts");

    let relative_path = if raw_path.is_empty() || raw_path == "/" {
        PathBuf::new()
    } else {
        parse_safe_relative_path(raw_path.trim_start_matches('/'), "artifact path")?
    };

    let resolved = resolve_scoped_path(
        &artifacts_root,
        relative_path.clone(),
        "artifact not found",
        "artifact path traversal is not allowed",
        StatusCode::NOT_FOUND,
    )
    .await;

    let resolved = match resolved {
        Ok(resolved) => resolved,
        Err(error) => {
            if relative_path.as_os_str().is_empty() {
                // The directory itself may not exist yet; show an empty index
                // rather than an error for the session root.
                return index_page(&page_base, &relative_path, Vec::new());
            }
            return Err(error);
        }
    };

    if resolved.metadata.is_dir() {
        return index_page(&page_base, &relative_path, collect_dir(&resolved).await?);
    }
    if !resolved.metadata.is_file() {
        return Err(skyzen::Error::msg("artifact not found").set_status(StatusCode::NOT_FOUND));
    }

    let self_url = artifact_url(&page_base, &relative_path);
    let download = query.download.is_some();
    if download || query.raw.is_some() || !accept.wants_html() {
        return raw_file_response(&resolved, download, &if_none_match).await;
    }

    preview_page(&self_url, &resolved).await
}

fn artifact_url(base: &str, relative: &Path) -> String {
    let mut url = base.to_string();
    for component in relative.components() {
        if let std::path::Component::Normal(segment) = component {
            url.push('/');
            url.push_str(&encode_path_segment(&segment.to_string_lossy()));
        }
    }
    url
}

fn encode_path_segment(segment: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

struct DirEntry {
    name: String,
    href: String,
    size_label: String,
    is_dir: bool,
}

async fn collect_dir(resolved: &ResolvedSessionFile) -> SkyResult<Vec<DirEntry>> {
    let mut entries = Vec::new();
    let mut rd = async_fs::read_dir(&resolved.canonical_path)
        .await
        .map_err(internal_error)?;
    while let Some(entry) = rd.next().await {
        let entry = entry.map_err(internal_error)?;
        let meta = entry.metadata().await.map_err(internal_error)?;
        if !meta.is_dir() && !meta.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = meta.is_dir();
        let mut href = encode_path_segment(&name);
        if is_dir {
            href.push('/');
        }
        entries.push(DirEntry {
            name,
            href,
            size_label: if is_dir {
                String::new()
            } else {
                format_size(meta.len())
            },
            is_dir,
        });
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    Ok(entries)
}

use futures_lite::StreamExt;

#[derive(Template)]
#[template(path = "artifact_index.html")]
struct ArtifactIndexTemplate {
    title: String,
    rel_path: String,
    entries: Vec<DirEntry>,
    parent_href: Option<String>,
}

fn index_page(
    page_base: &str,
    relative_path: &Path,
    mut entries: Vec<DirEntry>,
) -> SkyResult<skyzen::Response> {
    for entry in &mut entries {
        entry.href = if relative_path.as_os_str().is_empty() {
            format!("{page_base}/{}", entry.href)
        } else {
            format!("{}/{}", artifact_url(page_base, relative_path), entry.href)
        };
    }
    let title = relative_path.file_name().map_or_else(
        || "artifacts".to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let parent_href = if relative_path.as_os_str().is_empty() {
        None
    } else {
        Some(artifact_url(
            page_base,
            relative_path.parent().unwrap_or_else(|| Path::new("")),
        ))
    };
    Ok(html_response(
        ArtifactIndexTemplate {
            title,
            rel_path: relative_path.to_string_lossy().replace('\\', "/"),
            entries,
            parent_href,
        }
        .render()
        .map_err(internal_error)?,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviewKind {
    Markdown,
    Frame,
    Image,
    Video,
    Audio,
    Text,
    Binary,
}

#[derive(Template)]
#[template(path = "artifact_page.html")]
struct ArtifactPageTemplate<'a> {
    name: &'a str,
    rel_path: &'a str,
    size_label: String,
    raw_url: &'a str,
    download_url: &'a str,
    kind: PreviewKind,
    rendered_html: &'a str,
    media_src: &'a str,
    text: &'a str,
}

fn preview_kind(relative_path: &Path, size: u64) -> PreviewKind {
    let extension = relative_path
        .extension()
        .and_then(|value| value.to_str())
        .map_or_default(str::to_ascii_lowercase);
    match extension.as_str() {
        "md" | "markdown" => PreviewKind::Markdown,
        "html" | "htm" | "pdf" => PreviewKind::Frame,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "avif" | "ico" => {
            PreviewKind::Image
        }
        "mp4" | "webm" | "mov" | "m4v" | "mkv" => PreviewKind::Video,
        "mp3" | "wav" | "ogg" | "m4a" | "flac" => PreviewKind::Audio,
        _ if size <= PREVIEW_MAX_TEXT_BYTES => PreviewKind::Text,
        _ => PreviewKind::Binary,
    }
}

fn render_markdown(source: &str) -> String {
    let mut options = pulldown_cmark::Options::empty();
    options.insert(pulldown_cmark::Options::ENABLE_TABLES);
    options.insert(pulldown_cmark::Options::ENABLE_STRIKETHROUGH);
    options.insert(pulldown_cmark::Options::ENABLE_TASKLISTS);
    options.insert(pulldown_cmark::Options::ENABLE_FOOTNOTES);
    let parser = pulldown_cmark::Parser::new_ext(source, options);
    let mut html = String::with_capacity(source.len() + source.len() / 2);
    pulldown_cmark::html::push_html(&mut html, parser);
    html
}

async fn preview_page(
    self_url: &str,
    resolved: &ResolvedSessionFile,
) -> SkyResult<skyzen::Response> {
    let name = resolved
        .relative_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("artifact")
        .to_string();
    let rel_path = resolved.relative_path.to_string_lossy().replace('\\', "/");
    let raw_url = format!("{self_url}?raw");
    let download_url = format!("{self_url}?download");

    let mut kind = preview_kind(&resolved.relative_path, resolved.metadata.len());
    let mut rendered_html = String::new();
    let mut text = String::new();

    match kind {
        PreviewKind::Markdown => {
            if resolved.metadata.len() > PREVIEW_MAX_MARKDOWN_BYTES {
                kind = PreviewKind::Binary;
            } else {
                match async_fs::read_to_string(&resolved.canonical_path).await {
                    Ok(source) => rendered_html = render_markdown(&source),
                    Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                        kind = PreviewKind::Binary;
                    }
                    Err(error) => return Err(internal_error(error)),
                }
            }
        }
        PreviewKind::Text => match async_fs::read(&resolved.canonical_path).await {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(source) => text = source,
                Err(_) => kind = PreviewKind::Binary,
            },
            Err(error) => return Err(internal_error(error)),
        },
        _ => {}
    }

    Ok(html_response(
        ArtifactPageTemplate {
            name: &name,
            rel_path: &rel_path,
            size_label: format_size(resolved.metadata.len()),
            raw_url: &raw_url,
            download_url: &download_url,
            kind,
            rendered_html: &rendered_html,
            media_src: &raw_url,
            text: &text,
        }
        .render()
        .map_err(internal_error)?,
    ))
}

async fn raw_file_response(
    resolved: &ResolvedSessionFile,
    download: bool,
    if_none_match: &IfNoneMatchHeader,
) -> SkyResult<skyzen::Response> {
    let etag = etag_value(&resolved.metadata);
    if !download && etag_matches(if_none_match, &etag) {
        return Ok(not_modified_response(&etag, PAGE_CACHE_CONTROL));
    }
    let bytes = async_fs::read(&resolved.canonical_path)
        .await
        .map_err(internal_error)?;
    let mut response = file_response(
        bytes,
        content_type_for_path(&resolved.relative_path).as_str(),
        &etag,
        PAGE_CACHE_CONTROL,
    );
    if download {
        let name = resolved.relative_path.file_name().map_or_else(
            || "artifact".to_string(),
            |value| value.to_string_lossy().into_owned(),
        );
        insert_header(
            &mut response,
            skyzen::header::CONTENT_DISPOSITION,
            &content_disposition_attachment(&name),
        );
    }
    Ok(response)
}

fn content_disposition_attachment(name: &str) -> String {
    let fallback: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || ".-_ ".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "attachment; filename=\"{fallback}\"; filename*=UTF-8''{}",
        encode_path_segment(name)
    )
}

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn html_response(html: String) -> skyzen::Response {
    let mut response = skyzen::Response::new(skyzen::Body::from(html.into_bytes()));
    insert_header(
        &mut response,
        skyzen::header::CONTENT_TYPE,
        "text/html; charset=utf-8",
    );
    insert_header(
        &mut response,
        skyzen::header::CACHE_CONTROL,
        PAGE_CACHE_CONTROL,
    );
    insert_header(
        &mut response,
        skyzen::header::CONTENT_SECURITY_POLICY,
        PAGE_CSP,
    );
    insert_header(
        &mut response,
        skyzen::header::X_CONTENT_TYPE_OPTIONS,
        "nosniff",
    );
    response
}
