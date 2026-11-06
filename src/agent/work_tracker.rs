//! The [`WorkTracker`](aither_agent::WorkTracker) over the session's issue
//! and PR backends, which `tasks.md` reads its rows' state and its roots'
//! trees through.

use std::path::PathBuf;

use aither_agent::tasks::{RelatedWork, WorkComment, WorkKind};
use aither_agent::{WorkItem, WorkRef, WorkState, WorkTrackerError};

use crate::db::{ChatDb, PrRow};
use crate::issue::{
    IssueDetail, IssueError, IssueState, IssueStore, RelatedIssue, ResolvedIssueStore,
};
use crate::repo_clone::SessionRepos;

/// Reads issues from the session's issue store and PRs from its PR records.
#[derive(Debug, Clone)]
pub struct MayWorkTracker {
    issues: ResolvedIssueStore,
    db: ChatDb,
    repos: SessionRepos,
    /// The session's working directory: the repository an unqualified PR
    /// reference means when no clone of the session has it.
    cwd: PathBuf,
}

impl MayWorkTracker {
    /// The tracker of the session whose sandbox is `session_root`.
    #[must_use]
    pub fn new(
        issues: ResolvedIssueStore,
        db: ChatDb,
        session_root: &std::path::Path,
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

    async fn issue(&self, reference: &WorkRef, number: i64) -> Result<WorkItem, WorkTrackerError> {
        if let Some(repository) = reference.repository() {
            let ours = self.issues.info().repository;
            if ours.as_deref() != Some(repository) {
                return Err(unavailable(
                    reference,
                    "it is not this session's repository",
                ));
            }
        }
        match self.issues.get(number).await {
            Ok(detail) => Ok(issue_item(reference, detail)),
            Err(IssueError::NotFound(_)) => Err(WorkTrackerError::NotFound(reference.clone())),
            Err(error) => Err(unavailable(reference, &error.to_string())),
        }
    }

    async fn pull_request(
        &self,
        reference: &WorkRef,
        number: i64,
    ) -> Result<WorkItem, WorkTrackerError> {
        if reference.repository().is_some() {
            return Err(unavailable(
                reference,
                "PRs are read by number within this session's repositories",
            ));
        }
        let mut keys: Vec<String> = match self.repos.clones().await {
            Ok(clones) => clones.into_iter().map(|clone| clone.repo_key).collect(),
            Err(error) => return Err(unavailable(reference, &error.to_string())),
        };
        if let Ok(identity) = crate::issue::repository_identity(&self.cwd) {
            let identity = identity.display().to_string();
            if !keys.contains(&identity) {
                keys.push(identity);
            }
        }
        let found =
            futures_util::future::join_all(keys.iter().map(|key| self.db.get_pr(key, number)))
                .await;
        let mut rows = Vec::new();
        for row in found {
            match row {
                Ok(Some(row)) => rows.push(row),
                Ok(None) => {}
                Err(error) => return Err(unavailable(reference, &error.to_string())),
            }
        }
        match rows.as_slice() {
            [] => Err(WorkTrackerError::NotFound(reference.clone())),
            [row] => Ok(pr_item(reference, row)),
            _ => Err(unavailable(
                reference,
                "several of this session's repositories have a PR with that number",
            )),
        }
    }
}

impl aither_agent::WorkTracker for MayWorkTracker {
    async fn item(&self, reference: &WorkRef) -> Result<WorkItem, WorkTrackerError> {
        let Ok(number) = i64::try_from(reference.number()) else {
            return Err(WorkTrackerError::NotFound(reference.clone()));
        };
        match reference.kind() {
            WorkKind::Issue => self.issue(reference, number).await,
            WorkKind::PullRequest => self.pull_request(reference, number).await,
        }
    }
}

fn unavailable(reference: &WorkRef, reason: &str) -> WorkTrackerError {
    WorkTrackerError::Unavailable {
        reference: reference.clone(),
        reason: reason.to_string(),
    }
}

const fn issue_state(state: IssueState) -> WorkState {
    match state {
        IssueState::Open => WorkState::Open,
        IssueState::Closed => WorkState::Closed,
    }
}

/// A related issue, qualified like the reference it was reached from.
fn related(reference: &WorkRef, issue: RelatedIssue) -> RelatedWork {
    let mut related = WorkRef::issue(u64::try_from(issue.number).unwrap_or_default());
    if let Some(repository) = reference.repository() {
        related = related.in_repository(repository);
    }
    RelatedWork {
        reference: related,
        title: issue.title,
        state: issue_state(issue.state),
    }
}

fn issue_item(reference: &WorkRef, detail: IssueDetail) -> WorkItem {
    WorkItem {
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
        sub_issues: detail
            .sub_issues
            .into_iter()
            .map(|issue| related(reference, issue))
            .collect(),
        blocked_by: detail
            .blocked_by
            .into_iter()
            .map(|issue| related(reference, issue))
            .collect(),
    }
}

fn pr_item(reference: &WorkRef, row: &PrRow) -> WorkItem {
    let state = match row.state.as_str() {
        "merged" => WorkState::Merged,
        "closed" => WorkState::Closed,
        _ => WorkState::Open,
    };
    WorkItem {
        reference: reference.clone(),
        title: row.title.clone(),
        state,
        body: row.body.clone(),
        comments: Vec::new(),
        sub_issues: Vec::new(),
        blocked_by: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use aither_agent::WorkTracker;

    use super::*;
    use crate::issue::{IssueRelationKind, MayIssueStore};

    async fn tracker(dir: &std::path::Path) -> (MayWorkTracker, MayIssueStore, ChatDb) {
        let db = ChatDb::open(&dir.join("history.db")).await.expect("db");
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).expect("repo dir");
        let identity = crate::issue::repository_identity(&repo)
            .expect("identity")
            .display()
            .to_string();
        let store = MayIssueStore::new(db.clone(), identity);
        let tracker = MayWorkTracker::new(
            ResolvedIssueStore::May(store.clone()),
            db.clone(),
            &dir.join("session"),
            "session",
            repo,
        );
        (tracker, store, db)
    }

    /// A may issue reads with its state, comments and both relation kinds.
    #[tokio::test]
    async fn a_may_issue_reads_in_full() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tracker, store, _db) = tracker(dir.path()).await;
        let root = store.create("search", "the plan", &[]).await.expect("root");
        let child = store.create("index", "", &[]).await.expect("child");
        let blocker = store.create("schema", "", &[]).await.expect("blocker");
        store
            .edit_relation(root.number, IssueRelationKind::SubIssue, child.number, true)
            .await
            .expect("sub-issue");
        store
            .edit_relation(
                root.number,
                IssueRelationKind::BlockedBy,
                blocker.number,
                true,
            )
            .await
            .expect("blocked by");
        store
            .comment(root.number, "decided: sqlite")
            .await
            .expect("comment");
        store
            .edit(blocker.number, None, None, Some(IssueState::Closed))
            .await
            .expect("close");

