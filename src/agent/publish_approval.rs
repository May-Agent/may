//! Approval of what the agent publishes: issues, pull requests and
//! comments.
//!
//! [`IssueApprovalConfig`] decides per kind of publication whether the user is
//! asked. The question rides the `unsafe` approval subsystem
//! ([`PermissionApprover`]), so the run waits on the user the same way and an
//! unanswered question resolves by the entry: denied for `required`, approved
//! for `auto`.

use std::sync::Arc;

use arc_swap::ArcSwap;
use askama::Template;

use crate::agent::permission::{OnTimeout, PermissionApprover};
use crate::config::{ApprovalPolicy, IssueApprovalConfig, MayConfig};
use crate::issue::github::{GitHubRest, GitHubTokenSource};
use crate::issue::{IssueError, IssueKind, IssueStore};
use crate::workspace::GitHubRepository;

/// What the agent is about to publish.
#[derive(Debug, Clone, Copy)]
pub enum Publication<'a> {
    /// A new issue.
    Issue {
        /// What the issue is about.
        kind: IssueKind,
        /// Issue title.
        title: &'a str,
        /// Labels to apply.
        labels: &'a [String],
        /// Issue body.
        body: &'a str,
    },
    /// A pull request.
    PullRequest {
        /// PR title.
        title: &'a str,
        /// PR body.
        body: &'a str,
    },
    /// A comment on issue or pull request `number`.
    Comment {
        /// The commented thread.
        number: i64,
        /// Comment body.
        body: &'a str,
    },
}

/// Publishing was refused; the text tells the agent why.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct PublishDenied(String);

impl From<PublishDenied> for IssueError {
    fn from(denied: PublishDenied) -> Self {
        Self::ApprovalDenied(denied.0)
    }
}

/// The text the user is shown for a pending publication.
#[derive(Template)]
#[template(path = "publish_request.txt", escape = "none")]
struct PublishRequest<'a> {
    kind: Option<&'a str>,
    action: &'a str,
    number: Option<i64>,
    repository: &'a str,
    title: Option<&'a str>,
    labels: &'a [String],
    body: &'a str,
}

/// The text the user is shown for a pending merge.
#[derive(Template)]
#[template(path = "merge_request.txt", escape = "none")]
struct MergeRequest<'a> {
    number: i64,
    repository: &'a str,
    title: &'a str,
    body: &'a str,
}

/// The refusal the agent reads.
#[derive(Template)]
#[template(path = "publish_denied.txt", escape = "none")]
struct PublishDeniedText<'a> {
    reason: &'a str,
}

impl Publication<'_> {
    /// The approval entry that applies, given whether the repository is
    /// external to the user. Anything on an owned repository except the
    /// issue-kind entries, and a comment on a thread the user's account
    /// opened, needs no approval and has no entry.
    #[must_use]
    pub fn policy(&self, config: &IssueApprovalConfig, external: bool) -> ApprovalPolicy {
        match self {
            Self::Issue { kind, .. } => {
                let by_kind = match kind {
                    IssueKind::Bug => config.bug,
                    IssueKind::Feature => config.feature,
                    IssueKind::Performance => config.performance,
                };
                if external {
                    by_kind.max(config.external_issue)
                } else {
                    by_kind
                }
            }
            Self::PullRequest { .. } if external => config.external_pull_request,
            Self::Comment { .. } if external => config.external_comment,
            Self::PullRequest { .. } | Self::Comment { .. } => ApprovalPolicy::None,
        }
    }

    fn render(&self, repository: &str) -> Result<String, askama::Error> {
        let (kind, action, number, title, labels, body) = match *self {
            Self::Issue {
                kind,
                title,
                labels,
                body,
            } => (
                Some(kind.as_str()),
                "issue",
                None,
                Some(title),
                labels,
                body,
            ),
            Self::PullRequest { title, body } => {
                (None, "pull request", None, Some(title), &[][..], body)
            }
            Self::Comment { number, body } => (None, "comment", Some(number), None, &[][..], body),
        };
        PublishRequest {
            kind,
            action,
            number,
            repository,
            title,
            labels,
            body,
        }
        .render()
    }
}

/// Asks the user before the agent publishes, as the live `[issue_approval]`
/// settings say.
#[derive(Debug, Clone)]
pub struct PublishGate {
    config: Arc<ArcSwap<MayConfig>>,
    approver: PermissionApprover,
}

impl PublishGate {
    /// A gate reading `config` on every publication and asking through
    /// `approver`.
    #[must_use]
    pub const fn new(config: Arc<ArcSwap<MayConfig>>, approver: PermissionApprover) -> Self {
        Self { config, approver }
    }

    /// The questions waiting for the user, for tests that answer them.
    #[cfg(test)]
    pub(crate) fn pending_requests(&self) -> Vec<String> {
        self.approver.pending_request_ids()
    }

    /// Answer the waiting question `request_id`, as the user would.
    #[cfg(test)]
    pub(crate) fn answer(&self, request_id: &str, approved: bool) -> bool {
        self.approver.respond(request_id, approved)
    }

