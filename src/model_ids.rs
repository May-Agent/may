//! Model-facing ids: the names the model reads and types back.
//!
//! There are two spaces of them, disjoint by construction:
//!
//! - A **session name** names a session and the main agent running it. It is
//!   [`SESSION_NAME_WORDS`] words of [`WORDS`] joined by `-`, issued once by
//!   the daemon's one global generator
//!   ([`ChatDb::issue_session_name`](crate::db::ChatDb::issue_session_name)),
//!   never changed and never issued again.
//! - Every other id — a background job, a stored output, a subagent, an
//!   `ask_user` question, a tunnel, a scheduled task, a saved snapshot or
//!   image — is one word of [`WORDS`], leased from the pool of the session it
//!   belongs to ([`SessionIds`]). No two live ids of one session share a
//!   word, whatever their kinds.
//!
//! A pool word is one or more lowercase letters (aither refuses any other
//! lease), and a session name always holds a `-`, so no per-session id ever
//! equals a session name.
//!
//! Each word of [`WORDS`] is a common English word that is a single
//! `o200k_base` token with or without a leading space.

use std::collections::HashMap;
use std::sync::Mutex;

use aither_core::{AnyIdAllocator, IdLease, IdsExhausted, WordAllocator, WordUnavailable};

/// The words model-facing ids are made of, one per line.
const WORDS: &str = include_str!("model_ids/words.txt");

/// How many words a session name joins.
pub const SESSION_NAME_WORDS: u32 = 2;

/// What joins the words of a session name. No pool word holds it.
const SESSION_NAME_SEPARATOR: char = '-';

/// How many distinct session names the generator can issue.
///
/// # Panics
/// Never: the word list is far smaller than `u64` can count.
#[must_use]
pub fn session_name_capacity() -> u64 {
    let words = u64::try_from(WORDS.lines().count()).expect("the word count fits in u64");
    words.pow(SESSION_NAME_WORDS)
}

/// A candidate session name drawn from `rng`: [`SESSION_NAME_WORDS`] words of
/// [`WORDS`] joined by `-`. The generator issues it only if no session was
/// ever named by it.
#[must_use]
pub fn draw_session_name(rng: &mut fastrand::Rng) -> String {
    let words: Vec<&str> = WORDS.lines().collect();
    let mut name = String::new();
    for index in 0..SESSION_NAME_WORDS {
        if index > 0 {
            name.push(SESSION_NAME_SEPARATOR);
        }
        name.push_str(words[rng.usize(..words.len())]);
    }
    name
}

/// Where a pool holds the leases of the ids the database keeps (open
/// questions, scheduled tasks), which outlive whatever leased them.
pub trait StoredIds: Send + Sync + 'static {
    /// Keeps `lease` for an id the database now stores, until
    /// [`release`](Self::release) names its word.
    fn keep(&self, lease: IdLease);

    /// Ends the lease of the stored id `word`: what it named left the
    /// database.
    fn release(&self, word: &str);
}

/// The stored ids' leases of a pool this process owns, by word.
#[derive(Debug, Default)]
struct HeldStoredIds(Mutex<HashMap<String, IdLease>>);

impl HeldStoredIds {
    fn leases(&self) -> std::sync::MutexGuard<'_, HashMap<String, IdLease>> {
        self.0.lock().expect("session id leases lock poisoned")
    }
}

impl StoredIds for HeldStoredIds {
    fn keep(&self, lease: IdLease) {
        self.leases().insert(lease.to_string(), lease);
    }

    fn release(&self, word: &str) {
        let released = self.leases().remove(word);
        drop(released);
    }
}

/// The model-facing ids of one session.
///
/// It is the pool the session's terminals, subagents, questions, tunnels and
/// scheduled tasks lease their words from, and holds the leases of the ones
/// the database keeps (open questions, scheduled tasks). The daemon owns
/// every session's pool ([`restore`](Self::restore)); another process leases
/// from the daemon's ([`new`](Self::new) over
/// [`DaemonIds`](crate::client::ids::DaemonIds)).
pub struct SessionIds {
    ids: AnyIdAllocator,
    stored: Box<dyn StoredIds>,
}

impl std::fmt::Debug for SessionIds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionIds").finish_non_exhaustive()
    }
}

impl SessionIds {
    /// A pool over [`WORDS`] in which the words of the session's stored ids,
    /// `stored`, are leased.
    ///
    /// # Errors
    /// [`WordUnavailable`] when a stored word is not in [`WORDS`] or two
    /// stored ids share one.
    pub fn restore(stored: impl IntoIterator<Item = String>) -> Result<Self, WordUnavailable> {
        let ids = AnyIdAllocator::new(WordAllocator::new(WORDS.lines()));
        let held = HeldStoredIds::default();
        for word in stored {
            held.keep(ids.claim(&word)?);
        }
        Ok(Self::new(ids, held))
    }

    /// A pool leasing from `ids` and keeping stored ids' leases in `stored`.
    #[must_use]
    pub fn new(ids: AnyIdAllocator, stored: impl StoredIds) -> Self {
        Self {
            ids,
            stored: Box::new(stored),
        }
    }

    /// The allocator the session's terminals lease from.
    #[must_use]
    pub const fn allocator(&self) -> &AnyIdAllocator {
        &self.ids
    }