        let reference = WorkRef::issue(u64::try_from(root.number).expect("number"));
        let item = tracker.item(&reference).await.expect("root reads");
        assert_eq!(item.title, "search");
        assert_eq!(item.body, "the plan");
        assert_eq!(item.state, WorkState::Open);
        assert_eq!(item.comments[0].body, "decided: sqlite");
        assert_eq!(item.sub_issues[0].title, "index");
        assert_eq!(item.blocked_by[0].state, WorkState::Closed);
        assert!(!item.is_blocked());

        assert_eq!(
            tracker.item(&WorkRef::issue(999)).await,
            Err(WorkTrackerError::NotFound(WorkRef::issue(999)))
        );
        assert!(matches!(
            tracker.item(&WorkRef::issue(1).in_repository("o/r")).await,
            Err(WorkTrackerError::Unavailable { .. })
        ));
    }

    /// A PR of the session's repository reads with its landing state.
    #[tokio::test]
    async fn a_pr_reads_its_state_from_its_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tracker, _store, db) = tracker(dir.path()).await;
        let repo_key = crate::issue::repository_identity(&dir.path().join("repo"))
            .expect("identity")
            .display()
            .to_string();
        let pr = db
            .create_pr(
                &repo_key,
                "session",
                "land search",
                "body",
                "refs/may/pr/1",
                "base",
                "may",
                false,
                None,
                None,
            )
            .await
            .expect("pr");
        let reference = WorkRef::pull_request(u64::try_from(pr.number).expect("number"));
        assert_eq!(
            tracker.item(&reference).await.expect("open").state,
            WorkState::Open
        );
        db.update_pr(pr.id, "merged", Some("head"), Some("merge"), Some("now"))
            .await
            .expect("merge");
        let item = tracker.item(&reference).await.expect("merged");
        assert_eq!(item.state, WorkState::Merged);
        assert_eq!(item.title, "land search");
        assert_eq!(
            tracker.item(&WorkRef::pull_request(77)).await,
            Err(WorkTrackerError::NotFound(WorkRef::pull_request(77)))
        );
    }
}
