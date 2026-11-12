//! What a turn does after a provider request failed.
//!
//! A transient failure (a cut stream, a 5xx, a 529) retries on the provider
//! that produced it for as long as that provider's prompt cache is warm, so
//! the retry still reads the cache. Only once the cache is cold does the turn
//! move to the next candidate, and with none left it keeps retrying at the
//! capped interval; it never gives up on its own.

use std::time::Duration;

use aither_agent::CacheState;
use aither_core::llm::{Failure, FailureKind};

/// The first same-provider retry waits this long; each following one doubles.
const BACKOFF_START: Duration = Duration::from_secs(1);

/// The longest same-provider backoff.
pub(super) const BACKOFF_CAP: Duration = Duration::from_secs(60);

/// What the turn does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RetryDecision {
    /// The failure cannot be retried.
    Fail,
    /// Resubmit to the provider candidate that failed, after `wait`.
    SameProvider { wait: Duration },
    /// Resubmit to the next provider candidate after the short failover
    /// backoff.
    NextProvider,
    /// The provider's quota is exhausted and no candidate is left: the turn
    /// waits for the reset the upstream announced, when it announced one.
    QuotaReset,
    /// The machine has no network: the turn waits for it to return, then
    /// resubmits to the provider candidate that failed. Every provider
    /// shares the network, so no failover helps.
    WaitForNetwork,
}

/// Backoff of the `retries`-th (0-based) consecutive same-provider retry.
pub(super) fn backoff(retries: u32) -> Duration {
    BACKOFF_START
        .saturating_mul(2u32.saturating_pow(retries))
        .min(BACKOFF_CAP)
}

/// Decides how the turn continues after `failure`.
///
/// `warmth` is the state of the failed candidate's cache entry,
/// `candidate_left` whether another provider candidate remains, `retries`
/// how many same-provider retries this candidate already had, and `online`
/// whether the machine currently has a route to the network.
pub(super) fn decide(
    failure: Option<&Failure>,
    warmth: CacheState,
    candidate_left: bool,
    retries: u32,
    online: bool,
) -> RetryDecision {
    let Some(failure) = failure else {
        return RetryDecision::Fail;
    };
    match failure.kind {
        FailureKind::Permanent => RetryDecision::Fail,
        FailureKind::Quota if candidate_left => RetryDecision::NextProvider,
        FailureKind::Quota => RetryDecision::QuotaReset,
        FailureKind::Network if !online => RetryDecision::WaitForNetwork,
        // The route is up (or just came back after a wake) yet the request
        // still failed: retry on the same provider with backoff, never fail
        // over.
        FailureKind::Network => RetryDecision::SameProvider {
            wait: backoff(retries).max(failure.retry_after.unwrap_or_default()),
        },
        FailureKind::Transient => {
            let requested = failure.retry_after.unwrap_or_default();
            match warmth {
                CacheState::Warm { .. } => RetryDecision::SameProvider {
                    wait: backoff(retries).max(requested),
                },
                CacheState::Cold if candidate_left => RetryDecision::NextProvider,
                CacheState::Cold => RetryDecision::SameProvider {
                    wait: BACKOFF_CAP.max(requested),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WARM: CacheState = CacheState::Warm {
        cold_in: Some(Duration::from_secs(120)),
    };

    #[test]
    fn transient_while_warm_retries_the_same_provider() {
        let failure = Failure::transient();
        assert_eq!(
            decide(Some(&failure), WARM, true, 0, true),
            RetryDecision::SameProvider {
                wait: Duration::from_secs(1)
            }
        );
        assert_eq!(
            decide(Some(&failure), WARM, true, 3, true),
            RetryDecision::SameProvider {
                wait: Duration::from_secs(8)
            }
        );
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        assert_eq!(backoff(0), Duration::from_secs(1));
        assert_eq!(backoff(5), Duration::from_secs(32));
        assert_eq!(backoff(6), BACKOFF_CAP);
        assert_eq!(backoff(u32::MAX), BACKOFF_CAP);
    }

    #[test]
    fn a_cold_cache_fails_over_when_a_candidate_is_left() {
        let failure = Failure::transient();
        assert_eq!(
            decide(Some(&failure), CacheState::Cold, true, 2, true),
            RetryDecision::NextProvider
        );
    }

    #[test]
    fn a_cold_cache_with_no_candidate_keeps_retrying_at_the_cap() {
        let failure = Failure::transient();
        assert_eq!(
            decide(Some(&failure), CacheState::Cold, false, 0, true),
            RetryDecision::SameProvider { wait: BACKOFF_CAP }
        );
        assert_eq!(
            decide(Some(&failure), CacheState::Cold, false, 40, true),
            RetryDecision::SameProvider { wait: BACKOFF_CAP }
        );
    }

    #[test]
    fn a_longer_retry_after_wins_over_the_backoff() {
        let failure = Failure::transient().with_retry_after(Some(Duration::from_secs(90)));
        assert_eq!(
            decide(Some(&failure), WARM, false, 0, true),
            RetryDecision::SameProvider {
                wait: Duration::from_secs(90)
            }
        );
        assert_eq!(
            decide(Some(&failure), CacheState::Cold, false, 0, true),
            RetryDecision::SameProvider {
                wait: Duration::from_secs(90)
            }
        );
    }

    #[test]
    fn a_shorter_retry_after_leaves_the_backoff() {
        let failure = Failure::transient().with_retry_after(Some(Duration::from_millis(200)));
        assert_eq!(
            decide(Some(&failure), WARM, false, 2, true),
            RetryDecision::SameProvider {
                wait: Duration::from_secs(4)
            }
        );
    }

    #[test]
    fn permanent_failures_and_other_errors_fail() {
        let failure = Failure::permanent();
        assert_eq!(
            decide(Some(&failure), WARM, true, 0, true),
            RetryDecision::Fail
        );
        assert_eq!(decide(None, WARM, true, 0, true), RetryDecision::Fail);
    }

    #[test]
    fn quota_keeps_the_failover_then_reset_path() {
        let failure = Failure::quota();
        assert_eq!(
            decide(Some(&failure), WARM, true, 0, true),
            RetryDecision::NextProvider
        );
        assert_eq!(
            decide(Some(&failure), WARM, false, 0, true),
            RetryDecision::QuotaReset
        );
    }

    #[test]
    fn a_network_failure_offline_waits_for_the_network_whatever_the_cache() {
        let failure = Failure::network();
        for warmth in [WARM, CacheState::Cold] {
            for candidate_left in [true, false] {
                assert_eq!(
                    decide(Some(&failure), warmth, candidate_left, 7, false),
                    RetryDecision::WaitForNetwork
                );
            }
        }
    }

    #[test]
    fn a_network_failure_online_retries_the_same_provider_and_never_fails_over() {
        let failure = Failure::network();
        for warmth in [WARM, CacheState::Cold] {
            assert_eq!(
                decide(Some(&failure), warmth, true, 2, true),
                RetryDecision::SameProvider {
                    wait: Duration::from_secs(4)
                }
            );
        }
        let requested = Failure::network().with_retry_after(Some(Duration::from_secs(30)));
        assert_eq!(
            decide(Some(&requested), CacheState::Cold, true, 0, true),
            RetryDecision::SameProvider {
                wait: Duration::from_secs(30)
            }
        );
    }
}