    /// Leases a word no live id of the session holds.
    ///
    /// # Errors
    /// [`IdsExhausted`] when every word is leased.
    pub fn lease(&self) -> Result<IdLease, IdsExhausted> {
        self.ids.lease()
    }

    /// Keeps `lease` for an id the database now stores, until
    /// [`release`](Self::release) names its word.
    pub fn keep(&self, lease: IdLease) {
        self.stored.keep(lease);
    }

    /// Ends the lease of the stored id `word`: what it named left the
    /// database.
    pub fn release(&self, word: &str) {
        self.stored.release(word);
    }
}

/// Writes `data` to a new file of `dir` named `<word>.<ext>`, returning the
/// file name.
///
/// The word is leased from `ids` while the file is named, so it is no live
/// id's word then, and no file already in `dir` is named by it.
///
/// # Errors
/// I/O failures, or every word being leased or naming a file of `dir`.
pub async fn write_word_file(
    ids: &AnyIdAllocator,
    dir: &std::path::Path,
    ext: &str,
    data: &[u8],
) -> std::io::Result<String> {
    use futures_lite::{AsyncWriteExt, StreamExt};

    let mut taken = std::collections::HashSet::new();
    let mut entries = async_fs::read_dir(dir).await?;
    while let Some(entry) = entries.try_next().await? {
        if let Some(stem) = std::path::Path::new(&entry.file_name()).file_stem() {
            taken.insert(stem.to_string_lossy().into_owned());
        }
    }
    // Words skipped stay leased until a free one is found, so the allocator
    // hands out another.
    let mut skipped = Vec::new();
    let named = loop {
        let lease = match ids.lease() {
            Ok(lease) => lease,
            Err(exhausted) => break Err(std::io::Error::other(exhausted)),
        };
        if taken.contains(lease.as_str()) {
            skipped.push(lease);
            continue;
        }
        let name = format!("{lease}.{ext}");
        match async_fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(&name))
            .await
        {
            Ok(file) => break Ok((file, name)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                skipped.push(lease);
            }
            Err(error) => break Err(error),
        }
    };
    drop(skipped);
    let (mut file, name) = named?;
    file.write_all(data).await?;
    file.flush().await?;
    Ok(name)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn every_word_is_one_o200k_token_with_or_without_a_leading_space() {
        let o200k = tiktoken::get_encoding("o200k_base").expect("the o200k_base vocabulary");
        let split: Vec<_> = WORDS
            .lines()
            .filter(|word| {
                o200k.encode(word).len() != 1 || o200k.encode(&format!(" {word}")).len() != 1
            })
            .collect();
        assert!(split.is_empty(), "words that are not one token: {split:?}");
    }

    #[test]
    fn the_list_is_sorted_unique_lowercase_words() {
        let words: Vec<_> = WORDS.lines().collect();
        assert!(words.len() > 1000, "{} words", words.len());
        assert!(
            words.windows(2).all(|pair| pair[0] < pair[1]),
            "sorted, unique"
        );
        assert!(
            words
                .iter()
                .all(|word| word.len() >= 3 && word.bytes().all(|byte| byte.is_ascii_lowercase()))
        );
        // Building a pool checks every word's shape again.
        let _ = SessionIds::restore([]).expect("a pool");
    }

    #[test]
    fn live_ids_of_a_session_never_share_a_word() {
        let ids = SessionIds::restore([]).expect("a pool");
        let leases: Vec<_> = (0..WORDS.lines().count())
            .map(|_| ids.lease().expect("a free word"))
            .collect();
        let distinct: BTreeSet<_> = leases.iter().map(IdLease::as_str).collect();
        assert_eq!(distinct.len(), leases.len());
        assert!(ids.lease().is_err(), "every word is leased");
    }

    #[test]
    fn no_pool_word_is_a_session_name() {
        let mut rng = fastrand::Rng::with_seed(842);
        let ids = SessionIds::restore([]).expect("a pool");
        let words: BTreeSet<String> = (0..WORDS.lines().count())
            .map(|_| ids.lease().expect("a free word").to_string())
            .collect();
        for _ in 0..1000 {
            let name = draw_session_name(&mut rng);
            assert_eq!(
                name.split(SESSION_NAME_SEPARATOR).count(),
                usize::try_from(SESSION_NAME_WORDS).expect("a small count")
            );
            assert!(!words.contains(&name), "{name} is a pool word");
        }
        assert!(
            words
                .iter()
                .all(|word| !word.contains(SESSION_NAME_SEPARATOR))
        );
        assert!(session_name_capacity() > 1_000_000);
    }

    #[test]
    fn a_stored_word_stays_leased_until_released() {
        let ids = SessionIds::restore(["river".to_string()]).expect("a pool");
        assert!(
            ids.allocator().claim("river").is_err(),
            "restored as leased"
        );
        ids.release("river");
        let again = ids.allocator().claim("river").expect("released");
        ids.keep(again);
        assert!(ids.allocator().claim("river").is_err(), "kept");
        assert!(
            SessionIds::restore(["river".to_string(), "river".to_string()]).is_err(),
            "two stored ids never share a word"
        );
    }
}
