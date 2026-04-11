//! Attachment processing and upload helpers for chat requests.

use std::path::{Path, PathBuf};

use aither_models::{Ability, lookup as lookup_model_info};
use base64::Engine;
use serde::Serialize;
use skyzen::{Error as SkyError, Result as SkyResult, StatusCode};

use crate::agent::generate_session_title;
use crate::session::AttachmentKind;

use super::error::{bad_request_error, internal_error};
use super::helpers::load_session_paths;
use super::state::UiServerState;
use super::types::{ApiAttachment, ChatRequest};

pub(super) const INLINE_PASTED_TEXT_THRESHOLD: usize = 1200;

#[derive(Serialize)]
struct TextXmlBlock<'a> {
    #[serde(rename = "$text")]
    content: &'a str,
}

#[derive(Clone, Serialize)]
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

#[derive(Clone, Serialize)]
struct AttachmentsXml {
    #[serde(rename = "attachment")]
    attachments: Vec<AttachmentXml>,
}

#[derive(Clone, Serialize)]
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
    crate::xml_fragment::serialize_tagged_xml(tag, value).map_err(internal_error)
}

pub(super) async fn build_prompt_and_attachments(
    state: &UiServerState,
    req: &ChatRequest,
    model_id: &str,
) -> SkyResult<(String, Vec<url::Url>)> {
    let modality_support = ModelModalitySupport::for_model(model_id);
    let mut attachments = Vec::new();
    let mut prompt_sections = Vec::new();
    let mut xml_items = Vec::new();
    let mut offloaded_xml_items = Vec::new();

    for attachment in &req.attachments {
        let required_modality = AttachmentModality::for_attachment_type(attachment.attachment_type);
        let supported = required_modality
            .as_ref()
            .is_none_or(|modality| modality_support.supports(modality));

        let upload_data_url = attachment.upload_data_url.as_deref().or_else(|| {
            if attachment.attachment_type == AttachmentKind::Image
                && attachment.url.starts_with("data:")
            {
                Some(attachment.url.as_str())
            } else {
                None
            }
        });
        let uploaded = match upload_data_url {
            Some(data_url) => {
                save_upload_data_url(state, &req.session_id, attachment, data_url).await?
            }
            None => None,
        };
        let maybe_relative_path = uploaded
            .as_ref()
            .map(|(_, relative_path)| relative_path.clone());

        if !supported {
            offloaded_xml_items.push(build_offloaded_attachment_xml(
                attachment,
                required_modality
                    .as_ref()
                    .expect("unsupported attachment must have modality"),
                maybe_relative_path.as_deref(),
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
                attachment_type: attachment.attachment_type.clone(),
                path: Some(relative_path),
                offloaded: None,
                required_modality: None,
                reason: None,
            });

            if attachment.attachment_type == AttachmentKind::Image {
                let url = url::Url::from_file_path(saved_path).map_err(|()| {
                    SkyError::msg("Failed to map image path to URL")
                        .set_status(StatusCode::INTERNAL_SERVER_ERROR)
                })?;
                attachments.push(url);
            }
            continue;
        }

        if let Some(text) = &attachment.text_content {
            if text.chars().count() <= INLINE_PASTED_TEXT_THRESHOLD {
                prompt_sections.push(PromptSection::PastedText(text.clone()));
            } else {
                let (saved_path, relative_path) =
                    save_large_pasted_text(state, &req.session_id, text).await?;
                xml_items.push(AttachmentXml {
                    name: attachment.name.clone(),
                    attachment_type: AttachmentKind::Document,
                    path: Some(relative_path),
                    offloaded: None,
                    required_modality: None,
                    reason: None,
                });

                if attachment.attachment_type == AttachmentKind::Image {
                    let url = url::Url::from_file_path(saved_path).map_err(|()| {
                        SkyError::msg("Failed to map image path to URL")
                            .set_status(StatusCode::INTERNAL_SERVER_ERROR)
                    })?;
                    attachments.push(url);
                }
            }
            continue;
        }

        xml_items.push(AttachmentXml {
            name: attachment.name.clone(),
            attachment_type: attachment.attachment_type.clone(),
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

// ── Modality support ─────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
enum AttachmentModality {
    Vision,
    Audio,
    Video,
    Pdf,
}

impl AttachmentModality {
    fn for_attachment_type(attachment_type: AttachmentKind) -> Option<Self> {
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
        attachment_type: attachment.attachment_type.clone(),
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
    let filename_base = generate_pasted_text_filename(state, text).await;
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

    let generated = generate_session_title(&config, &snippet, "").await;
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
            .map(|d| d.as_secs())
            .unwrap_or(0);
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
