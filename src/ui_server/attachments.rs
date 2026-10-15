//! Attachment processing and upload helpers for chat requests.

use std::path::{Path, PathBuf};

use aither_core::llm::Attachment as LlmAttachment;
use aither_models::{Ability, lookup as lookup_model_info};
use base64::Engine;
use serde::Serialize;
use skyzen::{Error as SkyError, Result as SkyResult, StatusCode};

use crate::agent::generate_session_title;
use crate::session::AttachmentKind;

use super::error::{bad_request_error, internal_error};
use super::helpers::{load_session_paths, parse_safe_relative_path};
use super::state::UiServerState;
use super::types::{ApiAttachment, ChatRequest};

pub(super) const INLINE_PASTED_TEXT_THRESHOLD: usize = 1200;

#[derive(Serialize, skyzen::ToSchema)]
struct TextXmlBlock<'a> {
    #[serde(rename = "$text")]
    content: &'a str,
}

#[derive(Clone, Serialize, skyzen::ToSchema)]
struct AttachmentXml {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "@type")]
    attachment_type: AttachmentKind,
    #[serde(rename = "@path", skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(rename = "@offloaded", skip_serializing_if = "Option::is_none")]
    offloaded: Option<bool>,
    #[serde(rename = "@required_modality", skip_serializing_if = "Option::is_none")]
    required_modality: Option<String>,
    #[serde(rename = "@reason", skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Clone, Serialize, skyzen::ToSchema)]
struct AttachmentsXml {
    #[serde(rename = "attachment")]
    attachments: Vec<AttachmentXml>,
}

#[derive(Clone, Serialize, skyzen::ToSchema)]
struct UnsupportedModalityAlertXml {
    #[serde(rename = "@type")]
    alert_type: String,
    message: String,
    offloaded_attachments: AttachmentsXml,
}

enum PromptSection {
    PastedText(String),
    OffloadedText(String),
    Attachments(Vec<AttachmentXml>),
    UnsupportedModalityAlert(Vec<AttachmentXml>),
}

impl PromptSection {
    fn render(&self) -> SkyResult<String> {
        match self {
            Self::PastedText(content) => serialize_xml("pasted_text", &TextXmlBlock { content }),
            Self::OffloadedText(content) => {
                serialize_xml("offloaded_text", &TextXmlBlock { content })
            }
            Self::Attachments(attachments) => serialize_xml(
                "attachments",
                &AttachmentsXml {
                    attachments: attachments.clone(),
                },
            ),
            Self::UnsupportedModalityAlert(offloaded_attachments) => serialize_xml(
                "system-reminder",
                &UnsupportedModalityAlertXml {
                    alert_type: "unsupported_modality_offload".to_string(),
                    message:
                        "Some attachments were offloaded because the current model does not support their modality. Inspect the referenced files with tools before answering.".to_string(),
                    offloaded_attachments: AttachmentsXml {
                        attachments: offloaded_attachments.clone(),
                    },
                },
            ),
        }
    }
}

fn serialize_xml<T: Serialize>(tag: &str, value: &T) -> SkyResult<String> {
    crate::xml::serialize_xml_block(tag, value).map_err(internal_error)
}

