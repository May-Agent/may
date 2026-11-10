//! Attachment processing and upload helpers for chat requests.

use std::path::{Path, PathBuf};

use aither_core::llm::Attachment as LlmAttachment;
use aither_models::{Ability, lookup as lookup_model_info};
use askama::Template;
use base64::Engine;
use std::sync::Arc;

use serde::Deserialize;
use skyzen::extract::Query;
use skyzen::header::HeaderMap;
use skyzen::http_kit::utils::Bytes;
use skyzen::routing::Params;
use skyzen::utils::{Json, State};
use skyzen::{Error as SkyError, Result as SkyResult, StatusCode};

use crate::agent::generate_session_title;
use crate::session::AttachmentKind;

use super::error::{bad_request_error, internal_error, internal_error_message};
use super::helpers::{load_session_paths, parse_safe_relative_path};
use super::state::DaemonState;
use super::types::{ApiAttachment, ChatRequest};
use crate::config::{MayConfig, ProviderInstance, ProviderType};
use crate::model_selector::ModelSelector;

pub(super) const INLINE_PASTED_TEXT_THRESHOLD: usize = 1200;

#[derive(Template)]
#[template(path = "prompt_text_block.txt", escape = "none")]
struct PromptTextBlock<'a> {
    tag: &'static str,
    content: &'a str,
}

/// An attachment the prompt names for the model.
#[derive(Clone)]
struct PromptAttachment {
    name: String,
    kind: AttachmentKind,
    path: Option<String>,
}

#[derive(Template)]
#[template(path = "prompt_attachments.txt", escape = "none")]
struct PromptAttachments<'a> {
    attachments: &'a [PromptAttachment],
}

/// An attachment offloaded because the model lacks its modality.
#[derive(Clone)]
struct OffloadedAttachment {
    name: String,
    kind: AttachmentKind,
    path: Option<String>,
    modality: &'static str,
    model_id: String,
}

#[derive(Template)]
#[template(path = "reminder_unsupported_modality.txt", escape = "none")]
struct UnsupportedModalityReminder<'a> {
    attachments: &'a [OffloadedAttachment],
}

enum PromptSection {
    PastedText(String),
    OffloadedText(String),
    Attachments(Vec<PromptAttachment>),
    UnsupportedModalityAlert(Vec<OffloadedAttachment>),
}

impl PromptSection {
    fn render(&self) -> SkyResult<String> {
        match self {
            Self::PastedText(content) => PromptTextBlock {
                tag: "pasted_text",
                content,
            }
            .render(),
            Self::OffloadedText(content) => PromptTextBlock {
                tag: "offloaded_text",
                content,
            }
            .render(),
            Self::Attachments(attachments) => PromptAttachments { attachments }.render(),
            Self::UnsupportedModalityAlert(attachments) => {
                UnsupportedModalityReminder { attachments }.render()
            }
        }
        .map_err(internal_error)
    }
}

