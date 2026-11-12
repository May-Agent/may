//! `session find`: rule-based search over session history.
//!
//! Filters narrow the `messages` table in SQL; the regex then runs over the
//! remaining message text. It never touches the session search index, and
//! never reads a thinking block.

use std::collections::HashMap;

use askama::Template;
use regex::{Regex, RegexBuilder};
use serde_json::Value;

use super::session_tool::truncate_text;
use crate::db::{ChatDb, ChatDbError, MessageScan, ScannedMessage};

/// Rows read from SQLite per round trip.
const SCAN_BATCH: i64 = 500;
/// Matching lines shown per message.
const MAX_LINES: usize = 5;
/// Characters shown per line.
const LINE_WIDTH: usize = 240;

/// A `session find` request: a regular expression over session history,
/// newest messages first, narrowed by filters.
///
/// Matches user and assistant text; tool calls and their output only with
/// `tools`. Thinking is never searched.
#[derive(Debug, Clone)]
pub(super) struct FindRequest {
    /// Regular expression to look for.
    pub pattern: String,
    /// Only this session id.
    pub session: Option<String>,
    /// Only messages of this role (`user`, `assistant`, or `tool` with `tools`).
    pub role: Option<String>,
    /// Only messages at or after this timestamp (RFC 3339).
    pub since: Option<String>,
    /// Only messages at or before this timestamp (RFC 3339).
    pub until: Option<String>,
    /// Only sessions whose title contains this text, ignoring case.
    pub title: Option<String>,
    /// Match the pattern ignoring case.
    pub ignore_case: bool,
    /// Show this many neighbouring messages around each match.
    pub context: Option<usize>,
    /// Report the number of matching messages per session instead of the matches.
    pub count: bool,
    /// Also search tool calls and their output.
    pub tools: bool,
    /// Maximum matches shown, at most 200.
    pub limit: usize,
    /// Skip this many matches first.
    pub offset: usize,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum FindError {
    #[error("database error: {0}")]
    Database(#[from] ChatDbError),
    #[error("invalid pattern: {0}")]
    Pattern(#[from] regex::Error),
}

struct Hit {
    message: ScannedMessage,
    lines: Vec<String>,
    more_lines: usize,
}

struct ContextLine {
    message_id: i64,
    role: String,
    text: String,
}

struct MatchView {
    message_id: i64,
    role: String,
    time: String,
    session_id: String,
    title: String,
    before: Vec<ContextLine>,
    lines: Vec<String>,
    more_lines: usize,
    after: Vec<ContextLine>,
}

struct CountView {
    session_id: String,
    title: String,
    count: usize,
}

/// What the model reads for a `session find`.
#[derive(Template)]
#[template(path = "find_matches.txt", escape = "none")]
struct FindMatchesText {
    matches: Vec<MatchView>,
    offset: usize,
    has_more: bool,
}

/// What the model reads for a `session find --count`.
#[derive(Template)]
#[template(path = "find_counts.txt", escape = "none")]
struct FindCountsText {
    total: usize,
    sessions: Vec<CountView>,
}

pub(super) async fn find(db: &ChatDb, args: FindRequest) -> Result<String, FindError> {
    let regex = RegexBuilder::new(&args.pattern)
        .case_insensitive(args.ignore_case)
        .build()?;
    let mut roles = vec!["user".to_string(), "assistant".to_string()];
    if args.tools {
        roles.push("tool".to_string());
    }
    if let Some(role) = &args.role {
        roles.retain(|candidate| candidate == role);
    }
    let scan = MessageScan {
        session_id: args.session.clone(),
        roles,
        since: args.since.clone(),
        until: args.until.clone(),
        title_contains: args.title.clone(),
    };
    let limit = args.limit.clamp(1, 200);
    // One extra match tells whether more follow the page.
    let wanted = (!args.count).then(|| args.offset + limit + 1);

    let mut hits = Vec::new();
    let mut before_id = i64::MAX;
    while !scan.roles.is_empty() && wanted.is_none_or(|wanted| hits.len() < wanted) {
        let batch = db.scan_messages(&scan, before_id, SCAN_BATCH).await?;
        let Some(last) = batch.last() else { break };
        before_id = last.id;
        for message in batch {
            if let Some(hit) = match_message(&regex, message) {
                hits.push(hit);
            }
        }
    }

    if args.count {
        return Ok(render_counts(&hits));
    }

    let has_more = hits.len() > args.offset + limit;
    let page: Vec<Hit> = hits.into_iter().skip(args.offset).take(limit).collect();
    let context = args.context.unwrap_or(0);
    let mut matches = Vec::with_capacity(page.len());
    for hit in page {
        matches.push(match_view(db, hit, context).await?);
    }
    Ok(FindMatchesText {
        matches,
        offset: args.offset,
        has_more,
    }
    .render()
    .expect("find template must render"))
}

fn match_message(regex: &Regex, message: ScannedMessage) -> Option<Hit> {
    let text = searchable_text(&message.role, &message.content);
    if !regex.is_match(&text) {
        return None;
    }
    let mut lines: Vec<String> = text
        .lines()
        .filter(|line| regex.is_match(line))
        .map(|line| truncate_text(line.trim(), LINE_WIDTH))
        .collect();
    if lines.is_empty() {
        // The match spans lines; show where it starts.
        let found = regex.find(&text)?;
        lines.push(truncate_text(found.as_str().trim(), LINE_WIDTH));
    }
    let more_lines = lines.len().saturating_sub(MAX_LINES);
    lines.truncate(MAX_LINES);
    Some(Hit {
        message,
        lines,
        more_lines,
    })
}

async fn match_view(db: &ChatDb, hit: Hit, context: usize) -> Result<MatchView, FindError> {
    let Hit {
        message,
        lines,
        more_lines,
    } = hit;
    let mut before = Vec::new();
    let mut after = Vec::new();
    if context > 0 {
        let window = db
            .messages_around(
                &message.session_id,
                message.id,
                i64::try_from(context).unwrap_or(i64::MAX),
            )
            .await?
            .unwrap_or_default();
        for neighbour in window {
            let id = i64::from(neighbour.id);
            if id == message.id {
                continue;
            }
            let line = ContextLine {
                message_id: id,
                text: truncate_text(
                    searchable_text(&neighbour.role, &neighbour.content).trim(),
                    LINE_WIDTH,
                ),
                role: neighbour.role,
            };
            if id < message.id {
                before.push(line);
            } else {
                after.push(line);
            }
        }
    }
    Ok(MatchView {
        message_id: message.id,
        role: message.role,
        time: message
            .timestamp
            .unwrap_or_else(|| "no time recorded".to_string()),
        session_id: message.session_id,
        title: message.title.unwrap_or_default(),
        before,
        lines,
        more_lines,
        after,
    })
}

fn render_counts(hits: &[Hit]) -> String {
    let mut counts: HashMap<&str, CountView> = HashMap::new();
    for hit in hits {
        counts
            .entry(hit.message.session_id.as_str())
            .or_insert_with(|| CountView {
                session_id: hit.message.session_id.clone(),
                title: hit.message.title.clone().unwrap_or_default(),
                count: 0,
            })
            .count += 1;
    }
    let mut sessions: Vec<CountView> = counts.into_values().collect();
    sessions.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    FindCountsText {
        total: hits.len(),
        sessions,
    }
    .render()
    .expect("find counts template must render")
}

/// The text a pattern runs over: a message body as written, a tool event as
/// the strings it holds (its name, arguments and output) without the record's
/// own field names.
fn searchable_text(role: &str, content: &str) -> String {
    if role != "tool" {
        return content.to_string();
    }
    match serde_json::from_str::<Value>(content) {
        Ok(Value::Object(record)) => {
            let mut strings = Vec::new();
            for (key, value) in &record {
                if key != "stage" && key != "id" {
                    collect_strings(value, &mut strings);
                }
            }
            strings.join("\n")
        }
        _ => content.to_string(),
    }
}

fn collect_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            let nested = (text.starts_with('{') || text.starts_with('['))
                .then(|| serde_json::from_str::<Value>(text).ok())
                .flatten();
            match nested {
                Some(inner) => collect_strings(&inner, out),
                None => out.push(text.clone()),
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_strings(item, out)),
        Value::Object(fields) => fields
            .values()
            .for_each(|field| collect_strings(field, out)),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::MessageMeta;
    use crate::session::MessageRole;
    use tempfile::tempdir;

    fn args(pattern: &str) -> FindRequest {
        FindRequest {
            pattern: pattern.to_string(),
            session: None,
            role: None,
            since: None,
            until: None,
            title: None,
            ignore_case: false,
            context: None,
            count: false,
            tools: false,
            limit: 20,
            offset: 0,
        }
    }

    async fn add(db: &ChatDb, session: &str, role: MessageRole, time: &str, content: &str) -> i64 {
        i64::from(
            db.append_message(session, role, content, Some(time), MessageMeta::default())
                .await
                .expect("append")
                .id,
        )
    }

    /// Two sessions: "Deploy plan" (alpha) and "Billing" (beta).
    async fn history() -> (tempfile::TempDir, ChatDb) {
        let dir = tempdir().expect("tempdir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("open");
        db.create_session("alpha", Some("Deploy plan"), Some("m"))
            .await
            .expect("alpha");
        db.create_session("beta", Some("Billing"), Some("m"))
            .await
            .expect("beta");
        add(
            &db,
            "alpha",
            MessageRole::User,
            "2026-01-01T10:00:00Z",
            "roll out the canary",
        )
        .await;
        add(
            &db,
            "alpha",
            MessageRole::Assistant,
            "2026-01-01T10:01:00Z",
            "Canary is live",
        )
        .await;
        add(
            &db,
            "alpha",
            MessageRole::User,
            "2026-02-01T10:00:00Z",
            "canary looks fine",
        )
        .await;
        add(
            &db,
            "beta",
            MessageRole::User,
            "2026-03-01T10:00:00Z",
            "invoice canary mismatch",
        )
        .await;
        add(
            &db,
            "beta",
            MessageRole::Assistant,
            "2026-03-01T10:01:00Z",
            "checking invoices",
        )
        .await;
        (dir, db)
    }

    #[tokio::test]
    async fn matches_a_regex_across_sessions_and_applies_each_filter() {
        let (_dir, db) = history().await;

        let all = find(&db, args("canary")).await.expect("find");
        assert!(all.contains("Showing 3 matching message(s)"), "{all}");
        assert!(all.contains("session beta \"Billing\""), "{all}");
        assert!(!all.contains("Canary is live"), "case sensitive: {all}");

        let mut a = args("canary");
        a.ignore_case = true;
        let text = find(&db, a).await.expect("find");
        assert!(text.contains("Showing 4 matching message(s)"), "{text}");

        let mut a = args("canary");
        a.session = Some("alpha".into());
        let text = find(&db, a).await.expect("find");
        assert!(
            text.contains("Showing 2 matching") && !text.contains("beta"),
            "{text}"
        );

        let mut a = args("canary");
        a.role = Some("assistant".into());
        a.ignore_case = true;
        let text = find(&db, a).await.expect("find");
        assert!(
            text.contains("Showing 1 matching") && text.contains("Canary is live"),
            "{text}"
        );

        let mut a = args("canary");
        a.since = Some("2026-02-01T00:00:00Z".into());
        a.until = Some("2026-02-28T00:00:00Z".into());
        let text = find(&db, a).await.expect("find");
        assert!(
            text.contains("Showing 1 matching") && text.contains("canary looks fine"),
            "{text}"
        );

        let mut a = args("canary");
        a.title = Some("bill".into());
        let text = find(&db, a).await.expect("find");
        assert!(
            text.contains("Showing 1 matching") && text.contains("invoice canary"),
            "{text}"
        );

        let text = find(&db, args("no such text")).await.expect("find");
        assert_eq!(text, "No message matches.");
    }

    #[tokio::test]
    async fn returns_context_and_pages() {
        let (_dir, db) = history().await;
        let mut a = args("canary looks");
        a.context = Some(1);
        let text = find(&db, a).await.expect("find");
        assert!(text.contains("> canary looks fine"), "{text}");
        assert!(
            text.contains("context, message 2 (assistant): Canary is live"),
            "{text}"
        );
        // The next message belongs to another session.
        assert!(!text.contains("invoice"), "{text}");

        let mut a = args("canary");
        a.limit = 1;
        let text = find(&db, a).await.expect("find");
        assert!(text.contains("more follow"), "{text}");
        let mut a = args("canary");
        a.limit = 1;
        a.offset = 2;
        let text = find(&db, a).await.expect("find");
        assert!(
            !text.contains("more follow") && text.contains("roll out"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn counts_matching_messages_per_session() {
        let (_dir, db) = history().await;
        let mut a = args("canary");
        a.count = true;
        let text = find(&db, a).await.expect("find");
        assert!(
            text.starts_with("3 matching message(s) in 2 session(s):"),
            "{text}"
        );
        assert!(
            text.contains("2 in session alpha \"Deploy plan\""),
            "{text}"
        );
        assert!(text.contains("1 in session beta \"Billing\""), "{text}");
    }

    #[tokio::test]
    async fn never_matches_thinking_and_matches_tools_only_when_asked() {
        let (_dir, db) = history().await;
        db.append_message(
            "alpha",
            MessageRole::Assistant,
            "done",
            Some("2026-04-01T10:00:00Z"),
            MessageMeta {
                thinking: Some("zebra hidden reasoning"),
                reasoning: Some("[\"zebra encrypted state\"]"),
                ..MessageMeta::default()
            },
        )
        .await
        .expect("assistant with thinking");
        db.append_message(
            "alpha",
            MessageRole::Tool,
            r#"{"stage":"start","id":"c1","name":"terminal","arguments":"{\"command\":\"kubectl rollout zebra\"}"}"#,
            Some("2026-04-01T10:00:01Z"),
            MessageMeta {
                tool_call_id: Some("c1"),
                ..MessageMeta::default()
            },
        )
        .await
        .expect("tool input");
        db.append_message(
            "alpha",
            MessageRole::Tool,
            r#"{"stage":"end","id":"c1","name":"terminal","result":{"kind":"text","text":"zebra deployed"}}"#,
            Some("2026-04-01T10:00:02Z"),
            MessageMeta {
                tool_call_id: Some("c1"),
                ..MessageMeta::default()
            },
        )
        .await
        .expect("tool output");

        assert_eq!(
            find(&db, args("zebra")).await.expect("find"),
            "No message matches."
        );

        let mut a = args("zebra");
        a.tools = true;
        let text = find(&db, a).await.expect("find");
        assert!(text.contains("Showing 2 matching message(s)"), "{text}");
        assert!(text.contains("> kubectl rollout zebra"), "{text}");
        assert!(text.contains("> zebra deployed"), "{text}");
        assert!(
            !text.contains("thinking") && !text.contains("encrypted"),
            "{text}"
        );

        // Record field names are not text.
        let mut a = args("stage");
        a.tools = true;
        assert_eq!(find(&db, a).await.expect("find"), "No message matches.");

        // `--role tool` finds nothing unless tools are searched.
        let mut a = args("zebra");
        a.role = Some("tool".into());
        assert_eq!(find(&db, a).await.expect("find"), "No message matches.");
    }

    #[tokio::test]
    async fn rejects_an_invalid_pattern() {
        let (_dir, db) = history().await;
        let error = find(&db, args("(")).await.expect_err("bad pattern");
        assert!(error.to_string().starts_with("invalid pattern"), "{error}");
    }
}
