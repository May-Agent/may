//! Hidden-context block type.
//!
//! [`XmlBlock`] is a non-empty, trimmed string holding text rendered from a
//! typed template (a `<system-reminder>` or other tagged context payload)
//! that is safe to inject into an LLM prompt as hidden context. The newtype
//! enforces the non-empty invariant at the type level so downstream code
//! never has to defend against it.

use std::ops::Deref;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A validated XML context payload.
///
/// `XmlBlock` is constructed via [`XmlBlock::parse`], which rejects
/// empty or whitespace-only strings. Once constructed, the inner
/// content is treated as opaque markup and is injected verbatim into
/// the conversation (hidden context, resume hints, notification
/// callbacks, etc.). This type is deliberately not a full XML
/// validator — it only guarantees that the payload is non-empty so
/// callers can fail fast when a runtime assembler produces nothing
/// worth inserting into context.
#[derive(Debug, Clone, PartialEq, Eq, Hash, JsonSchema, skyzen::ToSchema)]
#[schemars(with = "String")]
pub struct XmlBlock(String);

/// Error returned when constructing an [`XmlBlock`] from invalid input.
#[derive(Debug, thiserror::Error)]
pub enum XmlBlockError {
    /// The provided content was empty after trimming whitespace.
    #[error("xml context block must not be empty")]
    Empty,
}

impl XmlBlock {
    /// Parse `value` into an [`XmlBlock`], trimming surrounding
    /// whitespace and rejecting empty input.
    ///
    /// # Errors
    ///
    /// Returns [`XmlBlockError`] when the trimmed input is empty.
    pub fn parse(value: impl Into<String>) -> Result<Self, XmlBlockError> {
        let value = value.into();
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(XmlBlockError::Empty);
        }
        Ok(Self(trimmed.to_string()))
    }

    /// Consumes the block and returns its inner string.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl Deref for XmlBlock {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.0.as_str()
    }
}

impl AsRef<str> for XmlBlock {
    fn as_ref(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Display for XmlBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl Serialize for XmlBlock {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.0.as_str())
    }
}

impl<'de> Deserialize<'de> for XmlBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

impl TryFrom<String> for XmlBlock {
    type Error = XmlBlockError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl TryFrom<&str> for XmlBlock {
    type Error = XmlBlockError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

#[cfg(test)]
mod tests {
    use super::XmlBlock;

    #[test]
    fn parse_accepts_plain_text_context() {
        let block =
            XmlBlock::parse("resume from previous result").expect("plain text should be accepted");
        assert_eq!(block.as_ref(), "resume from previous result");
    }

    #[test]
    fn parse_accepts_xml_like_markup_without_balanced_xml_validation() {
        let block = XmlBlock::parse("<system-reminder type=\"hint\">retry via terminal")
            .expect("xml-like context markup should be accepted without strict xml validation");
        assert_eq!(
            block.as_ref(),
            "<system-reminder type=\"hint\">retry via terminal"
        );
    }

    #[test]
    fn parse_rejects_empty_content() {
        let error = XmlBlock::parse("   ").expect_err("empty content must still fail");
        assert_eq!(error.to_string(), "xml context block must not be empty");
    }
}
