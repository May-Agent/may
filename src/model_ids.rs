//! The one namespace of model-facing ids.
//!
//! Every id the model reads or types back — a background job, a stored
//! output, a finished subagent, an agent, an `ask_user` question, a tunnel, a
//! scheduled task, a saved snapshot — is one word of [`WORDS`], each a common
//! English word that is a single `o200k_base` token with or without a leading
//! space. All of them are leased from the one allocator the daemon's
//! [`ChatDb`](crate::db::ChatDb) holds, so no two live ids share a word
//! whatever their kinds.
//!
//! An id that also names something outside the model (a database row, a
//! session sandbox) keeps its internal id there, and its word is an alias:
//! [`AliasKind`] names the kinds whose aliases the database keeps, so they
//! name the same thing across daemon restarts.

use aither_core::{AnyIdAllocator, WordAllocator};

/// The words model-facing ids are leased from, one per line.
const WORDS: &str = include_str!("model_ids/words.txt");

/// A fresh namespace over [`WORDS`] with no word leased.
#[must_use]
pub fn allocator() -> AnyIdAllocator {
    AnyIdAllocator::new(WordAllocator::new(WORDS.lines()))
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

/// A kind of thing whose model-facing word is an alias the database keeps
/// for as long as the thing can be addressed.
pub trait AliasKind {
    /// The kind's name in the `model_aliases` table.
    const KIND: &'static str;
}

/// A live main agent, aliasing its session id.
#[derive(Debug, Clone, Copy)]
pub struct AgentAlias;

impl AliasKind for AgentAlias {
    const KIND: &'static str = "agent";
}

/// An open `ask_user` question, aliasing its request id.
#[derive(Debug, Clone, Copy)]
pub struct QuestionAlias;

impl AliasKind for QuestionAlias {
    const KIND: &'static str = "question";
}

/// A scheduled task, aliasing its task id.
#[derive(Debug, Clone, Copy)]
pub struct TaskAlias;

impl AliasKind for TaskAlias {
    const KIND: &'static str = "task";
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
        // Building the allocator checks every word's shape again.
        let _ = allocator();
    }

    #[test]
    fn live_leases_never_share_a_word() {
        let ids = allocator();
        let leases: Vec<_> = (0..WORDS.lines().count())
            .map(|_| ids.lease().expect("a free word"))
            .collect();
        let distinct: BTreeSet<_> = leases.iter().map(aither_core::IdLease::as_str).collect();
        assert_eq!(distinct.len(), leases.len());
        assert!(ids.lease().is_err(), "every word is leased");
    }
}
