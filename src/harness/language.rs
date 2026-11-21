//! Communication and working languages.
//!
//! A host sets the language the agent talks to the user in and the language it
//! writes in for others to read. This module holds the detection of a text's
//! language on its prose and the enforcement ([`LanguageGuard`]) the agent
//! loop and the harness commands share; the settings themselves are
//! `may-config`'s.

mod guard;
mod prose;
mod watch;

pub use guard::{LanguageAlert, LanguageGuard, LanguageRefusal, WorkingText};
pub use guard::{default_alerts, default_refusal, translate};
pub use may_config::language::{Language, Languages, UnknownLanguage};
pub(crate) use watch::LanguageAlerts;
