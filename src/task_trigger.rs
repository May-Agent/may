use std::collections::BTreeMap;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Storage/serde kind string for time-based triggers.
pub const TRIGGER_KIND_TIME: &str = "time";
/// Storage/serde kind string for event triggers.
pub const TRIGGER_KIND_EVENT: &str = "event";
/// Storage/serde kind string for webhook triggers.
pub const TRIGGER_KIND_WEBHOOK: &str = "webhook";

/// Trigger-type string for a single run at a fixed timestamp.
pub const TRIGGER_TYPE_ONCE: &str = "once";
/// Trigger-type string for a recurring cron schedule.
pub const TRIGGER_TYPE_CRON: &str = "cron";

/// Event name emitted when a chat turn finishes.
pub const EVENT_CHAT_TURN_COMPLETED: &str = "chat.turn.completed";
const WEBHOOK_ROUTE_PREFIX: &str = "/api/tasks/webhook/";
/// The webhook route with its token replaced, for records that must not hold the token.
pub const WEBHOOK_ROUTE_REDACTED: &str = "/api/tasks/webhook/{token}";

fn prefixed_error(prefix: &str, error: &impl std::fmt::Display) -> String {
    format!("{prefix}{error}")
}

/// Random bytes in a webhook token (256 bits).
const WEBHOOK_TOKEN_BYTES: usize = 32;
/// Prefix GitHub-style senders put before the hex digest.
const SIGNATURE_PREFIX: &str = "sha256=";

/// A signed-delivery requirement on a webhook trigger.
///
/// The HMAC-SHA256 of the request body, keyed by the stored secret `secret`,
/// must arrive hex-encoded (optionally prefixed `sha256=`) in `header`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, skyzen::ToSchema)]
pub struct WebhookSignature {
    /// Name of a secret stored through `/secret`.
    pub secret: String,
    /// Request header the sender puts the signature in, such as `X-Hub-Signature-256`.
    pub header: String,
}

/// Why a delivery's signature was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WebhookSignatureError {
    /// The signature header is absent or repeated.
    #[error("the signature header is missing")]
    Missing,
    /// The header value is not a hex SHA-256 digest.
    #[error("the signature header is malformed")]
    Malformed,
    /// The digest does not match the body.
    #[error("the signature does not match the body")]
    Mismatch,
}

impl WebhookSignature {
    /// Validate the secret name and header name, lowercasing the header.
    ///
    /// # Errors
    /// Returns a message when the secret name is not env-var-safe or the header is not a valid HTTP header name.
    pub fn normalized(&self) -> Result<Self, String> {
        let secret = crate::secrets::SecretStore::validate_name(self.secret.trim())
            .map_err(|error| error.to_string())?
            .to_string();
        let header = skyzen::header::HeaderName::from_bytes(self.header.trim().as_bytes())
            .map_err(|_| format!("'{}' is not a valid HTTP header name", self.header))?;
        Ok(Self {
            secret,
            header: header.as_str().to_string(),
        })
    }

    /// Check the request's signature header against HMAC-SHA256 of its body keyed by `secret`.
    ///
    /// # Errors
    /// Returns [`WebhookSignatureError`] when the header is missing, malformed or does not match.
    ///
    /// # Panics
    /// Never in practice: HMAC accepts keys of any length.
    pub fn verify(
        &self,
        secret: &str,
        request: &TaskWebhookRequest,
    ) -> Result<(), WebhookSignatureError> {
        use hmac::{Hmac, Mac};

        let values = request
            .headers
            .get(&self.header)
            .ok_or(WebhookSignatureError::Missing)?;
        let [value] = values.as_slice() else {
            return Err(WebhookSignatureError::Missing);
        };
        let digest = value.trim();
        let digest = digest.strip_prefix(SIGNATURE_PREFIX).unwrap_or(digest);
        let digest = hex::decode(digest).map_err(|_| WebhookSignatureError::Malformed)?;
        let body = request
            .body_bytes()
            .map_err(|_| WebhookSignatureError::Malformed)?;
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes())
            .expect("HMAC accepts keys of any length");
        mac.update(&body);
        mac.verify_slice(&digest)
            .map_err(|_| WebhookSignatureError::Mismatch)
    }
}