pub(super) async fn build_prompt_and_attachments(
    state: &DaemonState,
    req: &ChatRequest,
    model_id: &str,
) -> SkyResult<(String, Vec<LlmAttachment>)> {
    let config = state.config_snapshot();
    let (modality_support, inline_images) = attachment_delivery(model_id, &config);
    let mut attachments = Vec::new();
    let mut prompt_sections = Vec::new();
    let mut xml_items = Vec::new();
    let mut offloaded_xml_items = Vec::new();
    let mut canonical_session_root = None;

    for attachment in &req.attachments {
        let required_modality = AttachmentModality::for_attachment_type(attachment.attachment_type);
        let supported = required_modality
            .as_ref()
            .is_none_or(|modality| modality_support.supports(modality));
        if !supported {
            let relative_path = if attachment.url.trim().is_empty() {
                None
            } else {
                let source = resolve_native_attachment_source(
                    state,
                    &req.session_id,
                    attachment.url.as_str(),
                    &mut canonical_session_root,
                )
                .await?;
                let Some(relative_path) = source.relative_path else {
                    return Err(bad_request_error(
                        "Unsupported remote modality must be uploaded so May can preserve it",
                    ));
                };
                Some(relative_path)
            };
            offloaded_xml_items.push(build_offloaded_attachment(
                attachment,
                required_modality
                    .as_ref()
                    .expect("unsupported attachment must have modality"),
                relative_path.as_deref(),
                model_id,
            ));
            if let Some(text) = &attachment.text_content {
                prompt_sections.push(PromptSection::OffloadedText(text.clone()));
            }
            continue;
        }

        if let Some(text) = &attachment.text_content {
            if text.len() <= INLINE_PASTED_TEXT_THRESHOLD {
                prompt_sections.push(PromptSection::PastedText(text.clone()));
            } else {
                let (_, relative_path) =
                    Box::pin(save_large_pasted_text(state, &req.session_id, text)).await?;
                xml_items.push(PromptAttachment {
                    name: attachment.name.clone(),
                    kind: AttachmentKind::Document,
                    path: Some(relative_path),
                });
            }
            continue;
        }

        if let Some(modality) = required_modality.as_ref() {
            let source = resolve_native_attachment_source(
                state,
                &req.session_id,
                attachment.url.as_str(),
                &mut canonical_session_root,
            )
            .await?;
            let media_type = resolve_attachment_media_type(
                attachment,
                modality,
                None,
                source.local_path.as_deref(),
                Some(&source.url),
            )?;
            xml_items.push(PromptAttachment {
                name: attachment.name.clone(),
                kind: attachment.attachment_type,
                path: source.relative_path,
            });
            let url = match (modality, inline_images, source.local_path.as_deref()) {
                (AttachmentModality::Vision, true, Some(path)) => {
                    file_to_data_url(path, &media_type).await?
                }
                _ => source.url,
            };
            attachments.push(LlmAttachment::new(url, media_type));
            continue;
        }

        xml_items.push(PromptAttachment {
            name: attachment.name.clone(),
            kind: attachment.attachment_type,
            path: None,
        });
    }

    if !xml_items.is_empty() {
        prompt_sections.push(PromptSection::Attachments(xml_items));
    }

    if !offloaded_xml_items.is_empty() {
        prompt_sections.push(PromptSection::UnsupportedModalityAlert(offloaded_xml_items));
    }

    Ok((
        render_prompt(req.content.as_str(), &prompt_sections)?,
        attachments,
    ))
}

fn render_prompt(base: &str, sections: &[PromptSection]) -> SkyResult<String> {
    let mut blocks = Vec::with_capacity(sections.len() + 1);
    blocks.push(base.to_string());
    for section in sections {
        blocks.push(section.render()?);
    }
    Ok(blocks.join("\n\n"))
}

struct NativeAttachmentSource {
    url: url::Url,
    relative_path: Option<String>,
    local_path: Option<PathBuf>,
}

async fn resolve_native_attachment_source(
    state: &DaemonState,
    session_id: &str,
    raw: &str,
    canonical_session_root: &mut Option<PathBuf>,
) -> SkyResult<NativeAttachmentSource> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(bad_request_error("multimodal attachment URL is required"));
    }

    if let Ok(url) = url::Url::parse(trimmed) {
        return match url.scheme() {
            "http" | "https" => Ok(NativeAttachmentSource {
                url,
                relative_path: None,
                local_path: None,
            }),
            "file" => {
                let path = url.to_file_path().map_err(|()| {
                    bad_request_error("attachment file URL could not be converted to a path")
                })?;
                resolve_session_file_source(state, session_id, path, canonical_session_root).await
            }
            scheme => Err(bad_request_error(format!(
                "unsupported multimodal attachment URL scheme '{scheme}'"
            ))),
        };
    }

    if let Some(rest) = trimmed
        .strip_prefix("/api/sessions/")
        .and_then(|rest| rest.split_once("/files/").map(|(_, rel)| rel))
    {
        let files_dir = load_session_paths(state, session_id).await?.files_dir();
        let rel = parse_safe_relative_path(rest, "attachment path")?;
        return resolve_session_file_source(
            state,
            session_id,
            files_dir.join(rel),
            canonical_session_root,
        )
        .await;
    }

    let relative_path = parse_safe_relative_path(trimmed, "attachment path")?;
    let session_root =
        canonical_session_root_path(state, session_id, canonical_session_root).await?;
    resolve_session_file_source(
        state,
        session_id,
        session_root.join(relative_path),
        canonical_session_root,
    )
    .await
}

