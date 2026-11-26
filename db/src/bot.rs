//! SQLite persistence for May Bots: bots, durable monotonic `bot_inbox`,
//! user chat messages, internal trace entries, context snapshots with
//! `SeenMemoryLedger`, delegated workers + checkpoints + timestamped messages,
//! supervised sessions, approval triage rows, schedules, and `store_fact` queue.

use std::str::FromStr;

use may_protocol::bot::{
    BotApprovalGate, BotApprovalItem, BotApprovalPolicy, BotApprovalStatus, BotChatMessage,
    BotDelegateStatus, BotDelegateSummary, BotInboxEntry, BotInboxKind, BotScheduleItem,
    BotTraceEntry, SeenMemoryLedger, format_monotonic_utc, next_monotonic_ts_ms, parse_utc_to_ms,
};

use crate::common::{I64ValueRow, storage_error};
use crate::context_snapshot::CONTEXT_SNAPSHOT_FORMAT;
use crate::{ChatDb, ChatDbError, query};

/// Persisted row for one May Bot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotRow {
    /// Primary bot identifier.
    pub id: String,
    /// Stable `word-word-word` `AgentDirectory` identifier.
    pub agent_id: String,
    /// Display name.
    pub name: String,
    /// Optional persona prompt.
    pub persona: Option<String>,
    /// Optional explicit model selector.
    pub model: Option<String>,
    /// Optional profile id.
    pub profile_id: Option<String>,
    /// Whether the bot's resident loop is enabled.
    pub enabled: bool,
    /// Live per-gate approval policy.
    pub approval_policy: BotApprovalPolicy,
    /// Last assigned monotonic UTC timestamp in Unix milliseconds.
    pub last_ts_ms: i64,
    /// Creation timestamp (UTC).
    pub created_at: String,
    /// Last update timestamp (UTC).
    pub updated_at: String,
}

/// Parameters for creating a new bot row.
#[derive(Debug, Clone)]
pub struct NewBotRow<'a> {
    /// Primary bot id.
    pub id: &'a str,
    /// Stable `word-word-word` `agent_id`.
    pub agent_id: &'a str,
    /// Display name.
    pub name: &'a str,
    /// Optional persona prompt.
    pub persona: Option<&'a str>,
    /// Optional model selector.
    pub model: Option<&'a str>,
    /// Optional profile id.
    pub profile_id: Option<&'a str>,
    /// Whether enabled.
    pub enabled: bool,
    /// Approval policy.
    pub approval_policy: &'a BotApprovalPolicy,
}

/// Parameters for updating an existing bot row.
#[derive(Debug, Clone, Default)]
pub struct UpdateBotRow<'a> {
    /// New name.
    pub name: Option<&'a str>,
    /// New persona (`Some(None)` clears).
    pub persona: Option<Option<&'a str>>,
    /// New model (`Some(None)` clears).
    pub model: Option<Option<&'a str>>,
    /// New enabled flag.
    pub enabled: Option<bool>,
    /// New approval policy.
    pub approval_policy: Option<&'a BotApprovalPolicy>,
}

/// Parameters for enqueueing an entry into `bot_inbox`.
#[derive(Debug, Clone)]
pub struct NewBotInboxEntry<'a> {
    /// Target bot id.
    pub bot_id: &'a str,
    /// Event kind.
    pub kind: BotInboxKind,
    /// Optional sender identifier.
    pub sender_id: Option<&'a str>,
    /// Optional sender display name.
    pub sender_name: Option<&'a str>,
    /// Event text payload.
    pub body: &'a str,
    /// Optional structured JSON metadata.
    pub meta_json: Option<&'a str>,
}

/// Stored bot context snapshot paired with its `SeenMemoryLedger`.
#[derive(Debug, Clone)]
pub struct StoredBotContextSnapshot {
    /// Restorable `aither_agent::AgentCheckpoint`.
    pub checkpoint: aither_agent::AgentCheckpoint,
    /// Uncompacted-window deduplication ledger.
    pub seen_memory: SeenMemoryLedger,
    /// Last update timestamp (UTC).
    pub updated_at: String,
}

/// One timestamped message recorded for a delegated worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotDelegateMessageRow {
    /// Row id.
    pub id: i64,
    /// Delegated worker `agent_id`.
    pub agent_id: String,
    /// Owning bot id.
    pub bot_id: String,
    /// Message role (`user`, `assistant`, `tool`, `system`).
    pub role: String,
    /// Message body.
    pub content: String,
    /// Monotonic UTC millisecond key.
    pub ts_ms: i64,
    /// Formatted UTC timestamp.
    pub ts_utc: String,
}

/// Stored `store_fact` queue row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotStoreFactRow {
    /// Queue row id.
    pub id: i64,
    /// Owning bot id.
    pub bot_id: String,
    /// Natural-language fact, correction, or deletion instruction.
    pub fact: String,
    /// Originating message UTC timestamp.
    pub source_time: String,
    /// Optional source agent id (`None` means the bot itself).
    pub source_agent: Option<String>,
    /// Queue status (`pending`, `done`, `failed`).
    pub status: String,
    /// Resulting git commit SHA in `memory.git`.
    pub commit_sha: Option<String>,
    /// Error message when `status == "failed"`.
    pub error: Option<String>,
    /// Enqueue timestamp (UTC).
    pub created_at: String,
    /// Completion timestamp (UTC).
    pub finished_at: Option<String>,
}

/// Parameters for creating a delegated worker row.
#[derive(Debug, Clone)]
pub struct NewBotDelegate<'a> {
    /// Stable `word-word-word` `agent_id`.
    pub agent_id: &'a str,
    /// Owning bot id.
    pub bot_id: &'a str,
    /// Display name.
    pub name: &'a str,
    /// Subagent definition name.
    pub definition: &'a str,
    /// Working directory path.
    pub work_dir: &'a str,
    /// Granted worktree paths.
    pub granted_worktrees: &'a [String],
}

/// Parameters for creating a bot-routed approval request.
#[derive(Debug, Clone)]
pub struct NewBotApproval<'a> {
    /// Request identifier.
    pub request_id: &'a str,
    /// Owning bot id.
    pub bot_id: &'a str,
    /// `"delegate"` or `"session"`.
    pub requester_kind: &'a str,
    /// Delegated worker `agent_id` or supervised `session_id`.
    pub requester_id: &'a str,
    /// Permission gate hit.
    pub gate: BotApprovalGate,
    /// Script, host, path, or publication text.
    pub subject: &'a str,
    /// Optional reason from requester.
    pub reason: Option<&'a str>,
    /// Initial status (`PendingBot` or `Escalated`).
    pub status: BotApprovalStatus,
    /// Optional escalation note.
    pub decision_note: Option<&'a str>,
}

