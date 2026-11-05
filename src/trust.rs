//! Repository trust: the first-contact decision, resolved per repo.
//!
//! A session must not bind a repository it does not know — the first
//! meeting asks the user once, the answer persists to `[[projects]]` in
//! `workspace.toml`, and every later session reuses it. Levels:
//!
//! - `full` — GitHub PRs and GitHub issues; work in a session clone
//! - `prs_only` — GitHub PRs, may issues; every published text is screened
//! - `local_branch` — may PR + may issue on a session clone
//! - `local` — write the launch checkout directly (not recommended)

use std::path::{Path, PathBuf};

use crate::ask_user::{AnswerValue, QuestionAnswer};
use crate::session::{PersistedAskUserQuestion, PersistedAskUserRequest};
use crate::workspace::{
    GitHubRepository, ProjectConfig, ProjectIssues, ProjectPr, WorkspaceConfig,
    WorkspaceConfigError,
};

/// The persisted trust level for one repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustLevel {
    /// GitHub PRs and issues.
    Full,
    /// GitHub PRs, internal may issues, outbound screen on published text.
    PrsOnly,
    /// May PR + may issue on a session clone; the checkout stays read-only.
    LocalBranch,
    /// Write the launch checkout directly (not recommended).
    Local,
}

impl TrustLevel {
    /// Parse a `repo trust set` argument.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "full" => Some(Self::Full),
            "prs-only" | "prs_only" => Some(Self::PrsOnly),
            "local-branch" | "local_branch" => Some(Self::LocalBranch),
            "local" => Some(Self::Local),
            _ => None,
        }
    }

    /// The stored representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::PrsOnly => "prs-only",
            Self::LocalBranch => "local-branch",
            Self::Local => "local",
        }
    }

    /// The session clone is made from the GitHub remote and its PRs open on
    /// GitHub; every other level keeps its clone local.
    #[must_use]
    pub const fn publishes_to_github(self) -> bool {
        matches!(self, Self::Full | Self::PrsOnly)
    }

    /// Work lands through a session clone and `pr`, not the launch checkout.
    #[must_use]
    pub const fn clones_work(self) -> bool {
        matches!(self, Self::Full | Self::PrsOnly | Self::LocalBranch)
    }
}

/// Whether the launch repo is GitHub-backed: decides the option set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoKind {
    /// Has a GitHub remote — `full` / `prs-only` / `local-branch` / `local` / exit.
    Github {
        /// `owner/repo` of the GitHub repository.
        owner_repo: String,
    },
    /// Any other git repository — the two-option dialog.
    Git,
}

/// Detect the repo kind for a worktree: GitHub remote → `Github`,
/// otherwise `Git`. Call only inside a repository.
#[must_use]
pub fn repo_kind(worktree: &Path) -> RepoKind {
    let url = std::process::Command::new("git")
        .args(["-C"])
        .arg(worktree)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let owner_repo = url.and_then(|u| {
        let u = u.trim_end_matches(".git");
        let tail = u
            .strip_prefix("https://github.com/")
            .or_else(|| u.strip_prefix("git@github.com:"))
            .or_else(|| u.strip_prefix("ssh://git@github.com/"))
            .or_else(|| u.strip_prefix("http://github.com/"))?;
        let mut parts = tail.split('/');
        Some(format!("{}/{}", parts.next()?, parts.next()?))
    });
    owner_repo.map_or(RepoKind::Git, |owner_repo| RepoKind::Github { owner_repo })
}

