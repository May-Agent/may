//! Approval of what the agent publishes: the ownership lookup the approval
//! gate needs, and the errors a refused or failed approval produces.

use may_client::issue::IssueError;
use may_client::issue::github::{GitHubRest, GitHubTokenSource};
use may_client::issue::gitlab::{GitLabRest, GitLabTokenSource};
use may_client::workspace::GitHubRepository;
use may_config::forge::ForgeRepo;

/// Ownership lookup over a forge's REST client, for the publish gate.
pub trait RemoteOwnership: std::fmt::Debug + Sync {
    /// Whether the authenticated user owns `repository`.
    fn repository_owned(
        &self,
        repository: &ForgeRepo,
    ) -> impl std::future::Future<Output = Result<bool, IssueError>> + Send;
}

impl<T: GitHubTokenSource> RemoteOwnership for GitHubRest<T> {
    async fn repository_owned(&self, repository: &ForgeRepo) -> Result<bool, IssueError> {
        let github: GitHubRepository = repository.full_path().parse().map_err(
            |error: may_client::workspace::GitHubRepositoryError| {
                IssueError::Invalid(error.to_string())
            },
        )?;
        Self::repository_owned(self, &github).await
    }
}

impl<T: GitLabTokenSource> RemoteOwnership for GitLabRest<T> {
    async fn repository_owned(&self, repository: &ForgeRepo) -> Result<bool, IssueError> {
        Self::repository_owned(self, repository).await
    }
}

/// Publishing was refused; the text tells the agent why.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct PublishDenied(String);

impl PublishDenied {
    /// A refusal carrying `text`, the wording the agent reads.
    #[must_use]
    pub const fn new(text: String) -> Self {
        Self(text)
    }
}

impl From<PublishDenied> for IssueError {
    fn from(denied: PublishDenied) -> Self {
        Self::ApprovalDenied(denied.0)
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
    Denied(#[from] PublishDenied),
}
