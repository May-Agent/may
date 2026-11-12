//! Per-session cache of prepared tool environments.
//!
//! Preparing a session's home environment downloads and verifies the managed
//! tools and probes their versions. Front ends that rebuild the agent for
//! every turn or provider retry keep the prepared [`ToolEnvironments`] here
//! and hand it to the next builder instead of preparing again. An SSH server
//! is prepared when its first command names it, so it is not part of the
//! cache.
//!
//! A single actor task owns the map. Requesters ask it for a session's
//! environment and either receive the cached one or a [`PreparationLease`]
//! that makes them the only preparer of that session; concurrent requesters
//! for the same session queue behind the lease and are answered once it
//! completes.

use std::collections::{HashMap, VecDeque};

use aither_sandbox::HostRoute;
use tokio::sync::{mpsc, oneshot};

use crate::session::SessionId;
use crate::tools::ToolEnvironments;

/// The backend that runs a session's commands on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HomeBackend {
    /// The local heel sandbox.
    Local,
    /// The session's container.
    Container {
        /// Id of the container the tools are mounted into.
        container_id: String,
        /// How the container reaches the host. A restarted container keeps
        /// its id but can come back on another address, so the route is part
        /// of the key.
        route: HostRoute,
    },
}

impl HomeBackend {
    /// The container backend reached through `route`.
    #[must_use]
    pub fn container(container_id: &str, route: &HostRoute) -> Self {
        Self::Container {
            container_id: container_id.to_owned(),
            route: route.clone(),
        }
    }
}

/// Why a session's tool environment could not be acquired.
#[derive(Debug, thiserror::Error)]
pub enum ToolEnvironmentCacheError {
    /// The session was deleted while its environment was being prepared.
    #[error("session {0} was deleted while its tool environment was being prepared")]
    SessionEvicted(SessionId),
}

/// What a requester does with a session's tool environment.
#[derive(Debug)]
pub enum Acquired {
    /// Adopt this registry, prepared for an equal key.
    Ready(Box<ToolEnvironments>),
    /// Prepare the environment and report the result through the lease.
    Prepare(PreparationLease),
}

/// Sole right to prepare one session's environment.
///
/// Completing it caches the registry and answers every queued requester.
/// Dropping it without completing — a failed or cancelled preparation —
/// caches nothing and lets the next queued requester prepare instead.
#[derive(Debug)]
pub struct PreparationLease {
    session: SessionId,
    generation: u64,
    requests: Option<mpsc::UnboundedSender<Request>>,
}

impl PreparationLease {
    /// Caches the prepared registry for the leased session.
    ///
    /// # Panics
    ///
    /// Panics if the lease's request channel was already taken, which cannot
    /// happen through the public API.
    pub fn complete(mut self, registry: ToolEnvironments) {
        let requests = self
            .requests
            .take()
            .expect("a preparation lease completes once");
        requests
            .send(Request::Complete {
                session: self.session.clone(),
                generation: self.generation,
                registry: Box::new(registry),
            })
            .expect("tool environment cache actor outlives its leases");
    }
}

impl Drop for PreparationLease {
    fn drop(&mut self) {
        if let Some(requests) = self.requests.take() {
            requests
                .send(Request::Abandon {
                    session: self.session.clone(),
                    generation: self.generation,
                })
                .expect("tool environment cache actor outlives its leases");
        }
    }
}

/// Handle to the per-session prepared tool environment cache.
///
/// Clones share one cache. The owning actor stops once every handle and
/// lease is dropped.
#[derive(Debug, Clone)]
pub struct ToolEnvironmentCache {
    requests: mpsc::UnboundedSender<Request>,
}

impl ToolEnvironmentCache {
    /// Starts the cache actor on the current tokio runtime.
    ///
    /// # Panics
    /// Panics when called outside a tokio runtime.
    #[must_use]
    pub fn spawn() -> Self {
        let (requests, receiver) = mpsc::unbounded_channel();
        tokio::spawn(run_cache_actor(requests.downgrade(), receiver));
        Self { requests }
    }

