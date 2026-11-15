//! Unified wake policy: one decision rule shared by every path that can
//! start an agent loop or wake an idle recipient.

/// What asks to start or wake a session's agent loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeCause {
    /// A user prompt or command opened a turn.
    UserMessage,
    /// Background work started by the agent reported a completion.
    BackgroundCompletion {
        /// Whether any report gives the model something new to act on.
        needs_model: bool,
        /// Whether the user interrupted the session after starting that work.
        interrupted: bool,
    },
    /// Another agent sent a message (`send`) or asked a side question (`btw`).
    AgentMessage {
        /// Whether the recipient's prompt cache has expired.
        recipient_cold: bool,
    },
    /// Recovering a session whose previous turn was interrupted by daemon exit.
    InterruptedRecovery {
        /// Whether the session's prompt cache has expired.
        cache_cold: bool,
    },
    /// Host state or live context changed while the agent was idle.
    ContextChange,
}

/// Whether a [`WakeCause`] starts a loop, waits for the user, or is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeDecision {
    /// Start or wake the agent loop now.
    Start,
    /// Do not start a loop; recorded state/events reach the model with the
    /// next user message.
    DeferToUserMessage,
    /// Refuse the inter-agent wake because the recipient's cache is cold.
    RefuseColdRecipient,
}

/// Evaluates whether `cause` is allowed to start a turn.
#[must_use]
pub const fn decide(cause: WakeCause) -> WakeDecision {
    match cause {
        WakeCause::UserMessage
        | WakeCause::BackgroundCompletion {
            needs_model: true,
            interrupted: false,
        }
        | WakeCause::AgentMessage {
            recipient_cold: false,
        }
        | WakeCause::InterruptedRecovery { cache_cold: false } => WakeDecision::Start,
        WakeCause::AgentMessage {
            recipient_cold: true,
        } => WakeDecision::RefuseColdRecipient,
        WakeCause::BackgroundCompletion { .. }
        | WakeCause::InterruptedRecovery { cache_cold: true }
        | WakeCause::ContextChange => WakeDecision::DeferToUserMessage,
    }
}

#[cfg(test)]
mod tests {
    use super::{WakeCause, WakeDecision, decide};

    #[test]
    fn wake_policy_matches_user_background_interagent_and_context_rules() {
        assert_eq!(decide(WakeCause::UserMessage), WakeDecision::Start);
        assert_eq!(
            decide(WakeCause::BackgroundCompletion {
                needs_model: true,
                interrupted: false,
            }),
            WakeDecision::Start
        );
        assert_eq!(
            decide(WakeCause::BackgroundCompletion {
                needs_model: true,
                interrupted: true,
            }),
            WakeDecision::DeferToUserMessage
        );
        assert_eq!(
            decide(WakeCause::BackgroundCompletion {
                needs_model: false,
                interrupted: false,
            }),
            WakeDecision::DeferToUserMessage
        );
        assert_eq!(
            decide(WakeCause::AgentMessage {
                recipient_cold: false,
            }),
            WakeDecision::Start
        );
        assert_eq!(
            decide(WakeCause::AgentMessage {
                recipient_cold: true,
            }),
            WakeDecision::RefuseColdRecipient
        );
        assert_eq!(
            decide(WakeCause::InterruptedRecovery { cache_cold: false }),
            WakeDecision::Start
        );
        assert_eq!(
            decide(WakeCause::InterruptedRecovery { cache_cold: true }),
            WakeDecision::DeferToUserMessage
        );
        assert_eq!(
            decide(WakeCause::ContextChange),
            WakeDecision::DeferToUserMessage
        );
    }
}