/// Parameters for creating a bot schedule.
#[derive(Debug, Clone)]
pub struct NewBotSchedule<'a> {
    /// Schedule identifier.
    pub id: &'a str,
    /// Owning bot id.
    pub bot_id: &'a str,
    /// Human-readable name.
    pub name: &'a str,
    /// `"once"` or `"cron"`.
    pub kind: &'a str,
    /// Cron expression (UTC) for `cron` schedules.
    pub cron_expression: Option<&'a str>,
    /// RFC 3339 UTC timestamp for `once` schedules.
    pub run_at: Option<&'a str>,
    /// Prompt text enqueued into `bot_inbox`.
    pub prompt: &'a str,
    /// Next fire timestamp (UTC).
    pub next_fire_at: Option<&'a str>,
}

#[derive(Debug, serde::Deserialize, skyzen::FromRow)]
struct RawBotRow {
    id: String,
    agent_id: String,
    name: String,
    persona: Option<String>,
    model: Option<String>,
    profile_id: Option<String>,
    enabled: bool,
    approval_policy_json: String,
    last_ts_ms: i64,
    created_at: String,
    updated_at: String,
}

impl TryFrom<RawBotRow> for BotRow {
    type Error = ChatDbError;

    fn try_from(row: RawBotRow) -> Result<Self, Self::Error> {
        let approval_policy = serde_json::from_str::<BotApprovalPolicy>(&row.approval_policy_json)
            .map_err(|error| storage_error(format!("invalid bot approval policy JSON: {error}")))?;
        Ok(Self {
            id: row.id,
            agent_id: row.agent_id,
            name: row.name,
            persona: row.persona,
            model: row.model,
            profile_id: row.profile_id,
            enabled: row.enabled,
            approval_policy,
            last_ts_ms: row.last_ts_ms,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[derive(Debug, serde::Deserialize, skyzen::FromRow)]
struct RawBotInboxRow {
    id: i64,
    bot_id: String,
    ts_ms: i64,
    ts_utc: String,
    kind: String,
    sender_id: Option<String>,
    sender_name: Option<String>,
    body: String,
    meta_json: Option<String>,
    consumed_at: Option<String>,
}

impl TryFrom<RawBotInboxRow> for BotInboxEntry {
    type Error = ChatDbError;

    fn try_from(row: RawBotInboxRow) -> Result<Self, Self::Error> {
        let kind = BotInboxKind::from_str(&row.kind)
            .map_err(|error| storage_error(format!("invalid bot inbox kind: {error}")))?;
        Ok(Self {
            id: row.id,
            bot_id: row.bot_id,
            ts_ms: row.ts_ms,
            ts_utc: row.ts_utc,
            kind,
            sender_id: row.sender_id,
            sender_name: row.sender_name,
            body: row.body,
            meta_json: row.meta_json,
            consumed_at: row.consumed_at,
        })
    }
}

#[derive(Debug, serde::Deserialize, skyzen::FromRow)]
struct RawBotChatRow {
    id: i64,
    bot_id: String,
    role: String,
    content: String,
    ts_ms: i64,
    ts_utc: String,
}

impl From<RawBotChatRow> for BotChatMessage {
    fn from(row: RawBotChatRow) -> Self {
        Self {
            id: row.id,
            bot_id: row.bot_id,
            role: row.role,
            content: row.content,
            ts_ms: row.ts_ms,
            ts_utc: row.ts_utc,
        }
    }
}

#[derive(Debug, serde::Deserialize, skyzen::FromRow)]
struct RawBotTraceRow {
    id: i64,
    bot_id: String,
    kind: String,
    content: String,
    ts_utc: String,
}

impl From<RawBotTraceRow> for BotTraceEntry {
    fn from(row: RawBotTraceRow) -> Self {
        Self {
            id: row.id,
            bot_id: row.bot_id,
            kind: row.kind,
            content: row.content,
            ts_utc: row.ts_utc,
        }
    }
}

#[derive(Debug, serde::Deserialize, skyzen::FromRow)]
struct RawBotSnapshotRow {
    format_version: i64,
    checkpoint_blob: Vec<u8>,
    seen_memory_json: String,
    updated_at: String,
}

#[derive(Debug, serde::Deserialize, skyzen::FromRow)]
struct RawBotDelegateRow {
    agent_id: String,
    bot_id: String,
    name: String,
    definition: String,
    work_dir: String,
    granted_worktrees_json: String,
    status: String,
    active_command: Option<String>,
    elapsed_ms: i64,
    prompt_tokens: i64,
    completion_tokens: i64,
    last_output: Option<String>,
    created_at: String,
    updated_at: String,
}

impl TryFrom<RawBotDelegateRow> for BotDelegateSummary {
    type Error = ChatDbError;

    fn try_from(row: RawBotDelegateRow) -> Result<Self, Self::Error> {
        let status = BotDelegateStatus::from_str(&row.status)
            .map_err(|error| storage_error(format!("invalid bot delegate status: {error}")))?;
        let granted_worktrees =
            serde_json::from_str::<Vec<String>>(&row.granted_worktrees_json).unwrap_or_default();
        Ok(Self {
            agent_id: row.agent_id,
            bot_id: row.bot_id,
            name: row.name,
            definition: row.definition,
            work_dir: row.work_dir,
            granted_worktrees,
            status,
            active_command: row.active_command,
            elapsed_ms: row.elapsed_ms,
            prompt_tokens: row.prompt_tokens,
            completion_tokens: row.completion_tokens,
            last_output: row.last_output,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[derive(Debug, serde::Deserialize, skyzen::FromRow)]
struct RawDelegateCheckpointRow {
    checkpoint_blob: Option<Vec<u8>>,
    checkpoint_format: Option<i64>,
}

#[derive(Debug, serde::Deserialize, skyzen::FromRow)]
struct RawBotDelegateMessageRow {
    id: i64,
    agent_id: String,
    bot_id: String,
    role: String,
    content: String,
    ts_ms: i64,
    ts_utc: String,
}

impl From<RawBotDelegateMessageRow> for BotDelegateMessageRow {
    fn from(row: RawBotDelegateMessageRow) -> Self {
        Self {
            id: row.id,
            agent_id: row.agent_id,
            bot_id: row.bot_id,
            role: row.role,
            content: row.content,
            ts_ms: row.ts_ms,
            ts_utc: row.ts_utc,
        }
    }
}

#[derive(Debug, serde::Deserialize, skyzen::FromRow)]
struct RawBotApprovalRow {
    request_id: String,
    bot_id: String,
    requester_kind: String,
    requester_id: String,
    gate: String,
    subject: String,
    reason: Option<String>,
    status: String,
    decision_note: Option<String>,
    created_at: String,
    resolved_at: Option<String>,
}

impl TryFrom<RawBotApprovalRow> for BotApprovalItem {
    type Error = ChatDbError;

    fn try_from(row: RawBotApprovalRow) -> Result<Self, Self::Error> {
        let gate = BotApprovalGate::from_str(&row.gate)
            .map_err(|error| storage_error(format!("invalid bot approval gate: {error}")))?;
        let status = BotApprovalStatus::from_str(&row.status)
            .map_err(|error| storage_error(format!("invalid bot approval status: {error}")))?;
        Ok(Self {
            request_id: row.request_id,
            bot_id: row.bot_id,
            requester_kind: row.requester_kind,
            requester_id: row.requester_id,
            gate,
            subject: row.subject,
            reason: row.reason,
            status,
            decision_note: row.decision_note,
            created_at: row.created_at,
            resolved_at: row.resolved_at,
        })
    }
}

#[derive(Debug, serde::Deserialize, skyzen::FromRow)]
struct RawBotScheduleRow {
    id: String,
    bot_id: String,
    name: String,
    kind: String,
    cron_expression: Option<String>,
    run_at: Option<String>,
    prompt: String,
    enabled: bool,
    next_fire_at: Option<String>,
    last_fired_at: Option<String>,
}

impl From<RawBotScheduleRow> for BotScheduleItem {
    fn from(row: RawBotScheduleRow) -> Self {
        Self {
            id: row.id,
            bot_id: row.bot_id,
            name: row.name,
            kind: row.kind,
            cron_expression: row.cron_expression,
            run_at: row.run_at,
            prompt: row.prompt,
            enabled: row.enabled,
            next_fire_at: row.next_fire_at,
            last_fired_at: row.last_fired_at,
        }
    }
}

#[derive(Debug, serde::Deserialize, skyzen::FromRow)]
struct RawStoreFactRow {
    id: i64,
    bot_id: String,
    fact: String,
    source_time: String,
    source_agent: Option<String>,
    status: String,
    commit_sha: Option<String>,
    error: Option<String>,
    created_at: String,
    finished_at: Option<String>,
}

impl From<RawStoreFactRow> for BotStoreFactRow {
    fn from(row: RawStoreFactRow) -> Self {
        Self {
            id: row.id,
            bot_id: row.bot_id,
            fact: row.fact,
            source_time: row.source_time,
            source_agent: row.source_agent,
            status: row.status,
            commit_sha: row.commit_sha,
            error: row.error,
            created_at: row.created_at,
            finished_at: row.finished_at,
        }
    }
}

#[derive(Debug, serde::Deserialize, skyzen::FromRow)]
struct StringValueRow {
    value: String,
}

impl ChatDb {
    // ── Bot CRUD ────────────────────────────────────────────────

    /// Inserts a new May Bot row.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage or serialization failure.
    pub async fn create_bot(&self, params: NewBotRow<'_>) -> Result<BotRow, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let policy_json = serde_json::to_string(params.approval_policy)
            .map_err(|error| storage_error(format!("failed to encode approval policy: {error}")))?;
        query!(
            self.db,
            "INSERT INTO bots (id, agent_id, name, persona, model, profile_id, enabled, approval_policy_json, last_ts_ms, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?)",
            params.id,
            params.agent_id,
            params.name,
            params.persona,
            params.model,
            params.profile_id,
            params.enabled,
            policy_json.as_str(),
            now.as_str(),
            now.as_str()
        )
        .execute()
        .await?;
        self.get_bot(params.id)
            .await?
            .ok_or_else(|| storage_error("inserted bot row not found"))
    }

    /// Lists all May Bots ordered by creation time ascending.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn list_bots(&self) -> Result<Vec<BotRow>, ChatDbError> {
        let rows = query!(
            self.db,
            "SELECT id, agent_id, name, persona, model, profile_id, enabled, approval_policy_json, last_ts_ms, created_at, updated_at FROM bots ORDER BY created_at ASC, id ASC"
        )
        .fetch_all::<RawBotRow>()
        .await?;
        rows.into_iter().map(BotRow::try_from).collect()
    }

    /// Looks up a bot by its primary `id`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn get_bot(&self, bot_id: &str) -> Result<Option<BotRow>, ChatDbError> {
        let row = query!(
            self.db,
            "SELECT id, agent_id, name, persona, model, profile_id, enabled, approval_policy_json, last_ts_ms, created_at, updated_at FROM bots WHERE id = ?",
            bot_id
        )
        .fetch_optional::<RawBotRow>()
        .await?;
        row.map(BotRow::try_from).transpose()
    }

    /// Looks up a bot by `id`, stable `agent_id`, or exact `name`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn find_bot_by_identifier(
        &self,
        identifier: &str,
    ) -> Result<Option<BotRow>, ChatDbError> {
        let row = query!(
            self.db,
            "SELECT id, agent_id, name, persona, model, profile_id, enabled, approval_policy_json, last_ts_ms, created_at, updated_at FROM bots WHERE id = ? OR agent_id = ? OR name = ? LIMIT 1",
            identifier,
            identifier,
            identifier
        )
        .fetch_optional::<RawBotRow>()
        .await?;
        row.map(BotRow::try_from).transpose()
    }

    /// Updates a bot's configuration in place (live-applied without restart).
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn update_bot(
        &self,
        bot_id: &str,
        patch: UpdateBotRow<'_>,
    ) -> Result<Option<BotRow>, ChatDbError> {
        let Some(existing) = self.get_bot(bot_id).await? else {
            return Ok(None);
        };
        let name = patch.name.unwrap_or(&existing.name);
        let persona = match patch.persona {
            Some(p) => p,
            None => existing.persona.as_deref(),
        };
        let model = match patch.model {
            Some(m) => m,
            None => existing.model.as_deref(),
        };
        let enabled = patch.enabled.unwrap_or(existing.enabled);
        let policy = patch.approval_policy.unwrap_or(&existing.approval_policy);
        let policy_json = serde_json::to_string(policy)
            .map_err(|error| storage_error(format!("failed to encode approval policy: {error}")))?;
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "UPDATE bots SET name = ?, persona = ?, model = ?, enabled = ?, approval_policy_json = ?, updated_at = ? WHERE id = ?",
            name,
            persona,
            model,
            enabled,
            policy_json.as_str(),
            now.as_str(),
            bot_id
        )
        .execute()
        .await?;
        self.get_bot(bot_id).await
    }

    /// Deletes a bot and all of its satellite rows in one transaction.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn delete_bot(&self, bot_id: &str) -> Result<bool, ChatDbError> {
        let existed = self.get_bot(bot_id).await?.is_some();
        if !existed {
            return Ok(false);
        }
        let mut tx = self.db.begin().await?;
        for sql in [
            "DELETE FROM bot_inbox WHERE bot_id = ?",
            "DELETE FROM bot_chat_messages WHERE bot_id = ?",
            "DELETE FROM bot_trace_entries WHERE bot_id = ?",
            "DELETE FROM bot_context_snapshots WHERE bot_id = ?",
            "DELETE FROM bot_delegate_messages WHERE bot_id = ?",
            "DELETE FROM bot_delegates WHERE bot_id = ?",
            "DELETE FROM bot_supervised_sessions WHERE bot_id = ?",
            "DELETE FROM bot_approvals WHERE bot_id = ?",
            "DELETE FROM bot_schedules WHERE bot_id = ?",
            "DELETE FROM bot_store_fact_queue WHERE bot_id = ?",
            "DELETE FROM bots WHERE id = ?",
        ] {
            query!(tx, sql, bot_id).execute().await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    // ── Monotonic Bot Inbox ─────────────────────────────────────

    /// Enqueues an entry into `bot_inbox` with a strictly monotonic per-bot
    /// UTC key (`UNIQUE(bot_id, ts_ms)`) derived from `now`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the bot does not exist or storage fails.
    pub async fn enqueue_bot_inbox_at(
        &self,
        entry: NewBotInboxEntry<'_>,
        now: jiff::Timestamp,
    ) -> Result<BotInboxEntry, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let Some(last) = query!(
            tx,
            "SELECT last_ts_ms AS value FROM bots WHERE id = ?",
            entry.bot_id
        )
        .fetch_optional::<I64ValueRow>()
        .await?
        else {
            return Err(storage_error(format!("bot \"{}\" not found", entry.bot_id)));
        };
        let ts_ms = next_monotonic_ts_ms(now, last.value);
        let ts_utc = format_monotonic_utc(ts_ms);
        query!(
            tx,
            "UPDATE bots SET last_ts_ms = ? WHERE id = ?",
            ts_ms,
            entry.bot_id
        )
        .execute()
        .await?;
        query!(
            tx,
            "INSERT INTO bot_inbox (bot_id, ts_ms, ts_utc, kind, sender_id, sender_name, body, meta_json, consumed_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL)",
            entry.bot_id,
            ts_ms,
            ts_utc.as_str(),
            entry.kind.as_str(),
            entry.sender_id,
            entry.sender_name,
            entry.body,
            entry.meta_json
        )
        .execute()
        .await?;
        let row = query!(
            tx,
            "SELECT id, bot_id, ts_ms, ts_utc, kind, sender_id, sender_name, body, meta_json, consumed_at FROM bot_inbox WHERE bot_id = ? AND ts_ms = ?",
            entry.bot_id,
            ts_ms
        )
        .fetch_one::<RawBotInboxRow>()
        .await?;
        tx.commit().await?;
        BotInboxEntry::try_from(row)
    }

    /// Enqueues an entry into `bot_inbox` stamped with the current wall clock.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn enqueue_bot_inbox(
        &self,
        entry: NewBotInboxEntry<'_>,
    ) -> Result<BotInboxEntry, ChatDbError> {
        self.enqueue_bot_inbox_at(entry, jiff::Timestamp::now())
            .await
    }

    /// Atomically drains and marks consumed all unconsumed `bot_inbox` rows
    /// for `bot_id` in strictly monotonic `ts_ms` order.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn drain_bot_inbox(&self, bot_id: &str) -> Result<Vec<BotInboxEntry>, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let rows = query!(
            tx,
            "SELECT id, bot_id, ts_ms, ts_utc, kind, sender_id, sender_name, body, meta_json, consumed_at FROM bot_inbox WHERE bot_id = ? AND consumed_at IS NULL ORDER BY ts_ms ASC",
            bot_id
        )
        .fetch_all::<RawBotInboxRow>()
        .await?;
        if !rows.is_empty() {
            let now = jiff::Timestamp::now().to_string();
            for row in &rows {
                query!(
                    tx,
                    "UPDATE bot_inbox SET consumed_at = ? WHERE id = ?",
                    now.as_str(),
                    row.id
                )
                .execute()
                .await?;
            }
        }
        tx.commit().await?;
        rows.into_iter().map(BotInboxEntry::try_from).collect()
    }

    /// Counts unconsumed `bot_inbox` entries for `bot_id`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn count_unconsumed_bot_inbox(&self, bot_id: &str) -> Result<usize, ChatDbError> {
        let row = query!(
            self.db,
            "SELECT COUNT(*) AS value FROM bot_inbox WHERE bot_id = ? AND consumed_at IS NULL",
            bot_id
        )
        .fetch_one::<I64ValueRow>()
        .await?;
        Ok(usize::try_from(row.value).unwrap_or(0))
    }