async fn resolve_session_file_source(
    state: &DaemonState,
    session_id: &str,
    path: PathBuf,
    canonical_session_root: &mut Option<PathBuf>,
) -> SkyResult<NativeAttachmentSource> {
    let session_root =
        canonical_session_root_path(state, session_id, canonical_session_root).await?;
    let canonical_path = async_fs::canonicalize(&path)
        .await
        .map_err(|_| bad_request_error("attachment file does not exist"))?;
    if !canonical_path.starts_with(&session_root) {
        return Err(bad_request_error(
            "attachment file must remain inside the session directory",
        ));
    }
    let metadata = async_fs::metadata(&canonical_path)
        .await
        .map_err(internal_error)?;
    if !metadata.is_file() {
        return Err(bad_request_error("attachment path must reference a file"));
    }
    let relative_path = canonical_path
        .strip_prefix(&session_root)
        .map_err(internal_error)?
        .to_string_lossy()
        .into_owned();
    Ok(NativeAttachmentSource {
        url: path_to_file_url(&canonical_path)?,
        relative_path: Some(relative_path),
        local_path: Some(canonical_path),
    })
}

async fn canonical_session_root_path(
    state: &DaemonState,
    session_id: &str,
    cached: &mut Option<PathBuf>,
) -> SkyResult<PathBuf> {
    if let Some(root) = cached {
        return Ok(root.clone());
    }
    let paths = load_session_paths(state, session_id).await?;
    let canonical = async_fs::canonicalize(paths.root())
        .await
        .map_err(internal_error)?;
    *cached = Some(canonical.clone());
    Ok(canonical)
}

fn path_to_file_url(path: &Path) -> SkyResult<url::Url> {
    url::Url::from_file_path(path)
        .map_err(|()| internal_error_message("Failed to map multimodal attachment path to URL"))
}

fn resolve_attachment_media_type(
    attachment: &ApiAttachment,
    modality: &AttachmentModality,
    data_url: Option<&str>,
    local_path: Option<&Path>,
    source_url: Option<&url::Url>,
) -> SkyResult<mime::Mime> {
    let media_type = data_url
        .map(data_url_media_type)
        .transpose()?
        .or_else(|| local_path.and_then(|path| mime_guess::from_path(path).first()))
        .or_else(|| mime_guess::from_path(&attachment.name).first())
        .or_else(|| source_url.and_then(|url| mime_guess::from_path(url.path()).first()))
        .or_else(|| matches!(modality, AttachmentModality::Pdf).then_some(mime::APPLICATION_PDF))
        .ok_or_else(|| {
            bad_request_error(format!(
                "unable to determine MIME type for attachment '{}'",
                attachment.name
            ))
        })?;

    if !modality.accepts(&media_type) {
        return Err(bad_request_error(format!(
            "attachment '{}' declares {} but MIME type is '{}'",
            attachment.name,
            modality.label(),
            media_type
        )));
    }
    Ok(media_type)
}

fn data_url_media_type(data_url: &str) -> SkyResult<mime::Mime> {
    let (header, _) = data_url
        .split_once(',')
        .ok_or_else(|| bad_request_error("invalid attachment data URL"))?;
    let metadata = header
        .strip_prefix("data:")
        .ok_or_else(|| bad_request_error("invalid attachment data URL"))?;
    let media_type = metadata
        .split(';')
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| bad_request_error("attachment data URL is missing MIME type"))?;
    media_type
        .parse()
        .map_err(|error| bad_request_error(format!("invalid attachment MIME type: {error}")))
}

// ── Modality support ─────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
enum AttachmentModality {
    Vision,
    Audio,
    Video,
    Pdf,
}

impl AttachmentModality {
    const fn for_attachment_type(attachment_type: AttachmentKind) -> Option<Self> {
        match attachment_type {
            AttachmentKind::Image => Some(Self::Vision),
            AttachmentKind::Audio => Some(Self::Audio),
            AttachmentKind::Video => Some(Self::Video),
            AttachmentKind::Pdf => Some(Self::Pdf),
            _ => None,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Vision => "vision",
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Pdf => "pdf",
        }
    }