/// Whether `token` has the shape [`new_webhook_token`] produces: 256 bits, URL-safe base64.
#[must_use]
pub fn is_strong_webhook_token(token: &str) -> bool {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token)
        .is_ok_and(|bytes| bytes.len() == WEBHOOK_TOKEN_BYTES)
}

/// Whether two webhook tokens are equal, in time independent of where they differ.
#[must_use]
pub fn webhook_tokens_match(stored: &str, presented: &str) -> bool {
    use subtle::ConstantTimeEq as _;
    stored.as_bytes().ct_eq(presented.as_bytes()).into()
}

/// How a task run is initiated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TaskTriggerKind {
    /// Runs on a schedule (once or cron).
    Time,
    /// Runs when a named event is emitted.
    Event,
    /// Runs when its unique HTTP endpoint is called.
    Webhook,
}

impl TaskTriggerKind {
    /// Persisted string form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Time => TRIGGER_KIND_TIME,
            Self::Event => TRIGGER_KIND_EVENT,
            Self::Webhook => TRIGGER_KIND_WEBHOOK,
        }
    }
}

/// Shape of a time-based schedule.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, skyzen::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum TimeTriggerType {
    /// Single run at a fixed timestamp.
    Once,
    /// Recurring run on a cron expression.
    Cron,
}

impl TimeTriggerType {
    /// Persisted string form.
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
            other => Err(format!(
                "unsupported time trigger_type '{other}', expected '{TRIGGER_TYPE_ONCE}' or '{TRIGGER_TYPE_CRON}'"
            )),
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
            other => Err(format!(
                "unsupported task trigger_kind '{other}', expected '{TRIGGER_KIND_TIME}', '{TRIGGER_KIND_EVENT}', or '{TRIGGER_KIND_WEBHOOK}'"
            )),
        }
    }
}

/// Borrowed trigger definition as supplied by an API caller, before normalization.
#[derive(Debug, Clone)]
pub struct TaskTriggerInput<'a> {
    /// Trigger kind string (`time`, `event`, or `webhook`).
    pub kind: &'a str,
    /// Time trigger sub-type (`once`/`cron`); rejected for other kinds.
    pub trigger_type: Option<&'a str>,
    /// One-shot ISO-8601 UTC timestamp for `once` triggers.
    pub run_at_utc: Option<&'a str>,
    /// Cron expression (5 or 6 fields) for `cron` triggers.
    pub cron_utc: Option<&'a str>,
    /// Event name for `event` triggers.
    pub event_name: Option<&'a str>,
}

/// Trigger definition as it arrives over the API, tagged by `kind`; normalized before persistence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, skyzen::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskTriggerRequest {
    /// Time-based schedule.
    Time {
        /// `once` or `cron`.
        #[serde(rename = "triggerType")]
        trigger_type: TimeTriggerType,
        /// One-shot timestamp for `once` triggers.
        #[serde(rename = "runAtUtc")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_at_utc: Option<String>,
        /// Cron expression for `cron` triggers.
        #[serde(rename = "cronUtc")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cron_utc: Option<String>,
    },
    /// Fired when a named event is emitted.
    Event {
        /// Normalized event name.
        #[serde(rename = "eventName")]
        event_name: String,
    },
    /// Fired by an HTTP call to the minted webhook path.
    Webhook {
        /// Require deliveries to carry an HMAC-SHA256 signature of the body.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<WebhookSignature>,
    },
}

