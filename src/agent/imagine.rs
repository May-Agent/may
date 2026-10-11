//! Image-generation tool exposed to May agents as `imagine`.
//!
//! The tool accepts a prompt + `WIDTHxHEIGHT` size and persists the
//! generated PNG into the session's `artifacts/` directory. The concrete
//! provider is resolved lazily per call from the configured
//! `image_generation` tier so that users can swap providers without
//! rebuilding the agent.

use std::borrow::Cow;
use std::path::PathBuf;

use aither_cloud::{CloudProvider, gemini::Gemini, openai::OpenAI};
use aither_core::image::{ImageGenerator, Prompt, Size};
use aither_core::llm::{Tool, ToolResult};
use async_fs;
use futures_lite::StreamExt;

use crate::config::{MayConfig, ProviderInstance, ProviderType};

/// Host-side configuration for the `imagine` tool.
///
/// Populated from [`resolve_image_generator_config`] when the agent
/// starts; `None` means no image-generation model is configured and the
/// tool will surface a friendly error to the agent on first call.
#[derive(Clone)]
pub struct ImageGeneratorConfig {
    pub provider_id: String,
    pub provider: ProviderInstance,
    pub model: String,
}

/// Arguments the agent supplies when invoking `imagine`.
#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
pub struct ImagineArgs {
    /// Prompt describing the image to generate.
    pub prompt: String,
    /// Image size as `WIDTHxHEIGHT`, for example 1024x1024.
    pub size: String,
}

#[derive(Clone)]
pub(crate) struct ImagineTool {
    pub(crate) generator: Option<ImageGeneratorConfig>,
    pub(crate) artifacts_dir: PathBuf,
}

#[derive(Debug, thiserror::Error)]
enum ImagineToolError {
    #[error("no image_generation model configured")]
    MissingGenerator,
    #[error("selected provider does not support image generation")]
    UnsupportedProvider,
    #[error("image generation returned no data")]
    EmptyResponse,
    #[error("invalid size format: expected WIDTHxHEIGHT")]
    InvalidSizeFormat,
    #[error("invalid width in size")]
    InvalidWidth,
    #[error("invalid height in size")]
    InvalidHeight,
    #[error("size must be greater than 0")]
    NonPositiveSize,
    #[error("{0}")]
    MissingApiKey(String),
    #[error("image generation failed: {0}")]
    Generate(String),
    #[error("failed to prepare artifacts directory: {0}")]
    CreateArtifactsDir(std::io::Error),
    #[error("failed to write generated image: {0}")]
    WriteImage(std::io::Error),
}

impl Tool for ImagineTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("imagine")
    }

    type Arguments = ImagineArgs;
    type Res = ToolResult;

    async fn call(
        &self,
        arguments: Self::Arguments,
        _cx: aither_core::llm::ToolContext,
    ) -> aither_core::Result<Self::Res> {
        match self.generate_image(arguments).await {
            Ok(artifact_path) => Ok(ToolResult::text(artifact_path)),
            Err(error) => Ok(ToolResult::error(error.to_string())),
        }
    }
}

impl ImagineTool {
    async fn generate_image(&self, arguments: ImagineArgs) -> Result<String, ImagineToolError> {
        let generator = self
            .generator
            .as_ref()
            .ok_or(ImagineToolError::MissingGenerator)?;

        let size = parse_imagine_size(&arguments.size)?;
        let prompt = Prompt::new(arguments.prompt);

        let image_provider = build_image_provider(generator)?;

        let mut latest = None;
        match image_provider {
            CloudProvider::OpenAI(provider) => {
                let mut stream = std::pin::pin!(provider.create(prompt, size));
                while let Some(chunk) = stream.next().await {
                    latest =
                        Some(chunk.map_err(|error| ImagineToolError::Generate(error.to_string()))?);
                }
            }
            CloudProvider::Gemini(provider) => {
                let mut stream = std::pin::pin!(provider.create(prompt, size));
                while let Some(chunk) = stream.next().await {
                    latest =
                        Some(chunk.map_err(|error| ImagineToolError::Generate(error.to_string()))?);
                }
            }
            _ => return Err(ImagineToolError::UnsupportedProvider),
        }

        let image = latest.ok_or(ImagineToolError::EmptyResponse)?;
        let mut filename = uuid::Uuid::new_v4().to_string();
        filename.push_str(".png");
        let path = self.artifacts_dir.join(&filename);
        async_fs::create_dir_all(&self.artifacts_dir)
            .await
            .map_err(ImagineToolError::CreateArtifactsDir)?;
        async_fs::write(&path, image)
            .await
            .map_err(ImagineToolError::WriteImage)?;

        let mut artifact_path = String::with_capacity(filename.len() + 10);
        artifact_path.push_str("artifacts/");
        artifact_path.push_str(filename.as_str());
        Ok(artifact_path)
    }
}

fn parse_imagine_size(raw: &str) -> Result<Size, ImagineToolError> {
    let value = raw.trim();
    let Some((w, h)) = value.split_once('x') else {
        return Err(ImagineToolError::InvalidSizeFormat);
    };

    let width: u32 = w
        .trim()
        .parse()
        .map_err(|_| ImagineToolError::InvalidWidth)?;
    let height: u32 = h
        .trim()
        .parse()
        .map_err(|_| ImagineToolError::InvalidHeight)?;

    if width == 0 || height == 0 {
        return Err(ImagineToolError::NonPositiveSize);
    }

    Ok(Size::new(width, height))
}

fn build_image_provider(config: &ImageGeneratorConfig) -> Result<CloudProvider, ImagineToolError> {
    let provider_type = config.provider.resolve_type(&config.provider_id);
    let api_key = config.provider.api_key.as_deref().unwrap_or("");
    let base_url = config.provider.effective_base_url(&config.provider_id);

    if provider_type.requires_api_key() && api_key.is_empty() {
        return Err(ImagineToolError::MissingApiKey(format!(
            "API key required for provider '{}'",
            config.provider_id
        )));
    }

    let provider = if provider_type == ProviderType::Google {
        let mut client = Gemini::new(api_key).with_image_model(&config.model);
        if let Some(url) = base_url {
            client = client.with_base_url(url);
        }
        CloudProvider::Gemini(client)
    } else {
        let mut client = OpenAI::new(api_key).with_image_model(&config.model);
        if let Some(url) = base_url {
            client = client.with_base_url(url);
        }
        CloudProvider::OpenAI(client)
    };

    Ok(provider)
}

pub fn resolve_image_generator_config(may_config: &MayConfig) -> Option<ImageGeneratorConfig> {
    let selector = may_config
        .default_model_for_tier(crate::config::ModelTier::ImageGeneration)?
        .clone();
    let provider_id = selector.provider_id().to_string();
    let provider = may_config.provider.get(&provider_id)?.clone();

    Some(ImageGeneratorConfig {
        provider_id,
        provider,
        model: selector.model_id().to_string(),
    })
}