    /// The session's cached environment when it was prepared for `key`,
    /// otherwise a lease to prepare it.
    ///
    /// Waits while another requester prepares the same session.
    ///
    /// # Errors
    /// Fails when the session is evicted while this request waits.
    pub async fn acquire(
        &self,
        session: &SessionId,
        key: HomeBackend,
    ) -> Result<Acquired, ToolEnvironmentCacheError> {
        let (reply, response) = oneshot::channel();
        self.send(Request::Acquire(Waiter {
            session: session.clone(),
            key,
            reply,
        }));
        response.await.map_err(|oneshot::error::RecvError { .. }| {
            ToolEnvironmentCacheError::SessionEvicted(session.clone())
        })
    }

    /// Forgets a deleted session's environment.
    ///
    /// Requesters waiting on its preparation fail, and a preparation still in
    /// flight is not cached.
    pub fn evict(&self, session: &SessionId) {
        self.send(Request::Evict(session.clone()));
    }

    /// Forgets every session's environment.
    pub fn evict_all(&self) {
        self.send(Request::EvictAll);
    }

    fn send(&self, request: Request) {
        self.requests
            .send(request)
            .expect("tool environment cache actor outlives its handles");
    }
}

#[derive(Debug)]
enum Request {
    Acquire(Waiter),
    Complete {
        session: SessionId,
        generation: u64,
        registry: Box<ToolEnvironments>,
    },
    Abandon {
        session: SessionId,
        generation: u64,
    },
    Evict(SessionId),
    EvictAll,
}

#[derive(Debug)]
struct Waiter {
    session: SessionId,
    key: HomeBackend,
    reply: oneshot::Sender<Acquired>,
}

#[derive(Debug)]
enum Slot {
    Ready {
        key: HomeBackend,
        registry: Box<ToolEnvironments>,
    },
    Preparing {
        key: HomeBackend,
        generation: u64,
        waiters: VecDeque<Waiter>,
    },
}

#[derive(Debug)]
struct CacheState {
    slots: HashMap<SessionId, Slot>,
    next_generation: u64,
    /// Leases report back through this; it is weak so that the actor stops
    /// once every handle and lease is gone.
    requests: mpsc::WeakUnboundedSender<Request>,
}

async fn run_cache_actor(
    weak_requests: mpsc::WeakUnboundedSender<Request>,
    mut requests: mpsc::UnboundedReceiver<Request>,
) {
    let mut state = CacheState {
        slots: HashMap::new(),
        next_generation: 0,
        requests: weak_requests,
    };
    while let Some(request) = requests.recv().await {
        match request {
            Request::Acquire(waiter) => state.serve(VecDeque::from([waiter])),
            Request::Complete {
                session,
                generation,
                registry,
            } => state.complete(session, generation, *registry),
            Request::Abandon {
                session,
                generation,
            } => state.abandon(&session, generation),
            Request::Evict(session) => {
                state.slots.remove(&session);
            }
            Request::EvictAll => state.slots.clear(),
        }
    }
}

impl CacheState {
    /// Answers requesters in order: an equal cached key adopts, a running
    /// preparation queues them, anything else starts a preparation.
    fn serve(&mut self, mut pending: VecDeque<Waiter>) {
        while let Some(waiter) = pending.pop_front() {
            match self.slots.get_mut(&waiter.session) {
                Some(Slot::Ready { key, registry }) if *key == waiter.key => {
                    // A requester that stopped waiting needs no answer.
                    let _ = waiter.reply.send(Acquired::Ready(registry.clone()));
                }
                Some(Slot::Preparing { waiters, .. }) => waiters.push_back(waiter),
                Some(Slot::Ready { .. }) | None => {
                    let Some(requests) = self.requests.upgrade() else {
                        // Every handle is gone, so nobody can receive a lease.
                        continue;
                    };
                    let generation = self.next_generation;
                    self.next_generation += 1;
                    let Waiter {
                        session,
                        key,
                        reply,
                    } = waiter;
                    tracing::debug!(
                        session_id = %session,
                        generation,
                        "tool environment preparation leased"
                    );
                    self.slots.insert(
                        session.clone(),
                        Slot::Preparing {
                            key,
                            generation,
                            waiters: VecDeque::new(),
                        },
                    );
                    // A requester that stopped waiting drops the lease, and
                    // its drop abandons this preparation.
                    let _ = reply.send(Acquired::Prepare(PreparationLease {
                        session,
                        generation,
                        requests: Some(requests),
                    }));
                }
            }
        }
    }

