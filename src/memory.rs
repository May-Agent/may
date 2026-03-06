//! Per-profile memory system.
//!
//! Maintains preference and recency memories for each profile by running
//! conversations through a fast LLM. Processing happens asynchronously
//! in a periodic background worker to avoid blocking the main chat flow.

use std::collections::HashSet;
use std::time::Duration;

use aither_cloud::CloudProvider;
use aither_core::LanguageModel;
use aither_core::llm::{LLMRequest, Message};
use futures_lite::StreamExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::db::ChatDb;

const PREFERENCE_PROMPT: &str = include_str!("../templates/preference_extraction_prompt.txt");
const RECENCY_PROMPT: &str = include_str!("../templates/recency_extraction_prompt.txt");
const MEMORY_PROCESS_INTERVAL: Duration = Duration::from_secs(45);
const MAX_PROFILE_SESSIONS: usize = 3;
const MAX_SESSION_MESSAGES: usize = 80;
const MAX_CONVERSATION_CHARS: usize = 30_000;

/// Profile memory processor.
///
/// Runs in the background, tracking dirty profiles and processing them on a
/// fixed interval.
pub struct MemoryProcessor {
    dirty_tx: UnboundedSender<String>,
}

#[derive(Clone)]
pub struct MemoryExtractionProvider {
    pub provider_id: String,
    pub model: String,
    pub llm: CloudProvider,
}

impl MemoryProcessor {
    /// Create a new memory processor.
    ///
    /// `llm` should be a fast model provider (e.g., Claude Haiku, GPT-4o-mini).
    /// Spawns a periodic background task that processes dirty profiles.
    pub fn new(db: ChatDb, providers: Vec<MemoryExtractionProvider>) -> Self {
        let (dirty_tx, dirty_rx) = unbounded_channel::<String>();

        tokio::spawn(async move {
            Self::worker(dirty_rx, db, providers).await;
        });

        Self { dirty_tx }
    }

    /// Mark a profile as dirty for background memory processing.
    pub fn mark_profile_dirty(&self, profile_id: &str) {
        if let Err(e) = self.dirty_tx.send(profile_id.to_string()) {
            tracing::error!(error = %e, profile_id = %profile_id, "failed to queue profile memory update");
        }
    }