/// The GitHub remote URL of a worktree, when it has one.
#[must_use]
pub fn remote_url(worktree: &Path) -> Option<String> {
    std::process::Command::new("git")
        .args(["-C"])
        .arg(worktree)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// A launch repository with no persisted trust decision: the worktree and
/// its detected kind, which pick the gate's option set.
#[derive(Debug, Clone)]
pub struct PendingTrust {
    /// The launch repository's worktree root.
    pub worktree: PathBuf,
    /// Whether the remote is GitHub-backed.
    pub kind: RepoKind,
}

/// The trust decision `cwd`'s repository still waits for.
///
/// `Some` when `cwd` is inside a worktree whose repository has no
/// `[[projects]]` entry. Outside a repository there is no repo gate —
/// the write-grant applies.
///
/// # Errors
/// Fails when `config_path` exists but cannot be read or parsed.
pub fn pending_decision(
    cwd: &Path,
    config_path: &Path,
) -> Result<Option<PendingTrust>, WorkspaceConfigError> {
    let Some(worktree) = crate::repo_search::worktree_root(cwd) else {
        return Ok(None);
    };
    let Ok(repo_key) = crate::issue::repository_identity(&worktree) else {
        return Ok(None);
    };
    let config = WorkspaceConfig::load(config_path)?;
    if trust_for(&config, &repo_key, &worktree).is_some() {
        return Ok(None);
    }
    Ok(Some(PendingTrust {
        kind: repo_kind(&worktree),
        worktree,
    }))
}

/// The trust level a gate answer selects. `exit`, dismissal and custom text
/// that is not a level name decide nothing — the repository stays as it was.
#[must_use]
pub fn level_for_answer(answer: &QuestionAnswer) -> Option<TrustLevel> {
    let text = match &answer.answer {
        AnswerValue::Selected(values) => values.first()?.as_str(),
        AnswerValue::Custom(text) => text.as_str(),
        AnswerValue::Skipped => return None,
    };
    // Options read `level — description`; custom text is a bare level name.
    TrustLevel::parse(text.split(" —").next().unwrap_or(text).trim())
}

/// The persisted trust for a repository identity, when a `[[projects]]`
/// entry exists for it.
#[must_use]
pub fn trust_for(
    config: &WorkspaceConfig,
    repo_key: &Path,
    _worktree: &Path,
) -> Option<TrustLevel> {
    let project = config.project_for_identity(repo_key)?;
    Some(match (project.pr.backend.as_deref(), &project.issues) {
        (Some("checkout"), _) => TrustLevel::Local,
        (Some("github"), ProjectIssues::Github { .. }) => TrustLevel::Full,
        (Some("github"), ProjectIssues::May) => TrustLevel::PrsOnly,
        _ => TrustLevel::LocalBranch,
    })
}

/// Persist a trust decision to `workspace.toml`.
///
/// The file is read afresh and the entry for the worktree's repository
/// identity is updated in place: only the fields trust owns (issue backend,
/// `pr.backend`, `pr.screen`) change; `checks` and the rest of the entry
/// stay. A repository without an entry gets one at `worktree`.
///
/// # Errors
/// Fails when the config cannot be read, parsed or written, or the
/// repository identity cannot be resolved.
pub fn persist(
    config_path: &Path,
    worktree: &Path,
    kind: Option<&RepoKind>,
    level: TrustLevel,
) -> Result<(), WorkspaceConfigError> {
    let invalid = |message: String| {
        WorkspaceConfigError::Read(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            message,
        ))
    };
    let mut config = WorkspaceConfig::load(config_path)?;
    let identity = crate::issue::repository_identity(worktree)
        .map_err(|e| invalid(format!("repository identity: {e}")))?;
    let issues = match (kind, level) {
        (Some(RepoKind::Github { owner_repo }), TrustLevel::Full) => ProjectIssues::Github {
            repository: owner_repo
                .parse::<GitHubRepository>()
                .map_err(|e| invalid(e.to_string()))?,
        },
        _ => ProjectIssues::May,
    };
    let backend = match (kind, level) {
        (_, TrustLevel::Local) => Some("checkout".to_string()),
        (Some(RepoKind::Github { .. }), TrustLevel::LocalBranch) => Some("may".to_string()),
        (Some(RepoKind::Github { .. }), _) => Some("github".to_string()),
        _ => None,
    };
    let screen = (level == TrustLevel::PrsOnly).then_some(true);
    if let Some(entry) = config
        .projects
        .iter_mut()
        .find(|p| p.has_identity(&identity))
    {
        entry.issues = issues;
        entry.pr.backend = backend;
        entry.pr.screen = screen;
    } else {
        config.projects.push(ProjectConfig {
            path: worktree.to_path_buf(),
            issues,
            checks: None,
            pr: ProjectPr {
                backend,
                screen,
                ..ProjectPr::default()
            },
        });
    }
    if level == TrustLevel::Local {
        config.add_workdir(worktree.to_path_buf(), Some("direct checkout".to_string()));
    } else {
        config.remove_workdir(worktree);
    }
    config.save(config_path)
}

/// A workspace config in which `worktree`'s repository is decided at
/// `level`, persisted through [`persist`] into `dir`.
#[cfg(test)]
pub(crate) fn config_at(dir: &Path, worktree: &Path, level: TrustLevel) -> WorkspaceConfig {
    let path = dir.join("workspace.toml");
    persist(&path, worktree, Some(&repo_kind(worktree)), level).expect("persist trust");
    WorkspaceConfig::load(&path).expect("load trust")
}

