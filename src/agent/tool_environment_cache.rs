//! Per-session cache of prepared tool environments.
//!
//! Preparing a session's tool environments downloads and verifies the managed
//! tools, probes their versions and, for attached SSH servers, asks the user
//! for consent. Front ends that rebuild the agent for every turn or provider
//! retry keep the prepared [`ShellSessionRegistry`] here and hand it to the
//! next builder instead of preparing again.
//!
//! A single actor task owns the map. Requesters ask it for a session's
//! environment and either receive the cached registry or a
//! [`PreparationLease`] that makes them the only preparer of that session;
//! concurrent requesters for the same session queue behind the lease and are
//! answered once it completes, so the user is never asked for SSH consent
//! twice for one preparation.

use std::collections::{BTreeSet, HashMap, VecDeque};

use aither_sandbox::{
    BridgeAddress, ContainerIpcRoute, ShellRuntimeAvailability, ShellSessionRegistry, SshServer,
};
use tokio::sync::{mpsc, oneshot};

use crate::session::SessionId;

/// The backend that runs a session's commands on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HomeBackend {
    /// The local heel sandbox.
    Local,
    /// The session's container.
    Container {
        /// Id of the container the tools are mounted into.
        container_id: String,
        /// Host the container reaches the IPC bridge on. A restarted
        /// container keeps its id but can come back on another address, so
        /// the route is part of the key.
        ipc_host: String,
        /// Address the host binds the IPC bridge on.
        ipc_bind: BridgeAddress,
    },
}

impl HomeBackend {
    /// The container backend reached through `route`.
    #[must_use]
    pub fn container(container_id: &str, route: &ContainerIpcRoute) -> Self {
        Self::Container {
            container_id: container_id.to_owned(),
            ipc_host: route.host().to_owned(),
            ipc_bind: route.bind(),
        }
    }
}

/// The configuration a prepared registry is bound to: its home backend,
/// container and SSH server set.
///
/// The builder's backend setters are fed from the same values the key is
/// built from ([`Self::availability`] and the SSH servers passed to
/// [`Self::new`]), so equal keys describe the registry the builder would have
/// prepared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolEnvironmentKey {
    home: HomeBackend,
    ssh_servers: BTreeSet<(String, String)>,
}

impl ToolEnvironmentKey {
    /// Key for a home backend with the given attached SSH servers.
    #[must_use]
    pub fn new(home: HomeBackend, ssh_servers: &[SshServer]) -> Self {
        Self {
            home,
            ssh_servers: ssh_servers
                .iter()
                .map(|server| (server.name.clone(), server.target.clone()))
                .collect(),
        }
    }

    /// Shell backends the terminal offers for this configuration.
    #[must_use]
    pub fn availability(&self) -> ShellRuntimeAvailability {
        ShellRuntimeAvailability {
            local: self.home == HomeBackend::Local,
            container: matches!(self.home, HomeBackend::Container { .. }),
            ssh: !self.ssh_servers.is_empty(),
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
    Ready(Box<ShellSessionRegistry>),
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
    pub fn complete(mut self, registry: ShellSessionRegistry) {
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
        key: ToolEnvironmentKey,
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
        registry: Box<ShellSessionRegistry>,
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
    key: ToolEnvironmentKey,
    reply: oneshot::Sender<Acquired>,
}

#[derive(Debug)]
enum Slot {
    Ready {
        key: ToolEnvironmentKey,
        registry: Box<ShellSessionRegistry>,
    },
    Preparing {
        key: ToolEnvironmentKey,
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

    fn complete(&mut self, session: SessionId, generation: u64, registry: ShellSessionRegistry) {
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
    ) -> Option<(ToolEnvironmentKey, VecDeque<Waiter>)> {
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

    fn registry() -> ShellSessionRegistry {
        ShellSessionRegistry::new(ShellRuntimeAvailability {
            local: true,
            container: false,
            ssh: false,
        })
    }

    fn server(name: &str, target: &str) -> SshServer {
        SshServer {
            name: name.to_string(),
            target: target.to_string(),
        }
    }

    fn route(host: &str) -> ContainerIpcRoute {
        ContainerIpcRoute::new(
            host,
            BridgeAddress::try_from(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)).unwrap(),
        )
    }

    fn local_key(servers: &[SshServer]) -> ToolEnvironmentKey {
        ToolEnvironmentKey::new(HomeBackend::Local, servers)
    }

    async fn acquire(
        cache: &ToolEnvironmentCache,
        session: &SessionId,
        key: ToolEnvironmentKey,
    ) -> Acquired {
        tokio::time::timeout(Duration::from_secs(5), cache.acquire(session, key))
            .await
            .expect("acquire should answer")
            .expect("acquire should succeed")
    }

    async fn lease(
        cache: &ToolEnvironmentCache,
        session: &SessionId,
        key: ToolEnvironmentKey,
    ) -> PreparationLease {
        match acquire(cache, session, key).await {
            Acquired::Prepare(lease) => lease,
            Acquired::Ready(_) => panic!("expected a preparation lease"),
        }
    }

    async fn ready(
        cache: &ToolEnvironmentCache,
        session: &SessionId,
        key: ToolEnvironmentKey,
    ) -> ShellSessionRegistry {
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

    #[test]
    fn key_ignores_ssh_server_order_and_derives_availability() {
        let a = server("a", "user@a");
        let b = server("b", "user@b");
        assert_eq!(
            local_key(&[a.clone(), b.clone()]),
            local_key(&[b, a.clone()])
        );
        assert_ne!(
            local_key(std::slice::from_ref(&a)),
            local_key(&[server("a", "user@c")])
        );

        let container =
            ToolEnvironmentKey::new(HomeBackend::container("c1", &route("10.0.0.1")), &[a])
                .availability();
        assert!(!container.local && container.container && container.ssh);
        let local = local_key(&[]).availability();
        assert!(local.local && !local.container && !local.ssh);
    }

    #[tokio::test]
    async fn equal_key_prepares_once_then_adopts() {
        let cache = ToolEnvironmentCache::spawn();
        let session = SessionId::from_string("one");
        let key = local_key(&[server("a", "user@a")]);

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
        let attached = local_key(&[server("a", "user@a")]);

        lease(&cache, &session, attached.clone())
            .await
            .complete(registry());

        let detached = local_key(&[]);
        lease(&cache, &session, detached.clone())
            .await
            .complete(registry());
        ready(&cache, &session, detached).await;

        let retargeted = local_key(&[server("a", "user@b")]);
        lease(&cache, &session, retargeted.clone())
            .await
            .complete(registry());

        let container = ToolEnvironmentKey::new(
            HomeBackend::container("c1", &route("10.0.0.1")),
            &[server("a", "user@b")],
        );
        lease(&cache, &session, container.clone())
            .await
            .complete(registry());
        ready(&cache, &session, container).await;

        let restarted = ToolEnvironmentKey::new(
            HomeBackend::container("c1", &route("10.0.0.2")),
            &[server("a", "user@b")],
        );
        lease(&cache, &session, restarted.clone())
            .await
            .complete(registry());
        ready(&cache, &session, restarted).await;

        let other_container = ToolEnvironmentKey::new(
            HomeBackend::container("c2", &route("10.0.0.2")),
            &[server("a", "user@b")],
        );
        lease(&cache, &session, other_container).await;
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_preparation() {
        let cache = ToolEnvironmentCache::spawn();
        let session = SessionId::from_string("one");
        let key = local_key(&[server("a", "user@a")]);

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
        let key = local_key(&[]);

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
        let key = local_key(&[]);

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
        let key = local_key(&[]);

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
        let key = local_key(&[]);

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
