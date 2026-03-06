use std::str::FromStr;

pub const TRIGGER_KIND_TIME: &str = "time";
pub const TRIGGER_KIND_EVENT: &str = "event";

pub const TRIGGER_TYPE_ONCE: &str = "once";
pub const TRIGGER_TYPE_CRON: &str = "cron";

pub const EVENT_CHAT_TURN_COMPLETED: &str = "chat.turn.completed";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskTriggerKind {
    Time,
    Event,
}

impl TaskTriggerKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Time => TRIGGER_KIND_TIME,
            Self::Event => TRIGGER_KIND_EVENT,
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
            other => Err(format!(
                "unsupported task trigger_kind '{other}', expected '{TRIGGER_KIND_TIME}' or '{TRIGGER_KIND_EVENT}'"
            )),
        }
    }
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
        return Err(format!(
            "invalid event name '{raw}': only [a-z0-9._-] are allowed"
        ));
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::{TaskTriggerKind, normalize_event_name};

    #[test]
    fn task_trigger_kind_parses_case_insensitive_values() {
        let time = TaskTriggerKind::from_str(" Time ").expect("time kind should parse");
        let event = TaskTriggerKind::from_str("EVENT").expect("event kind should parse");
        assert_eq!(time, TaskTriggerKind::Time);
        assert_eq!(event, TaskTriggerKind::Event);
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
}
