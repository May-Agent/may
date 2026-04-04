use std::ops::Deref;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, PartialEq, Eq, Hash, JsonSchema)]
#[schemars(with = "String")]
pub struct XmlFragment(String);

#[derive(Debug, thiserror::Error)]
pub enum XmlFragmentError {
    #[error("context fragment must not be empty")]
    Empty,
}

pub fn serialize_tagged_xml<T: Serialize>(
    tag: &str,
    value: &T,
) -> Result<String, quick_xml::SeError> {
    let mut output = String::new();
    let serializer = quick_xml::se::Serializer::with_root(&mut output, Some(tag))?;
    value.serialize(serializer)?;
    Ok(output)
}

impl XmlFragment {
    pub fn parse(value: impl Into<String>) -> Result<Self, XmlFragmentError> {
        let value = value.into();
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(XmlFragmentError::Empty);
        }
        Ok(Self(trimmed.to_string()))
    }

    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl Deref for XmlFragment {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.0.as_str()
    }
}

impl AsRef<str> for XmlFragment {
    fn as_ref(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Display for XmlFragment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl Serialize for XmlFragment {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.0.as_str())
    }
}

impl<'de> Deserialize<'de> for XmlFragment {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

impl TryFrom<String> for XmlFragment {
    type Error = XmlFragmentError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl TryFrom<&str> for XmlFragment {
    type Error = XmlFragmentError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

#[cfg(test)]
mod tests {
    use super::XmlFragment;

    #[test]
    fn parse_accepts_plain_text_context() {
        let fragment = XmlFragment::parse("resume from previous result")
            .expect("plain text should be accepted");
        assert_eq!(fragment.as_ref(), "resume from previous result");
    }

    #[test]
    fn parse_accepts_xml_like_markup_without_balanced_xml_validation() {
        let fragment = XmlFragment::parse("<system-alert type=\"hint\">retry via terminal")
            .expect("xml-like context markup should be accepted without strict xml validation");
        assert_eq!(
            fragment.as_ref(),
            "<system-alert type=\"hint\">retry via terminal"
        );
    }

    #[test]
    fn parse_rejects_empty_content() {
        let error = XmlFragment::parse("   ").expect_err("empty content must still fail");
        assert_eq!(error.to_string(), "context fragment must not be empty");
    }
}
