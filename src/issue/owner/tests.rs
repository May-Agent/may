use std::path::PathBuf;
use std::sync::Arc;

use aither_core::llm::{Tool, ToolContext};

use super::{IssueOwners, OwnershipError, references};
use crate::agent::issue_tool::{IssueArgs, IssueTool};
use crate::db::ChatDb;
use crate::issue::GhCliTokenSource;
use crate::issue::github::GitHubRest;
use crate::issue::provenance::IssueProvenance;
use crate::issue::target::IssueTargets;
use crate::issue::{IssueStore, MayIssueStore, repository_identity};
use crate::session::{MayHomePaths, SessionRuntimeService};
use crate::ui_event_bus::UiEventBus;

/// Two sessions of one repository in one daemon, and the `issue` command of
/// the first.
struct Host {
    db: ChatDb,
    may_dir: tempfile::TempDir,
    owners: IssueOwners,
    tool: IssueTool<MayIssueStore>,
    issue: i64,
    _work: tempfile::TempDir,
}

impl Host {
    async fn new() -> Self {
        let _ = executor_core::try_init_global_executor(executor_core::tokio::TokioGlobal);
        let may_dir = tempfile::tempdir().expect("may dir");
        let work = tempfile::tempdir().expect("work dir");
        let db = ChatDb::open(&may_dir.path().join("history.db"))
            .await
            .expect("history.db opens");
        let runtime = Arc::new(SessionRuntimeService::new(db.clone(), UiEventBus::new()));
        for (id, title) in [("a1", "alpha"), ("b1", "beta")] {
            db.create_session(id, Some(title), None)
                .await
                .expect("session");
            db.set_session_cwd(id, work.path().to_str().expect("utf-8 path"))
                .await
                .expect("cwd");
        }
        let directory = crate::agent_link::AgentDirectory::new(
            runtime,
            may_dir.path().to_path_buf(),
            |_session_id| {},
        );
        let identity = repository_identity(work.path()).expect("identity");
        let store = MayIssueStore::new(db.clone(), identity.display().to_string());
        let issue = store.create("shared work", "", &[]).await.expect("issue");
        let targets = IssueTargets::new(
            db.clone(),
            work.path().to_path_buf(),
            may_dir.path().join("workspace.toml"),
            GitHubRest::new(GhCliTokenSource),
        );
        let tool = IssueTool::new(store, IssueProvenance::new(db.clone(), "a1".to_string()))
            .with_owners(IssueOwners::new(directory.clone()))
            .with_targets(targets);
        Self {
            db,
            may_dir,
            owners: IssueOwners::new(directory),
            tool,
            issue: issue.number,
            _work: work,
        }
    }

    /// The word the model knows the agent of `session_id` by.
    async fn alias(&self, session_id: &str) -> String {
        self.db
            .alias::<crate::model_ids::AgentAlias>(session_id)
            .await
            .expect("the agent has a word")
    }

    fn store_for(&self, repo_key: &str) -> MayIssueStore {
        MayIssueStore::new(self.db.clone(), repo_key.to_string())
    }

    fn tasks_path(&self, session_id: &str) -> PathBuf {
        let root = MayHomePaths::new(self.may_dir.path().to_path_buf())
            .session_paths(session_id)
            .root()
            .to_path_buf();
        std::fs::create_dir_all(&root).expect("session dir");
        root.join("tasks.md")
    }

    fn write_tasks(&self, session_id: &str, content: &str) {
        std::fs::write(self.tasks_path(session_id), content).expect("tasks.md");
    }

    fn read_tasks(&self, session_id: &str) -> String {
        std::fs::read_to_string(self.tasks_path(session_id)).expect("tasks.md")
    }

    async fn issue(&self, args: IssueArgs) -> String {
        let result = self
            .tool
            .call(args, ToolContext::new())
            .await
            .expect("issue command");
        format!("{result:?}")
    }
}

/// An issue listed in session A's `tasks.md` is reported as owned by A
/// through `issue view`, `issue owner` and `issue list`.
#[tokio::test]
async fn an_issue_in_a_tasks_md_is_owned_by_its_agent() {
    let host = Host::new().await;
    let number = host.issue;
    let a1 = host.alias("a1").await;
    let owned_by_a = |text: &str| owned_by(text, &a1);

    let owner = host.issue(IssueArgs::Owner { number, repo: None }).await;
    assert!(owner.contains("has no owner"), "{owner}");

    host.write_tasks("a1", &format!("## Open\n- #{number} doing: x\n"));
    let owner = host.issue(IssueArgs::Owner { number, repo: None }).await;
    assert!(owned_by_a(&owner), "{owner}");
    let viewed = host.issue(IssueArgs::View { number, repo: None }).await;
    assert!(
        viewed.contains("owner: agent") && owned_by_a(&viewed),
        "{viewed}"
    );
    let listed = host
        .issue(IssueArgs::List {
            state: crate::issue::IssueListState::Open,
            labels: Vec::new(),
            limit: 10,
            repo: None,
        })
        .await;
    assert!(listed.contains("owner:") && owned_by_a(&listed), "{listed}");
}

