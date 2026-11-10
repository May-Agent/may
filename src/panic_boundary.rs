//! Catches a panic at the boundary of the operation that raised it.
//!
//! The operation fails with the panic's message and the rest of the daemon
//! keeps running. The process-wide panic hook still writes its bug log first.

use std::any::Any;
use std::future::Future;
use std::panic::AssertUnwindSafe;

use futures_util::FutureExt as _;

/// The message a panic carried.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Panicked(String);

impl Panicked {
    fn from_payload(payload: &(dyn Any + Send)) -> Self {
        let message = payload
            .downcast_ref::<&str>()
            .map(|message| (*message).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic with a non-string payload".to_owned());
        Self(message)
    }

    /// The panic message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.0
    }
}

/// Runs `future`, turning a panic in it into [`Panicked`].
///
/// # Errors
/// Returns the panic message when `future` panics.
pub async fn catch<F: Future>(future: F) -> Result<F::Output, Panicked> {
    AssertUnwindSafe(future)
        .catch_unwind()
        .await
        .map_err(|payload| Panicked::from_payload(payload.as_ref()))
}

/// Runs one round of the background worker `worker`: a panic is logged with
/// its message and the round yields `None`, so the caller's loop goes on.
pub async fn round<F: Future>(worker: &'static str, future: F) -> Option<F::Output> {
    match catch(future).await {
        Ok(output) => Some(output),
        Err(panicked) => {
            tracing::error!(worker, panic = %panicked, "a background round panicked; the worker continues");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_panic_message_of_either_payload_kind_is_extracted() {
        let literal = catch(async { panic!("literal") }).await.unwrap_err();
        assert_eq!(literal.message(), "literal");
        let detail = 7;
        let formatted = catch(async move { panic!("formatted {detail}") })
            .await
            .unwrap_err();
        assert_eq!(formatted.message(), "formatted 7");
    }

    #[tokio::test]
    async fn a_round_that_panics_yields_none_and_the_next_round_runs() {
        assert_eq!(round("test", async { panic!("boom") }).await, None::<()>);
        assert_eq!(round("test", async { 3 }).await, Some(3));
    }
}