/// The first-contact `ask_user` request for one repository.
/// `include_exit` appends the quit option — the launch gate offers it,
/// `/permission`'s re-ask does not.
#[must_use]
pub fn question_for(
    kind: &RepoKind,
    worktree: &Path,
    include_exit: bool,
) -> PersistedAskUserRequest {
    let mut options = match kind {
        RepoKind::Github { .. } => vec![
            "full — Allow GitHub PRs and issues".to_string(),
            "prs-only — GitHub PRs only; issues stay internal, published text is screened"
                .to_string(),
            "local-branch — Isolated clone and local PRs; the checkout stays read-only".to_string(),
            "local — Write the checkout directly (not recommended)".to_string(),
        ],
        RepoKind::Git => vec![
            "local-branch — Isolated clone and local PRs; the checkout stays read-only".to_string(),
            "local — Write the checkout directly (not recommended)".to_string(),
        ],
    };
    if include_exit {
        options.push("exit — Do not bind this repository".to_string());
    }
    let question = match kind {
        RepoKind::Github { owner_repo } => {
            format!(
                "May can open pull requests on {owner_repo}. How should it work with this repository?"
            )
        }
        RepoKind::Git => format!("How should May work with {}?", worktree.display()),
    };
    PersistedAskUserRequest {
        id: crate::id::new(),
        questions: vec![PersistedAskUserQuestion {
            section: "repo-trust".to_string(),
            question,
            option: options,
            multi_select: false,
            secret: None,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ask_user::{AnswerValue, QuestionAnswer};
    use std::process::Command;

    fn git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let status = Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(dir.path())
            .status()
            .expect("git init");
        assert!(status.success());
        dir
    }

    fn identity(path: &Path) -> std::path::PathBuf {
        crate::issue::repository_identity(path).expect("identity")
    }

    #[test]
    fn persist_round_trip_github_levels() {
        let repo = git_repo();
        let worktree = repo.path();
        let config_path = repo.path().join("workspace.toml");
        let kind = RepoKind::Github {
            owner_repo: "acme/app".to_string(),
        };
        persist(&config_path, worktree, Some(&kind), TrustLevel::Full).expect("full");
        let loaded = WorkspaceConfig::load(&config_path).expect("load");
        let key = identity(worktree);
        assert_eq!(trust_for(&loaded, &key, worktree), Some(TrustLevel::Full));

        persist(&config_path, worktree, Some(&kind), TrustLevel::LocalBranch)
            .expect("local-branch");
        let loaded = WorkspaceConfig::load(&config_path).expect("load");
        assert_eq!(
            trust_for(&loaded, &key, worktree),
            Some(TrustLevel::LocalBranch)
        );
        assert!(!loaded.has_write_access(worktree));

        persist(&config_path, worktree, Some(&kind), TrustLevel::Local).expect("local");
        let loaded = WorkspaceConfig::load(&config_path).expect("load");
        assert_eq!(trust_for(&loaded, &key, worktree), Some(TrustLevel::Local));
        assert!(loaded.has_write_access(worktree));
        assert_eq!(loaded.projects[0].pr.backend.as_deref(), Some("checkout"));
    }

    fn github() -> RepoKind {
        RepoKind::Github {
            owner_repo: "acme/app".to_string(),
        }
    }

    fn seed_project(config_path: &Path, repo: &Path) {
        let config = WorkspaceConfig {
            projects: vec![ProjectConfig {
                path: repo.to_path_buf(),
                issues: ProjectIssues::May,
                checks: Some("cargo test".to_string()),
                pr: ProjectPr {
                    auto_merge: Some(false),
                    require_review: Some(true),
                    ..ProjectPr::default()
                },
            }],
            ..WorkspaceConfig::default()
        };
        config.save(config_path).expect("seed");
    }

    #[test]
    fn re_setting_trust_keeps_checks_and_pr_policy() {
        let repo = git_repo();
        let config_path = repo.path().join("workspace.toml");
        seed_project(&config_path, repo.path());
        for level in [TrustLevel::Full, TrustLevel::Full, TrustLevel::PrsOnly] {
            persist(&config_path, repo.path(), Some(&github()), level).expect("persist");
            let loaded = WorkspaceConfig::load(&config_path).expect("load");
            assert_eq!(loaded.projects.len(), 1);
            let project = &loaded.projects[0];
            assert_eq!(project.checks.as_deref(), Some("cargo test"));
            assert_eq!(project.pr.auto_merge, Some(false));
            assert_eq!(project.pr.require_review, Some(true));
            assert_eq!(
                trust_for(&loaded, &identity(repo.path()), repo.path()),
                Some(level)
            );
        }
    }

    #[test]
    fn trust_set_from_a_linked_worktree_covers_the_main_checkout() {
        let repo = git_repo();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .current_dir(repo.path())
                .status()
                .expect("git");
            assert!(status.success());
        };
        git(&["commit", "-q", "--allow-empty", "-m", "init"]);
        let linked_parent = tempfile::tempdir().expect("tempdir");
        let linked = linked_parent.path().join("linked");
        git(&["worktree", "add", "-q", linked.to_str().expect("utf8")]);
        let config_path = repo.path().join("workspace.toml");
        seed_project(&config_path, repo.path());

        persist(
            &config_path,
            &linked,
            Some(&github()),
            TrustLevel::LocalBranch,
        )
        .expect("persist");
        let loaded = WorkspaceConfig::load(&config_path).expect("load");
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].checks.as_deref(), Some("cargo test"));
        for dir in [repo.path(), linked.as_path()] {
            assert_eq!(
                trust_for(&loaded, &identity(dir), dir),
                Some(TrustLevel::LocalBranch)
            );
        }
    }

    #[test]
    fn persist_keeps_fields_changed_in_the_file_since_the_caller_loaded() {
        let repo = git_repo();
        let config_path = repo.path().join("workspace.toml");
        persist(&config_path, repo.path(), Some(&github()), TrustLevel::Full).expect("first");
        let mut newer = WorkspaceConfig::load(&config_path).expect("load");
        newer.projects[0].checks = Some("cargo clippy".to_string());
        newer.save(&config_path).expect("save");
        persist(
            &config_path,
            repo.path(),
            Some(&github()),
            TrustLevel::PrsOnly,
        )
        .expect("second");
        let loaded = WorkspaceConfig::load(&config_path).expect("load");
        assert_eq!(loaded.projects[0].checks.as_deref(), Some("cargo clippy"));
    }

    #[test]
    fn several_entries_for_one_repository_fail_to_load() {
        let repo = git_repo();
        let config_path = repo.path().join("workspace.toml");
        let sub = repo.path().join("sub");
        std::fs::create_dir(&sub).expect("mkdir");
        let entry = |path: &Path| ProjectConfig {
            path: path.to_path_buf(),
            issues: ProjectIssues::May,
            checks: None,
            pr: ProjectPr::default(),
        };
        WorkspaceConfig {
            projects: vec![entry(repo.path()), entry(&sub)],
            ..WorkspaceConfig::default()
        }
        .save(&config_path)
        .expect("save");
        let error = WorkspaceConfig::load(&config_path).expect_err("duplicate");
        assert!(matches!(
            error,
            WorkspaceConfigError::DuplicateProject { .. }
        ));
        assert!(error.to_string().contains("sub"), "{error}");
    }

    #[test]
    fn legacy_may_backend_is_local_branch() {
        let repo = git_repo();
        let mut config = WorkspaceConfig::default();
        config.projects.push(ProjectConfig {
            path: repo.path().to_path_buf(),
            issues: ProjectIssues::May,
            checks: None,
            pr: ProjectPr {
                backend: Some("may".to_string()),
                ..ProjectPr::default()
            },
        });
        let key = identity(repo.path());
        assert_eq!(
            trust_for(&config, &key, repo.path()),
            Some(TrustLevel::LocalBranch)
        );
    }

    #[test]
    fn parse_aliases() {
        assert_eq!(
            TrustLevel::parse("local-branch"),
            Some(TrustLevel::LocalBranch)
        );
        assert_eq!(
            TrustLevel::parse("local_branch"),
            Some(TrustLevel::LocalBranch)
        );
        assert_eq!(TrustLevel::parse("local"), Some(TrustLevel::Local));
        assert!(TrustLevel::LocalBranch.clones_work());
        assert!(!TrustLevel::Local.clones_work());
    }

    fn answer(value: AnswerValue) -> QuestionAnswer {
        QuestionAnswer {
            section: "repo-trust".to_string(),
            answer: value,
        }
    }

    #[test]
    fn a_gate_answer_maps_to_its_level() {
        for (option, level) in [
            ("full — Allow GitHub PRs and issues", TrustLevel::Full),
            (
                "prs-only — GitHub PRs only; issues stay internal",
                TrustLevel::PrsOnly,
            ),
            (
                "local-branch — Isolated clone and local PRs",
                TrustLevel::LocalBranch,
            ),
            (
                "local — Write the checkout directly (not recommended)",
                TrustLevel::Local,
            ),
        ] {
            let selected = answer(AnswerValue::Selected(vec![option.to_string()]));
            assert_eq!(level_for_answer(&selected), Some(level));
        }
        for value in [
            AnswerValue::Selected(vec!["exit — Do not bind this repository".to_string()]),
            AnswerValue::Custom("not a level".to_string()),
            AnswerValue::Skipped,
        ] {
            assert_eq!(level_for_answer(&answer(value)), None);
        }
        let custom = answer(AnswerValue::Custom("prs-only".to_string()));
        assert_eq!(level_for_answer(&custom), Some(TrustLevel::PrsOnly));
    }
}