    fn settings(&self) -> IssueApprovalConfig {
        self.config.load().issue_approval
    }

    /// Approve creating an issue in `store`'s repository.
    ///
    /// # Errors
    /// Fails when the ownership lookup fails or the user denies (or, for a
    /// `required` entry, does not answer).
    pub async fn approve_issue<S: IssueStore>(
        &self,
        store: &S,
        kind: IssueKind,
        title: &str,
        labels: &[String],
        body: &str,
    ) -> Result<(), IssueError> {
        let external = !store.owned_by_user().await?;
        let repository = store.info().repository.unwrap_or_default();
        self.decide(
            Publication::Issue {
                kind,
                title,
                labels,
                body,
            },
            external,
            &repository,
        )
        .await
        .map_err(Into::into)
    }

    /// Approve commenting on issue `number` of `store`'s repository.
    ///
    /// # Errors
    /// Fails when a lookup fails or the user denies (or, for a `required`
    /// entry, does not answer).
    pub async fn approve_comment<S: IssueStore>(
        &self,
        store: &S,
        number: i64,
        body: &str,
    ) -> Result<(), IssueError> {
        let publication = Publication::Comment { number, body };
        if publication.policy(&self.settings(), true) == ApprovalPolicy::None {
            return Ok(());
        }
        if store.owned_by_user().await? || store.opened_by_user(number).await? {
            return Ok(());
        }
        let repository = store.info().repository.unwrap_or_default();
        self.decide(publication, true, &repository)
            .await
            .map_err(Into::into)
    }

    /// Approve opening a pull request on `repository`.
    ///
    /// # Errors
    /// Fails when the ownership lookup fails or the user denies (or, for a
    /// `required` entry, does not answer).
    pub async fn approve_pull_request<T: GitHubTokenSource>(
        &self,
        github: &GitHubRest<T>,
        repository: &GitHubRepository,
        title: &str,
        body: &str,
    ) -> Result<(), PublishError> {
        if (Publication::PullRequest { title, body }).policy(&self.settings(), true)
            == ApprovalPolicy::None
        {
            return Ok(());
        }
        let external = !github.repository_owned(repository).await?;
        self.decide(
            Publication::PullRequest { title, body },
            external,
            &repository.to_string(),
        )
        .await
        .map_err(PublishError::Denied)
    }

    /// Ask the user whether pull request `number` of `repository` may merge,
    /// as the project's `pr.require_review` demands: an unanswered question
    /// is a refusal.
    ///
    /// # Errors
    /// Fails when the user denies or does not answer.
    ///
    /// # Panics
    /// Never: the request renders for every input.
    pub async fn approve_merge(
        &self,
        repository: &str,
        number: i64,
        title: &str,
        body: &str,
    ) -> Result<(), PublishError> {
        let text = MergeRequest {
            number,
            repository,
            title,
            body,
        }
        .render()
        .unwrap_or_else(|error| panic!("failed to render the merge request: {error}"));
        // A project that asked for review gets a human: a yolo session's
        // auto-approval does not stand in for one.
        Self::verdict(self.approver.request_review(text).await).map_err(PublishError::Denied)
    }

    /// Apply the entry for `publication`: nothing, or a question whose
    /// unanswered outcome the entry names.
    async fn decide(
        &self,
        publication: Publication<'_>,
        external: bool,
        repository: &str,
    ) -> Result<(), PublishDenied> {
        let on_timeout = match publication.policy(&self.settings(), external) {
            ApprovalPolicy::None => return Ok(()),
            ApprovalPolicy::Auto => OnTimeout::Approve,
            ApprovalPolicy::Required => OnTimeout::Deny,
        };
        let text = publication
            .render(repository)
            .unwrap_or_else(|error| panic!("failed to render the publish request: {error}"));
        self.ask(text, on_timeout).await
    }

    /// Put `text` to the user through the live-response machinery.
    async fn ask(&self, text: String, on_timeout: OnTimeout) -> Result<(), PublishDenied> {
        Self::verdict(self.approver.request_publication(text, on_timeout).await)
    }

    /// The agent's view of the user's answer.
    fn verdict(
        answer: Result<bool, aither_sandbox::permission::PermissionError>,
    ) -> Result<(), PublishDenied> {
        let reason = match answer {
            Ok(true) => return Ok(()),
            Ok(false) => "the user denied it".to_string(),
            Err(error) => error.to_string(),
        };
        Err(PublishDenied(
            PublishDeniedText { reason: &reason }
                .render()
                .unwrap_or_else(|error| panic!("failed to render the refusal: {error}")),
        ))
    }
}

/// Why a pull request could not be approved.
#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    /// Ownership could not be looked up.
    #[error(transparent)]
    Lookup(#[from] IssueError),
    /// The user refused, or the question went unanswered.
    #[error(transparent)]
    Denied(PublishDenied),
}

#[cfg(test)]
#[path = "publish_approval_tests.rs"]
mod tests;