    fn accepts(self, media_type: &mime::Mime) -> bool {
        let raw = media_type.as_ref();
        match self {
            Self::Vision => raw.starts_with("image/"),
            Self::Audio => raw.starts_with("audio/"),
            Self::Video => raw.starts_with("video/"),
            Self::Pdf => raw == "application/pdf",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ModelModalitySupport {
    vision: bool,
    audio: bool,
    video: bool,
    pdf: bool,
}

impl ModelModalitySupport {
    const fn all() -> Self {
        Self {
            vision: true,
            audio: true,
            video: true,
            pdf: true,
        }
    }

    const fn none() -> Self {
        Self {
            vision: false,
            audio: false,
            video: false,
            pdf: false,
        }
    }

    fn from_entry(info: &aither_models::ModelEntry) -> Self {
        Self {
            vision: info.has_ability(Ability::Vision),
            audio: info.has_ability(Ability::Audio),
            video: info.has_ability(Ability::Video),
            pdf: info.has_ability(Ability::Pdf),
        }
    }

    /// What the provider's wire protocol can deliver an attachment through,
    /// independent of what the model accepts: the aither client each
    /// provider type drives decides whether a modality reaches the backend.
    fn wire(ptype: ProviderType, instance: &ProviderInstance, provider_id: &str) -> Self {
        match ptype {
            // Claude accepts base64 image and PDF document blocks.
            ProviderType::Anthropic => Self {
                vision: true,
                pdf: true,
                ..Self::none()
            },
            // Gemini inlines every modality it accepts.
            ProviderType::Google => Self::all(),
            // The Copilot chat-completions bridge inlines images only.
            ProviderType::Copilot => Self {
                vision: true,
                ..Self::none()
            },
            // aither-codex does not serialize attachments — anything handed
            // to it is dropped before it reaches the wire.
            ProviderType::Chatgpt => Self::none(),
            _ => {
                // OpenAI-compatible wire: images ride `data:`/`http(s)` URLs
                // or a Files API upload, audio falls back to inlined
                // chat-completions input, and video has no input block. Only
                // first-party OpenAI is guaranteed to host the Files API a
                // PDF upload needs.
                let files_api =
                    ptype == ProviderType::OpenAI && !instance.is_generic_model_proxy(provider_id);
                Self {
                    vision: true,
                    audio: true,
                    video: false,
                    pdf: files_api,
                }
            }
        }
    }

    const fn intersect(self, other: Self) -> Self {
        Self {
            vision: self.vision && other.vision,
            audio: self.audio && other.audio,
            video: self.video && other.video,
            pdf: self.pdf && other.pdf,
        }
    }

    const fn supports(self, modality: &AttachmentModality) -> bool {
        match modality {
            AttachmentModality::Vision => self.vision,
            AttachmentModality::Audio => self.audio,
            AttachmentModality::Video => self.video,
            AttachmentModality::Pdf => self.pdf,
        }
    }
}

/// How attachments reach the model behind a `provider::model` selector: the
/// modalities the conversation may carry, and whether image payloads are
/// inlined as `data:` URLs instead of `file://` paths.
fn attachment_delivery(selector_str: &str, config: &MayConfig) -> (ModelModalitySupport, bool) {
    let selector = selector_str.parse::<ModelSelector>().ok();
    let provider = selector
        .as_ref()
        .and_then(|s| config.provider.get(s.provider_id()).map(|i| (s, i)));

    let wire = provider.map_or_else(ModelModalitySupport::none, |(s, instance)| {
        ModelModalitySupport::wire(
            instance.resolve_type(s.provider_id()),
            instance,
            s.provider_id(),
        )
    });
    let inline_images = provider.is_some_and(|(s, instance)| {
        should_inline_images(
            instance.resolve_type(s.provider_id()),
            instance,
            s.provider_id(),
        )
    });

    let model_id = selector
        .as_ref()
        .map_or(selector_str, ModelSelector::model_id);
    let model = match lookup_model_info(model_id) {
        Some(info) => ModelModalitySupport::from_entry(info),
        None if provider.is_some_and(|(s, i)| i.is_generic_model_proxy(s.provider_id())) => {
            // Generic proxies serve models the registry cannot index; the
            // backend's own catalog decides what the model accepts.
            ModelModalitySupport::all()
        }
        None => {
            tracing::warn!(
                model_id = %selector_str,
                "model metadata not found; treating advanced modalities as unsupported"
            );
            ModelModalitySupport::none()
        }
    };
    (model.intersect(wire), inline_images)
}

/// Whether image attachments reach this provider inline as `data:` URLs
/// rather than as `file://` paths. On the Responses API a `file://` image is
/// uploaded through the Files API, which only first-party `OpenAI` is
/// guaranteed to serve — every other OpenAI-wire endpoint gets inline bytes.
fn should_inline_images(
    ptype: ProviderType,
    instance: &ProviderInstance,
    provider_id: &str,
) -> bool {
    let openai_wire = !matches!(
        ptype,
        ProviderType::Anthropic
            | ProviderType::Google
            | ProviderType::Copilot
            | ProviderType::Chatgpt
    );
    openai_wire && (ptype != ProviderType::OpenAI || instance.is_generic_model_proxy(provider_id))
}

/// Read `path` and wrap it in a `data:` URL carrying `media_type`.
async fn file_to_data_url(path: &Path, media_type: &mime::Mime) -> SkyResult<url::Url> {
    let bytes = async_fs::read(path).await.map_err(internal_error)?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    url::Url::parse(&format!("data:{media_type};base64,{encoded}"))
        .map_err(|error| internal_error_message(format!("invalid attachment data URL: {error}")))
}

// ── Prompt text rendering ────────────────────────────────────

fn build_offloaded_attachment(
    attachment: &ApiAttachment,
    modality: &AttachmentModality,
    relative_path: Option<&str>,
    model_id: &str,
) -> OffloadedAttachment {
    OffloadedAttachment {
        name: attachment.name.clone(),
        kind: attachment.attachment_type,
        path: relative_path.map(ToOwned::to_owned),
        modality: modality.label(),
        model_id: model_id.to_string(),
    }
}

async fn save_large_pasted_text(
    state: &DaemonState,
    session_id: &str,
    text: &str,
) -> SkyResult<(PathBuf, String)> {
    let filename_base = Box::pin(generate_pasted_text_filename(state, text)).await;
    let (path, relative_path) =
        allocate_upload_path(state, session_id, &filename_base, "txt").await?;

    if let Some(parent) = path.parent() {
        async_fs::create_dir_all(parent)
            .await
            .map_err(internal_error)?;
    }

    async_fs::write(&path, text.as_bytes())
        .await
        .map_err(internal_error)?;

    Ok((path, relative_path))
}

async fn generate_pasted_text_filename(state: &DaemonState, text: &str) -> String {
    let config = state.config_snapshot();
    let snippet: String = text.chars().take(600).collect();

    let generated = Box::pin(generate_session_title(
        &config,
        &snippet,
        "",
        &crate::secrets::SecretRedactor::new(&state.may_dir),
    ))
    .await;
    let base = generated
        .as_deref()
        .map(sanitize_upload_stem)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "pasted-text".to_string());

    if base.is_empty() {
        "pasted-text".to_string()
    } else {
        base
    }
}

async fn allocate_upload_path(
    state: &DaemonState,
    session_id: &str,
    base_name: &str,
    ext: &str,
) -> SkyResult<(PathBuf, String)> {
    let uploads_dir = load_session_paths(state, session_id).await?.files_dir();

    async_fs::create_dir_all(&uploads_dir)
        .await
        .map_err(internal_error)?;

    let clean_ext = ext.trim_start_matches('.');
    let mut file_name = format!("{base_name}.{clean_ext}");
    let mut path = uploads_dir.join(&file_name);

    if async_fs::metadata(&path).await.is_ok() {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        file_name = format!("{base_name}-{ts}.{clean_ext}");
        path = uploads_dir.join(&file_name);
    }

    Ok((path, ["files/", file_name.as_str()].concat()))
}

fn sanitize_upload_stem(raw: &str) -> String {
    let stem = Path::new(raw)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(raw);

    let mut out = String::new();
    for ch in stem.chars() {
        let safe = match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            c if c.is_control() => '-',
            c => c,
        };
        out.push(safe);
    }

    let cleaned = out.trim_matches([' ', '.', '-']).to_string();
    if cleaned.is_empty() {
        "upload".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachment(name: &str, attachment_type: AttachmentKind) -> ApiAttachment {
        ApiAttachment {
            id: "attachment-test".to_string(),
            name: name.to_string(),
            attachment_type,
            url: String::new(),

            text_content: None,
        }
    }

    #[test]
    fn prompt_sections_render_as_tagged_text_and_lists() {
        assert_eq!(
            PromptSection::PastedText("some text".to_string())
                .render()
                .expect("render"),
            "<pasted_text>\nsome text\n</pasted_text>"
        );
        assert_eq!(
            PromptSection::Attachments(vec![
                PromptAttachment {
                    name: "a.png".to_string(),
                    kind: AttachmentKind::Image,
                    path: Some("files/a.png".to_string()),
                },
                PromptAttachment {
                    name: "b.txt".to_string(),
                    kind: AttachmentKind::File,
                    path: None,
                },
            ])
            .render()
            .expect("render"),
            "<attachments>\n- a.png (image), saved at files/a.png\n- b.txt (file)\n</attachments>"
        );
        assert_eq!(
            PromptSection::UnsupportedModalityAlert(vec![OffloadedAttachment {
                name: "clip.wav".to_string(),
                kind: AttachmentKind::Audio,
                path: Some("files/clip.wav".to_string()),
                modality: "audio",
                model_id: "m1".to_string(),
            }])
            .render()
            .expect("render"),
            "<system-reminder>\nSome attachments were offloaded because the current model does not support their modality. Inspect the referenced files with tools before answering.\n- clip.wav (audio), saved at files/clip.wav: needs audio support, which model m1 lacks\n</system-reminder>"
        );
    }

    #[test]
    fn data_url_media_type_is_parsed_exactly() {
        let media_type =
            data_url_media_type("data:audio/wav;base64,AA==").expect("parse data URL MIME type");
        assert_eq!(media_type.as_ref(), "audio/wav");
    }

    #[test]
    fn supported_modalities_keep_exact_media_types() {
        let cases = [
            (
                AttachmentKind::Image,
                AttachmentModality::Vision,
                "image/png",
            ),
            (
                AttachmentKind::Audio,
                AttachmentModality::Audio,
                "audio/wav",
            ),
            (
                AttachmentKind::Video,
                AttachmentModality::Video,
                "video/mp4",
            ),
            (
                AttachmentKind::Pdf,
                AttachmentModality::Pdf,
                "application/pdf",
            ),
        ];
        for (kind, modality, media_type) in cases {
            let value = attachment("upload.bin", kind);
            let data_url = format!("data:{media_type};base64,AA==");
            let resolved = resolve_attachment_media_type(
                &value,
                &modality,
                Some(data_url.as_str()),
                None,
                None,
            )
            .expect("resolve attachment media type");
            assert_eq!(resolved.as_ref(), media_type);
        }
    }

    #[test]
    fn mismatched_attachment_modality_fails() {
        let value = attachment("recording.wav", AttachmentKind::Image);
        let error = resolve_attachment_media_type(
            &value,
            &AttachmentModality::Vision,
            Some("data:audio/wav;base64,AA=="),
            None,
            None,
        )
        .expect_err("MIME mismatch must fail");
        assert!(error.to_string().contains("declares vision"));
    }

    #[test]
    fn text_attachment_has_no_native_modality() {
        assert!(AttachmentModality::for_attachment_type(AttachmentKind::Text).is_none());
        assert!(AttachmentModality::for_attachment_type(AttachmentKind::Document).is_none());
    }
}

#[derive(Debug, Deserialize, skyzen::ToSchema)]
pub(super) struct AttachmentUploadQuery {
    #[serde(default)]
    name: String,
}

/// `POST /api/sessions/{id}/attachments?name=` — store raw upload bytes under
/// the session's `files/` and return the attachment record clients then send
/// with a message.
pub(super) async fn post_attachment(
    params: Params,
    Query(query): Query<AttachmentUploadQuery>,
    headers: HeaderMap,
    State(state): State<Arc<DaemonState>>,
    body: Bytes,
) -> SkyResult<Json<ApiAttachment>> {
    let session_id = params.get("id").map_err(bad_request_error)?.to_string();
    let name = query.name.trim().to_string();
    if name.is_empty() {
        return Err(bad_request_error("attachment name is required"));
    }
    let content_type = headers
        .get(skyzen::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let bytes = body;

    let ext = mime_guess::get_mime_extensions_str(content_type.split(';').next().unwrap_or(""))
        .and_then(|exts| exts.first().map(|e| (*e).to_string()))
        .or_else(|| {
            Path::new(&name)
                .extension()
                .and_then(|e| e.to_str())
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| "bin".to_string());
    let base_name = sanitize_upload_stem(&name);
    let (path, relative_path) = allocate_upload_path(&state, &session_id, &base_name, &ext).await?;
    if let Some(parent) = path.parent() {
        async_fs::create_dir_all(parent)
            .await
            .map_err(internal_error)?;
    }
    async_fs::write(&path, &bytes)
        .await
        .map_err(internal_error)?;

    Ok(Json(ApiAttachment {
        id: crate::id::new(),
        name,
        attachment_type: attachment_kind_for_mime(&content_type),
        url: session_file_url(
            &session_id,
            relative_path
                .strip_prefix("files/")
                .unwrap_or(&relative_path),
        ),
        text_content: None,
    }))
}

/// `GET /api/sessions/{id}/files/{*path}` — serve a file stored under the
/// session's `files/` directory. Upload names carry a timestamped suffix, so
/// the bytes a name addresses never change.
pub(super) async fn get_session_file(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<skyzen::Response> {
    let session_id = params.get("id").map_err(bad_request_error)?.to_string();
    let wildcard = params.get("path").map_err(bad_request_error)?;
    let relative = parse_safe_relative_path(wildcard, "file path")?;
    let files_dir = load_session_paths(&state, &session_id).await?.files_dir();
    let canonical_root = async_fs::canonicalize(&files_dir)
        .await
        .map_err(internal_error)?;
    let candidate = files_dir.join(&relative);
    let canonical = async_fs::canonicalize(&candidate)
        .await
        .map_err(|_| SkyError::msg("file not found").set_status(StatusCode::NOT_FOUND))?;
    if !canonical.starts_with(&canonical_root) {
        return Err(bad_request_error(
            "file path escapes the session files directory",
        ));
    }
    let bytes = async_fs::read(&canonical).await.map_err(internal_error)?;
    let mime = mime_guess::from_path(&canonical).first_or_octet_stream();
    let mut response = skyzen::Response::new(skyzen::Body::from(bytes));
    response.headers_mut().insert(
        skyzen::header::CONTENT_TYPE,
        mime.as_ref().parse().map_err(internal_error)?,
    );
    response.headers_mut().insert(
        skyzen::header::CACHE_CONTROL,
        "public, max-age=31536000, immutable"
            .parse()
            .map_err(internal_error)?,
    );
    Ok(response)
}

/// The daemon-relative URL a stored session file is served at.
pub(super) fn session_file_url(session_id: &str, relative_path: &str) -> String {
    format!("/api/sessions/{session_id}/files/{relative_path}")
}

fn attachment_kind_for_mime(content_type: &str) -> AttachmentKind {
    let mime = content_type.split(';').next().unwrap_or("").trim();
    if mime.starts_with("image/") {
        AttachmentKind::Image
    } else if mime == "application/pdf" {
        AttachmentKind::Pdf
    } else if mime.starts_with("audio/") {
        AttachmentKind::Audio
    } else if mime.starts_with("text/") {
        AttachmentKind::Document
    } else {
        AttachmentKind::File
    }
}

#[cfg(test)]
mod file_route_tests {
    use std::sync::Arc;

    use std::str::FromStr as _;

    use super::*;
    use crate::config::MayConfig;
    use crate::daemon::state::DaemonState;

    async fn state() -> (Arc<DaemonState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        let state = DaemonState::for_test(dir.path(), MayConfig::default())
            .await
            .expect("daemon state");
        (Arc::new(state), dir)
    }

    #[tokio::test]
    async fn a_session_file_url_resolves_to_the_stored_file() {
        let (state, dir) = state().await;
        state
            .db()
            .create_session("s", Some("s"), Some("m"))
            .await
            .expect("session");
        let files_dir = load_session_paths(&state, "s")
            .await
            .expect("paths")
            .files_dir();
        async_fs::create_dir_all(&files_dir).await.expect("mkdir");
        async_fs::write(files_dir.join("a.png"), b"png")
            .await
            .expect("write");

        let mut root = None;
        let source =
            resolve_native_attachment_source(&state, "s", "/api/sessions/s/files/a.png", &mut root)
                .await
                .expect("resolves");
        let local_path = source.local_path.clone().expect("local path");
        assert_eq!(
            local_path,
            async_fs::canonicalize(files_dir.join("a.png"))
                .await
                .expect("canonical")
        );
        let url = file_to_data_url(
            source.local_path.as_ref().expect("path"),
            &mime::Mime::from_str("image/png").expect("mime"),
        )
        .await
        .expect("data url");
        assert!(url.as_str().starts_with("data:image/png;base64,"), "{url}");
        drop(dir);
    }
}
