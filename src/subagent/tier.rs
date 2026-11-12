//! The models a subagent runs on, by tier, and the routing of a brief to one.

use aither_core::LanguageModel;
use aither_core::llm::Message;
use aither_models::ModelTier;
use askama::Template;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::agent::ModelSlot;

/// A tier a subagent runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, JsonSchema, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// The most capable model.
    Advanced,
    /// The balanced model.
    Balanced,
    /// The fast model.
    Fast,
}

impl Tier {
    /// The word the model reads for the tier.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Advanced => "advanced",
            Self::Balanced => "balanced",
            Self::Fast => "fast",
        }
    }
}

impl TryFrom<ModelTier> for Tier {
    type Error = anyhow::Error;

    fn try_from(tier: ModelTier) -> anyhow::Result<Self> {
        match tier {
            ModelTier::Advanced => Ok(Self::Advanced),
            ModelTier::Balanced => Ok(Self::Balanced),
            ModelTier::Fast => Ok(Self::Fast),
            ModelTier::ImageGeneration => {
                anyhow::bail!("a subagent runs on the advanced, balanced or fast tier")
            }
        }
    }
}

/// The model and request parameters of each tier.
#[derive(Debug, Clone)]
pub struct TierModels<LLM> {
    /// The session's own model: a fork continues the main agent on it.
    pub advanced: ModelSlot<LLM>,
    /// The balanced tier.
    pub balanced: ModelSlot<LLM>,
    /// The fast tier, which also routes briefs that name no tier.
    pub fast: ModelSlot<LLM>,
}

#[derive(Debug, JsonSchema, Deserialize)]
struct Routing {
    /// The configured model tier that best fits the task.
    model: Tier,
}

struct RouterModel<'a> {
    tier: &'static str,
    name: &'a str,
    description: &'a str,
}

#[derive(Template)]
#[template(path = "subagent/router.txt", escape = "none")]
struct RouterPrompt<'a> {
    models: Vec<RouterModel<'a>>,
}

impl<LLM: LanguageModel + Clone + Sync + 'static> TierModels<LLM> {
    /// The slot behind `tier`.
    #[must_use]
    pub const fn slot(&self, tier: Tier) -> &ModelSlot<LLM> {
        match tier {
            Tier::Advanced => &self.advanced,
            Tier::Balanced => &self.balanced,
            Tier::Fast => &self.fast,
        }
    }

    /// The tier a call runs on and the name of the model behind it: `chosen`
    /// as given, else the fast model's pick for `brief`.
    ///
    /// # Errors
    /// Returns why a profile cannot be read or the routing failed; nothing
    /// falls back to another tier.
    ///
    /// # Panics
    /// Panics when the router prompt fails to render, which typed input
    /// never does.
    pub async fn select(
        &self,
        brief: &str,
        chosen: Option<Tier>,
    ) -> anyhow::Result<(Tier, String)> {
        if let Some(tier) = chosen {
            let profile = self.slot(tier).llm.profile().await.map_err(|error| {
                anyhow::anyhow!(
                    "failed to fetch the {} model profile: {error}",
                    tier.label()
                )
            })?;
            return Ok((tier, profile.name));
        }
        let (advanced, balanced, fast) = futures::join!(
            self.advanced.llm.profile(),
            self.balanced.llm.profile(),
            self.fast.llm.profile(),
        );
        let profile = |tier: Tier, profile: Result<_, LLM::Error>| {
            profile.map_err(|error| {
                anyhow::anyhow!(
                    "failed to fetch the {} model profile for subagent routing: {error}",
                    tier.label()
                )
            })
        };
        let advanced = profile(Tier::Advanced, advanced)?;
        let balanced = profile(Tier::Balanced, balanced)?;
        let fast = profile(Tier::Fast, fast)?;
        let system = RouterPrompt {
            models: [
                (Tier::Advanced, &advanced),
                (Tier::Balanced, &balanced),
                (Tier::Fast, &fast),
            ]
            .map(|(tier, profile)| RouterModel {
                tier: tier.label(),
                name: &profile.name,
                description: &profile.description,
            })
            .into(),
        }
        .render()
        .expect("the router prompt renders typed input");
        let routing: Routing = self
            .fast
            .generate(vec![Message::system(system), Message::user(brief)])
            .await
            .map_err(|error| anyhow::anyhow!("the subagent router chose no tier: {error}"))?;
        let name = match routing.model {
            Tier::Advanced => advanced.name,
            Tier::Balanced => balanced.name,
            Tier::Fast => fast.name,
        };
        Ok((routing.model, name))
    }
}
