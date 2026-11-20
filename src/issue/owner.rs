//! One owner per open issue.
//!
//! An agent owns the open issues its session's `tasks.md` lists. Ownership is
//! read from the live `tasks.md` of every session the daemon hosts, so it
//! cannot drift from the files. The file is a plain document the agent edits
//! in its sandbox; the daemon observes it after each tool call and
//! [`IssueOwners::admit`] reverts a change that adds an issue another agent
//! owns.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use askama::Template;

use super::{IssueError, IssueState, IssueStore, ResolvedIssueStore, resolve_bound_issue_store};
use crate::agent_link::{AgentDirectory, AgentLinkError, AgentTasks, AttributeText};

/// The agent whose `tasks.md` lists an issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueOwner {
    /// Session the agent runs; `agent send` addresses it by this id.
    pub session_id: String,
    /// The agent's name.
    pub name: String,
}

impl IssueOwner {
    /// The name, safe inside a quoted sentence.
    #[must_use]
    pub fn display_name(&self) -> String {
        AttributeText(&self.name).to_string()
    }
}

/// An issue a rejected change tried to take, and its owner.
#[derive(Debug, Clone)]
pub struct OwnedConflict {
    /// Issue number.
    pub number: i64,
    /// Agent that owns it.
    pub owner: IssueOwner,
}

/// A `tasks.md` change that added issues other agents own.
#[derive(Debug, Clone, Template)]
#[template(path = "issue_owned.txt", escape = "none")]
pub struct IssueOwned {
    /// Every issue of the change that is owned elsewhere.
    pub conflicts: Vec<OwnedConflict>,
}

impl std::error::Error for IssueOwned {}

impl IssueOwned {
    /// The error as the notice the rejected agent receives.
    ///
    /// # Panics
    /// When the compile-time template fails to render.
    #[must_use]
    pub fn notice(&self) -> String {
        #[derive(Template)]
        #[template(path = "issue_owned_notice.txt", escape = "none")]
        struct Notice {
            error: String,
        }
        Notice {
            error: self.to_string(),
        }
        .render()
        .expect("issue owned notice template must render")
    }
}

