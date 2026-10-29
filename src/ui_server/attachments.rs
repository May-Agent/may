//! Attachment processing and upload helpers for chat requests.

use std::path::{Path, PathBuf};

use aither_core::llm::Attachment as LlmAttachment;
use aither_models::{Ability, lookup as lookup_model_info};
use askama::Template;
use base64::Engine;
use skyzen::Result as SkyResult;

use crate::agent::generate_session_title;
use crate::session::AttachmentKind;

use super::error::{bad_request_error, internal_error, internal_error_message};
use super::helpers::{load_session_paths, parse_safe_relative_path};
use super::state::UiServerState;
use super::types::{ApiAttachment, ChatRequest};

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

        if let Some((saved_path, relative_path)) = uploaded {
            xml_items.push(PromptAttachment {
                name: attachment.name.clone(),
                kind: attachment.attachment_type,
                path: Some(relative_path),
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
            attachments.push(LlmAttachment::new(source.url, media_type));
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