    fn complete(&mut self, session: SessionId, generation: u64, registry: ToolEnvironments) {
        let Some((key, waiters)) = self.take_preparation(&session, generation) else {
            tracing::debug!(
                session_id = %session,
                generation,
                "discarding tool environment prepared for an evicted session"
            );
            return;
        };
        self.slots.insert(
            session,
            Slot::Ready {
                key,
                registry: Box::new(registry),
            },
        );
        self.serve(waiters);
    }

    fn abandon(&mut self, session: &SessionId, generation: u64) {
        if let Some((_, waiters)) = self.take_preparation(session, generation) {
            tracing::debug!(
                session_id = %session,
                generation,
                "tool environment preparation abandoned"
            );
            self.serve(waiters);
        }
    }

    /// Removes the session's preparation `generation` and returns its key and
    /// queued requesters, or `None` when that preparation is no longer
    /// current because the session was evicted.
    fn take_preparation(
        &mut self,
        session: &SessionId,
        generation: u64,
    ) -> Option<(HomeBackend, VecDeque<Waiter>)> {
        match self.slots.remove(session) {
            Some(Slot::Preparing {
                key,
                generation: current,
                waiters,
            }) if current == generation => Some((key, waiters)),
            Some(slot) => {
                self.slots.insert(session.clone(), slot);
                None
            }
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::tools::{PreparedTools, ToolCatalog};

    fn registry() -> ToolEnvironments {
        ToolEnvironments::with_test_tools(
            PreparedTools {
                bin_dir: std::path::PathBuf::new(),
                uv_dir: None,
                heel: None,
                catalog: ToolCatalog::empty(),
            },
            &crate::tools::embedded_distribution().expect("the embedded distribution validates"),
            std::path::Path::new(""),
        )
    }

    fn route(host: &str) -> HostRoute {
        HostRoute::new(host, std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)).unwrap()
    }

    async fn acquire(
        cache: &ToolEnvironmentCache,
        session: &SessionId,
        key: HomeBackend,
    ) -> Acquired {
        tokio::time::timeout(Duration::from_secs(5), cache.acquire(session, key))
            .await
            .expect("acquire should answer")
            .expect("acquire should succeed")
    }

    async fn lease(
        cache: &ToolEnvironmentCache,
        session: &SessionId,
        key: HomeBackend,
    ) -> PreparationLease {
        match acquire(cache, session, key).await {
            Acquired::Prepare(lease) => lease,
            Acquired::Ready(_) => panic!("expected a preparation lease"),
        }
    }

    async fn ready(
        cache: &ToolEnvironmentCache,
        session: &SessionId,
        key: HomeBackend,
    ) -> ToolEnvironments {
        match acquire(cache, session, key).await {
            Acquired::Ready(registry) => *registry,
            Acquired::Prepare(_) => panic!("expected the cached registry"),
        }
    }

    /// Asserts the requester is queued: it has sent its request and still
    /// waits for an answer.
    async fn assert_pending<T>(waiter: &mut tokio::task::JoinHandle<T>) {
        assert!(
            tokio::time::timeout(Duration::from_millis(100), waiter)
                .await
                .is_err(),
            "requester should wait for the running preparation"
        );
    }

    #[tokio::test]
    async fn equal_key_prepares_once_then_adopts() {
        let cache = ToolEnvironmentCache::spawn();
        let session = SessionId::from_string("one");
        let key = HomeBackend::Local;

        lease(&cache, &session, key.clone())
            .await
            .complete(registry());
        ready(&cache, &session, key.clone()).await;
        ready(&cache, &session, key).await;
    }

    #[tokio::test]
    async fn changed_configuration_prepares_again() {
        let cache = ToolEnvironmentCache::spawn();
        let session = SessionId::from_string("one");
        let local = HomeBackend::Local;

        lease(&cache, &session, local.clone())
            .await
            .complete(registry());

        let container = HomeBackend::container("c1", &route("10.0.0.1"));
        lease(&cache, &session, container.clone())
            .await
            .complete(registry());
        ready(&cache, &session, container).await;

        let restarted = HomeBackend::container("c1", &route("10.0.0.2"));
        lease(&cache, &session, restarted.clone())
            .await
            .complete(registry());
        ready(&cache, &session, restarted).await;

        let other_container = HomeBackend::container("c2", &route("10.0.0.2"));
        lease(&cache, &session, other_container).await;
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_preparation() {
        let cache = ToolEnvironmentCache::spawn();
        let session = SessionId::from_string("one");
        let key = HomeBackend::Local;

        let first = lease(&cache, &session, key.clone()).await;
        let mut waiters = (0..3)
            .map(|_| {
                let cache = cache.clone();
                let session = session.clone();
                let key = key.clone();
                tokio::spawn(async move { acquire(&cache, &session, key).await })
            })
            .collect::<Vec<_>>();
        for waiter in &mut waiters {
            assert_pending(waiter).await;
        }

        first.complete(registry());
        for waiter in waiters {
            assert!(matches!(
                waiter.await.expect("waiter should join"),
                Acquired::Ready(_)
            ));
        }
    }

    #[tokio::test]
    async fn failed_preparation_is_not_cached() {
        let cache = ToolEnvironmentCache::spawn();
        let session = SessionId::from_string("one");
        let key = HomeBackend::Local;

        let first = lease(&cache, &session, key.clone()).await;
        let waiter = tokio::spawn({
            let cache = cache.clone();
            let session = session.clone();
            let key = key.clone();
            async move { acquire(&cache, &session, key).await }
        });
        let mut waiter = waiter;
        assert_pending(&mut waiter).await;
        drop(first);

        let Acquired::Prepare(retry) = waiter.await.expect("waiter should join") else {
            panic!("the queued requester retries a failed preparation");
        };
        drop(retry);
        lease(&cache, &session, key).await;
    }

    #[tokio::test]
    async fn cancelled_requester_does_not_hold_the_preparation() {
        let cache = ToolEnvironmentCache::spawn();
        let session = SessionId::from_string("one");
        let key = HomeBackend::Local;

        let first = lease(&cache, &session, key.clone()).await;
        let mut cancelled = tokio::spawn({
            let cache = cache.clone();
            let session = session.clone();
            let key = key.clone();
            async move { acquire(&cache, &session, key).await }
        });
        assert_pending(&mut cancelled).await;
        cancelled.abort();
        // The lease handed to the cancelled requester is dropped undelivered,
        // which must abandon its preparation rather than strand the session.
        drop(first);
        lease(&cache, &session, key).await;
    }

    #[tokio::test]
    async fn eviction_forgets_the_session() {
        let cache = ToolEnvironmentCache::spawn();
        let session = SessionId::from_string("one");
        let other = SessionId::from_string("two");
        let key = HomeBackend::Local;

        lease(&cache, &session, key.clone())
            .await
            .complete(registry());
        lease(&cache, &other, key.clone())
            .await
            .complete(registry());
        cache.evict(&session);
        lease(&cache, &session, key.clone()).await;
        ready(&cache, &other, key.clone()).await;

        cache.evict_all();
        lease(&cache, &other, key).await;
    }

    #[tokio::test]
    async fn eviction_fails_waiters_and_discards_inflight_preparation() {
        let cache = ToolEnvironmentCache::spawn();
        let session = SessionId::from_string("one");
        let key = HomeBackend::Local;

        let first = lease(&cache, &session, key.clone()).await;
        let waiter = tokio::spawn({
            let cache = cache.clone();
            let session = session.clone();
            let key = key.clone();
            async move { cache.acquire(&session, key).await }
        });
        let mut waiter = waiter;
        assert_pending(&mut waiter).await;
        cache.evict(&session);
        assert!(matches!(
            waiter.await.expect("waiter should join"),
            Err(ToolEnvironmentCacheError::SessionEvicted(_))
        ));

        first.complete(registry());
        lease(&cache, &session, key).await;
    }
}