/// Why ownership could not be decided or a change was refused.
#[derive(Debug, thiserror::Error)]
pub enum OwnershipError {
    /// The sessions or their `tasks.md` could not be read.
    #[error(transparent)]
    Link(#[from] AgentLinkError),
    /// An issue backend failed.
    #[error(transparent)]
    Issue(#[from] IssueError),
    /// `tasks.md` could not be reverted.
    #[error("failed to revert tasks.md: {0}")]
    Revert(#[source] std::io::Error),
    /// The change adds an issue another agent owns.
    #[error(transparent)]
    Owned(#[from] IssueOwned),
}

impl From<OwnershipError> for IssueError {
    fn from(error: OwnershipError) -> Self {
        Self::Ownership(error.to_string())
    }
}

/// The owners of one issue namespace's open issues, by number.
#[derive(Debug, Default)]
pub struct OwnerIndex(HashMap<i64, IssueOwner>);

impl OwnerIndex {
    /// The owner of issue `number`, if an agent lists it.
    #[must_use]
    pub fn get(&self, number: i64) -> Option<&IssueOwner> {
        self.0.get(&number)
    }
}

/// The issues one agent's `tasks.md` lists, in the namespace of its backend.
struct Claims {
    owner: IssueOwner,
    key: String,
    numbers: BTreeSet<i64>,
}

/// Ownership of issues across every session the daemon hosts.
#[derive(Debug, Clone)]
pub struct IssueOwners {
    directory: AgentDirectory,
    may_config: std::sync::Arc<crate::config::MayConfig>,
    forge_auth: crate::forge::LiveForgeAuth,
}

impl IssueOwners {
    /// Ownership over the sessions of `directory`.
    #[must_use]
    pub fn new(directory: AgentDirectory) -> Self {
        Self {
            directory,
            may_config: std::sync::Arc::new(crate::config::MayConfig::default()),
            forge_auth: crate::forge::LiveForgeAuth::default(),
        }
    }

    /// Resolve agents' remote issue backends with the `[forge]`
    /// configuration `may_config` holds.
    #[must_use]
    pub fn with_may_config(
        mut self,
        may_config: impl Into<std::sync::Arc<crate::config::MayConfig>>,
    ) -> Self {
        self.may_config = may_config.into();
        self
    }

    /// Resolve agents' GitLab stores with the live authenticator
    /// `forge_auth`.
    #[must_use]
    pub fn with_forge_auth(mut self, forge_auth: crate::forge::LiveForgeAuth) -> Self {
        self.forge_auth = forge_auth;
        self
    }

    /// Owners of the issues of the backend named `key` (its
    /// [`IssueStore::search_key`]).
    ///
    /// # Errors
    /// [`OwnershipError`] when the sessions cannot be read.
    pub async fn owners_in(&self, key: &str) -> Result<OwnerIndex, OwnershipError> {
        let agents = self.directory.agent_tasks().await?;
        let mut index = HashMap::new();
        for claims in self.claims_of(&agents).await {
            if claims.key == key {
                for number in claims.numbers {
                    index.entry(number).or_insert_with(|| claims.owner.clone());
                }
            }
        }
        Ok(OwnerIndex(index))
    }

    /// The owner of one issue of the backend named `key`.
    ///
    /// # Errors
    /// [`OwnershipError`] when the sessions cannot be read.
    pub async fn owner_of(
        &self,
        key: &str,
        number: i64,
    ) -> Result<Option<IssueOwner>, OwnershipError> {
        Ok(self.owners_in(key).await?.0.remove(&number))
    }

    /// Accept the current `tasks.md` of `session_id`, whose previously
    /// accepted content is `accepted`, or revert it.
    ///
    /// A change is refused when it adds a reference to an open issue that
    /// another agent's `tasks.md` lists; the file then returns to `accepted`.
    ///
    /// # Errors
    /// [`OwnershipError::Owned`] when the change is refused; the other
    /// variants when ownership could not be decided, which leaves the file and
    /// `accepted` as they were.
    pub async fn admit(
        &self,
        session_id: &str,
        accepted: &mut Option<String>,
    ) -> Result<(), OwnershipError> {
        let agents = self.directory.agent_tasks().await?;
        let Some(me) = agents
            .iter()
            .find(|agent| agent.status.session_id == session_id)
        else {
            return Ok(());
        };
        let Some(current) = me.tasks.clone() else {
            *accepted = None;
            return Ok(());
        };
        if accepted.as_deref() == Some(current.as_str()) {
            return Ok(());
        }
        let store = self.store_of(me).await?;
        let key = store.search_key();
        let before = accepted
            .as_deref()
            .map_or_default(|tasks| numbers_in(tasks, &key));
        let gained = numbers_in(&current, &key)
            .difference(&before)
            .copied()
            .collect::<Vec<_>>();
        let others = agents
            .iter()
            .filter(|agent| agent.status.session_id != session_id)
            .cloned()
            .collect::<Vec<_>>();
        let others = if gained.is_empty() {
            Vec::new()
        } else {
            self.claims_of(&others).await
        };
        let mut conflicts = Vec::new();
        for number in gained {
            let Some(claims) = others
                .iter()
                .find(|claims| claims.key == key && claims.numbers.contains(&number))
            else {
                continue;
            };
            match store.get(number).await {
                Ok(detail) if detail.state == IssueState::Open => conflicts.push(OwnedConflict {
                    number,
                    owner: claims.owner.clone(),
                }),
                Ok(_) | Err(IssueError::NotFound(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
        if conflicts.is_empty() {
            *accepted = Some(current);
            return Ok(());
        }
        match accepted {
            Some(previous) => async_fs::write(&me.tasks_path, previous).await,
            None => async_fs::remove_file(&me.tasks_path).await,
        }
        .map_err(OwnershipError::Revert)?;
        Err(IssueOwned { conflicts }.into())
    }

    /// The issue backend of an agent's working directory.
    async fn store_of(&self, agent: &AgentTasks) -> Result<ResolvedIssueStore, IssueError> {
        let cwd: PathBuf = agent.status.cwd.clone();
        let workspace_config = agent.workspace_config.clone();
        let binding = agent.binding.clone();
        let db = self.directory.db();
        let may_config = self.may_config.clone();
        let forge_auth = self.forge_auth.clone();
        blocking::unblock(move || {
            resolve_bound_issue_store(
                binding.as_ref(),
                &cwd,
                &workspace_config,
                may_config.as_ref(),
                &forge_auth,
                db,
            )
        })
        .await
    }

    /// What each agent lists. An agent whose backend cannot be resolved (its
    /// directory is gone) lists nothing. Backend resolution runs in
    /// parallel, bounded so many agents cannot stampede it.
    async fn claims_of(&self, agents: &[AgentTasks]) -> Vec<Claims> {
        use futures_util::StreamExt as _;
        let claims: Vec<_> = agents.iter().map(|agent| self.claims_one(agent)).collect();
        futures_util::stream::iter(claims)
            .buffered(4)
            .filter_map(async |claims| claims)
            .collect()
            .await
    }

    /// The claims of one agent; `None` when it lists none or its backend
    /// cannot be resolved.
    async fn claims_one(&self, agent: &AgentTasks) -> Option<Claims> {
        let tasks = agent.tasks.as_deref()?;
        if references(tasks).is_empty() {
            return None;
        }
        let store = match self.store_of(agent).await {
            Ok(store) => store,
            Err(error) => {
                tracing::warn!(
                    session_id = %agent.status.session_id,
                    %error,
                    "issue ownership skips a session whose issue backend cannot be resolved"
                );
                return None;
            }
        };
        let key = store.search_key();
        Some(Claims {
            owner: IssueOwner {
                session_id: agent.status.session_id.clone(),
                name: agent.status.name.clone(),
            },
            numbers: numbers_in(tasks, &key),
            key,
        })
    }
}

/// An issue reference of a `tasks.md`: `#N` in the agent's own backend, or
/// `namespace/…/repo#N` naming a forge repository, with or without its
/// host.
#[derive(Debug, PartialEq, Eq)]
struct Reference {
    repository: Option<String>,
    number: i64,
}

/// The numbers `tasks` lists in the backend named `key`.
fn numbers_in(tasks: &str, key: &str) -> BTreeSet<i64> {
    let coordinate = key
        .strip_prefix("github:")
        .or_else(|| key.strip_prefix("gitlab:"))
        .unwrap_or(key);
    references(tasks)
        .into_iter()
        .filter(|reference| {
            reference
                .repository
                .as_ref()
                .is_none_or(|repository| super::repository_matches(repository, coordinate))
        })
        .map(|reference| reference.number)
        .collect()
}

/// The issues `tasks` lists: the subject of each unfinished list item, which
/// is the first token of a `- ` or `* ` item, nested items and `- [ ]` items
/// included, anywhere but inside a free-text section. The sections come from
/// [`may_tasks::TasksFile::parse`], so `## Notes` and `## Decisions` prose,
/// fenced code included, never reads as a claim, and references elsewhere in
/// a line are mentions, not claims.
fn references(tasks: &str) -> Vec<Reference> {
    let file = may_tasks::TasksFile::parse(tasks);
    let lines: Vec<&str> = tasks.lines().collect();
    // A free-text span's lines run to the next recorded heading.
    let mut excluded = Vec::new();
    for (index, span) in file.spans().iter().enumerate() {
        if matches!(
            span.section,
            may_tasks::Section::Notes | may_tasks::Section::Decisions
        ) {
            let end = file
                .spans()
                .get(index + 1)
                .map_or(lines.len() + 1, |next| next.heading);
            excluded.extend(span.heading + 1..end);
        }
    }
    let mut excluded = excluded.into_iter().peekable();
    let mut found = Vec::new();
    // A fenced code block keeps its `- #N` lines from claiming, wherever
    // it sits — the list items inside are literals, not task rows.
    let mut fence: Option<&str> = None;
    for (number, &line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let marker = trimmed
            .strip_prefix("```")
            .map(|_| "```")
            .or_else(|| trimmed.strip_prefix("~~~").map(|_| "~~~"));
        match (fence, marker) {
            (Some(mark), Some(m)) if m == mark => fence = None,
            (Some(_), _) => continue,
            (None, Some(m)) => {
                fence = Some(m);
                continue;
            }
            (None, None) => {}
        }
        let at = number + 1;
        while excluded.peek().is_some_and(|next| *next < at) {
            excluded.next();
        }
        if excluded.peek() == Some(&at) {
            continue;
        }
        if let Some(reference) = item(line).and_then(subject) {
            found.push(reference);
        }
    }
    found
}

/// The item text of a `- ` or `* ` line, wherever indented.
fn item(line: &str) -> Option<&str> {
    line.trim_start()
        .strip_prefix("- ")
        .or_else(|| line.trim_start().strip_prefix("* "))
}

/// The issue an unfinished list item is about: the first token of its text,
/// after an optional `[ ]`.
fn subject(item: &str) -> Option<Reference> {
    let item = item.strip_prefix("[ ]").map_or(item, str::trim_start);
    let token = item.split_whitespace().next()?;
    let (prefix, rest) = token.split_once('#')?;
    let is_name = |name: &str| {
        !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-.".contains(&byte))
    };
    let repository = if prefix.is_empty() {
        None
    } else {
        // A repository coordinate is two or more `name` segments joined by
        // `/`: `owner/repo` or `gitlab.example/group/sub/repo`.
        if !prefix.contains('/') || !prefix.split('/').all(is_name) {
            return None;
        }
        Some(prefix.to_string())
    };
    let (digits, after) = rest.split_at(
        rest.find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len()),
    );
    if digits.is_empty() || after.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some(Reference {
        repository,
        number: digits.parse().ok()?,
    })
}

#[cfg(test)]
mod tests;
