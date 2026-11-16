//! Finished subagents kept so their caller can continue them until their
//! prompt cache expires, each under a word leased for as long as it can be
//! continued.

use std::collections::HashMap;
use std::sync::Mutex;

use aither_agent::context::CacheLifetime;
use aither_core::{AnyIdAllocator, IdLease};

/// What a kept subagent says about its cache.
pub trait Expires {
    /// The cache lifetime it outlived, when it did: its cache is cold.
    fn expired(&self) -> Option<CacheLifetime>;
}

enum Slot<T> {
    /// Waiting to be continued, under the lease of its id.
    Idle(IdLease, Box<T>),
    /// Checked out by a continuation in flight, which holds the lease.
    Running,
    /// Its cache lifetime passed; the context was dropped and its id
    /// released.
    Expired(CacheLifetime),
}

/// The id a call's subagent is continued by.
pub struct CallId {
    /// The leased word.
    pub lease: IdLease,
    /// Whether `resume` checked it out.
    pub resumed: bool,
}

/// The subagents a session may continue.
pub struct Finished<T> {
    slots: Mutex<HashMap<String, Slot<T>>>,
}

impl<T> Default for Finished<T> {
    fn default() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
        }
    }
}

impl<T: Expires> Finished<T> {
    /// Leases the id of a fresh subagent from `ids`.
    ///
    /// # Errors
    /// Returns why no word could be leased.
    pub fn lease(ids: &AnyIdAllocator) -> anyhow::Result<CallId> {
        Ok(CallId {
            lease: ids.lease()?,
            resumed: false,
        })
    }

    fn expire(slots: &mut HashMap<String, Slot<T>>) {
        for slot in slots.values_mut() {
            if let Slot::Idle(_, finished) = slot
                && let Some(lifetime) = finished.expired()
            {
                *slot = Slot::Expired(lifetime);
            }
        }
    }

    fn slots(&self) -> std::sync::MutexGuard<'_, HashMap<String, Slot<T>>> {
        self.slots.lock().expect("finished subagents lock poisoned")
    }

    /// Takes the subagent out for a continuation.
    ///
    /// # Errors
    /// Returns why `id` cannot be continued.
    pub fn checkout(&self, id: &str) -> anyhow::Result<(CallId, Box<T>)> {
        let mut slots = self.slots();
        Self::expire(&mut slots);
        match slots.get_mut(id) {
            None => anyhow::bail!("unknown subagent id {id}"),
            Some(Slot::Expired(lifetime)) => {
                let CacheLifetime::Limited(lifetime) = lifetime else {
                    unreachable!("a permanent cache lifetime never expires");
                };
                anyhow::bail!(
                    "subagent {id} expired: its prompt-cache entry lived {lifetime:?} and its context was released"
                );
            }
            Some(Slot::Running) => anyhow::bail!("subagent {id} is still running a continuation"),
            Some(slot @ Slot::Idle(..)) => {
                let Slot::Idle(lease, finished) = std::mem::replace(slot, Slot::Running) else {
                    unreachable!("slot matched as idle");
                };
                Ok((
                    CallId {
                        lease,
                        resumed: true,
                    },
                    finished,
                ))
            }
        }
    }

    /// Keeps a finished subagent under `id`, the lease of its call.
    pub fn store(&self, id: IdLease, finished: T) {
        let mut slots = self.slots();
        Self::expire(&mut slots);
        slots.insert(id.to_string(), Slot::Idle(id, Box::new(finished)));
    }

    /// Releases `id` only while a continuation holds it checked out.
    pub fn release_checked_out(&self, id: &str) {
        let mut slots = self.slots();
        if matches!(slots.get(id), Some(Slot::Running)) {
            slots.remove(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::time::Duration;

    use super::*;

    /// A kept subagent whose cache can be made to expire.
    struct Kept(Cell<Option<CacheLifetime>>);

    impl Expires for Kept {
        fn expired(&self) -> Option<CacheLifetime> {
            self.0.get()
        }
    }

    fn lease(ids: &AnyIdAllocator) -> IdLease {
        Finished::<Kept>::lease(ids).expect("a word is free").lease
    }

    #[test]
    fn a_finished_subagent_is_continued_under_the_id_of_its_call() {
        let ids = AnyIdAllocator::default();
        let finished = Finished::default();
        let id = lease(&ids);
        let word = id.to_string();
        finished.store(id, Kept(Cell::new(None)));
        let (call, _) = finished.checkout(&word).expect("it is idle");
        assert!(call.resumed);
        assert_eq!(call.lease.to_string(), word);
        let busy = finished
            .checkout(&word)
            .err()
            .expect("one continuation at a time");
        assert!(busy.to_string().contains("still running"), "{busy}");
        finished.store(call.lease, Kept(Cell::new(None)));
        finished.checkout(&word).expect("idle again");
    }

    #[test]
    fn an_unknown_id_is_refused() {
        let finished = Finished::<Kept>::default();
        let error = finished.checkout("amber").err().expect("unknown");
        assert!(error.to_string().contains("unknown subagent id"), "{error}");
    }

    #[test]
    fn a_subagent_whose_cache_lifetime_passed_is_expired_and_says_so() {
        let ids = AnyIdAllocator::default();
        let finished = Finished::default();
        let id = lease(&ids);
        let word = id.to_string();
        finished.store(
            id,
            Kept(Cell::new(Some(CacheLifetime::Limited(
                Duration::from_secs(300),
            )))),
        );
        let error = finished.checkout(&word).err().expect("expired");
        assert!(error.to_string().contains("expired"), "{error}");
    }

    #[test]
    fn releasing_a_checked_out_id_frees_it_and_leaves_an_idle_one_alone() {
        let ids = AnyIdAllocator::default();
        let finished = Finished::default();
        let idle = lease(&ids);
        let idle_word = idle.to_string();
        finished.store(idle, Kept(Cell::new(None)));
        finished.release_checked_out(&idle_word);
        let (running, _) = finished.checkout(&idle_word).expect("still kept");
        let running_word = running.lease.to_string();
        finished.release_checked_out(&running_word);
        let gone = finished.checkout(&running_word).err().expect("released");
        assert!(gone.to_string().contains("unknown"), "{gone}");
    }
}
