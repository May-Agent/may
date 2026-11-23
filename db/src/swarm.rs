//! SQLite persistence for `[experimental] swarm` coordination boards,
//! monotonic message sequences, per-session read cursors, and wake loop state.

use std::collections::BTreeSet;

use may_protocol::xml::XmlBlock;

use super::{
    ChatDb, ChatDbError,
    common::{I64ValueRow, overflow_error, storage_error, to_i64_from_u64},
    query,
};

/// Active coordination board for a normalized worktree root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwarmBoardRow {
    /// Canonical worktree path owning this board.
    pub workdir: String,
    /// Monotonically increasing generation counter across board recreations.
    pub board_epoch: u64,
    /// ISO-8601 creation timestamp.
    pub created_at: String,
}

/// Category of an entry posted to a swarm board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwarmMessageKind {
    /// Agent-authored message via `board post`.
    Post,
    /// Membership / board lifecycle event recorded by the daemon.
    Lifecycle,
    /// Pull-request or merge-queue transition recorded by `PrStore`.
    Pr,
}

impl SwarmMessageKind {
    /// Stored text representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Post => "post",
            Self::Lifecycle => "lifecycle",
            Self::Pr => "pr",
        }
    }

    fn parse(raw: &str) -> Result<Self, ChatDbError> {
        match raw {
            "post" => Ok(Self::Post),
            "lifecycle" => Ok(Self::Lifecycle),
            "pr" => Ok(Self::Pr),
            other => Err(storage_error(format!(
                "unknown swarm message kind '{other}'"
            ))),
        }
    }
}

/// Persisted message on a swarm board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwarmMessageRow {
    /// Canonical worktree path owning the board.
    pub workdir: String,
    /// Board generation when the message was written.
    pub board_epoch: u64,
    /// Monotonic per-board sequence number starting at `1`.
    pub seq: u64,
    /// Originating session id, or `None` for daemon lifecycle entries.
    pub session_id: Option<String>,
    /// Display name of the sender at post time.
    pub sender: String,
    /// Entry category.
    pub kind: SwarmMessageKind,
    /// Raw UTF-8 message body.
    pub body: String,
    /// ISO-8601 timestamp.
    pub created_at: String,
}

/// Parameters for appending a new entry onto an active board.
#[derive(Debug, Clone, Copy)]
pub struct NewSwarmMessage<'a> {
    /// Target board's normalized worktree path.
    pub workdir: &'a str,
    /// Originating session id (`None` for daemon-generated entries).
    pub session_id: Option<&'a str>,
    /// Sender label stored with the entry.
    pub sender: &'a str,
    /// Entry category.
    pub kind: SwarmMessageKind,
    /// Message text.
    pub body: &'a str,
}

/// Which slice of a board `read_swarm_messages` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwarmReadMode {
    /// Entries strictly after the caller's persisted cursor, advancing it.
    Unread,
    /// Entries with `seq >= since`.
    Since(u64),
    /// Entries starting from `seq = 1`.
    All,
}

/// Slice returned from reading an active board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwarmReadResult {
    /// Current generation of the board.
    pub board_epoch: u64,
    /// Entries in ascending `seq` order, bounded by `limit`.
    pub messages: Vec<SwarmMessageRow>,
    /// First sequence number still beyond this page, when truncated by `limit`.
    pub next_seq: Option<u64>,
    /// Highest sequence number currently on the board (`0` when empty).
    pub max_seq: u64,
}

/// Unread range summary checked on active model requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwarmUnreadStatus {
    /// Active board generation.
    pub board_epoch: u64,
    /// Highest `seq` the session has read (`0` when none).
    pub cursor: u64,
    /// Highest `seq` currently on the board (`0` when empty).
    pub max_seq: u64,
}

