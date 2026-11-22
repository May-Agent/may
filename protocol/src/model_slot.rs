//! A model and the [`Parameters`] of the requests sent to it, paired.
//!
//! aither takes the parameters next to the model and adds the fields it owns
//! on top. The slot is what a side question — a review, a suggestion, a recap
//! — asks instead of a session's live model.

use std::future::Future;

use aither_core::llm::model::Parameters;
use aither_core::llm::{Event as LlmEvent, GenerateError, LLMRequest, LanguageModel, Message};
use futures_lite::Stream;

/// A model and the [`Parameters`] of the requests sent to it. aither takes
/// the parameters next to the model and adds the fields it owns on top.
#[derive(Debug, Clone)]
pub struct ModelSlot<M> {
    /// The model.
    pub llm: M,
    /// The parameters its requests carry.
    pub parameters: Parameters,
}

impl<M> ModelSlot<M> {
    /// Pairs `llm` with the `parameters` of its requests.
    pub const fn new(llm: M, parameters: Parameters) -> Self {
        Self { llm, parameters }
    }
}

impl<M: LanguageModel> ModelSlot<M> {
    /// Streams the model's answer to a standalone request over `messages`
    /// (a side question outside any agent) carrying this slot's parameters.
    pub fn respond(
        &self,
        messages: Vec<Message>,
    ) -> impl Stream<Item = Result<LlmEvent, M::Error>> + Send {
        self.llm
            .respond(LLMRequest::new(messages).with_parameters(self.parameters.clone()))
    }

    /// Generates a `T` from `messages` through the model's structured output,
    /// carrying this slot's parameters.
    pub fn generate<T: schemars::JsonSchema + serde::de::DeserializeOwned + 'static>(
        &self,
        messages: Vec<Message>,
    ) -> impl Future<Output = Result<T, GenerateError<M::Error>>> + Send {
        self.llm
            .generate(LLMRequest::new(messages).with_parameters(self.parameters.clone()))
    }
}
