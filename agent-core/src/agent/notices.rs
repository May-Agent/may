//! The text May's agents read when a command refuses their text for its
//! language.

use aither_sandbox::Refused;

use crate::harness::language::{LanguageRefusal, default_refusal};

/// A command's text refused for its language, as the error its caller
/// reads.
#[must_use]
pub fn language_refused(refusal: LanguageRefusal) -> Refused<LanguageRefusal> {
    Refused::rendered(refusal, default_refusal)
}

/// The error a subagent's prompt refused for its language is, as its caller
/// reads it.
#[must_use]
pub fn subagent_language_refused(refusal: &LanguageRefusal) -> anyhow::Error {
    anyhow::anyhow!(default_refusal(refusal))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::language::Language;

    #[test]
    fn a_refusal_reads_as_the_languages_wording() {
        let refusal = LanguageRefusal::Question {
            communication: Language::ENGLISH,
        };
        let text = default_refusal(&refusal);
        assert_eq!(language_refused(refusal.clone()).text(), text);
        assert_eq!(subagent_language_refused(&refusal).to_string(), text);
    }
}
