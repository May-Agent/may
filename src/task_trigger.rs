use std::collections::BTreeMap;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const TRIGGER_KIND_TIME: &str = "time";
pub const TRIGGER_KIND_EVENT: &str = "event";
pub const TRIGGER_KIND_WEBHOOK: &str = "webhook";

pub const TRIGGER_TYPE_ONCE: &str = "once";
pub const TRIGGER_TYPE_CRON: &str = "cron";

pub const EVENT_CHAT_TURN_COMPLETED: &str = "chat.turn.completed";
const WEBHOOK_ROUTE_PREFIX: &str = "/api/tasks/webhook/";

fn join_text(parts: &[&str]) -> String {
    let capacity = parts.iter().map(|part| part.len()).sum();
    let mut output = String::with_capacity(capacity);
    for part in parts {
        output.push_str(part);
    }
    output
}

fn prefixed_error(prefix: &str, error: &impl std::fmt::Display) -> String {
    let error_text = error.to_string();
    join_text(&[prefix, error_text.as_str()])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TaskTriggerKind {
    Time,
    Event,
    Webhook,
}

impl TaskTriggerKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Time => TRIGGER_KIND_TIME,
            Self::Event => TRIGGER_KIND_EVENT,
            Self::Webhook => TRIGGER_KIND_WEBHOOK,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TimeTriggerType {
    Once,
    Cron,
}

impl TimeTriggerType {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Once => TRIGGER_TYPE_ONCE,
            Self::Cron => TRIGGER_TYPE_CRON,
        }
    }
}

impl FromStr for TimeTriggerType {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let trigger_type = value.trim().to_lowercase();
        match trigger_type.as_str() {
            TRIGGER_TYPE_ONCE => Ok(Self::Once),
            TRIGGER_TYPE_CRON => Ok(Self::Cron),
            other => Err(join_text(&[
                "unsupported time trigger_type '",
                other,
                "', expected '",
                TRIGGER_TYPE_ONCE,
                "' or '",
                TRIGGER_TYPE_CRON,
                "'",
            ])),
        }
    }
}

