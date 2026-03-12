use std::ops::Deref;

use quick_xml::Reader;
use quick_xml::events::Event;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, PartialEq, Eq, Hash, JsonSchema)]
#[schemars(with = "String")]
pub struct XmlFragment(String);

#[derive(Debug, thiserror::Error)]
pub enum XmlFragmentError {
    #[error("XML fragment must not be empty")]
    Empty,
    #[error("invalid XML fragment: {0}")]
    Invalid(String),
}

impl XmlFragment {
    pub fn parse(value: impl Into<String>) -> Result<Self, XmlFragmentError> {
        let value = value.into();
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(XmlFragmentError::Empty);
        }

        validate_xml_fragment(trimmed)?;
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

fn validate_xml_fragment(fragment: &str) -> Result<(), XmlFragmentError> {
    let wrapped = ["<may-fragment-root>", fragment, "</may-fragment-root>"].concat();
    let mut reader = Reader::from_str(&wrapped);
    reader.config_mut().trim_text(false);

    loop {
        match reader.read_event() {
            Ok(Event::Eof) => return Ok(()),
            Ok(_) => {}
            Err(error) => return Err(XmlFragmentError::Invalid(error.to_string())),
        }
    }
}