impl TaskTriggerRequest {
    /// Validate this request into a [`NormalizedTaskTrigger`].
    ///
    /// # Errors
    /// Returns a human-readable message when the kind, timestamp, cron expression, or event name fails validation.
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
            Self::Webhook { signature } => {
                let mut normalized = normalize_trigger(&TaskTriggerInput {
                    kind: TaskTriggerKind::Webhook.as_str(),
                    trigger_type: None,
                    run_at_utc: None,
                    cron_utc: None,
                    event_name: None,
                })?;
                normalized.signature = signature
                    .as_ref()
                    .map(WebhookSignature::normalized)
                    .transpose()?;
                Ok(normalized)
            }
        }
    }
}

/// Trigger definition as stored in the database (`kind` plus a
/// `trigger_type` payload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistedTaskTrigger {
    /// Time-based schedule.
    Time {
        /// `once` or `cron`.
        trigger_type: TimeTriggerType,
    },
    /// Fired when a named event is emitted.
    Event {
        /// Normalized event name (`[a-z0-9._-]`).
        event_name: String,
    },
    /// Fired by an HTTP call to the webhook path minted from `token`.
    Webhook {
        /// Secret token embedded in the webhook path.
        token: String,
        /// Signed-delivery requirement, when the task has one.
        signature: Option<WebhookSignature>,
    },
}

impl PersistedTaskTrigger {
    /// Reconstruct a persisted trigger from the `(kind, trigger_type)` pair stored in the database
    /// and the webhook `signature` columns.
    ///
    /// # Errors
    /// Returns a message when `kind` or `trigger_type` is not recognized or fails per-kind validation.
    pub fn from_storage(
        kind: &str,
        trigger_type: &str,
        signature: Option<WebhookSignature>,
    ) -> Result<Self, String> {
        match TaskTriggerKind::from_str(kind)? {
            TaskTriggerKind::Time => Ok(Self::Time {
                trigger_type: TimeTriggerType::from_str(trigger_type)?,
            }),
            TaskTriggerKind::Event => Ok(Self::Event {
                event_name: normalize_event_name(trigger_type)?,
            }),
            TaskTriggerKind::Webhook => Ok(Self::Webhook {
                token: trigger_type.to_string(),
                signature,
            }),
        }
    }

    /// Kind of trigger this persisted value represents.
    #[must_use]
    pub const fn kind(&self) -> TaskTriggerKind {
        match self {
            Self::Time { .. } => TaskTriggerKind::Time,
            Self::Event { .. } => TaskTriggerKind::Event,
            Self::Webhook { .. } => TaskTriggerKind::Webhook,
        }
    }

    /// Type payload stored alongside the kind: the time sub-type, event name, or webhook token.
    #[must_use]
    pub const fn storage_trigger_type(&self) -> &str {
        match self {
            Self::Time { trigger_type } => trigger_type.as_str(),
            Self::Event { event_name } => event_name.as_str(),
            Self::Webhook { token, .. } => token.as_str(),
        }
    }

    /// Signed-delivery requirement for webhook triggers that have one.
    #[must_use]
    pub const fn webhook_signature(&self) -> Option<&WebhookSignature> {
        match self {
            Self::Webhook { signature, .. } => signature.as_ref(),
            Self::Time { .. } | Self::Event { .. } => None,
        }
    }

    /// Public webhook URL path for webhook triggers; `None` for other kinds.
    #[must_use]
    pub fn webhook_path(&self) -> Option<String> {
        match self {
            Self::Webhook { token, .. } => Some(webhook_path(token.as_str())),
            Self::Time { .. } | Self::Event { .. } => None,
        }
    }

    /// API-facing representation of this persisted trigger.
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
            Self::Webhook { token, signature } => TaskTriggerResponse::Webhook {
                token: token.clone(),
                webhook_path: webhook_path(token.as_str()),
                signature: signature.clone(),
            },
        }
    }
}

