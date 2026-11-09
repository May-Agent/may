//! Desktop notifications raised through the terminal.
//!
//! The terminal turns an escape sequence into a notification on the machine
//! it runs on, so a TUI attached to a daemon elsewhere still notifies where
//! the user sits.

use std::fmt;

use crate::notification::UserNotification;

/// The escape sequence a terminal understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// `OSC 9`: a single message line (iTerm2).
    Osc9,
    /// `OSC 99`: the kitty desktop-notification protocol.
    Osc99,
    /// `OSC 777;notify`: title and body (Ghostty, `WezTerm`, foot and others).
    Osc777,
}

impl Protocol {
    /// The protocol of the terminal named by `TERM_PROGRAM` and `TERM`.
    #[must_use]
    pub fn detect(term_program: Option<&str>, term: Option<&str>) -> Self {
        if term_program == Some("iTerm.app") {
            Self::Osc9
        } else if term == Some("xterm-kitty") {
            Self::Osc99
        } else {
            Self::Osc777
        }
    }
}

/// A notification ready to be written to the terminal.
#[derive(Debug, Clone, Copy)]
pub struct DesktopNotification<'a> {
    protocol: Protocol,
    title: &'a str,
    body: &'a str,
}

impl<'a> DesktopNotification<'a> {
    /// Present `notification` over `protocol`.
    #[must_use]
    pub fn new(protocol: Protocol, notification: &'a UserNotification) -> Self {
        Self {
            protocol,
            title: &notification.title,
            body: &notification.body,
        }
    }
}

/// Control characters and the `;` separating the parameters end a sequence
/// early or split it, so neither reaches the terminal.
struct Clean<'a>(&'a str);

impl fmt::Display for Clean<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for c in self.0.chars() {
            match c {
                ';' => f.write_str(":")?,
                c if c.is_control() => f.write_str(" ")?,
                c => fmt::Write::write_char(f, c)?,
            }
        }
        Ok(())
    }
}

impl fmt::Display for DesktopNotification<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (title, body) = (Clean(self.title), Clean(self.body));
        match self.protocol {
            Protocol::Osc9 => write!(f, "\x1b]9;{title}: {body}\x07"),
            Protocol::Osc99 => write!(
                f,
                "\x1b]99;i=1:d=0:p=title;{title}\x1b\\\x1b]99;i=1:d=1:p=body;{body}\x1b\\"
            ),
            Protocol::Osc777 => write!(f, "\x1b]777;notify;{title};{body}\x07"),
        }
    }
}

/// Write `notification` to the terminal on stdout. Blocking: callers run it
/// off the event loop.
///
/// # Errors
/// Fails when stdout cannot be written.
pub fn raise(notification: &UserNotification, protocol: Protocol) -> std::io::Result<()> {
    use std::io::Write;

    let sequence = DesktopNotification::new(protocol, notification).to_string();
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(sequence.as_bytes())?;
    stdout.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification() -> UserNotification {
        UserNotification {
            session_id: "s1".to_owned(),
            profile_id: None,
            title: "Fix the build".to_owned(),
            body: "Approve running: ls; echo\u{1b}".to_owned(),
        }
    }

    #[test]
    fn the_terminal_program_picks_the_protocol() {
        assert_eq!(
            Protocol::detect(Some("iTerm.app"), Some("xterm-256color")),
            Protocol::Osc9
        );
        assert_eq!(Protocol::detect(None, Some("xterm-kitty")), Protocol::Osc99);
        assert_eq!(
            Protocol::detect(Some("ghostty"), Some("xterm-ghostty")),
            Protocol::Osc777
        );
    }

    #[test]
    fn osc_777_carries_the_session_title_and_the_ask() {
        let notification = notification();
        assert_eq!(
            DesktopNotification::new(Protocol::Osc777, &notification).to_string(),
            "\x1b]777;notify;Fix the build;Approve running: ls: echo \x07"
        );
    }

    #[test]
    fn osc_9_joins_title_and_body() {
        let notification = notification();
        assert_eq!(
            DesktopNotification::new(Protocol::Osc9, &notification).to_string(),
            "\x1b]9;Fix the build: Approve running: ls: echo \x07"
        );
    }

    #[test]
    fn osc_99_sends_title_then_body() {
        let notification = notification();
        assert_eq!(
            DesktopNotification::new(Protocol::Osc99, &notification).to_string(),
            "\x1b]99;i=1:d=0:p=title;Fix the build\x1b\\\x1b]99;i=1:d=1:p=body;Approve running: ls: echo \x1b\\"
        );
    }
}