/// Per-session state persisted across daemon restarts for swarm alerts and wake caps.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SwarmSessionStateRow {
    /// Workdir whose protocol alert was last delivered to the warm context.
    pub told_workdir: Option<String>,
    /// `board_epoch` last delivered to the warm context (`0` = none).
    pub told_epoch: u64,
    /// Unattended `@mention` wakes since the user last sent a prompt.
    pub consecutive_mention_wakes: u32,
}

#[derive(Debug, serde::Deserialize, skyzen::FromRow)]
struct RawBoardRow {
    workdir: String,
    board_epoch: i64,
    created_at: String,
}

impl RawBoardRow {
    fn into_model(self) -> Result<SwarmBoardRow, ChatDbError> {
        Ok(SwarmBoardRow {
            workdir: self.workdir,
            board_epoch: u64::try_from(self.board_epoch)
                .map_err(|e| overflow_error("swarm_boards.board_epoch", &e))?,
            created_at: self.created_at,
        })
    }
}

#[derive(Debug, serde::Deserialize, skyzen::FromRow)]
struct RawMessageRow {
    workdir: String,
    board_epoch: i64,
    seq: i64,
    session_id: Option<String>,
    sender: String,
    kind: String,
    body: String,
    created_at: String,
}

impl RawMessageRow {
    fn into_model(self) -> Result<SwarmMessageRow, ChatDbError> {
        Ok(SwarmMessageRow {
            workdir: self.workdir,
            board_epoch: u64::try_from(self.board_epoch)
                .map_err(|e| overflow_error("swarm_messages.board_epoch", &e))?,
            seq: u64::try_from(self.seq).map_err(|e| overflow_error("swarm_messages.seq", &e))?,
            session_id: self.session_id,
            sender: self.sender,
            kind: SwarmMessageKind::parse(&self.kind)?,
            body: self.body,
            created_at: self.created_at,
        })
    }
}

#[derive(Debug, serde::Deserialize, skyzen::FromRow)]
struct RawSessionStateRow {
    told_workdir: Option<String>,
    told_epoch: i64,
    consecutive_mention_wakes: i64,
}

#[derive(Debug, serde::Deserialize, skyzen::FromRow)]
struct StringValueRow {
    value: String,
}

#[derive(Debug, serde::Deserialize, skyzen::FromRow)]
struct PendingXmlRow {
    id: i64,
    hidden_context_xml: String,
}

/// Action to take on a pending queue entry when a board dissolves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DissolvedSteerAction {
    /// Leave the entry untouched (belongs to another board or feature).
    Keep,
    /// Drop an unconsumed lifecycle or PR notice for the dissolved board.
    Drop,
    /// Keep an `@mention` entry but rewrite its hidden XML with a dissolution note.
    RewriteXml(XmlBlock),
}