/// Validated trigger definition produced by [`normalize_trigger`].
#[derive(Debug, Clone)]
pub struct NormalizedTaskTrigger {
    kind: TaskTriggerKind,
    time_trigger_type: Option<TimeTriggerType>,
    event_name: Option<String>,
    run_at: Option<String>,
    cron_expression: Option<String>,
    signature: Option<WebhookSignature>,
}

impl NormalizedTaskTrigger {
    /// Persisted kind string.
    #[must_use]
    pub const fn kind_str(&self) -> &'static str {
        self.kind.as_str()
    }

    /// Trigger kind.
    #[must_use]
    pub const fn kind(&self) -> TaskTriggerKind {
        self.kind
    }

    /// One-shot UTC timestamp for `once` triggers.
    #[must_use]
    pub fn run_at(&self) -> Option<&str> {
        self.run_at.as_deref()
    }

    /// Normalized cron expression for `cron` triggers.
    #[must_use]
    pub fn cron_expression(&self) -> Option<&str> {
        self.cron_expression.as_deref()
    }

    /// Type payload stored alongside the kind; mints a fresh webhook token for webhook triggers.
    ///
    /// # Panics
    /// Panics if the kind/field combination is not one produced by `normalize_trigger`; the fields are private, so this is an internal invariant violation.
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

    /// Convert to the persisted form; mints a webhook token for webhook triggers.
    ///
    /// # Panics
    /// Panics if the kind/field combination is not one produced by `normalize_trigger`; the fields are private, so this is an internal invariant violation.
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
                signature: self.signature,
            },
            _ => panic!("invalid normalized task trigger invariant"),
        }
    }
}

/// HTTP request data captured for a webhook invocation.
#[derive(Debug, Clone, Serialize)]
pub struct TaskWebhookRequest {
    /// HTTP method (uppercase).
    pub method: String,
    /// Request path.
    pub path: String,
    /// Raw query string, when present.
    pub query: Option<String>,
    /// Request headers, multi-valued per name; names are lowercase.
    pub headers: BTreeMap<String, Vec<String>>,
    /// Raw request body, base64-encoded.
    pub body_base64: String,
    /// Body decoded as UTF-8 text when valid.
    pub body_text: Option<String>,
    /// Body parsed as JSON when it is valid JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_json: Option<serde_json::Value>,
    /// Caller socket address when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_addr: Option<String>,
    /// Raw body length in bytes.
    pub body_size_bytes: usize,
}

impl TaskWebhookRequest {
    /// The raw request body.
    ///
    /// # Errors
    /// Returns the decode error when `body_base64` is not valid base64.
    pub fn body_bytes(&self) -> Result<Vec<u8>, base64::DecodeError> {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.decode(&self.body_base64)
    }
}

/// A recorded task run and what triggered it, serialized tagged by `kind`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskInvocation {
    /// Fired on a time schedule.
    Time {
        /// `once` or `cron`.
        trigger_type: TimeTriggerType,
    },
    /// Triggered directly by the user.
    Manual,
    /// Fired by a named event.
    Event {
        /// Event that fired.
        event_name: String,
        /// Profile the emitting event was scoped to, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        profile_id: Option<String>,
        /// Session the emitting event was scoped to, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    /// Fired by an HTTP webhook call.
    Webhook {
        /// Captured HTTP request data.
        request: TaskWebhookRequest,
    },
}

/// API-facing view of a trigger definition, tagged by `kind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, skyzen::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskTriggerResponse {
    /// Time-based schedule.
    Time {
        /// `once` or `cron`.
        #[serde(rename = "triggerType")]
        trigger_type: TimeTriggerType,
        /// One-shot timestamp for `once` triggers.
        #[serde(rename = "runAtUtc")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_at_utc: Option<String>,
        /// Cron expression for `cron` triggers.
        #[serde(rename = "cronUtc")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cron_utc: Option<String>,
    },
    /// Fired when a named event is emitted.
    Event {
        /// Normalized event name.
        #[serde(rename = "eventName")]
        event_name: String,
    },
    /// Fired by an HTTP call to the minted webhook path.
    Webhook {
        /// Secret token embedded in the path.
        token: String,
        /// Computed public webhook path.
        #[serde(rename = "webhookPath")]
        webhook_path: String,
        /// Signed-delivery requirement, when the task has one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<WebhookSignature>,
    },
}