    /// Lists recent `bot_inbox` entries for inspection.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn list_bot_inbox(
        &self,
        bot_id: &str,
        limit: i64,
    ) -> Result<Vec<BotInboxEntry>, ChatDbError> {
        let rows = query!(
            self.db,
            "SELECT id, bot_id, ts_ms, ts_utc, kind, sender_id, sender_name, body, meta_json, consumed_at FROM bot_inbox WHERE bot_id = ? ORDER BY ts_ms DESC LIMIT ?",
            bot_id,
            limit
        )
        .fetch_all::<RawBotInboxRow>()
        .await?;
        let mut items: Vec<BotInboxEntry> = rows
            .into_iter()
            .map(BotInboxEntry::try_from)
            .collect::<Result<_, _>>()?;
        items.reverse();
        Ok(items)
    }

    /// Resolves a `bot_inbox` entry by `(bot_id, source_time)` (matching either
    /// exact `ts_utc` or parsed `ts_ms`) for `store_fact` verbatim quote resolution.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn find_bot_inbox_by_time(
        &self,
        bot_id: &str,
        source_time: &str,
    ) -> Result<Option<BotInboxEntry>, ChatDbError> {
        if let Some(row) = query!(
            self.db,
            "SELECT id, bot_id, ts_ms, ts_utc, kind, sender_id, sender_name, body, meta_json, consumed_at FROM bot_inbox WHERE bot_id = ? AND ts_utc = ?",
            bot_id,
            source_time
        )
        .fetch_optional::<RawBotInboxRow>()
        .await?
        {
            return Ok(Some(BotInboxEntry::try_from(row)?));
        }
        if let Ok(ts_ms) = parse_utc_to_ms(source_time)
            && let Some(row) = query!(
                self.db,
                "SELECT id, bot_id, ts_ms, ts_utc, kind, sender_id, sender_name, body, meta_json, consumed_at FROM bot_inbox WHERE bot_id = ? AND ts_ms = ?",
                bot_id,
                ts_ms
            )
            .fetch_optional::<RawBotInboxRow>()
            .await?
        {
            return Ok(Some(BotInboxEntry::try_from(row)?));
        }
        Ok(None)
    }

    // ── User Chat Messages & Internal Run Trace ─────────────────

    /// Appends a user-visible chat message (`role` = `"user"` or `"bot"`).
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn append_bot_chat_message(
        &self,
        bot_id: &str,
        role: &str,
        content: &str,
        ts_ms: i64,
        ts_utc: &str,
        inbox_id: Option<i64>,
    ) -> Result<BotChatMessage, ChatDbError> {
        query!(
            self.db,
            "INSERT INTO bot_chat_messages (bot_id, role, content, ts_ms, ts_utc, inbox_id) VALUES (?, ?, ?, ?, ?, ?)",
            bot_id,
            role,
            content,
            ts_ms,
            ts_utc,
            inbox_id
        )
        .execute()
        .await?;
        let row = query!(
            self.db,
            "SELECT id, bot_id, role, content, ts_ms, ts_utc FROM bot_chat_messages WHERE bot_id = ? ORDER BY id DESC LIMIT 1",
            bot_id
        )
        .fetch_one::<RawBotChatRow>()
        .await?;
        Ok(BotChatMessage::from(row))
    }

    /// Lists user <-> bot chat messages in chronological order.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn list_bot_chat_messages(
        &self,
        bot_id: &str,
        limit: i64,
    ) -> Result<Vec<BotChatMessage>, ChatDbError> {
        let rows = query!(
            self.db,
            "SELECT id, bot_id, role, content, ts_ms, ts_utc FROM bot_chat_messages WHERE bot_id = ? ORDER BY id DESC LIMIT ?",
            bot_id,
            limit
        )
        .fetch_all::<RawBotChatRow>()
        .await?;
        let mut items: Vec<BotChatMessage> = rows.into_iter().map(BotChatMessage::from).collect();
        items.reverse();
        Ok(items)
    }

    /// Appends an internal run trace entry (plain `AgentEvent::Text`, tool call, etc.).
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn append_bot_trace_entry(
        &self,
        bot_id: &str,
        kind: &str,
        content: &str,
    ) -> Result<BotTraceEntry, ChatDbError> {
        let ts_utc = format_monotonic_utc(jiff::Timestamp::now().as_millisecond());
        query!(
            self.db,
            "INSERT INTO bot_trace_entries (bot_id, kind, content, ts_utc) VALUES (?, ?, ?, ?)",
            bot_id,
            kind,
            content,
            ts_utc.as_str()
        )
        .execute()
        .await?;
        let row = query!(
            self.db,
            "SELECT id, bot_id, kind, content, ts_utc FROM bot_trace_entries WHERE bot_id = ? ORDER BY id DESC LIMIT 1",
            bot_id
        )
        .fetch_one::<RawBotTraceRow>()
        .await?;
        Ok(BotTraceEntry::from(row))
    }

    /// Lists internal run trace entries in chronological order.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn list_bot_trace_entries(
        &self,
        bot_id: &str,
        limit: i64,
    ) -> Result<Vec<BotTraceEntry>, ChatDbError> {
        let rows = query!(
            self.db,
            "SELECT id, bot_id, kind, content, ts_utc FROM bot_trace_entries WHERE bot_id = ? ORDER BY id DESC LIMIT ?",
            bot_id,
            limit
        )
        .fetch_all::<RawBotTraceRow>()
        .await?;
        let mut items: Vec<BotTraceEntry> = rows.into_iter().map(BotTraceEntry::from).collect();
        items.reverse();
        Ok(items)
    }

    // ── Bot Context Snapshot & SeenMemoryLedger ─────────────────

    /// Persists a bot's `AgentCheckpoint` and `SeenMemoryLedger` together.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on encoding or storage failure.
    pub async fn upsert_bot_context_snapshot(
        &self,
        bot_id: &str,
        checkpoint: &aither_agent::AgentCheckpoint,
        seen_memory: &SeenMemoryLedger,
    ) -> Result<(), ChatDbError> {
        let blob = rmp_serde::to_vec_named(checkpoint)
            .map_err(|error| storage_error(format!("failed to encode bot checkpoint: {error}")))?;
        let seen_json = serde_json::to_string(seen_memory).map_err(|error| {
            storage_error(format!("failed to encode seen memory ledger: {error}"))
        })?;
        let updated_at = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "INSERT INTO bot_context_snapshots (bot_id, format_version, checkpoint_blob, seen_memory_json, updated_at) VALUES (?, ?, ?, ?, ?) ON CONFLICT(bot_id) DO UPDATE SET format_version = excluded.format_version, checkpoint_blob = excluded.checkpoint_blob, seen_memory_json = excluded.seen_memory_json, updated_at = excluded.updated_at",
            bot_id,
            CONTEXT_SNAPSHOT_FORMAT,
            blob.as_slice(),
            seen_json.as_str(),
            updated_at.as_str()
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Loads a bot's persisted `AgentCheckpoint` and `SeenMemoryLedger`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when storage fails or format version mismatches.
    pub async fn get_bot_context_snapshot(
        &self,
        bot_id: &str,
    ) -> Result<Option<StoredBotContextSnapshot>, ChatDbError> {
        let Some(row) = query!(
            self.db,
            "SELECT format_version, checkpoint_blob, seen_memory_json, updated_at FROM bot_context_snapshots WHERE bot_id = ?",
            bot_id
        )
        .fetch_optional::<RawBotSnapshotRow>()
        .await?
        else {
            return Ok(None);
        };
        if row.format_version != CONTEXT_SNAPSHOT_FORMAT {
            return Err(storage_error(format!(
                "bot context snapshot for {bot_id} has format {}, expected {CONTEXT_SNAPSHOT_FORMAT}",
                row.format_version
            )));
        }
        let checkpoint = rmp_serde::from_slice::<aither_agent::AgentCheckpoint>(
            &row.checkpoint_blob,
        )
        .map_err(|error| {
            storage_error(format!(
                "failed to decode bot checkpoint for {bot_id}: {error}"
            ))
        })?;
        let seen_memory =
            serde_json::from_str::<SeenMemoryLedger>(&row.seen_memory_json).unwrap_or_default();
        Ok(Some(StoredBotContextSnapshot {
            checkpoint,
            seen_memory,
            updated_at: row.updated_at,
        }))
    }

    // ── Delegated Workers & Timestamped Transcript ──────────────

    /// Inserts a newly spawned delegated worker row.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn create_bot_delegate(
        &self,
        params: NewBotDelegate<'_>,
    ) -> Result<BotDelegateSummary, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let worktrees_json = serde_json::to_string(params.granted_worktrees)
            .map_err(|error| storage_error(format!("failed to encode worktrees: {error}")))?;
        query!(
            self.db,
            "INSERT INTO bot_delegates (agent_id, bot_id, name, definition, work_dir, granted_worktrees_json, status, active_command, elapsed_ms, prompt_tokens, completion_tokens, checkpoint_blob, checkpoint_format, last_output, last_ts_ms, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, NULL, 0, 0, 0, NULL, NULL, NULL, 0, ?, ?)",
            params.agent_id,
            params.bot_id,
            params.name,
            params.definition,
            params.work_dir,
            worktrees_json.as_str(),
            BotDelegateStatus::Running.as_str(),
            now.as_str(),
            now.as_str()
        )
        .execute()
        .await?;
        self.get_bot_delegate(params.agent_id)
            .await?
            .ok_or_else(|| storage_error("inserted bot delegate not found"))
    }

    /// Gets one delegated worker summary by `agent_id`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn get_bot_delegate(
        &self,
        agent_id: &str,
    ) -> Result<Option<BotDelegateSummary>, ChatDbError> {
        let row = query!(
            self.db,
            "SELECT agent_id, bot_id, name, definition, work_dir, granted_worktrees_json, status, active_command, elapsed_ms, prompt_tokens, completion_tokens, last_output, created_at, updated_at FROM bot_delegates WHERE agent_id = ?",
            agent_id
        )
        .fetch_optional::<RawBotDelegateRow>()
        .await?;
        row.map(BotDelegateSummary::try_from).transpose()
    }

    /// Lists all delegated workers belonging to `bot_id`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn list_bot_delegates(
        &self,
        bot_id: &str,
    ) -> Result<Vec<BotDelegateSummary>, ChatDbError> {
        let rows = query!(
            self.db,
            "SELECT agent_id, bot_id, name, definition, work_dir, granted_worktrees_json, status, active_command, elapsed_ms, prompt_tokens, completion_tokens, last_output, created_at, updated_at FROM bot_delegates WHERE bot_id = ? ORDER BY updated_at DESC",
            bot_id
        )
        .fetch_all::<RawBotDelegateRow>()
        .await?;
        rows.into_iter().map(BotDelegateSummary::try_from).collect()
    }

    /// Updates a delegated worker's live metrics, status, active command, and optional checkpoint.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage or encoding failure.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_bot_delegate_state(
        &self,
        agent_id: &str,
        status: BotDelegateStatus,
        active_command: Option<&str>,
        elapsed_ms: i64,
        prompt_tokens: i64,
        completion_tokens: i64,
        last_output: Option<&str>,
        checkpoint: Option<&aither_agent::AgentCheckpoint>,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        if let Some(cp) = checkpoint {
            let blob = rmp_serde::to_vec_named(cp).map_err(|error| {
                storage_error(format!("failed to encode delegate checkpoint: {error}"))
            })?;
            query!(
                self.db,
                "UPDATE bot_delegates SET status = ?, active_command = ?, elapsed_ms = ?, prompt_tokens = ?, completion_tokens = ?, last_output = COALESCE(?, last_output), checkpoint_blob = ?, checkpoint_format = ?, updated_at = ? WHERE agent_id = ?",
                status.as_str(),
                active_command,
                elapsed_ms,
                prompt_tokens,
                completion_tokens,
                last_output,
                blob.as_slice(),
                CONTEXT_SNAPSHOT_FORMAT,
                now.as_str(),
                agent_id
            )
            .execute()
            .await?;
        } else {
            query!(
                self.db,
                "UPDATE bot_delegates SET status = ?, active_command = ?, elapsed_ms = ?, prompt_tokens = ?, completion_tokens = ?, last_output = COALESCE(?, last_output), updated_at = ? WHERE agent_id = ?",
                status.as_str(),
                active_command,
                elapsed_ms,
                prompt_tokens,
                completion_tokens,
                last_output,
                now.as_str(),
                agent_id
            )
            .execute()
            .await?;
        }
        Ok(())
    }

    /// Loads a delegated worker's persisted `AgentCheckpoint` (for `resume_agent`).
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage or decode failure.
    pub async fn get_bot_delegate_checkpoint(
        &self,
        agent_id: &str,
    ) -> Result<Option<aither_agent::AgentCheckpoint>, ChatDbError> {
        let Some(row) = query!(
            self.db,
            "SELECT checkpoint_blob, checkpoint_format FROM bot_delegates WHERE agent_id = ?",
            agent_id
        )
        .fetch_optional::<RawDelegateCheckpointRow>()
        .await?
        else {
            return Ok(None);
        };
        let (Some(blob), Some(format)) = (row.checkpoint_blob, row.checkpoint_format) else {
            return Ok(None);
        };
        if format != CONTEXT_SNAPSHOT_FORMAT {
            return Err(storage_error(format!(
                "delegate checkpoint for {agent_id} has format {format}, expected {CONTEXT_SNAPSHOT_FORMAT}"
            )));
        }
        let cp =
            rmp_serde::from_slice::<aither_agent::AgentCheckpoint>(&blob).map_err(|error| {
                storage_error(format!(
                    "failed to decode delegate checkpoint for {agent_id}: {error}"
                ))
            })?;
        Ok(Some(cp))
    }

    /// Appends a timestamped message to a delegated worker's transcript with a
    /// strictly monotonic UTC key so `read_agent` and `store_fact` can cite it.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn append_bot_delegate_message(
        &self,
        agent_id: &str,
        bot_id: &str,
        role: &str,
        content: &str,
    ) -> Result<BotDelegateMessageRow, ChatDbError> {
        let now = jiff::Timestamp::now();
        let mut tx = self.db.begin().await?;
        let last = query!(
            tx,
            "SELECT last_ts_ms AS value FROM bot_delegates WHERE agent_id = ?",
            agent_id
        )
        .fetch_optional::<I64ValueRow>()
        .await?
        .map_or(0, |row| row.value);
        let ts_ms = next_monotonic_ts_ms(now, last);
        let ts_utc = format_monotonic_utc(ts_ms);
        query!(
            tx,
            "UPDATE bot_delegates SET last_ts_ms = ? WHERE agent_id = ?",
            ts_ms,
            agent_id
        )
        .execute()
        .await?;
        query!(
            tx,
            "INSERT INTO bot_delegate_messages (agent_id, bot_id, role, content, ts_ms, ts_utc) VALUES (?, ?, ?, ?, ?, ?)",
            agent_id,
            bot_id,
            role,
            content,
            ts_ms,
            ts_utc.as_str()
        )
        .execute()
        .await?;
        let row = query!(
            tx,
            "SELECT id, agent_id, bot_id, role, content, ts_ms, ts_utc FROM bot_delegate_messages WHERE agent_id = ? AND ts_ms = ?",
            agent_id,
            ts_ms
        )
        .fetch_one::<RawBotDelegateMessageRow>()
        .await?;
        tx.commit().await?;
        Ok(BotDelegateMessageRow::from(row))
    }

    /// Lists the last `tail` messages of a delegated worker in chronological order.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn list_bot_delegate_messages(
        &self,
        agent_id: &str,
        tail: i64,
    ) -> Result<Vec<BotDelegateMessageRow>, ChatDbError> {
        let rows = query!(
            self.db,
            "SELECT id, agent_id, bot_id, role, content, ts_ms, ts_utc FROM bot_delegate_messages WHERE agent_id = ? ORDER BY ts_ms DESC LIMIT ?",
            agent_id,
            tail
        )
        .fetch_all::<RawBotDelegateMessageRow>()
        .await?;
        let mut items: Vec<BotDelegateMessageRow> =
            rows.into_iter().map(BotDelegateMessageRow::from).collect();
        items.reverse();
        Ok(items)
    }

    /// Resolves a delegated worker message by `(bot_id, agent_id, source_time)`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn find_bot_delegate_message_by_time(
        &self,
        bot_id: &str,
        agent_id: &str,
        source_time: &str,
    ) -> Result<Option<BotDelegateMessageRow>, ChatDbError> {
        if let Some(row) = query!(
            self.db,
            "SELECT id, agent_id, bot_id, role, content, ts_ms, ts_utc FROM bot_delegate_messages WHERE bot_id = ? AND agent_id = ? AND ts_utc = ?",
            bot_id,
            agent_id,
            source_time
        )
        .fetch_optional::<RawBotDelegateMessageRow>()
        .await?
        {
            return Ok(Some(BotDelegateMessageRow::from(row)));
        }
        if let Ok(ts_ms) = parse_utc_to_ms(source_time)
            && let Some(row) = query!(
                self.db,
                "SELECT id, agent_id, bot_id, role, content, ts_ms, ts_utc FROM bot_delegate_messages WHERE bot_id = ? AND agent_id = ? AND ts_ms = ?",
                bot_id,
                agent_id,
                ts_ms
            )
            .fetch_optional::<RawBotDelegateMessageRow>()
            .await?
        {
            return Ok(Some(BotDelegateMessageRow::from(row)));
        }
        Ok(None)
    }

    // ── Supervised Sessions ─────────────────────────────────────

    /// Records that `bot_id` supervises `session_id`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn register_bot_supervised_session(
        &self,
        bot_id: &str,
        session_id: &str,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "INSERT INTO bot_supervised_sessions (session_id, bot_id, created_at) VALUES (?, ?, ?) ON CONFLICT(session_id) DO UPDATE SET bot_id = excluded.bot_id",
            session_id,
            bot_id,
            now.as_str()
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Returns the supervising `bot_id` for `session_id`, if any.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn supervising_bot_for_session(
        &self,
        session_id: &str,
    ) -> Result<Option<String>, ChatDbError> {
        let row = query!(
            self.db,
            "SELECT bot_id AS value FROM bot_supervised_sessions WHERE session_id = ?",
            session_id
        )
        .fetch_optional::<StringValueRow>()
        .await?;
        Ok(row.map(|r| r.value))
    }

    /// Lists session ids supervised by `bot_id`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn list_bot_supervised_sessions(
        &self,
        bot_id: &str,
    ) -> Result<Vec<String>, ChatDbError> {
        let rows = query!(
            self.db,
            "SELECT session_id AS value FROM bot_supervised_sessions WHERE bot_id = ? ORDER BY created_at DESC",
            bot_id
        )
        .fetch_all::<StringValueRow>()
        .await?;
        Ok(rows.into_iter().map(|r| r.value).collect())
    }

    // ── Bot Approvals ───────────────────────────────────────────

    /// Persists a newly created bot-routed approval request.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn create_bot_approval(
        &self,
        params: NewBotApproval<'_>,
    ) -> Result<BotApprovalItem, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "INSERT INTO bot_approvals (request_id, bot_id, requester_kind, requester_id, gate, subject, reason, status, decision_note, created_at, resolved_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL)",
            params.request_id,
            params.bot_id,
            params.requester_kind,
            params.requester_id,
            params.gate.as_str(),
            params.subject,
            params.reason,
            params.status.as_str(),
            params.decision_note,
            now.as_str()
        )
        .execute()
        .await?;
        self.get_bot_approval(params.request_id)
            .await?
            .ok_or_else(|| storage_error("inserted bot approval not found"))
    }

    /// Looks up one bot approval by `request_id`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn get_bot_approval(
        &self,
        request_id: &str,
    ) -> Result<Option<BotApprovalItem>, ChatDbError> {
        let row = query!(
            self.db,
            "SELECT request_id, bot_id, requester_kind, requester_id, gate, subject, reason, status, decision_note, created_at, resolved_at FROM bot_approvals WHERE request_id = ?",
            request_id
        )
        .fetch_optional::<RawBotApprovalRow>()
        .await?;
        row.map(BotApprovalItem::try_from).transpose()
    }

    /// Updates a bot approval's status and note.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn update_bot_approval_status(
        &self,
        request_id: &str,
        status: BotApprovalStatus,
        decision_note: Option<&str>,
    ) -> Result<Option<BotApprovalItem>, ChatDbError> {
        let resolved_at = matches!(
            status,
            BotApprovalStatus::Approved | BotApprovalStatus::Denied
        )
        .then(|| jiff::Timestamp::now().to_string());
        query!(
            self.db,
            "UPDATE bot_approvals SET status = ?, decision_note = COALESCE(?, decision_note), resolved_at = COALESCE(?, resolved_at) WHERE request_id = ?",
            status.as_str(),
            decision_note,
            resolved_at.as_deref(),
            request_id
        )
        .execute()
        .await?;
        self.get_bot_approval(request_id).await
    }

    /// Lists approvals for `bot_id` (pending/escalated first, newest first).
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn list_bot_approvals(
        &self,
        bot_id: &str,
    ) -> Result<Vec<BotApprovalItem>, ChatDbError> {
        let rows = query!(
            self.db,
            "SELECT request_id, bot_id, requester_kind, requester_id, gate, subject, reason, status, decision_note, created_at, resolved_at FROM bot_approvals WHERE bot_id = ? ORDER BY created_at DESC LIMIT 100",
            bot_id
        )
        .fetch_all::<RawBotApprovalRow>()
        .await?;
        rows.into_iter().map(BotApprovalItem::try_from).collect()
    }

    // ── Bot Schedules ───────────────────────────────────────────

    /// Creates a bot schedule (`once` or `cron`).
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn create_bot_schedule(
        &self,
        params: NewBotSchedule<'_>,
    ) -> Result<BotScheduleItem, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "INSERT INTO bot_schedules (id, bot_id, name, kind, cron_expression, run_at, prompt, enabled, next_fire_at, last_fired_at, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, 1, ?, NULL, ?, ?)",
            params.id,
            params.bot_id,
            params.name,
            params.kind,
            params.cron_expression,
            params.run_at,
            params.prompt,
            params.next_fire_at,
            now.as_str(),
            now.as_str()
        )
        .execute()
        .await?;
        self.get_bot_schedule(params.id)
            .await?
            .ok_or_else(|| storage_error("inserted bot schedule not found"))
    }

    /// Gets one bot schedule by id.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn get_bot_schedule(
        &self,
        schedule_id: &str,
    ) -> Result<Option<BotScheduleItem>, ChatDbError> {
        let row = query!(
            self.db,
            "SELECT id, bot_id, name, kind, cron_expression, run_at, prompt, enabled, next_fire_at, last_fired_at FROM bot_schedules WHERE id = ?",
            schedule_id
        )
        .fetch_optional::<RawBotScheduleRow>()
        .await?;
        Ok(row.map(BotScheduleItem::from))
    }

    /// Lists all schedules owned by `bot_id`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn list_bot_schedules(
        &self,
        bot_id: &str,
    ) -> Result<Vec<BotScheduleItem>, ChatDbError> {
        let rows = query!(
            self.db,
            "SELECT id, bot_id, name, kind, cron_expression, run_at, prompt, enabled, next_fire_at, last_fired_at FROM bot_schedules WHERE bot_id = ? ORDER BY created_at ASC",
            bot_id
        )
        .fetch_all::<RawBotScheduleRow>()
        .await?;
        Ok(rows.into_iter().map(BotScheduleItem::from).collect())
    }

    /// Lists enabled schedules whose `next_fire_at <= now_iso`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn list_due_bot_schedules(
        &self,
        now_iso: &str,
    ) -> Result<Vec<BotScheduleItem>, ChatDbError> {
        let rows = query!(
            self.db,
            "SELECT id, bot_id, name, kind, cron_expression, run_at, prompt, enabled, next_fire_at, last_fired_at FROM bot_schedules WHERE enabled = 1 AND next_fire_at IS NOT NULL AND next_fire_at <= ? ORDER BY next_fire_at ASC",
            now_iso
        )
        .fetch_all::<RawBotScheduleRow>()
        .await?;
        Ok(rows.into_iter().map(BotScheduleItem::from).collect())
    }

    /// Advances a fired schedule's `last_fired_at`, `next_fire_at`, and `enabled`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn advance_bot_schedule(
        &self,
        schedule_id: &str,
        fired_at: &str,
        next_fire_at: Option<&str>,
        enabled: bool,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "UPDATE bot_schedules SET last_fired_at = ?, next_fire_at = ?, enabled = ?, updated_at = ? WHERE id = ?",
            fired_at,
            next_fire_at,
            enabled,
            now.as_str(),
            schedule_id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Deletes a bot schedule by id (scoped to `bot_id`).
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn delete_bot_schedule(
        &self,
        bot_id: &str,
        schedule_id: &str,
    ) -> Result<bool, ChatDbError> {
        let exists = self
            .get_bot_schedule(schedule_id)
            .await?
            .is_some_and(|s| s.bot_id == bot_id);
        if !exists {
            return Ok(false);
        }
        query!(
            self.db,
            "DELETE FROM bot_schedules WHERE id = ? AND bot_id = ?",
            schedule_id,
            bot_id
        )
        .execute()
        .await?;
        Ok(true)
    }

    // ── Serial store_fact Queue ─────────────────────────────────

    /// Enqueues a `store_fact` request into the bot's durable FIFO queue.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn enqueue_bot_store_fact(
        &self,
        bot_id: &str,
        fact: &str,
        source_time: &str,
        source_agent: Option<&str>,
    ) -> Result<BotStoreFactRow, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "INSERT INTO bot_store_fact_queue (bot_id, fact, source_time, source_agent, status, created_at) VALUES (?, ?, ?, ?, 'pending', ?)",
            bot_id,
            fact,
            source_time,
            source_agent,
            now.as_str()
        )
        .execute()
        .await?;
        let row = query!(
            self.db,
            "SELECT id, bot_id, fact, source_time, source_agent, status, commit_sha, error, created_at, finished_at FROM bot_store_fact_queue WHERE bot_id = ? ORDER BY id DESC LIMIT 1",
            bot_id
        )
        .fetch_one::<RawStoreFactRow>()
        .await?;
        Ok(BotStoreFactRow::from(row))
    }

    /// Returns the next pending `store_fact` item in FIFO order for `bot_id`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn next_pending_bot_store_fact(
        &self,
        bot_id: &str,
    ) -> Result<Option<BotStoreFactRow>, ChatDbError> {
        let row = query!(
            self.db,
            "SELECT id, bot_id, fact, source_time, source_agent, status, commit_sha, error, created_at, finished_at FROM bot_store_fact_queue WHERE bot_id = ? AND status = 'pending' ORDER BY id ASC LIMIT 1",
            bot_id
        )
        .fetch_optional::<RawStoreFactRow>()
        .await?;
        Ok(row.map(BotStoreFactRow::from))
    }

    /// Marks a `store_fact` queue row completed or failed.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn finish_bot_store_fact(
        &self,
        id: i64,
        commit_sha: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), ChatDbError> {
        let status = if error.is_some() { "failed" } else { "done" };
        let finished_at = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "UPDATE bot_store_fact_queue SET status = ?, commit_sha = ?, error = ?, finished_at = ? WHERE id = ?",
            status,
            commit_sha,
            error,
            finished_at.as_str(),
            id
        )
        .execute()
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn bot_inbox_assigns_strictly_monotonic_utc_keys() {
        let dir = tempdir().expect("tempdir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db");
        let policy = BotApprovalPolicy::default();
        db.create_bot(NewBotRow {
            id: "b1",
            agent_id: "swift-amber-fox",
            name: "Personal Bridge",
            persona: None,
            model: None,
            profile_id: None,
            enabled: true,
            approval_policy: &policy,
        })
        .await
        .expect("create bot");

        let t0: jiff::Timestamp = "2026-06-15T12:00:00.800Z".parse().expect("ts");
        let e1 = db
            .enqueue_bot_inbox_at(
                NewBotInboxEntry {
                    bot_id: "b1",
                    kind: BotInboxKind::UserMessage,
                    sender_id: Some("user"),
                    sender_name: Some("User"),
                    body: "你好 / こんにちは / 안녕하세요",
                    meta_json: None,
                },
                t0,
            )
            .await
            .expect("e1");
        let e2 = db
            .enqueue_bot_inbox_at(
                NewBotInboxEntry {
                    bot_id: "b1",
                    kind: BotInboxKind::UserMessage,
                    sender_id: Some("user"),
                    sender_name: Some("User"),
                    body: "same-second burst",
                    meta_json: None,
                },
                t0,
            )
            .await
            .expect("e2");
        let earlier: jiff::Timestamp = "2026-06-15T11:59:50Z".parse().expect("ts");
        let e3 = db
            .enqueue_bot_inbox_at(
                NewBotInboxEntry {
                    bot_id: "b1",
                    kind: BotInboxKind::ScheduleWake,
                    sender_id: None,
                    sender_name: None,
                    body: "clock stepped back",
                    meta_json: None,
                },
                earlier,
            )
            .await
            .expect("e3");
        let t1: jiff::Timestamp = "2026-06-15T12:00:01.200Z".parse().expect("ts");
        let e4 = db
            .enqueue_bot_inbox_at(
                NewBotInboxEntry {
                    bot_id: "b1",
                    kind: BotInboxKind::AgentMessage,
                    sender_id: Some("session-1"),
                    sender_name: Some("Session 1"),
                    body: "next whole second",
                    meta_json: None,
                },
                t1,
            )
            .await
            .expect("e4");

        assert_eq!(e1.ts_utc, "2026-06-15T12:00:00Z");
        assert_eq!(e2.ts_utc, "2026-06-15T12:00:00.001Z");
        assert_eq!(e3.ts_utc, "2026-06-15T12:00:00.002Z");
        assert_eq!(e4.ts_utc, "2026-06-15T12:00:01Z");
        assert!(e1.ts_ms < e2.ts_ms && e2.ts_ms < e3.ts_ms && e3.ts_ms < e4.ts_ms);

        let found = db
            .find_bot_inbox_by_time("b1", &e1.ts_utc)
            .await
            .expect("lookup")
            .expect("present");
        assert_eq!(found.body, "你好 / こんにちは / 안녕하세요");

        let drained = db.drain_bot_inbox("b1").await.expect("drain");
        assert_eq!(drained.len(), 4);
        assert_eq!(db.count_unconsumed_bot_inbox("b1").await.expect("count"), 0);
    }
}
