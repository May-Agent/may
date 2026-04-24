use std::fmt::{Display, Formatter};
use std::str::FromStr;

use serde::de::{Error as DeError, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const SELECTOR_SEPARATOR: &str = "::";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelSelector {
    provider_id: String,
    model_id: String,
}

impl ModelSelector {
    #[must_use]
    pub fn new(provider_id: impl Into<String>, model_id: impl Into<String>) -> Self {
        let provider_id = provider_id.into();
        let model_id = model_id.into();
        if provider_id.trim().is_empty() {
            panic!("model selector provider_id must not be empty");
        }
        if model_id.trim().is_empty() {
            panic!("model selector model_id must not be empty");
        }
        if provider_id.contains(SELECTOR_SEPARATOR) {
            panic!("model selector provider_id must not contain '::'");
        }
        Self {
            provider_id,
            model_id,
        }
    }

    #[must_use]
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    #[must_use]
    pub fn model_id(&self) -> &str {
        &self.model_id
    }
}

impl Display for ModelSelector {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}{}{}",
            self.provider_id, SELECTOR_SEPARATOR, self.model_id
        )
    }
}

impl FromStr for ModelSelector {
    type Err = ModelSelectorParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some((provider_id, model_id)) = value.split_once(SELECTOR_SEPARATOR) else {
            return Err(ModelSelectorParseError::MissingSeparator {
                value: value.to_string(),
            });
        };

        if provider_id.trim().is_empty() {
            return Err(ModelSelectorParseError::EmptyProviderId {
                value: value.to_string(),
            });
        }
        if model_id.trim().is_empty() {
            return Err(ModelSelectorParseError::EmptyModelId {
                value: value.to_string(),
            });
        }

        Ok(Self {
            provider_id: provider_id.to_string(),
            model_id: model_id.to_string(),
        })
    }
}

impl Serialize for ModelSelector {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ModelSelector {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ModelSelectorVisitor;

        impl Visitor<'_> for ModelSelectorVisitor {
            type Value = ModelSelector;

            fn expecting(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a model selector string in the form <provider_id>::<model_id>")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                value.parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_str(ModelSelectorVisitor)
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ModelSelectorParseError {
    #[error("invalid model selector '{value}': expected '<provider_id>::<model_id>'")]
    MissingSeparator { value: String },
    #[error("invalid model selector '{value}': provider id must not be empty")]
    EmptyProviderId { value: String },
    #[error("invalid model selector '{value}': model id must not be empty")]
    EmptyModelId { value: String },
}

#[cfg(test)]
mod tests {
    use super::{ModelSelector, ModelSelectorParseError};

    #[test]
    fn parses_selector() {
        let selector: ModelSelector = "openai::gpt-5".parse().unwrap();
        assert_eq!(selector.provider_id(), "openai");
        assert_eq!(selector.model_id(), "gpt-5");
        assert_eq!(selector.to_string(), "openai::gpt-5");
    }

    #[test]
    fn rejects_legacy_bare_model_ids() {
        let error = "gpt-5".parse::<ModelSelector>().unwrap_err();
        assert_eq!(
            error,
            ModelSelectorParseError::MissingSeparator {
                value: "gpt-5".to_string()
            }
        );
    }
}
