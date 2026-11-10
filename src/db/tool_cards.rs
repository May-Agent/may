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
    use crate::command_reading::{AskedCard, Card};

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
