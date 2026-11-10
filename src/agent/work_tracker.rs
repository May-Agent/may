//! The [`WorkTracker`](crate::harness::working_document::WorkTracker) over the session's issue
//! and PR backends, which `tasks.md` reads its rows' state and its roots'
//! trees through.

use std::path::{Path, PathBuf};

use crate::harness::working_document::{
    RelatedWork, WorkComment, WorkItem, WorkKind, WorkRef, WorkState, WorkStatus, WorkTrackerError,
};

use crate::db::{ChatDb, PrRow};
use crate::issue::{
    IssueDetail, IssueError, IssueState, IssueStatus, IssueStore, RelatedIssue, ResolvedIssueStore,
};
use crate::repo_clone::SessionRepos;

/// Reads issues from the session's issue store and PRs from May's PR
/// records, numbered as the `pr` command numbers them.
#[derive(Debug, Clone)]
pub struct MayWorkTracker {
    issues: ResolvedIssueStore,
    db: ChatDb,
    repos: SessionRepos,
    /// The session's working directory: the repository an unqualified PR
    /// reference means when the session has no clone.
    cwd: PathBuf,
}

impl MayWorkTracker {
    /// The tracker of the session whose sandbox is `session_root`.
    #[must_use]
    pub fn new(
        issues: ResolvedIssueStore,
        db: ChatDb,
        session_root: &Path,
        session_id: impl Into<String>,
        cwd: PathBuf,
    ) -> Self {
        Self {
            issues,
            repos: SessionRepos::new(session_root, session_id, db.clone()),
            db,
            cwd,
        }
    }

    /// The issue store's own repository, when it names one.
    fn our_repository(&self) -> Option<String> {
        self.issues.info().repository
    }

    /// Refuses a reference qualified by a repository other than the issue
    /// store's own; owner and repository names compare without case.
    fn issue_number(&self, reference: &WorkRef) -> Result<i64, WorkTrackerError> {
        if let Some(repository) = reference.repository()
            && !self
                .our_repository()
                .is_some_and(|ours| ours.eq_ignore_ascii_case(repository))
        {
            return Err(unavailable(
                reference,
                "it is not this session's issue repository",
            ));
        }
        number(reference)
    }

    async fn issue(&self, reference: &WorkRef) -> Result<WorkItem, WorkTrackerError> {
        let number = self.issue_number(reference)?;
        let detail = self
            .issues
            .get(number)
            .await
            .map_err(|error| issue_error(reference, &error))?;
        issue_item(reference, self.our_repository().as_deref(), detail)
    }

    async fn issue_status(&self, reference: &WorkRef) -> Result<WorkStatus, WorkTrackerError> {
        let number = self.issue_number(reference)?;
        let IssueStatus { state, blocked_by } = self
            .issues
            .status(number)
            .await
            .map_err(|error| issue_error(reference, &error))?;
        let ours = self.our_repository();
        Ok(WorkStatus {
            reference: reference.clone(),
            state: issue_state(state),
            blocked_by: related_all(reference, ours.as_deref(), blocked_by)?,
        })
    }

    /// The PR record `reference` names.
    ///
    /// A qualified `PR owner/repo#N` is read in each of the session's clones
    /// of that repository, and in the working directory's repository when it
    /// is that one. An unqualified one is read as `pr` reads it: in the
    /// session's clones, or the working directory's repository when it has
    /// none; a number several of them hold must be qualified.
    async fn pull_request(&self, reference: &WorkRef) -> Result<PrRow, WorkTrackerError> {
        let number = number(reference)?;
        let clones = self
            .repos
            .clones()
            .await
            .map_err(|error| unavailable(reference, &error.to_string()))?;
        let keys: Vec<String> = match reference.repository() {
            Some(repository) => {
                // Which GitHub repository each local clone, and the working
                // directory, comes from is read from git, off the executor.
                let local: Vec<(String, PathBuf)> = clones
                    .iter()
                    .filter(|clone| clone.local)
                    .map(|clone| (clone.repo_key.clone(), PathBuf::from(&clone.source)))
                    .collect();
                let cwd = self.cwd.clone();
                let (local, cwd) = blocking::unblock(move || {
                    let local: Vec<(String, Option<String>)> = local
                        .into_iter()
                        .map(|(key, source)| (key, github_repository(&source)))
                        .collect();
                    let cwd = crate::repo_search::worktree_root(&cwd)
                        .and_then(|worktree| github_repository(&worktree));
                    (local, cwd)
                })
                .await;
                let mut keys: Vec<String> = clones
                    .iter()
                    .filter(|clone| !clone.local && clone.repo_key.eq_ignore_ascii_case(repository))
                    .map(|clone| clone.repo_key.clone())
                    .chain(
                        local
                            .into_iter()
                            .filter(|(_, github)| {
                                github
                                    .as_deref()
                                    .is_some_and(|github| github.eq_ignore_ascii_case(repository))
                            })
                            .map(|(key, _)| key),
                    )
                    .collect();
                // Uncloned, the working directory's PRs are recorded under
                // the key its clone would get: the remote's or its identity.
                if let Some(github) = cwd.filter(|github| github.eq_ignore_ascii_case(repository)) {
                    keys.push(github);
                    keys.push(self.cwd_identity(reference)?);
                }
                keys
            }
            None if clones.is_empty() => vec![self.cwd_identity(reference)?],
            None => clones.into_iter().map(|clone| clone.repo_key).collect(),
        };
        let found =
            futures_util::future::join_all(keys.iter().map(|key| self.db.get_pr(key, number)))
                .await;
        let mut rows: Vec<PrRow> = Vec::new();
        for row in found {
            match row {
                Ok(Some(row)) if !rows.iter().any(|known| known.id == row.id) => rows.push(row),
                Ok(_) => {}
                Err(error) => return Err(unavailable(reference, &error.to_string())),
            }
        }
        match <[PrRow; 1]>::try_from(rows) {
            Ok([row]) => Ok(row),
            Err(rows) if rows.is_empty() => Err(WorkTrackerError::NotFound(reference.clone())),
            Err(_) => Err(unavailable(
                reference,
                "several of this session's repositories have a PR with that number; write it as PR owner/repo#N",
            )),
        }
    }