    async fn worker(
        mut dirty_rx: UnboundedReceiver<String>,
        db: ChatDb,
        providers: Vec<MemoryExtractionProvider>,
    ) {
        let mut dirty_profiles = HashSet::new();
        let mut ticker = tokio::time::interval(MEMORY_PROCESS_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                maybe_profile_id = dirty_rx.recv() => {
                    let Some(profile_id) = maybe_profile_id else {
                        break;
                    };
                    dirty_profiles.insert(profile_id);
                }
                _ = ticker.tick() => {
                    if dirty_profiles.is_empty() {
                        continue;
                    }

                    let mut batch = dirty_profiles.drain().collect::<Vec<_>>();
                    batch.sort_unstable();

                    for profile_id in batch {
                        let conversation = match Self::build_profile_conversation(&db, &profile_id).await {
                            Ok(conversation) => conversation,
                            Err(e) => {
                                tracing::warn!(profile_id = %profile_id, error = %e, "failed to build profile conversation for memory");
                                continue;
                            }
                        };

                        if conversation.is_empty() {
                            continue;
                        }

                        if let Err(e) = Self::process_request(&db, &providers, &profile_id, &conversation).await {
                            tracing::warn!(profile_id = %profile_id, error = %e, "memory extraction failed");
                        }
                    }
                }
            }
        }
    }

    async fn build_profile_conversation(db: &ChatDb, profile_id: &str) -> anyhow::Result<String> {
        let sessions = db
            .list_sessions_for_profile(profile_id)
            .await?
            .into_iter()
            .take(MAX_PROFILE_SESSIONS)
            .collect::<Vec<_>>();

        if sessions.is_empty() {
            return Ok(String::new());
        }

        let mut turns = Vec::new();
        // Process from older to newer among the selected recent sessions.
        for session in sessions.iter().rev() {
            let messages = db.get_messages(&session.id).await?;
            let skip = messages.len().saturating_sub(MAX_SESSION_MESSAGES);
            for message in messages.into_iter().skip(skip) {
                match message.role.as_str() {
                    "user" => turns.push(format!("User: {}", message.content)),
                    "assistant" => turns.push(format!("Assistant: {}", message.content)),
                    _ => {}
                }
            }
        }

        if turns.is_empty() {
            return Ok(String::new());
        }

        let conversation = turns.join("\n\n");
        if conversation.len() <= MAX_CONVERSATION_CHARS {
            return Ok(conversation);
        }

        let start = conversation.len() - MAX_CONVERSATION_CHARS;
        let start = conversation.ceil_char_boundary(start);
        Ok(conversation[start..].to_string())
    }

    /// Process a single profile extraction request.
    async fn process_request(
        db: &ChatDb,
        providers: &[MemoryExtractionProvider],
        profile_id: &str,
        conversation: &str,
    ) -> anyhow::Result<()> {
        if providers.is_empty() {
            return Err(anyhow::anyhow!("no memory extraction providers configured"));
        }

        // Extract preference memory
        let current_preference = db
            .get_profile_memory(profile_id, "preference")
            .await?
            .map(|m| m.content)
            .unwrap_or_default();

        let preference_result = Self::extract(
            providers,
            PREFERENCE_PROMPT,
            &current_preference,
            conversation,
        )
        .await?;

        if !preference_result.trim().is_empty() {
            db.upsert_profile_memory(profile_id, "preference", &preference_result)
                .await?;
            tracing::debug!(profile_id = %profile_id, "Updated preference memory");
        }

        // Extract recency memory
        let current_recency = db
            .get_profile_memory(profile_id, "recency")
            .await?
            .map(|m| m.content)
            .unwrap_or_default();

        let recency_result =
            Self::extract(providers, RECENCY_PROMPT, &current_recency, conversation).await?;

        if !recency_result.trim().is_empty() {
            db.upsert_profile_memory(profile_id, "recency", &recency_result)
                .await?;
            tracing::debug!(profile_id = %profile_id, "Updated recency memory");
        }

        Ok(())
    }

    /// Run a single extraction prompt through the fast LLM.
    async fn extract(
        providers: &[MemoryExtractionProvider],
        template: &str,
        current: &str,
        conversation: &str,
    ) -> anyhow::Result<String> {
        let prompt = template
            .replace("{current}", current)
            .replace("{conversation}", conversation);
        let mut errors = Vec::new();

        for provider in providers {
            match Self::extract_with_provider(&provider.llm, &prompt).await {
                Ok(content) => return Ok(content),
                Err(error) => {
                    let error_text = error.to_string();
                    tracing::warn!(
                        provider_id = %provider.provider_id,
                        model = %provider.model,
                        error = %error_text,
                        "memory extraction provider failed; trying next candidate",
                    );
                    errors.push(format!(
                        "{}:{}:{}",
                        provider.provider_id, provider.model, error_text
                    ));
                }
            }
        }

        Err(anyhow::anyhow!(
            "all memory extraction providers failed: {}",
            errors.join(" | ")
        ))
    }

    async fn extract_with_provider(llm: &CloudProvider, prompt: &str) -> anyhow::Result<String> {
        let request = LLMRequest::new(vec![Message::user(prompt)]);

        let mut stream = std::pin::pin!(llm.respond(request));
        let mut content = String::new();
        while let Some(event) = stream.next().await {
            if let aither_core::llm::Event::Text(chunk) = event? {
                content.push_str(&chunk);
            }
        }

        Ok(content)
    }
}

/// Profile memories to inject into agent context (read-only).
pub struct ProfileMemories {
    pub preference: Option<String>,
    pub recency: Option<String>,
}

impl ProfileMemories {
    /// Load profile memories from the database.
    pub async fn load(db: &ChatDb, profile_id: &str) -> Result<Self, crate::db::ChatDbError> {
        let memories = db.get_profile_memories(profile_id).await?;
        let mut preference = None;
        let mut recency = None;

        for mem in memories {
            match mem.memory_type.as_str() {
                "preference" => preference = Some(mem.content),
                "recency" => recency = Some(mem.content),
                _ => {}
            }
        }

        Ok(Self {
            preference,
            recency,
        })
    }

    /// Check if any memories are present.
    pub fn is_empty(&self) -> bool {
        self.preference.is_none() && self.recency.is_none()
    }

    /// Format memories as context for agent injection.
    pub fn to_context_string(&self) -> String {
        let mut parts = Vec::new();

        if let Some(ref pref) = self.preference {
            parts.push(format!("## User Preferences\n{pref}"));
        }

        if let Some(ref rec) = self.recency {
            parts.push(format!("## Recent Context\n{rec}"));
        }

        parts.join("\n\n")
    }
}