pub(super) async fn build_prompt_and_attachments(
    state: &UiServerState,
    req: &ChatRequest,
    model_id: &str,
) -> SkyResult<(String, Vec<LlmAttachment>)> {
    let modality_support = ModelModalitySupport::for_model(model_id);
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
        let upload_data_url = attachment.upload_data_url.as_deref().or_else(|| {
            attachment
                .url
                .starts_with("data:")
                .then_some(attachment.url.as_str())
        });
        let uploaded = match upload_data_url {
            Some(data_url) => {
                save_upload_data_url(state, &req.session_id, attachment, data_url).await?
            }
            None => None,
        };

        if !supported {
            let relative_path = match uploaded.as_ref() {
                Some((_, relative_path)) => Some(relative_path.clone()),
                None if !attachment.url.trim().is_empty() => {
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
                }
                None => None,
            };
            offloaded_xml_items.push(build_offloaded_attachment_xml(
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

        if let Some((saved_path, relative_path)) = uploaded {
            xml_items.push(AttachmentXml {
                name: attachment.name.clone(),
                attachment_type: attachment.attachment_type,
                path: Some(relative_path),
                offloaded: None,
                required_modality: None,
                reason: None,
            });

            if let Some(modality) = required_modality.as_ref() {
                let media_type = resolve_attachment_media_type(
                    attachment,
                    modality,
                    upload_data_url,
                    Some(saved_path.as_path()),
                    None,
                )?;
                let url = path_to_file_url(saved_path.as_path())?;
                attachments.push(LlmAttachment::new(url, media_type));
            }
            continue;
        }

        if let Some(text) = &attachment.text_content {
            if text.len() <= INLINE_PASTED_TEXT_THRESHOLD {
                prompt_sections.push(PromptSection::PastedText(text.clone()));
            } else {
                let (_, relative_path) =
                    Box::pin(save_large_pasted_text(state, &req.session_id, text)).await?;
                xml_items.push(AttachmentXml {
                    name: attachment.name.clone(),
                    attachment_type: AttachmentKind::Document,
                    path: Some(relative_path),
                    offloaded: None,
                    required_modality: None,
                    reason: None,
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
            xml_items.push(AttachmentXml {
                name: attachment.name.clone(),
                attachment_type: attachment.attachment_type,
                path: source.relative_path,
                offloaded: None,
                required_modality: None,
                reason: None,
            });
            attachments.push(LlmAttachment::new(source.url, media_type));
            continue;
        }

        xml_items.push(AttachmentXml {
            name: attachment.name.clone(),
            attachment_type: attachment.attachment_type,
            path: None,
            offloaded: None,
            required_modality: None,
            reason: None,
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
    state: &UiServerState,
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
    state: &UiServerState,
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
    state: &UiServerState,
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
    url::Url::from_file_path(path).map_err(|()| {
        SkyError::msg("Failed to map multimodal attachment path to URL")
            .set_status(StatusCode::INTERNAL_SERVER_ERROR)
    })
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
    fn for_model(model_id: &str) -> Self {
        let Some(info) = lookup_model_info(model_id) else {
            tracing::warn!(
                model_id = %model_id,
                "model metadata not found; treating advanced modalities as unsupported"
            );
            return Self {
                vision: false,
                audio: false,
                video: false,
                pdf: false,
            };
        };

        Self {
            vision: info.has_ability(Ability::Vision),
            audio: info.has_ability(Ability::Audio),
            video: info.has_ability(Ability::Video),
            pdf: info.has_ability(Ability::Pdf),
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

// ── XML rendering ────────────────────────────────────────────

fn build_offloaded_attachment_xml(
    attachment: &ApiAttachment,
    modality: &AttachmentModality,
    relative_path: Option<&str>,
    model_id: &str,
) -> AttachmentXml {
    AttachmentXml {
        name: attachment.name.clone(),
        attachment_type: attachment.attachment_type,
        path: relative_path.map(ToOwned::to_owned),
        offloaded: Some(true),
        required_modality: Some(modality.label().to_string()),
        reason: Some(["model:", model_id, " lacks ", modality.label()].concat()),
    }
}

// ── Upload helpers ───────────────────────────────────────────

async fn save_upload_data_url(
    state: &UiServerState,
    session_id: &str,
    attachment: &ApiAttachment,
    data_url: &str,
) -> SkyResult<Option<(PathBuf, String)>> {
    let Some((header, data)) = data_url.split_once(',') else {
        return Ok(None);
    };

    if !header.starts_with("data:") {
        return Ok(None);
    }
    if !header.ends_with(";base64") {
        return Err(bad_request_error(
            "attachment data URL must use base64 encoding",
        ));
    }

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(bad_request_error)?;

    let ext = guess_extension(header, &attachment.name);
    let base_name = sanitize_upload_stem(&attachment.name);
    let (path, relative_path) = allocate_upload_path(state, session_id, &base_name, &ext).await?;

    if let Some(parent) = path.parent() {
        async_fs::create_dir_all(parent)
            .await
            .map_err(internal_error)?;
    }

    async_fs::write(&path, bytes)
        .await
        .map_err(internal_error)?;

    Ok(Some((path, relative_path)))
}

async fn save_large_pasted_text(
    state: &UiServerState,
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

async fn generate_pasted_text_filename(state: &UiServerState, text: &str) -> String {
    let config = state.config_snapshot();
    let snippet: String = text.chars().take(600).collect();

    let generated = Box::pin(generate_session_title(&config, &snippet, "")).await;
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
    state: &UiServerState,
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

fn guess_extension(data_url_header: &str, fallback_name: &str) -> String {
    // Extract MIME type from data URL header (e.g. "data:image/png;base64,")
    let mime_str = data_url_header
        .strip_prefix("data:")
        .and_then(|s| s.split(';').next())
        .unwrap_or("");

    if !mime_str.is_empty()
        && let Some(ext) =
            mime_guess::get_mime_extensions_str(mime_str).and_then(|exts| exts.first())
    {
        return (*ext).to_string();
    }

    Path::new(fallback_name)
        .extension()
        .and_then(|e| e.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("bin")
        .to_string()
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
            upload_data_url: None,
            text_content: None,
        }
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