/// Generate a fresh webhook token: 256 bits from the OS CSPRNG, URL-safe base64.
///
/// # Panics
/// Panics when the operating system has no secure random source.
#[must_use]
pub fn new_webhook_token() -> String {
    use base64::Engine as _;
    let mut bytes = [0_u8; WEBHOOK_TOKEN_BYTES];
    getrandom::fill(&mut bytes).expect("secure random source unavailable");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// URL path a webhook trigger with `token` listens on.
#[must_use]
pub fn webhook_path(token: &str) -> String {
    format!("{WEBHOOK_ROUTE_PREFIX}{token}")
}

/// Validate and normalize a [`TaskTriggerInput`] into a [`NormalizedTaskTrigger`].
///
/// # Errors
/// Returns a human-readable message when the kind string is unknown, a field is set for the wrong kind, or a timestamp/cron/event value fails validation.
pub fn normalize_trigger(input: &TaskTriggerInput<'_>) -> Result<NormalizedTaskTrigger, String> {
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
                signature: None,
            })
        }
    }
}

/// Validate and canonicalize an ISO-8601 UTC timestamp, which must end in `Z`.
///
/// # Errors
/// Returns a message when the value is not a valid UTC timestamp ending in `Z`.
pub fn normalize_utc_timestamp(value: &str) -> Result<String, String> {
    let timestamp = value.trim();
    if !timestamp.ends_with('Z') {
        return Err("timestamp must be an ISO 8601 UTC value ending with 'Z'".to_string());
    }
    jiff::Timestamp::from_str(timestamp)
        .map(|parsed| parsed.to_string())
        .map_err(|error| prefixed_error("invalid UTC timestamp: ", &error))
}

/// Validate and lowercase an event name; allowed characters are `[a-z0-9._-]`.
///
/// # Errors
/// Returns a message when the name is empty or contains other characters.
pub fn normalize_event_name(value: &str) -> Result<String, String> {
    let raw = value.trim().to_lowercase();
    if raw.is_empty() {
        return Err("event trigger requires a non-empty event name".to_string());
    }
    let valid = raw
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '_' | '-'));
    if !valid {
        return Err(format!(
            "invalid event name '{raw}': only [a-z0-9._-] are allowed"
        ));
    }
    Ok(raw)
}