    /// The working directory's repository identity, the key `pr` records
    /// its PRs under when it is cloned locally.
    fn cwd_identity(&self, reference: &WorkRef) -> Result<String, WorkTrackerError> {
        crate::issue::repository_identity(&self.cwd)
            .map(|identity| identity.display().to_string())
            .map_err(|error| unavailable(reference, &error.to_string()))
    }
}

impl crate::harness::working_document::WorkTracker for MayWorkTracker {
    async fn item(&self, reference: &WorkRef) -> Result<WorkItem, WorkTrackerError> {
        match reference.kind() {
            WorkKind::Issue => self.issue(reference).await,
            WorkKind::PullRequest => {
                let row = self.pull_request(reference).await?;
                Ok(WorkItem {
                    reference: reference.clone(),
                    title: row.title,
                    state: pr_state(reference, &row.state)?,
                    body: row.body,
                    comments: Vec::new(),
                    sub_issues: Vec::new(),
                    blocked_by: Vec::new(),
                })
            }
        }
    }

    async fn status(&self, reference: &WorkRef) -> Result<WorkStatus, WorkTrackerError> {
        match reference.kind() {
            WorkKind::Issue => self.issue_status(reference).await,
            WorkKind::PullRequest => {
                let row = self.pull_request(reference).await?;
                Ok(WorkStatus {
                    reference: reference.clone(),
                    state: pr_state(reference, &row.state)?,
                    blocked_by: Vec::new(),
                })
            }
        }
    }
}

/// `owner/repo` of the GitHub `origin` of the checkout at `dir`.
fn github_repository(dir: &Path) -> Option<String> {
    match crate::trust::repo_kind(dir) {
        crate::trust::RepoKind::Github { owner_repo } => Some(owner_repo),
        crate::trust::RepoKind::Git => None,
    }
}

fn number(reference: &WorkRef) -> Result<i64, WorkTrackerError> {
    i64::try_from(reference.number()).map_err(|_| WorkTrackerError::NotFound(reference.clone()))
}

fn unavailable(reference: &WorkRef, reason: &str) -> WorkTrackerError {
    WorkTrackerError::Unavailable {
        reference: reference.clone(),
        reason: reason.to_string(),
    }
}

fn issue_error(reference: &WorkRef, error: &IssueError) -> WorkTrackerError {
    match error {
        IssueError::NotFound(_) => WorkTrackerError::NotFound(reference.clone()),
        error => unavailable(reference, &error.to_string()),
    }
}

const fn issue_state(state: IssueState) -> WorkState {
    match state {
        IssueState::Open => WorkState::Open,
        IssueState::Closed => WorkState::Closed,
    }
}

/// A PR record's state; one this tracker does not know is an error, never
/// taken for open.
fn pr_state(reference: &WorkRef, state: &str) -> Result<WorkState, WorkTrackerError> {
    match state {
        "open" | "checks_pending" | "failed" | "rejected" | "merging" => Ok(WorkState::Open),
        "merged" => Ok(WorkState::Merged),
        "closed" => Ok(WorkState::Closed),
        other => Err(unavailable(
            reference,
            &format!("its PR record has the unknown state {other}"),
        )),
    }
}

/// A related issue reached from `reference`. One in the issue store's own
/// repository is written as `reference` is; one a GitHub relation takes into
/// another repository is qualified by it.
fn related(
    reference: &WorkRef,
    ours: Option<&str>,
    issue: RelatedIssue,
) -> Result<RelatedWork, WorkTrackerError> {
    let number = u64::try_from(issue.number).map_err(|_| {
        unavailable(
            reference,
            &format!(
                "the backend relates it to issue number {}, which is no issue number",
                issue.number
            ),
        )
    })?;
    let related = WorkRef::issue(number);
    let related = match issue.repository {
        Some(repository) if !ours.is_some_and(|ours| ours.eq_ignore_ascii_case(&repository)) => {
            related.in_repository(repository)
        }
        _ => match reference.repository() {
            Some(repository) => related.in_repository(repository),
            None => related,
        },
    };
    Ok(RelatedWork {
        reference: related,
        title: issue.title,
        state: issue_state(issue.state),
    })
}

fn related_all(
    reference: &WorkRef,
    ours: Option<&str>,
    issues: Vec<RelatedIssue>,
) -> Result<Vec<RelatedWork>, WorkTrackerError> {
    issues
        .into_iter()
        .map(|issue| related(reference, ours, issue))
        .collect()
}

fn issue_item(
    reference: &WorkRef,
    ours: Option<&str>,
    detail: IssueDetail,
) -> Result<WorkItem, WorkTrackerError> {
    Ok(WorkItem {
        reference: reference.clone(),
        title: detail.title,
        state: issue_state(detail.state),
        body: detail.body,
        comments: detail
            .comments
            .into_iter()
            .map(|comment| WorkComment {
                author: comment.author,
                body: comment.body,
            })
            .collect(),
        sub_issues: related_all(reference, ours, detail.sub_issues)?,
        blocked_by: related_all(reference, ours, detail.blocked_by)?,
    })
}

#[cfg(test)]
#[path = "work_tracker_tests.rs"]
mod tests;
