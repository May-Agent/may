//! Notifications for popups that wait on the user.

use askama::Template;

use crate::agent::PermissionRequestMode;
use crate::session::{UserAsk, UserWait};

/// Longest a command, headline, path or question runs in a notification
/// body, in characters.
const BRIEF_MAX_CHARS: usize = 160;

/// A notification that a session waits on the user: what push delivers to
/// browsers and what the daemon streams to attached terminals.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserNotification {
    /// Session that waits.
    pub session_id: String,
    /// Profile of that session, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    /// The session's name.
    pub title: String,
    /// What the user is asked.
    pub body: String,
}

#[derive(Template)]
#[template(path = "user_wait_title.txt", escape = "none")]
struct TitleText<'a> {
    title: Option<&'a str>,
    session_id: &'a str,
}

#[derive(Template)]
#[template(path = "user_wait_body.txt", escape = "none")]
struct BodyText<'a> {
    ask: &'a UserAsk,
}

impl UserNotification {
    /// Present `wait` for the session named `title` under `profile_id`.
    ///
    /// # Panics
    /// Panics if a template fails to render, which its typed inputs rule out.
    #[must_use]
    pub fn new(wait: &UserWait, title: Option<&str>, profile_id: Option<String>) -> Self {
        let title = title.map(str::trim).filter(|title| !title.is_empty());
        Self {
            title: TitleText {
                title,
                session_id: &wait.session_id,
            }
            .render()
            .expect("user wait title template must render"),
            body: BodyText {
                ask: &brief(&wait.ask),
            }
            .render()
            .expect("user wait body template must render"),
            session_id: wait.session_id.clone(),
            profile_id,
        }
    }
}

/// `ask` with every text cut to its first line and [`BRIEF_MAX_CHARS`].
fn brief(ask: &UserAsk) -> UserAsk {
    match ask {
        UserAsk::Command { mode, script } => UserAsk::Command {
            mode: *mode,
            script: shorten(script),
        },
        UserAsk::Publication { headline } => UserAsk::Publication {
            headline: shorten(headline),
        },
        UserAsk::Workspace { path } => UserAsk::Workspace {
            path: shorten(path),
        },
        UserAsk::Question { text } => UserAsk::Question {
            text: shorten(text),
        },
    }
}

fn shorten(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or_default();
    match line.char_indices().nth(BRIEF_MAX_CHARS) {
        Some((end, _)) => format!("{}...", &line[..end]),
        None => line.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait(ask: UserAsk) -> UserWait {
        UserWait {
            session_id: "s1".to_owned(),
            ask,
        }
    }

    #[test]
    fn a_command_approval_names_the_session_and_the_command() {
        let notification = UserNotification::new(
            &wait(UserAsk::permission(
                PermissionRequestMode::Unsafe,
                "rm -rf build\nls",
            )),
            Some("Fix the build"),
            Some("p1".to_owned()),
        );
        assert_eq!(notification.title, "Fix the build");
        assert_eq!(notification.body, "Approve running: rm -rf build");
        assert_eq!(notification.profile_id.as_deref(), Some("p1"));
    }

    #[test]
    fn an_unnamed_session_is_named_by_its_id() {
        let notification = UserNotification::new(
            &wait(UserAsk::Workspace {
                path: "/srv/data".to_owned(),
            }),
            None,
            None,
        );
        assert_eq!(notification.title, "Session s1");
        assert_eq!(notification.body, "Allow access to /srv/data?");
    }

    #[test]
    fn a_publication_approval_shows_its_headline() {
        let notification = UserNotification::new(
            &wait(UserAsk::permission(
                PermissionRequestMode::Publish,
                "bug issue on May-Agent/may\n\nTitle: Crash\n\nbody",
            )),
            Some("Triage"),
            None,
        );
        assert_eq!(
            notification.body,
            "Approve publishing: bug issue on May-Agent/may"
        );
    }

    #[test]
    fn a_long_cjk_question_is_cut_on_a_character_boundary() {
        let text = "确认".repeat(100);
        let notification =
            UserNotification::new(&wait(UserAsk::Question { text }), Some("調査"), None);
        assert_eq!(notification.body.chars().count(), BRIEF_MAX_CHARS + 3);
        assert!(notification.body.ends_with("..."));
    }
}