impl ChatDb {
    /// Non-archived session ids that have started real work: at least one
    /// committed message, an unconsumed pending entry, or a non-idle runtime.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the query fails.
    pub async fn working_session_ids(&self) -> Result<BTreeSet<String>, ChatDbError> {
        let rows = query!(
            self.db,
            "SELECT id AS value FROM sessions WHERE archived = 0 AND (
                message_count > 0
                OR EXISTS (SELECT 1 FROM messages WHERE messages.session_id = sessions.id)
                OR EXISTS (SELECT 1 FROM session_pending_message_entries WHERE session_pending_message_entries.session_id = sessions.id AND state != 'committed')
                OR EXISTS (SELECT 1 FROM session_runtimes WHERE session_runtimes.session_id = sessions.id AND state != 'idle')
            )"
        )
        .fetch_all::<StringValueRow>()
        .await?;
        Ok(rows.into_iter().map(|row| row.value).collect())
    }

    /// Opens a swarm board for `workdir` when absent, incrementing its durable
    /// `board_epoch` across recreations. Returns `(board, created)`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn open_or_get_swarm_board(
        &self,
        workdir: &str,
    ) -> Result<(SwarmBoardRow, bool), ChatDbError> {
        let mut tx = self.db.begin().await?;
        if let Some(existing) = query!(
            tx,
            "SELECT workdir, board_epoch, created_at FROM swarm_boards WHERE workdir = ?",
            workdir
        )
        .fetch_optional::<RawBoardRow>()
        .await?
        {
            tx.commit().await?;
            return Ok((existing.into_model()?, false));
        }

        query!(
            tx,
            "INSERT INTO swarm_epochs (workdir, last_epoch) VALUES (?, 1)
             ON CONFLICT(workdir) DO UPDATE SET last_epoch = swarm_epochs.last_epoch + 1",
            workdir
        )
        .execute()
        .await?;

        let epoch_row = query!(
            tx,
            "SELECT last_epoch AS value FROM swarm_epochs WHERE workdir = ?",
            workdir
        )
        .fetch_one::<I64ValueRow>()
        .await?;

        let now = jiff::Timestamp::now().to_string();
        query!(
            tx,
            "INSERT INTO swarm_boards (workdir, board_epoch, created_at) VALUES (?, ?, ?)",
            workdir,
            epoch_row.value,
            now.as_str()
        )
        .execute()
        .await?;
        tx.commit().await?;

        let board_epoch =
            u64::try_from(epoch_row.value).map_err(|e| overflow_error("board_epoch", &e))?;
        Ok((
            SwarmBoardRow {
                workdir: workdir.to_string(),
                board_epoch,
                created_at: now,
            },
            true,
        ))
    }

    /// Active board for `workdir`, if any.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn get_swarm_board(
        &self,
        workdir: &str,
    ) -> Result<Option<SwarmBoardRow>, ChatDbError> {
        query!(
            self.db,
            "SELECT workdir, board_epoch, created_at FROM swarm_boards WHERE workdir = ?",
            workdir
        )
        .fetch_optional::<RawBoardRow>()
        .await?
        .map(RawBoardRow::into_model)
        .transpose()
    }

    /// All currently active swarm boards.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn list_swarm_boards(&self) -> Result<Vec<SwarmBoardRow>, ChatDbError> {
        let rows = query!(
            self.db,
            "SELECT workdir, board_epoch, created_at FROM swarm_boards ORDER BY workdir ASC"
        )
        .fetch_all::<RawBoardRow>()
        .await?;
        rows.into_iter().map(RawBoardRow::into_model).collect()
    }

    /// Destroys the active board for `workdir`, deleting its messages and read
    /// cursors while inspecting unconsumed pending queue entries through
    /// `inspect_pending_xml` to drop stale notices or annotate dissolved
    /// `@mention` steers. Returns the dissolved `board_epoch` when a board
    /// existed, plus session ids whose pending queues were modified.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn destroy_swarm_board(
        &self,
        workdir: &str,
        inspect_pending_xml: impl Fn(u64, &str) -> DissolvedSteerAction,
    ) -> Result<Option<(u64, BTreeSet<String>)>, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let Some(existing) = query!(
            tx,
            "SELECT workdir, board_epoch, created_at FROM swarm_boards WHERE workdir = ?",
            workdir
        )
        .fetch_optional::<RawBoardRow>()
        .await?
        else {
            tx.commit().await?;
            return Ok(None);
        };
        let board = existing.into_model()?;
        query!(tx, "DELETE FROM swarm_boards WHERE workdir = ?", workdir)
            .execute()
            .await?;
        query!(tx, "DELETE FROM swarm_messages WHERE workdir = ?", workdir)
            .execute()
            .await?;
        query!(tx, "DELETE FROM swarm_cursors WHERE workdir = ?", workdir)
            .execute()
            .await?;

        let candidates = query!(
            tx,
            "SELECT id, hidden_context_xml FROM session_pending_message_entries
             WHERE state = 'pending' AND hidden_context_xml IS NOT NULL
               AND (hidden_context_xml LIKE '%<board-notice %' OR hidden_context_xml LIKE '%<board-message %')"
        )
        .fetch_all::<PendingXmlRow>()
        .await?;

        let mut touched_sessions = BTreeSet::new();
        for row in candidates {
            match inspect_pending_xml(board.board_epoch, &row.hidden_context_xml) {
                DissolvedSteerAction::Keep => {}
                DissolvedSteerAction::Drop => {
                    if let Some(owner) = query!(
                        tx,
                        "SELECT session_id AS value FROM session_pending_message_entries WHERE id = ?",
                        row.id
                    )
                    .fetch_optional::<StringValueRow>()
                    .await?
                    {
                        touched_sessions.insert(owner.value);
                    }
                    query!(
                        tx,
                        "DELETE FROM session_pending_message_attachments WHERE message_id = ?",
                        row.id
                    )
                    .execute()
                    .await?;
                    query!(
                        tx,
                        "DELETE FROM session_pending_message_entries WHERE id = ? AND state = 'pending'",
                        row.id
                    )
                    .execute()
                    .await?;
                }
                DissolvedSteerAction::RewriteXml(updated) => {
                    if let Some(owner) = query!(
                        tx,
                        "SELECT session_id AS value FROM session_pending_message_entries WHERE id = ?",
                        row.id
                    )
                    .fetch_optional::<StringValueRow>()
                    .await?
                    {
                        touched_sessions.insert(owner.value);
                    }
                    query!(
                        tx,
                        "UPDATE session_pending_message_entries SET hidden_context_xml = ? WHERE id = ? AND state = 'pending'",
                        updated.into_inner(),
                        row.id
                    )
                    .execute()
                    .await?;
                }
            }
        }

        tx.commit().await?;
        Ok(Some((board.board_epoch, touched_sessions)))
    }

    /// Appends a message to `msg.workdir`'s active board with the next
    /// monotonic `seq`. Returns `None` if no board is active for `msg.workdir`.
    ///
    /// When `msg.session_id` has caught up through `seq - 1`, its cursor
    /// advances to `seq` so an agent's own post does not appear as unread to
    /// itself while preserving any earlier unread gap.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn append_swarm_message(
        &self,
        msg: NewSwarmMessage<'_>,
    ) -> Result<Option<SwarmMessageRow>, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let Some(board_row) = query!(
            tx,
            "SELECT workdir, board_epoch, created_at FROM swarm_boards WHERE workdir = ?",
            msg.workdir
        )
        .fetch_optional::<RawBoardRow>()
        .await?
        else {
            tx.commit().await?;
            return Ok(None);
        };
        let board = board_row.into_model()?;
        let board_epoch_i64 = to_i64_from_u64(board.board_epoch, "board_epoch")?;

        let next_seq_row = query!(
            tx,
            "SELECT COALESCE(MAX(seq), 0) + 1 AS value FROM swarm_messages WHERE workdir = ?",
            msg.workdir
        )
        .fetch_one::<I64ValueRow>()
        .await?;
        let seq = next_seq_row.value;
        let now = jiff::Timestamp::now().to_string();

        query!(
            tx,
            "INSERT INTO swarm_messages (workdir, board_epoch, seq, session_id, sender, kind, body, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            msg.workdir,
            board_epoch_i64,
            seq,
            msg.session_id,
            msg.sender,
            msg.kind.as_str(),
            msg.body,
            now.as_str()
        )
        .execute()
        .await?;

        if let Some(author) = msg.session_id {
            let current_cursor = query!(
                tx,
                "SELECT last_seq AS value FROM swarm_cursors WHERE workdir = ? AND session_id = ?",
                msg.workdir,
                author
            )
            .fetch_optional::<I64ValueRow>()
            .await?
            .map_or(0, |row| row.value);
            if current_cursor == seq - 1 {
                query!(
                    tx,
                    "INSERT INTO swarm_cursors (workdir, session_id, last_seq) VALUES (?, ?, ?)
                     ON CONFLICT(workdir, session_id) DO UPDATE SET last_seq = excluded.last_seq",
                    msg.workdir,
                    author,
                    seq
                )
                .execute()
                .await?;
            }
        }

        tx.commit().await?;
        Ok(Some(SwarmMessageRow {
            workdir: msg.workdir.to_string(),
            board_epoch: board.board_epoch,
            seq: u64::try_from(seq).map_err(|e| overflow_error("swarm_messages.seq", &e))?,
            session_id: msg.session_id.map(str::to_string),
            sender: msg.sender.to_string(),
            kind: msg.kind,
            body: msg.body.to_string(),
            created_at: now,
        }))
    }

    /// Reads a bounded page of messages from `workdir`'s active board and
    /// updates `session_id`'s cursor when contiguous unread messages are read.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn read_swarm_messages(
        &self,
        workdir: &str,
        session_id: &str,
        mode: SwarmReadMode,
        limit: usize,
    ) -> Result<Option<SwarmReadResult>, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let Some(board_row) = query!(
            tx,
            "SELECT workdir, board_epoch, created_at FROM swarm_boards WHERE workdir = ?",
            workdir
        )
        .fetch_optional::<RawBoardRow>()
        .await?
        else {
            tx.commit().await?;
            return Ok(None);
        };
        let board = board_row.into_model()?;
        let cursor = query!(
            tx,
            "SELECT last_seq AS value FROM swarm_cursors WHERE workdir = ? AND session_id = ?",
            workdir,
            session_id
        )
        .fetch_optional::<I64ValueRow>()
        .await?
        .map_or(0_i64, |row| row.value);

        let max_seq_i64 = query!(
            tx,
            "SELECT COALESCE(MAX(seq), 0) AS value FROM swarm_messages WHERE workdir = ?",
            workdir
        )
        .fetch_one::<I64ValueRow>()
        .await?
        .value;

        let start_seq: i64 = match mode {
            SwarmReadMode::Unread => cursor.saturating_add(1),
            SwarmReadMode::Since(since) => to_i64_from_u64(since.max(1), "since")?,
            SwarmReadMode::All => 1,
        };
        let fetch_limit = i64::try_from(limit.max(1).saturating_add(1))
            .map_err(|e| overflow_error("swarm read limit", &e))?;

        let mut raw_rows = query!(
            tx,
            "SELECT workdir, board_epoch, seq, session_id, sender, kind, body, created_at
             FROM swarm_messages
             WHERE workdir = ? AND seq >= ?
             ORDER BY seq ASC
             LIMIT ?",
            workdir,
            start_seq,
            fetch_limit
        )
        .fetch_all::<RawMessageRow>()
        .await?;

        let next_seq = if raw_rows.len() > limit.max(1)
            && let Some(overflow) = raw_rows.pop()
        {
            Some(
                u64::try_from(overflow.seq)
                    .map_err(|e| overflow_error("swarm_messages.seq", &e))?,
            )
        } else {
            None
        };

        let messages: Vec<SwarmMessageRow> = raw_rows
            .into_iter()
            .map(RawMessageRow::into_model)
            .collect::<Result<_, _>>()?;

        // Advance the cursor whenever the read covers from `cursor + 1` (no unread gap skipped).
        if start_seq <= cursor.saturating_add(1)
            && let Some(last) = messages.last()
        {
            let last_i64 = to_i64_from_u64(last.seq, "last_seq")?;
            if last_i64 > cursor {
                query!(
                    tx,
                    "INSERT INTO swarm_cursors (workdir, session_id, last_seq) VALUES (?, ?, ?)
                     ON CONFLICT(workdir, session_id) DO UPDATE SET last_seq = excluded.last_seq",
                    workdir,
                    session_id,
                    last_i64
                )
                .execute()
                .await?;
            }
        }

        tx.commit().await?;
        Ok(Some(SwarmReadResult {
            board_epoch: board.board_epoch,
            messages,
            next_seq,
            max_seq: u64::try_from(max_seq_i64).map_err(|e| overflow_error("max_seq", &e))?,
        }))
    }

    /// Returns the board generation, read cursor and highest sequence number
    /// for `session_id` on `workdir`, or `None` when no board is active.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn swarm_unread_status(
        &self,
        workdir: &str,
        session_id: &str,
    ) -> Result<Option<SwarmUnreadStatus>, ChatDbError> {
        let Some(board) = self.get_swarm_board(workdir).await? else {
            return Ok(None);
        };
        let (cursor_row, max_row) = futures_lite::future::zip(
            query!(
                self.db,
                "SELECT last_seq AS value FROM swarm_cursors WHERE workdir = ? AND session_id = ?",
                workdir,
                session_id
            )
            .fetch_optional::<I64ValueRow>(),
            query!(
                self.db,
                "SELECT COALESCE(MAX(seq), 0) AS value FROM swarm_messages WHERE workdir = ?",
                workdir
            )
            .fetch_one::<I64ValueRow>(),
        )
        .await;
        let cursor = u64::try_from(cursor_row?.map_or(0, |row| row.value))
            .map_err(|e| overflow_error("swarm_cursors.last_seq", &e))?;
        let max_seq =
            u64::try_from(max_row?.value).map_err(|e| overflow_error("swarm_messages.seq", &e))?;
        Ok(Some(SwarmUnreadStatus {
            board_epoch: board.board_epoch,
            cursor,
            max_seq,
        }))
    }

    /// Persisted swarm alert and mention-wake state for `session_id`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn get_swarm_session_state(
        &self,
        session_id: &str,
    ) -> Result<SwarmSessionStateRow, ChatDbError> {
        let Some(row) = query!(
            self.db,
            "SELECT told_workdir, told_epoch, consecutive_mention_wakes FROM swarm_session_state WHERE session_id = ?",
            session_id
        )
        .fetch_optional::<RawSessionStateRow>()
        .await?
        else {
            return Ok(SwarmSessionStateRow::default());
        };
        Ok(SwarmSessionStateRow {
            told_workdir: row.told_workdir,
            told_epoch: u64::try_from(row.told_epoch)
                .map_err(|e| overflow_error("swarm_session_state.told_epoch", &e))?,
            consecutive_mention_wakes: u32::try_from(row.consecutive_mention_wakes)
                .map_err(|e| overflow_error("swarm_session_state.consecutive_mention_wakes", &e))?,
        })
    }

    /// Records which `(workdir, epoch)` protocol alert `session_id`'s warm
    /// context currently holds (`(None, 0)` when cleared).
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn set_swarm_told(
        &self,
        session_id: &str,
        workdir: Option<&str>,
        epoch: u64,
    ) -> Result<(), ChatDbError> {
        let epoch_i64 = to_i64_from_u64(epoch, "told_epoch")?;
        query!(
            self.db,
            "INSERT INTO swarm_session_state (session_id, told_workdir, told_epoch, consecutive_mention_wakes)
             VALUES (?, ?, ?, 0)
             ON CONFLICT(session_id) DO UPDATE SET told_workdir = excluded.told_workdir, told_epoch = excluded.told_epoch",
            session_id,
            workdir,
            epoch_i64
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Increments `session_id`'s unattended `@mention` wake counter when below
    /// `cap`, returning `true` when the caller should wake the session and
    /// `false` once the loop breaker trips.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn record_swarm_mention_wake(
        &self,
        session_id: &str,
        cap: u32,
    ) -> Result<bool, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let current = query!(
            tx,
            "SELECT consecutive_mention_wakes AS value FROM swarm_session_state WHERE session_id = ?",
            session_id
        )
        .fetch_optional::<I64ValueRow>()
        .await?
        .map_or(0_i64, |row| row.value);

        if current >= i64::from(cap) {
            tx.commit().await?;
            return Ok(false);
        }

        let next = current.saturating_add(1);
        query!(
            tx,
            "INSERT INTO swarm_session_state (session_id, told_workdir, told_epoch, consecutive_mention_wakes)
             VALUES (?, NULL, 0, ?)
             ON CONFLICT(session_id) DO UPDATE SET consecutive_mention_wakes = excluded.consecutive_mention_wakes",
            session_id,
            next
        )
        .execute()
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Resets `session_id`'s unattended `@mention` wake counter when the user
    /// submits a message.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn reset_swarm_mention_wakes(&self, session_id: &str) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "UPDATE swarm_session_state SET consecutive_mention_wakes = 0 WHERE session_id = ?",
            session_id
        )
        .execute()
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn board_epoch_advances_across_destroy_and_reopen_while_seq_restarts() {
        let dir = tempdir().expect("temp dir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db opens");

        let (first, created) = db
            .open_or_get_swarm_board("/repo")
            .await
            .expect("board opens");
        assert!(created);
        assert_eq!(first.board_epoch, 1);

        let (same, created_again) = db
            .open_or_get_swarm_board("/repo")
            .await
            .expect("idempotent open");
        assert!(!created_again);
        assert_eq!(same.board_epoch, 1);

        let msg1 = db
            .append_swarm_message(NewSwarmMessage {
                workdir: "/repo",
                session_id: Some("otter"),
                sender: "otter",
                kind: SwarmMessageKind::Post,
                body: "touching auth.rs",
            })
            .await
            .expect("append succeeds")
            .expect("board active");
        assert_eq!((msg1.board_epoch, msg1.seq), (1, 1));

        let msg2 = db
            .append_swarm_message(NewSwarmMessage {
                workdir: "/repo",
                session_id: Some("lynx"),
                sender: "lynx",
                kind: SwarmMessageKind::Post,
                body: "noted",
            })
            .await
            .expect("append succeeds")
            .expect("board active");
        assert_eq!((msg2.board_epoch, msg2.seq), (1, 2));

        // Author's cursor advanced on msg1 to 1, so otter has 1 unread (#2); lynx had 0 before msg2 so cursor stayed 0 (#1..#2 unread).
        let otter_unread = db
            .swarm_unread_status("/repo", "otter")
            .await
            .expect("status")
            .expect("active");
        assert_eq!(otter_unread.cursor, 1);
        assert_eq!(otter_unread.max_seq, 2);

        let read = db
            .read_swarm_messages("/repo", "otter", SwarmReadMode::Unread, 50)
            .await
            .expect("read")
            .expect("active");
        assert_eq!(read.messages.len(), 1);
        assert_eq!(read.messages[0].seq, 2);

        let dissolved = db
            .destroy_swarm_board("/repo", |_, _| DissolvedSteerAction::Keep)
            .await
            .expect("destroy");
        assert_eq!(dissolved.map(|(e, _)| e), Some(1));

        let (second, reopened) = db.open_or_get_swarm_board("/repo").await.expect("reopen");
        assert!(reopened);
        assert_eq!(second.board_epoch, 2);

        let fresh = db
            .append_swarm_message(NewSwarmMessage {
                workdir: "/repo",
                session_id: None,
                sender: "swarm",
                kind: SwarmMessageKind::Lifecycle,
                body: "reopened",
            })
            .await
            .expect("append")
            .expect("active");
        assert_eq!((fresh.board_epoch, fresh.seq), (2, 1));
    }

    #[tokio::test]
    async fn mention_wake_counter_caps_and_resets_on_user_turn() {
        let dir = tempdir().expect("temp dir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db opens");

        assert!(db.record_swarm_mention_wake("otter", 2).await.unwrap());
        assert!(db.record_swarm_mention_wake("otter", 2).await.unwrap());
        assert!(!db.record_swarm_mention_wake("otter", 2).await.unwrap());

        db.reset_swarm_mention_wakes("otter").await.unwrap();
        assert!(db.record_swarm_mention_wake("otter", 2).await.unwrap());
    }
}
