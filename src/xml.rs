//! XML helpers for structured context payloads.
//!
//! The two items in this module are:
//!
//! - [`XmlBlock`] — a non-empty, trimmed string that wraps an XML
//!   fragment (typically emitted by [`serialize_xml_block`]) and is
//!   safe to inject into an LLM prompt as hidden context or a tagged
//!   context payload. The newtype enforces the non-empty invariant at
//!   the type level so downstream code never has to defend against it.
//! - [`serialize_xml_block`] — a thin, shared wrapper around
//!   [`quick_xml::se::Serializer::with_root`] for emitting a single
//!   `<tag>...</tag>` block from a serde-serialisable value. Callers
//!   that need to build a structured system-reminder, attachment
//!   preamble, or `@mention` payload share this helper so the wire
//!   format stays consistent.

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
#[derive(Debug, Clone, PartialEq, Eq, Hash, JsonSchema)]
#[schemars(with = "String")]
pub struct XmlBlock(String);

/// Error returned when constructing an [`XmlBlock`] from invalid input.
#[derive(Debug, thiserror::Error)]
pub enum XmlBlockError {
    /// The provided content was empty after trimming whitespace.
    #[error("xml context block must not be empty")]
    Empty,
}

/// Serializes `value` as a single `<tag>...</tag>` XML block.
///
/// This is a thin wrapper around [`quick_xml::se::Serializer::with_root`]
/// used to build context-payload XML for runtime injection. It returns
/// a raw `String` so callers can post-process before wrapping the result
/// in an [`XmlBlock`].
pub fn serialize_xml_block<T: Serialize>(
    tag: &str,
    value: &T,
) -> Result<String, quick_xml::SeError> {
    let mut output = String::new();
    let serializer = quick_xml::se::Serializer::with_root(&mut output, Some(tag))?;
    value.serialize(serializer)?;
    Ok(output)
}

impl XmlBlock {
    /// Parse `value` into an [`XmlBlock`], trimming surrounding
    /// whitespace and rejecting empty input.
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
    use super::{XmlBlock, serialize_xml_block};

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

    #[test]
    fn serialize_xml_block_emits_single_tagged_root() {
        #[derive(serde::Serialize)]
        struct Payload {
            status: &'static str,
        }

        let xml = serialize_xml_block("system-reminder", &Payload { status: "ok" })
            .expect("serialize should succeed");
        assert!(xml.contains("<system-reminder>"));
        assert!(xml.contains("<status>ok</status>"));
        assert!(xml.contains("</system-reminder>"));
    }
}
