//! The cards of `terminal` calls that ran one of May's commands, kept per
//! call so history shows what the live stream showed.

use std::collections::HashMap;

use crate::command_reading::Card;

use super::{ChatDb, ChatDbError, query};

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
struct CardRow {
    call_id: String,
    card: String,
}

fn decode(row: CardRow) -> Result<(String, Card), ChatDbError> {
    let card = serde_json::from_str(&row.card)
        .map_err(|error| ChatDbError::Storage(format!("unreadable tool card: {error}")))?;
    Ok((row.call_id, card))
}

impl ChatDb {
    /// Stores the card of the terminal call `call_id`, replacing an earlier
    /// one.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn set_tool_card(
        &self,
        session_id: &str,
        call_id: &str,
        card: &Card,
    ) -> Result<(), ChatDbError> {
        let card =
            serde_json::to_string(card).map_err(|error| ChatDbError::Storage(error.to_string()))?;
        query!(
            self.db,
            "INSERT OR REPLACE INTO message_tool_cards (session_id, call_id, card) VALUES (?, ?, ?)",
            session_id,
            call_id,
            card
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Removes the card of the terminal call `call_id`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn clear_tool_card(
        &self,
        session_id: &str,
        call_id: &str,
    ) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "DELETE FROM message_tool_cards WHERE session_id = ? AND call_id = ?",
            session_id,
            call_id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Every stored card of the session, by call id.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or an unreadable card.
    pub async fn tool_cards(&self, session_id: &str) -> Result<HashMap<String, Card>, ChatDbError> {
        query!(
            self.db,
            "SELECT call_id, card FROM message_tool_cards WHERE session_id = ?",
            session_id
        )
        .fetch_all::<CardRow>()
        .await?
        .into_iter()
        .map(decode)
        .collect()
    }

    /// The stored cards of the session's calls `call_ids`, by call id, in
    /// one query.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or an unreadable card.
    pub async fn tool_cards_for(
        &self,
        session_id: &str,
        call_ids: &[&str],
    ) -> Result<HashMap<String, Card>, ChatDbError> {
        let call_ids = serde_json::to_string(call_ids)
            .map_err(|error| ChatDbError::Storage(error.to_string()))?;
        query!(
            self.db,
            "SELECT call_id, card FROM message_tool_cards WHERE session_id = ? AND call_id IN (SELECT value FROM json_each(?))",
            session_id,
            call_ids
        )
        .fetch_all::<CardRow>()
        .await?
        .into_iter()
        .map(decode)
        .collect()
    }

    /// Settles every stored subagent card still running as interrupted: no
    /// run survives a daemon exit, so a card left running never ends.
    /// Returns how many cards changed.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or an unreadable card.
    pub async fn interrupt_running_subagent_cards(&self) -> Result<usize, ChatDbError> {
        #[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
        struct StoredCard {
            session_id: String,
            call_id: String,
            card: String,
        }
        let running = query!(
            self.db,
            "SELECT session_id, call_id, card FROM message_tool_cards WHERE json_extract(card, '$.kind') = 'subagent' AND json_extract(card, '$.state') = 'running'"
        )
        .fetch_all::<StoredCard>()
        .await?;
        let settled = running.len();
        for stored in running {
            let (_, card) = decode(CardRow {
                call_id: stored.call_id.clone(),
                card: stored.card,
            })?;
            self.set_tool_card(&stored.session_id, &stored.call_id, &card.interrupted())
                .await?;
        }
        Ok(settled)
    }

    /// The call and card of the `ask_user` question `request_id`, when a
    /// terminal call of the session asked it.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or an unreadable card.
    pub async fn ask_user_card(
        &self,
        session_id: &str,
        request_id: &str,
    ) -> Result<Option<(String, Card)>, ChatDbError> {
        query!(
            self.db,
            "SELECT call_id, card FROM message_tool_cards WHERE session_id = ? AND json_extract(card, '$.kind') = 'askUser' AND json_extract(card, '$.requestId') = ?",
            session_id,
            request_id
        )
        .fetch_optional::<CardRow>()
        .await?
        .map(decode)
        .transpose()
    }
}

#[cfg(test)]
mod tests {
    use crate::command_reading::{AskedCard, Card, SubagentState};

    use super::*;

    fn question(request_id: &str) -> Card {
        Card::AskUser {
            request_id: request_id.to_owned(),
            questions: vec![AskedCard {
                section: String::new(),
                question: "Which database?".to_owned(),
                options: vec!["SQLite".to_owned()],
                multi_select: false,
                secret: false,
            }],
            answers: None,
        }
    }

    async fn open() -> (tempfile::TempDir, ChatDb) {
        let dir = tempfile::tempdir().expect("dir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db opens");
        (dir, db)
    }

    fn subagent(state: SubagentState) -> Card {
        Card::Subagent {
            agent: "explorer".to_owned(),
            title: "Map".to_owned(),
            state,
            started_at: jiff::Timestamp::UNIX_EPOCH,
            finished_at: None,
            result: None,
        }
    }

    #[tokio::test]
    async fn running_subagent_cards_settle_as_interrupted_and_others_stay() {
        let (_dir, db) = open().await;
        db.set_tool_card("s", "run", &subagent(SubagentState::Running))
            .await
            .expect("store");
        db.set_tool_card("t", "run2", &subagent(SubagentState::Running))
            .await
            .expect("store");
        db.set_tool_card("s", "done", &subagent(SubagentState::Done))
            .await
            .expect("store");
        db.set_tool_card("s", "ask", &question("r1"))
            .await
            .expect("store");

        assert_eq!(
            db.interrupt_running_subagent_cards().await.expect("settle"),
            2
        );

        let cards = db.tool_cards("s").await.expect("read");
        assert_eq!(cards["run"], subagent(SubagentState::Interrupted));
        assert_eq!(cards["done"], subagent(SubagentState::Done));
        assert_eq!(cards["ask"], question("r1"));
        assert_eq!(
            db.tool_cards("t").await.expect("read")["run2"],
            subagent(SubagentState::Interrupted)
        );
        assert_eq!(
            db.interrupt_running_subagent_cards().await.expect("again"),
            0
        );
    }

    #[tokio::test]
    async fn a_card_is_stored_replaced_found_by_its_question_and_cleared() {
        let (_dir, db) = open().await;
        db.set_tool_card("s", "c1", &question("r1"))
            .await
            .expect("store");
        db.set_tool_card("s", "c2", &question("r2"))
            .await
            .expect("store");
        db.set_tool_card("other", "c1", &question("r3"))
            .await
            .expect("store");
        assert_eq!(db.tool_cards("s").await.expect("read").len(), 2);

        let (call, card) = db
            .ask_user_card("s", "r2")
            .await
            .expect("read")
            .expect("found");
        assert_eq!((call.as_str(), card), ("c2", question("r2")));
        assert_eq!(db.ask_user_card("s", "r3").await.expect("read"), None);

        db.set_tool_card("s", "c1", &question("r9"))
            .await
            .expect("replace");
        assert_eq!(
            db.tool_cards("s").await.expect("read")["c1"],
            question("r9")
        );

        db.clear_tool_card("s", "c1").await.expect("clear");
        assert!(!db.tool_cards("s").await.expect("read").contains_key("c1"));
    }
}
