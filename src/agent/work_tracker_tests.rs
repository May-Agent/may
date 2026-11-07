use aither_agent::WorkTracker;

use super::*;
use crate::db::RepoCloneRow;
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

async fn open_pr(db: &ChatDb, repo_key: &str, title: &str) -> PrRow {
    db.create_pr(
        repo_key,
        "session",
        title,
        "body",
        "refs/may/pr/1",
        "base",
        "may",
        false,
        None,
        None,
    )
    .await
    .expect("pr")
}

fn pr_ref(row: &PrRow) -> WorkRef {
    WorkRef::pull_request(u64::try_from(row.number).expect("number"))
}

/// A may issue reads with its state, comments and both relation kinds, and
/// its status with its state and blockers alone.
#[tokio::test]
async fn a_may_issue_reads_in_full_and_as_a_status() {
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

    let reference = WorkRef::issue(u64::try_from(root.number).expect("number"));
    let item = tracker.item(&reference).await.expect("root reads");
    assert_eq!(item.title, "search");
    assert_eq!(item.body, "the plan");
    assert_eq!(item.state, WorkState::Open);
    assert_eq!(item.comments[0].body, "decided: sqlite");
    assert_eq!(item.sub_issues[0].title, "index");
    assert_eq!(
        item.sub_issues[0].reference,
        WorkRef::issue(u64::try_from(child.number).expect("number"))
    );
    assert!(item.is_blocked());

    let status = tracker.status(&reference).await.expect("status reads");
    assert_eq!(status.state, WorkState::Open);
    assert!(status.is_blocked());
    store
        .edit(blocker.number, None, None, Some(IssueState::Closed))
        .await
        .expect("close");
    assert!(
        !tracker
            .status(&reference)
            .await
            .expect("status")
            .is_blocked()
    );

    assert_eq!(
        tracker.item(&WorkRef::issue(999)).await,
        Err(WorkTrackerError::NotFound(WorkRef::issue(999)))
    );
    assert!(matches!(
        tracker
            .status(&WorkRef::issue(1).in_repository("o/r"))
            .await,
        Err(WorkTrackerError::Unavailable { .. })
    ));
}

/// A related issue in the store's own repository is written as the item
/// it hangs under, whatever the case of the names; one in another
/// repository carries it; a number no issue can have is an error, never #0.
#[test]
fn a_related_issue_carries_its_own_repository() {
    let related_issue = |repository: Option<&str>, number| RelatedIssue {
        repository: repository.map(str::to_string),
        number,
        title: "t".to_string(),
        state: IssueState::Open,
    };
    let parent = WorkRef::issue(1);
    let ours = Some("May-Agent/may");
    assert_eq!(
        related(&parent, ours, related_issue(Some("may-agent/MAY"), 2))
            .expect("related")
            .reference,
        WorkRef::issue(2)
    );
    assert_eq!(
        related(&parent, ours, related_issue(Some("lexoliu/aither"), 3))
            .expect("related")
            .reference
            .to_string(),
        "lexoliu/aither#3"
    );
    assert_eq!(
        related(&parent, None, related_issue(None, 4))
            .expect("related")
            .reference,
        WorkRef::issue(4)
    );
    assert!(matches!(
        related(&parent, ours, related_issue(None, -1)),
        Err(WorkTrackerError::Unavailable { .. })
    ));
}

/// A PR of the session's repository reads with its landing state; a state
/// the tracker does not know is an error, never open.
#[tokio::test]
async fn a_pr_reads_its_state_from_its_record() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (tracker, _store, db) = tracker(dir.path()).await;
    let repo_key = crate::issue::repository_identity(&dir.path().join("repo"))
        .expect("identity")
        .display()
        .to_string();
    let pr = open_pr(&db, &repo_key, "land search").await;
    let reference = pr_ref(&pr);
    assert_eq!(
        tracker.status(&reference).await.expect("open").state,
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

    db.update_pr(pr.id, "abandoned", None, None, None)
        .await
        .expect("state");
    assert!(matches!(
        tracker.status(&reference).await,
        Err(WorkTrackerError::Unavailable { .. })
    ));
}

/// A qualified PR reads in the session's clone of that repository, names
/// compared without case; an unqualified number two clones hold must be
/// qualified.
#[tokio::test]
async fn a_qualified_pr_reads_in_the_clone_of_its_repository() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (tracker, _store, db) = tracker(dir.path()).await;
    for (repo_key, dir) in [("May-Agent/may", "may-1"), ("lexoliu/aither", "aither-1")] {
        db.insert_repo_clone(
            "session",
            &RepoCloneRow {
                repo_key: repo_key.to_string(),
                dir: dir.to_string(),
                source: format!("https://github.com/{repo_key}.git"),
                local: false,
                base_sha: None,
            },
        )
        .await
        .expect("clone");
    }
    let may = open_pr(&db, "May-Agent/may", "may change").await;
    let aither = open_pr(&db, "lexoliu/aither", "aither change").await;
    assert_eq!(may.number, aither.number, "each project numbers its own");

    let qualified = pr_ref(&aither).in_repository("LexoLiu/Aither");
    let item = tracker.item(&qualified).await.expect("qualified reads");
    assert_eq!(item.title, "aither change");
    assert!(matches!(
        tracker.item(&pr_ref(&may)).await,
        Err(WorkTrackerError::Unavailable { reason, .. }) if reason.contains("PR owner/repo#N")
    ));
    assert_eq!(
        tracker
            .item(&pr_ref(&may).in_repository("other/repo"))
            .await,
        Err(WorkTrackerError::NotFound(
            pr_ref(&may).in_repository("other/repo")
        ))
    );
}