/// Session B adding an issue session A lists is refused with an error naming
/// A, and B's `tasks.md` does not keep it; once A drops the issue, B can add
/// it.
#[tokio::test]
async fn a_second_agent_cannot_take_an_owned_issue_until_the_owner_drops_it() {
    let host = Host::new().await;
    let number = host.issue;
    host.write_tasks("a1", &format!("## Open\n- #{number} doing: x\n"));

    let mut accepted = None;
    host.write_tasks("b1", "## Open\n");
    host.owners
        .admit("b1", &mut accepted)
        .await
        .expect("a tasks.md without the issue is accepted");
    host.write_tasks("b1", &format!("## Open\n- #{number} doing: y\n"));
    let Err(OwnershipError::Owned(owned)) = host.owners.admit("b1", &mut accepted).await else {
        panic!("taking an owned issue is refused");
    };
    let error = owned.to_string();
    let a1 = host.alias("a1").await;
    assert!(
        error.contains("alpha") && error.contains(&format!("(id {a1})")),
        "{error}"
    );
    assert_eq!(host.read_tasks("b1"), "## Open\n", "the change is reverted");
    assert_eq!(accepted.as_deref(), Some("## Open\n"));

    host.write_tasks("a1", "## Open\n");
    host.write_tasks("b1", &format!("## Open\n- #{number} doing: y\n"));
    host.owners
        .admit("b1", &mut accepted)
        .await
        .expect("the owner dropped the issue");
    assert!(host.read_tasks("b1").contains(&format!("#{number}")));
    let answer = host.issue(IssueArgs::Owner { number, repo: None }).await;
    let b1 = host.alias("b1").await;
    assert!(
        answer.contains("beta") && answer.contains(&format!("(id {b1})")),
        "{answer}"
    );
}

/// `--repo <path>` reads another local repository's may issues; ownership is
/// looked up under that repository, so an issue number owned here is not
/// reported as owned there, and the tasks.md check itself is untouched.
#[tokio::test]
async fn a_local_repository_target_reads_its_issues_and_its_own_ownership() {
    let host = Host::new().await;
    let number = host.issue;
    let a1 = host.alias("a1").await;
    host.write_tasks("a1", &format!("## Open\n- #{number} doing: x\n"));

    let other = tempfile::tempdir().expect("other repository");
    git2::Repository::init(other.path()).expect("git init");
    let other_path = other.path().to_str().expect("utf-8 path").to_string();
    let other_key = repository_identity(other.path()).expect("identity");
    let other_issue = host
        .store_for(&other_key.display().to_string())
        .create("in the other repository", "", &[])
        .await
        .expect("issue");
    assert_eq!(
        other_issue.number, number,
        "both repositories number from 1"
    );

    let target = Some(other_path);
    let listed = host
        .issue(IssueArgs::List {
            state: crate::issue::IssueListState::Open,
            labels: Vec::new(),
            limit: 10,
            repo: target.clone(),
        })
        .await;
    assert!(
        listed.contains("in the other repository") && !listed.contains("shared work"),
        "{listed}"
    );
    let viewed = host
        .issue(IssueArgs::View {
            number,
            repo: target.clone(),
        })
        .await;
    assert!(
        viewed.contains("in the other repository") && !owned_by(&viewed, &a1),
        "{viewed}"
    );
    let owner = host
        .issue(IssueArgs::Owner {
            number,
            repo: target,
        })
        .await;
    assert!(owner.contains("has no owner"), "{owner}");

    // The session's own issue is still owned, and still cannot be taken.
    let own = host.issue(IssueArgs::Owner { number, repo: None }).await;
    assert!(owned_by(&own, &a1), "{own}");
    let mut accepted = None;
    host.write_tasks("b1", "## Open\n");
    host.owners
        .admit("b1", &mut accepted)
        .await
        .expect("empty tasks.md");
    host.write_tasks("b1", &format!("## Open\n- #{number} doing: y\n"));
    assert!(matches!(
        host.owners.admit("b1", &mut accepted).await,
        Err(OwnershipError::Owned(_))
    ));
}

/// Whether `text` names agent "alpha" as the owner, by its word `id`.
fn owned_by(text: &str, id: &str) -> bool {
    text.contains("alpha") && text.contains(&format!("id {id})"))
}

#[test]
fn only_the_subject_of_an_unfinished_item_is_listed() {
    let found = references(
        "## Roots\n- May-Agent/may#144 x\n- [x] #9 done\n- [ ] #10, blocked by #568\n\
         - #634 merged, then #635\n  - [ ] May-Agent/may#12 nested\n- see #7\n\
         - C#4\n- a/b/c#5\n- #11x\n#13 plain line\n# 14\n",
    );
    let found = found
        .iter()
        .map(|reference| (reference.repository.as_deref(), reference.number))
        .collect::<Vec<_>>();
    assert_eq!(
        found,
        [
            (Some("May-Agent/may"), 144),
            (None, 10),
            (None, 634),
            (Some("May-Agent/may"), 12)
        ],
        "a mention inside an item's text, a finished item and a bare line claim nothing"
    );
}