impl FromStr for TaskTriggerKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let kind = value.trim().to_lowercase();
        match kind.as_str() {
            TRIGGER_KIND_TIME => Ok(Self::Time),
            TRIGGER_KIND_EVENT => Ok(Self::Event),
            TRIGGER_KIND_WEBHOOK => Ok(Self::Webhook),
            other => Err(join_text(&[
                "unsupported task trigger_kind '",
                other,
                "', expected '",
                TRIGGER_KIND_TIME,
                "', '",
                TRIGGER_KIND_EVENT,
                "', or '",
                TRIGGER_KIND_WEBHOOK,
                "'",
            ])),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TaskTriggerInput<'a> {
    pub kind: &'a str,
    pub trigger_type: Option<&'a str>,
    pub run_at_utc: Option<&'a str>,
    pub cron_utc: Option<&'a str>,
    pub event_name: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskTriggerRequest {
    Time {
        #[serde(rename = "triggerType")]
        trigger_type: TimeTriggerType,
        #[serde(rename = "runAtUtc")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_at_utc: Option<String>,
        #[serde(rename = "cronUtc")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cron_utc: Option<String>,
    },
    Event {
        #[serde(rename = "eventName")]
        event_name: String,
    },
    Webhook,
}

impl TaskTriggerRequest {
    pub fn normalize(&self) -> Result<NormalizedTaskTrigger, String> {
        match self {
            Self::Time {
                trigger_type,
                run_at_utc,
                cron_utc,
            } => normalize_time_trigger(
                Some(trigger_type.as_str()),
                run_at_utc.as_deref(),
                cron_utc.as_deref(),
            ),
            Self::Event { event_name } => {
                normalize_event_trigger(None, None, None, Some(event_name.as_str()))
            }
            Self::Webhook => normalize_trigger(TaskTriggerInput {
                kind: TaskTriggerKind::Webhook.as_str(),
                trigger_type: None,
                run_at_utc: None,
                cron_utc: None,
                event_name: None,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistedTaskTrigger {
    Time { trigger_type: TimeTriggerType },
    Event { event_name: String },
    Webhook { token: String },
}

impl PersistedTaskTrigger {
    pub fn from_storage(kind: &str, trigger_type: &str) -> Result<Self, String> {
        match TaskTriggerKind::from_str(kind)? {
            TaskTriggerKind::Time => Ok(Self::Time {
                trigger_type: TimeTriggerType::from_str(trigger_type)?,
            }),
            TaskTriggerKind::Event => Ok(Self::Event {
                event_name: normalize_event_name(trigger_type)?,
            }),
            TaskTriggerKind::Webhook => Ok(Self::Webhook {
                token: trigger_type.to_string(),
            }),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> TaskTriggerKind {
        match self {
            Self::Time { .. } => TaskTriggerKind::Time,
            Self::Event { .. } => TaskTriggerKind::Event,
            Self::Webhook { .. } => TaskTriggerKind::Webhook,
        }
    }

    #[must_use]
    pub const fn storage_trigger_type(&self) -> &str {
        match self {
            Self::Time { trigger_type } => trigger_type.as_str(),
            Self::Event { event_name } => event_name.as_str(),
            Self::Webhook { token } => token.as_str(),
        }
    }

    #[must_use]
    pub fn webhook_path(&self) -> Option<String> {
        match self {
            Self::Webhook { token } => Some(webhook_path(token.as_str())),
            Self::Time { .. } | Self::Event { .. } => None,
        }
    }

    #[must_use]
    pub fn response(
        &self,
        run_at_utc: Option<String>,
        cron_utc: Option<String>,
    ) -> TaskTriggerResponse {
        match self {
            Self::Time { trigger_type } => TaskTriggerResponse::Time {
                trigger_type: *trigger_type,
                run_at_utc,
                cron_utc,
            },
            Self::Event { event_name } => TaskTriggerResponse::Event {
                event_name: event_name.clone(),
            },
            Self::Webhook { token } => TaskTriggerResponse::Webhook {
                token: token.clone(),
                webhook_path: webhook_path(token.as_str()),
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct NormalizedTaskTrigger {
    kind: TaskTriggerKind,
    time_trigger_type: Option<TimeTriggerType>,
    event_name: Option<String>,
    run_at: Option<String>,
    cron_expression: Option<String>,
}

impl NormalizedTaskTrigger {
    #[must_use]
    pub const fn kind_str(&self) -> &'static str {
        self.kind.as_str()
    }

    #[must_use]
    pub const fn kind(&self) -> TaskTriggerKind {
        self.kind
    }

    #[must_use]
    pub fn run_at(&self) -> Option<&str> {
        self.run_at.as_deref()
    }

    #[must_use]
    pub fn cron_expression(&self) -> Option<&str> {
        self.cron_expression.as_deref()
    }

    #[must_use]
    pub fn storage_trigger_type(&self) -> String {
        match (
            self.time_trigger_type,
            self.event_name.as_deref(),
            self.kind,
        ) {
            (Some(trigger_type), None, TaskTriggerKind::Time) => trigger_type.as_str().to_string(),
            (None, Some(event_name), TaskTriggerKind::Event) => event_name.to_string(),
            (None, None, TaskTriggerKind::Webhook) => new_webhook_token(),
            _ => panic!("invalid normalized task trigger invariant"),
        }
    }

    #[must_use]
    pub fn into_persisted(self) -> PersistedTaskTrigger {
        match (self.kind, self.time_trigger_type, self.event_name) {
            (TaskTriggerKind::Time, Some(trigger_type), None) => {
                PersistedTaskTrigger::Time { trigger_type }
            }
            (TaskTriggerKind::Event, None, Some(event_name)) => {
                PersistedTaskTrigger::Event { event_name }
            }
            (TaskTriggerKind::Webhook, None, None) => PersistedTaskTrigger::Webhook {
                token: new_webhook_token(),
            },
            _ => panic!("invalid normalized task trigger invariant"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskWebhookRequest {
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    pub headers: BTreeMap<String, Vec<String>>,
    pub body_base64: String,
    pub body_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_json: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_addr: Option<String>,
    pub body_size_bytes: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskInvocation {
    Time {
        trigger_type: TimeTriggerType,
    },
    Manual,
    Event {
        event_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        profile_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    Webhook {
        token: String,
        request: TaskWebhookRequest,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskTriggerResponse {
    Time {
        #[serde(rename = "triggerType")]
        trigger_type: TimeTriggerType,
        #[serde(rename = "runAtUtc")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_at_utc: Option<String>,
        #[serde(rename = "cronUtc")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cron_utc: Option<String>,
    },
    Event {
        #[serde(rename = "eventName")]
        event_name: String,
    },
    Webhook {
        token: String,
        #[serde(rename = "webhookPath")]
        webhook_path: String,
    },
}

#[must_use]
pub fn new_webhook_token() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[must_use]
pub fn webhook_path(token: &str) -> String {
    join_text(&[WEBHOOK_ROUTE_PREFIX, token])
}

pub fn normalize_trigger(input: TaskTriggerInput<'_>) -> Result<NormalizedTaskTrigger, String> {
    let kind = TaskTriggerKind::from_str(input.kind)?;
    let trigger_type = normalize_optional(input.trigger_type);
    let run_at = normalize_optional(input.run_at_utc);
    let cron_expression = normalize_optional(input.cron_utc);
    let event_name = normalize_optional(input.event_name);

    match kind {
        TaskTriggerKind::Time => normalize_time_trigger(trigger_type, run_at, cron_expression),
        TaskTriggerKind::Event => {
            normalize_event_trigger(trigger_type, run_at, cron_expression, event_name)
        }
        TaskTriggerKind::Webhook => {
            if trigger_type.is_some()
                || run_at.is_some()
                || cron_expression.is_some()
                || event_name.is_some()
            {
                return Err(
                    "webhook trigger must not set trigger_type, run_at_utc, cron_utc, or event_name"
                        .to_string(),
                );
            }
            Ok(NormalizedTaskTrigger {
                kind,
                time_trigger_type: None,
                event_name: None,
                run_at: None,
                cron_expression: None,
            })
        }
    }
}

pub fn normalize_utc_timestamp(value: &str) -> Result<String, String> {
    let timestamp = value.trim();
    if !timestamp.ends_with('Z') {
        return Err("timestamp must be an ISO 8601 UTC value ending with 'Z'".to_string());
    }
    jiff::Timestamp::from_str(timestamp)
        .map(|parsed| parsed.to_string())
        .map_err(|error| prefixed_error("invalid UTC timestamp: ", &error))
}

pub fn normalize_event_name(value: &str) -> Result<String, String> {
    let raw = value.trim().to_lowercase();
    if raw.is_empty() {
        return Err("event trigger requires a non-empty event name".to_string());
    }
    let valid = raw
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '_' | '-'));
    if !valid {
        return Err(join_text(&[
            "invalid event name '",
            raw.as_str(),
            "': only [a-z0-9._-] are allowed",
        ]));
    }
    Ok(raw)
}

pub fn normalize_cron_expression(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    let field_count = trimmed.split_whitespace().count();
    let canonical = match field_count {
        5 => join_text(&["0 ", trimmed]),
        6 => trimmed.to_string(),
        _ => {
            return Err(
                "cron_utc must contain 5 fields (minute-hour-day-month-weekday) or 6 fields (including seconds)"
                    .to_string(),
            )
        }
    };
    cron::Schedule::from_str(canonical.as_str())
        .map(|_| canonical)
        .map_err(|error| prefixed_error("invalid cron_utc expression: ", &error))
}

fn normalize_time_trigger(
    trigger_type: Option<&str>,
    run_at: Option<&str>,
    cron_expression: Option<&str>,
) -> Result<NormalizedTaskTrigger, String> {
    if trigger_type.is_none() {
        return match (run_at, cron_expression) {
            (Some(run_at), None) => {
                normalize_time_trigger(Some(TRIGGER_TYPE_ONCE), Some(run_at), None)
            }
            (None, Some(cron_expression)) => {
                normalize_time_trigger(Some(TRIGGER_TYPE_CRON), None, Some(cron_expression))
            }
            (Some(_), Some(_)) => {
                Err("time trigger requires exactly one of run_at_utc or cron_utc".to_string())
            }
            (None, None) => {
                Err("time trigger requires trigger_type or one of run_at_utc/cron_utc".to_string())
            }
        };
    }

    match TimeTriggerType::from_str(trigger_type.expect("trigger_type checked above"))? {
        TimeTriggerType::Once => {
            if cron_expression.is_some() {
                return Err("once trigger must not include cron_utc".to_string());
            }
            let run_at = run_at.ok_or_else(|| "once trigger requires run_at_utc".to_string())?;
            Ok(NormalizedTaskTrigger {
                kind: TaskTriggerKind::Time,
                time_trigger_type: Some(TimeTriggerType::Once),
                event_name: None,
                run_at: Some(normalize_utc_timestamp(run_at)?),
                cron_expression: None,
            })
        }
        TimeTriggerType::Cron => {
            if run_at.is_some() {
                return Err("cron trigger must not include run_at_utc".to_string());
            }
            let cron_expression =
                cron_expression.ok_or_else(|| "cron trigger requires cron_utc".to_string())?;
            Ok(NormalizedTaskTrigger {
                kind: TaskTriggerKind::Time,
                time_trigger_type: Some(TimeTriggerType::Cron),
                event_name: None,
                run_at: None,
                cron_expression: Some(normalize_cron_expression(cron_expression)?),
            })
        }
    }
}

fn normalize_event_trigger(
    trigger_type: Option<&str>,
    run_at: Option<&str>,
    cron_expression: Option<&str>,
    event_name: Option<&str>,
) -> Result<NormalizedTaskTrigger, String> {
    if run_at.is_some() || cron_expression.is_some() {
        return Err("event trigger must not include run_at_utc or cron_utc".to_string());
    }
    if trigger_type.is_some() {
        return Err("trigger_type is only valid when trigger_kind is 'time'".to_string());
    }
    let event_name = event_name.ok_or_else(|| "event trigger requires event_name".to_string())?;
    Ok(NormalizedTaskTrigger {
        kind: TaskTriggerKind::Event,
        time_trigger_type: None,
        event_name: Some(normalize_event_name(event_name)?),
        run_at: None,
        cron_expression: None,
    })
}

fn normalize_optional(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::{
        PersistedTaskTrigger, TaskTriggerInput, TaskTriggerKind, TaskTriggerRequest,
        TimeTriggerType, normalize_event_name, normalize_trigger, normalize_utc_timestamp,
        webhook_path,
    };

    #[test]
    fn task_trigger_kind_parses_case_insensitive_values() {
        let time = TaskTriggerKind::from_str(" Time ").expect("time kind should parse");
        let event = TaskTriggerKind::from_str("EVENT").expect("event kind should parse");
        let webhook = TaskTriggerKind::from_str("webhook").expect("webhook kind should parse");
        assert_eq!(time, TaskTriggerKind::Time);
        assert_eq!(event, TaskTriggerKind::Event);
        assert_eq!(webhook, TaskTriggerKind::Webhook);
    }

    #[test]
    fn task_trigger_kind_rejects_unknown_values() {
        let err = TaskTriggerKind::from_str("manual").expect_err("manual is not supported");
        assert!(
            err.contains("unsupported task trigger_kind"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn normalize_event_name_accepts_and_normalizes_valid_value() {
        let normalized =
            normalize_event_name(" Chat.Turn_Completed ").expect("event name should normalize");
        assert_eq!(normalized, "chat.turn_completed");
    }

    #[test]
    fn normalize_event_name_rejects_invalid_value() {
        let err = normalize_event_name("chat turn").expect_err("space is invalid");
        assert!(
            err.contains("only [a-z0-9._-] are allowed"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn webhook_path_uses_public_task_route() {
        assert_eq!(webhook_path("abc123"), "/api/tasks/webhook/abc123");
    }

    #[test]
    fn normalize_trigger_derives_once_from_run_at() {
        let normalized = normalize_trigger(TaskTriggerInput {
            kind: "time",
            trigger_type: None,
            run_at_utc: Some("2026-03-12T01:02:03Z"),
            cron_utc: None,
            event_name: None,
        })
        .expect("run_at should normalize");
        assert_eq!(normalized.kind(), TaskTriggerKind::Time);
        assert_eq!(
            normalized.storage_trigger_type(),
            TimeTriggerType::Once.as_str()
        );
        assert_eq!(normalized.run_at(), Some("2026-03-12T01:02:03Z"));
    }

    #[test]
    fn task_trigger_request_normalizes_nested_time_payload() {
        let normalized = TaskTriggerRequest::Time {
            trigger_type: TimeTriggerType::Cron,
            run_at_utc: None,
            cron_utc: Some("0 6 * * *".to_string()),
        }
        .normalize()
        .expect("time trigger request should normalize");
        assert_eq!(normalized.kind(), TaskTriggerKind::Time);
        assert_eq!(normalized.cron_expression(), Some("0 0 6 * * *"));
    }

    #[test]
    fn normalize_trigger_requires_event_name_field() {
        let normalized = normalize_trigger(TaskTriggerInput {
            kind: "event",
            trigger_type: None,
            run_at_utc: None,
            cron_utc: None,
            event_name: Some("chat.turn.completed"),
        })
        .expect("event should normalize");
        assert_eq!(normalized.kind(), TaskTriggerKind::Event);
        assert_eq!(normalized.storage_trigger_type(), "chat.turn.completed");
    }

    #[test]
    fn normalize_trigger_generates_webhook_token_on_storage() {
        let normalized = normalize_trigger(TaskTriggerInput {
            kind: "webhook",
            trigger_type: None,
            run_at_utc: None,
            cron_utc: None,
            event_name: None,
        })
        .expect("webhook should normalize");
        let token = normalized.storage_trigger_type();
        assert_eq!(normalized.kind(), TaskTriggerKind::Webhook);
        assert!(!token.is_empty());
        assert_eq!(
            webhook_path(&token),
            super::join_text(&["/api/tasks/webhook/", token.as_str()])
        );
    }

    #[test]
    fn persisted_webhook_trigger_builds_api_response() {
        let trigger = PersistedTaskTrigger::Webhook {
            token: "abc123".to_string(),
        };
        let response = trigger.response(None, None);
        match response {
            super::TaskTriggerResponse::Webhook {
                token,
                webhook_path,
            } => {
                assert_eq!(token, "abc123");
                assert_eq!(webhook_path, "/api/tasks/webhook/abc123");
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn normalize_utc_timestamp_rejects_non_utc_suffix() {
        let err = normalize_utc_timestamp("2026-03-12T01:02:03+08:00")
            .expect_err("non-utc timestamp should fail");
        assert!(err.contains("ending with 'Z'"), "unexpected error: {err}");
    }
}