/// Validate a cron expression; a 5-field expression is canonicalized to 6 fields with a leading `0` seconds field.
///
/// # Errors
/// Returns a message when the field count is not 5 or 6, or the expression fails to parse.
pub fn normalize_cron_expression(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    let field_count = trimmed.split_whitespace().count();
    let canonical = match field_count {
        5 => format!("0 {trimmed}"),
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
                signature: None,
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
                signature: None,
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
        signature: None,
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
        let normalized = normalize_trigger(&TaskTriggerInput {
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
        let normalized = normalize_trigger(&TaskTriggerInput {
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
        let normalized = normalize_trigger(&TaskTriggerInput {
            kind: "webhook",
            trigger_type: None,
            run_at_utc: None,
            cron_utc: None,
            event_name: None,
        })
        .expect("webhook should normalize");
        let token = normalized.storage_trigger_type();
        assert_eq!(normalized.kind(), TaskTriggerKind::Webhook);
        assert_ne!(token, "");
        assert_eq!(webhook_path(&token), format!("/api/tasks/webhook/{token}"));
    }

    #[test]
    fn persisted_webhook_trigger_builds_api_response() {
        let trigger = PersistedTaskTrigger::Webhook {
            token: "abc123".to_string(),
            signature: None,
        };
        let response = trigger.response(None, None);
        match response {
            super::TaskTriggerResponse::Webhook {
                token,
                webhook_path,
                ..
            } => {
                assert_eq!(token, "abc123");
                assert_eq!(webhook_path, "/api/tasks/webhook/abc123");
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn webhook_tokens_are_256_bit_random_secrets() {
        let first = super::new_webhook_token();
        let second = super::new_webhook_token();
        assert_ne!(first, second);
        assert!(super::is_strong_webhook_token(&first));
        assert_eq!(first.len(), 43);
        assert!(!super::is_strong_webhook_token("quiet-brave-mint-otter"));
        assert!(!super::is_strong_webhook_token(""));
    }

    #[test]
    fn webhook_token_comparison_requires_equality() {
        let token = super::new_webhook_token();
        assert!(super::webhook_tokens_match(&token, &token));
        assert!(!super::webhook_tokens_match(
            &token,
            &super::new_webhook_token()
        ));
        assert!(!super::webhook_tokens_match(&token, &token[..10]));
    }

    fn signed_request(
        header: &str,
        signature: Option<String>,
        body: &[u8],
    ) -> super::TaskWebhookRequest {
        use base64::Engine as _;
        let mut headers = std::collections::BTreeMap::new();
        if let Some(signature) = signature {
            headers.insert(header.to_string(), vec![signature]);
        }
        super::TaskWebhookRequest {
            method: "POST".to_string(),
            path: "/hook".to_string(),
            query: None,
            headers,
            body_base64: base64::engine::general_purpose::STANDARD.encode(body),
            body_text: None,
            body_json: None,
            remote_addr: None,
            body_size_bytes: body.len(),
        }
    }

    fn sign(secret: &str, body: &[u8]) -> String {
        use hmac::{Hmac, Mac};
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("key");
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn signature_accepts_a_correct_digest_and_refuses_the_rest() {
        let requirement = super::WebhookSignature {
            secret: "HOOK_SECRET".to_string(),
            header: "X-Hub-Signature-256".to_string(),
        }
        .normalized()
        .expect("signature requirement is valid");
        assert_eq!(requirement.header, "x-hub-signature-256");
        let body = br#"{"a":1}"#;
        let good = sign("s3cret", body);
        let verify = |signature: Option<String>, body: &[u8]| {
            requirement.verify(
                "s3cret",
                &signed_request("x-hub-signature-256", signature, body),
            )
        };
        assert_eq!(verify(Some(good.clone()), body), Ok(()));
        assert_eq!(
            verify(Some(good.trim_start_matches("sha256=").to_string()), body),
            Ok(())
        );
        assert_eq!(
            verify(None, body),
            Err(super::WebhookSignatureError::Missing)
        );
        assert_eq!(
            verify(Some(sign("other", body)), body),
            Err(super::WebhookSignatureError::Mismatch)
        );
        assert_eq!(
            verify(Some(good), b"tampered"),
            Err(super::WebhookSignatureError::Mismatch)
        );
        assert_eq!(
            verify(Some("sha256=zz".to_string()), body),
            Err(super::WebhookSignatureError::Malformed)
        );
    }

    #[test]
    fn signature_requirement_rejects_bad_names() {
        let bad_secret = super::WebhookSignature {
            secret: "not a name".to_string(),
            header: "x-sig".to_string(),
        };
        assert!(bad_secret.normalized().is_err());
        let bad_header = super::WebhookSignature {
            secret: "OK".to_string(),
            header: "bad header".to_string(),
        };
        assert!(bad_header.normalized().is_err());
    }

    #[test]
    fn normalize_utc_timestamp_rejects_non_utc_suffix() {
        let err = normalize_utc_timestamp("2026-03-12T01:02:03+08:00")
            .expect_err("non-utc timestamp should fail");
        assert!(err.contains("ending with 'Z'"), "unexpected error: {err}");
    }
}
