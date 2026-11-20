//! The `models` command: what each model tier resolves to, and the
//! `provider::model` selectors a `subagent` call may name.

use std::borrow::Cow;
use std::collections::HashSet;

use aither_core::llm::{Tool, ToolResult};
use askama::Template;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::subagent::Tier;

/// What `models` lists: the session's model and the selectors each tier
/// resolves, captured when the session's commands are built — a config
/// change rebuilds them with a fresh listing.
#[derive(Debug, Clone)]
pub struct ModelListing {
    /// The session's own model, as `provider::model`.
    pub session: crate::ModelSelector,
    /// `(tier, selectors)` in priority order; an empty list resolves to
    /// `session`.
    pub tiers: Vec<(Tier, Vec<crate::ModelSelector>)>,
}

/// List the models a `subagent` call may name and what each tier resolves to.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ModelsArgs {
    /// Also list the models a newer generation of their line replaced,
    /// hidden by default; `--model` keeps accepting them either way.
    #[serde(default)]
    pub full: bool,
}

/// The `models` command.
#[derive(Debug)]
pub struct ModelsTool {
    listing: ModelListing,
}

impl ModelsTool {
    /// Lists `listing`.
    #[must_use]
    pub const fn new(listing: ModelListing) -> Self {
        Self { listing }
    }
}

/// A tier's resolution row.
struct TierRow {
    /// `advanced`, `balanced` or `fast`.
    label: &'static str,
    /// The `provider::model` it resolves to.
    resolved: String,
    /// The resolution is the session model rather than a configured selector.
    session: bool,
    /// The resolved model is retired or superseded.
    outdated: bool,
}

/// One in-play selector.
struct ModelLine {
    /// The `provider::model` a call may name.
    selector: String,
    /// What it is in play as, when it is not a bare extra.
    note: String,
    /// It is retired or superseded.
    outdated: bool,
}

#[derive(Template)]
#[template(path = "models.txt", escape = "none")]
struct ModelsText {
    /// The three tier rows.
    tiers: Vec<TierRow>,
    /// The in-play selectors this call shows.
    lines: Vec<ModelLine>,
    /// In-play selectors hidden as outdated; 0 under `--full`.
    hidden: usize,
}

#[allow(clippy::unused_async_trait_impl, reason = "the trait method is async")]
impl Tool for ModelsTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("models")
    }

    type Arguments = ModelsArgs;
    type Res = ToolResult;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: aither_core::llm::ToolContext,
    ) -> aither_core::Result<Self::Res> {
        let registry = aither_models::ModelRegistry::bundled();
        let index = crate::models::OutdatedIndex::of(registry);
        let outdated =
            |selector: &crate::ModelSelector| index.is_outdated_id(registry, selector.model_id());
        // What a tier resolves to is shown either way — hiding it would
        // misreport what runs; other outdated selectors are hidden unless
        // `--full` asks for everything.
        let mut resolved = HashSet::from([self.listing.session.to_string()]);
        let mut tiers = Vec::new();
        for (tier, selectors) in &self.listing.tiers {
            let (model, session) = selectors.first().map_or_else(
                || (self.listing.session.clone(), true),
                |selector| (selector.clone(), false),
            );
            resolved.insert(model.to_string());
            tiers.push(TierRow {
                label: tier.label(),
                resolved: model.to_string(),
                session,
                outdated: outdated(&model),
            });
        }
        let mut lines = Vec::new();
        let mut seen = HashSet::new();
        let mut hidden = 0;
        let mut push = |selector: &crate::ModelSelector, note: String| {
            let name = selector.to_string();
            if !seen.insert(name.clone()) {
                return;
            }
            let is_outdated = outdated(selector);
            if is_outdated && !resolved.contains(&name) && !args.full {
                hidden += 1;
                return;
            }
            lines.push(ModelLine {
                selector: name,
                note,
                outdated: is_outdated,
            });
        };
        push(&self.listing.session, "the session model".to_string());
        for (tier, selectors) in &self.listing.tiers {
            for selector in selectors {
                push(selector, tier.label().to_string());
            }
        }
        let text = ModelsText {
            tiers,
            lines,
            hidden,
        }
        .render()
        .expect("the models template renders");
        Ok(ToolResult::text(text))
    }
}
