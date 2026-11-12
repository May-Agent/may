//! GitHub Codespaces as a [`CloudProvider`], over the REST API.
//!
//! The credential is the profile's GitHub account in the `gh` login; the
//! account's token needs the `codespace` scope. A codespace always belongs
//! to a repository: the session's GitHub repository when it has one, so the
//! machine starts with the code and the project's devcontainer, else the
//! private environment repository May creates in the account.
//!
//! Endpoints and shapes are GitHub's published Codespaces REST API
//! (`2022-11-28`): <https://docs.github.com/en/rest/codespaces>.

use serde::{Deserialize, Serialize};
use zenwave::{Method, ResponseExt};

use super::{
    CloudError, CloudMachine, CloudProvider, MachineId, MachineState, MachineType, NewMachine,
};
use crate::issue::github::{GhAccountTokenSource, GitHubRest, GitHubTokenSource};
use crate::workspace::GitHubRepository;

/// The provider's name in errors.
const PROVIDER: &str = "GitHub";

/// The OAuth scope every Codespaces endpoint requires.
const CODESPACE_SCOPE: &str = "codespace";

/// The header GitHub lists an OAuth token's granted scopes in.
const SCOPES_HEADER: &str = "x-oauth-scopes";

/// The repository, in the account, that codespaces of sessions without a
/// GitHub repository are created from.
pub const ENVIRONMENT_REPOSITORY: &str = "may-environment";

/// The GitHub account a profile's codespaces belong to, as `/cloud` set it
/// up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CodespacesAccount {
    /// The account's login, one of the `gh` login's accounts.
    pub login: String,
}

/// GitHub Codespaces for one account.
#[derive(Debug, Clone)]
pub struct Codespaces<T> {
    rest: GitHubRest<T>,
    account: CodespacesAccount,
}

impl Codespaces<GhAccountTokenSource> {
    /// Codespaces of `account`, authenticated by its `gh` token.
    #[must_use]
    pub fn for_account(account: CodespacesAccount) -> Self {
        let rest = GitHubRest::new(GhAccountTokenSource::new(account.login.clone()));
        Self::new(rest, account)
    }
}

impl<T: GitHubTokenSource> Codespaces<T> {
    /// Codespaces of `account`, through `rest`.
    #[must_use]
    pub const fn new(rest: GitHubRest<T>, account: CodespacesAccount) -> Self {
        Self { rest, account }
    }

    /// The account `rest` authenticates as, once GitHub confirms its token
    /// carries the `codespace` scope: what `/cloud` saves.
    ///
    /// # Errors
    /// [`CloudError::MissingGitHubScope`] when the token lacks the scope;
    /// otherwise fails when GitHub cannot be reached or refuses.
    pub async fn verify(rest: &GitHubRest<T>) -> Result<CodespacesAccount, CloudError> {
        let response = rest
            .request_unchecked(Method::GET, format!("{}/user", rest.api_base()), None)
            .await?;
        let scoped = response
            .headers()
            .get(SCOPES_HEADER)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|scopes| {
                scopes
                    .split(',')
                    .any(|scope| scope.trim() == CODESPACE_SCOPE)
            });
        let user: GhUser = decode(succeeded(response).await?).await?;
        if !scoped {
            return Err(CloudError::MissingGitHubScope {
                login: user.login,
                scope: CODESPACE_SCOPE,
            });
        }
        Ok(CodespacesAccount { login: user.login })
    }

    /// The account these codespaces belong to.
    #[must_use]
    pub const fn account(&self) -> &CodespacesAccount {
        &self.account
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.rest.api_base())
    }

    fn codespace_url(&self, id: &MachineId, action: &str) -> String {
        self.url(&format!("/user/codespaces/{id}{action}"))
    }

    async fn send(
        &self,
        method: Method,
        url: String,
        body: Option<serde_json::Value>,
    ) -> Result<zenwave::Response, CloudError> {
        Ok(self.rest.request_unchecked(method, url, body).await?)
    }

    /// A lifecycle action on codespace `id` that answers with the codespace.
    async fn act(
        &self,
        method: Method,
        id: &MachineId,
        action: &str,
        body: Option<serde_json::Value>,
    ) -> Result<CloudMachine, CloudError> {
        let response = self
            .send(method, self.codespace_url(id, action), body)
            .await?;
        if response.status().as_u16() == 404 {
            return Err(CloudError::Gone(id.clone()));
        }
        let codespace: Codespace = decode(succeeded(response).await?).await?;
        Ok(codespace.into())
    }

    /// The account's private environment repository, created when missing.
    async fn environment_repository(&self) -> Result<GitHubRepository, CloudError> {
        let repository: GitHubRepository =
            format!("{}/{ENVIRONMENT_REPOSITORY}", self.account.login)
                .parse()
                .map_err(
                    |error: crate::workspace::GitHubRepositoryError| CloudError::Decode {
                        provider: PROVIDER,
                        message: error.to_string(),
                    },
                )?;
        let response = self
            .send(Method::GET, self.url(&format!("/repos/{repository}")), None)
            .await?;
        let found: GhRepository = if response.status().as_u16() == 404 {
            let body = serde_json::to_value(CreateRepository {
                name: ENVIRONMENT_REPOSITORY,
                description: "Where May creates cloud machines for sessions without a GitHub repository",
                private: true,
                auto_init: true,
            })
            .expect("a repository request always serializes");
            decode(
                succeeded(
                    self.send(Method::POST, self.url("/user/repos"), Some(body))
                        .await?,
                )
                .await?,
            )
            .await?
        } else {
            decode(succeeded(response).await?).await?
        };
        if !found.private {
            return Err(CloudError::PublicSource(repository));
        }
        Ok(repository)
    }
}

impl<T: GitHubTokenSource> CloudProvider for Codespaces<T> {
    type Source = GitHubRepository;

    async fn source(
        &self,
        project: Option<&GitHubRepository>,
    ) -> Result<GitHubRepository, CloudError> {
        match project {
            Some(project) => Ok(project.clone()),
            None => self.environment_repository().await,
        }
    }

    async fn catalog(&self, source: &GitHubRepository) -> Result<Vec<MachineType>, CloudError> {
        let url = self.url(&format!("/repos/{source}/codespaces/machines"));
        let listing: MachineListing =
            decode(succeeded(self.send(Method::GET, url, None).await?).await?).await?;
        Ok(listing.machines.into_iter().map(Into::into).collect())
    }

    async fn create(
        &self,
        source: &GitHubRepository,
        request: &NewMachine,
    ) -> Result<CloudMachine, CloudError> {
        let body = serde_json::to_value(CreateCodespace {
            machine: &request.machine_type,
            display_name: &request.display_name,
        })
        .expect("a codespace request always serializes");
        let url = self.url(&format!("/repos/{source}/codespaces"));
        // 201 is a created codespace, 202 one GitHub is still creating;
        // both answer with the codespace.
        let codespace: Codespace =
            decode(succeeded(self.send(Method::POST, url, Some(body)).await?).await?).await?;
        Ok(codespace.into())
    }

    async fn machine(&self, id: &MachineId) -> Result<Option<CloudMachine>, CloudError> {
        let response = self
            .send(Method::GET, self.codespace_url(id, ""), None)
            .await?;
        if response.status().as_u16() == 404 {
            return Ok(None);
        }
        let codespace: Codespace = decode(succeeded(response).await?).await?;
        Ok(Some(codespace.into()))
    }

    async fn start(&self, id: &MachineId) -> Result<CloudMachine, CloudError> {
        self.act(Method::POST, id, "/start", None).await
    }

    async fn stop(&self, id: &MachineId) -> Result<CloudMachine, CloudError> {
        self.act(Method::POST, id, "/stop", None).await
    }

    async fn resize(&self, id: &MachineId, machine_type: &str) -> Result<CloudMachine, CloudError> {
        // GitHub applies a new machine type at the codespace's next start.
        let body = serde_json::to_value(UpdateCodespace {
            machine: machine_type,
        })
        .expect("a codespace update always serializes");
        self.act(Method::PATCH, id, "", Some(body)).await
    }

    async fn delete(&self, id: &MachineId) -> Result<(), CloudError> {
        let response = self
            .send(Method::DELETE, self.codespace_url(id, ""), None)
            .await?;
        if response.status().as_u16() == 404 {
            return Ok(());
        }
        succeeded(response).await.map(drop)
    }
}

/// `response` when its status is a success, else GitHub's refusal.
async fn succeeded(response: zenwave::Response) -> Result<zenwave::Response, CloudError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let text = response
        .into_string()
        .await
        .map_err(|error| CloudError::Decode {
            provider: PROVIDER,
            message: error.to_string(),
        })?;
    let message = serde_json::from_str::<GhError>(&text)
        .map_or_else(|_| text.to_string(), |error| error.message);
    Err(CloudError::Refused {
        provider: PROVIDER,
        status: status.as_u16(),
        message,
    })
}

async fn decode<R: serde::de::DeserializeOwned>(
    response: zenwave::Response,
) -> Result<R, CloudError> {
    response
        .into_json::<R>()
        .await
        .map_err(|error| CloudError::Decode {
            provider: PROVIDER,
            message: error.to_string(),
        })
}

/// GitHub's error body (`basic-error`), to the field May reads.
#[derive(Debug, Deserialize)]
struct GhError {
    message: String,
}

/// `GET /user`, to the field May reads.
#[derive(Debug, Deserialize)]
struct GhUser {
    login: String,
}

/// A repository (`full-repository`), to the field May reads.
#[derive(Debug, Deserialize)]
struct GhRepository {
    private: bool,
}

/// The body of `POST /user/repos`.
#[derive(Debug, Serialize)]
struct CreateRepository<'a> {
    name: &'a str,
    description: &'a str,
    /// A codespace on a public repository is one other accounts could open.
    private: bool,
    /// A codespace needs a commit to build from.
    auto_init: bool,
}

/// The body of `POST /repos/{owner}/{repo}/codespaces`.
#[derive(Debug, Serialize)]
struct CreateCodespace<'a> {
    machine: &'a str,
    display_name: &'a str,
}

/// The body of `PATCH /user/codespaces/{codespace_name}`.
#[derive(Debug, Serialize)]
struct UpdateCodespace<'a> {
    machine: &'a str,
}

/// `GET /repos/{owner}/{repo}/codespaces/machines`.
#[derive(Debug, Deserialize)]
struct MachineListing {
    machines: Vec<GhMachine>,
}

/// A machine type (`codespace-machine`), to the fields May reads.
#[derive(Debug, Deserialize)]
struct GhMachine {
    name: String,
    display_name: String,
    cpus: u32,
    memory_in_bytes: u64,
    storage_in_bytes: u64,
}

impl From<GhMachine> for MachineType {
    fn from(machine: GhMachine) -> Self {
        Self {
            name: machine.name,
            display_name: machine.display_name,
            cpus: machine.cpus,
            memory_bytes: machine.memory_in_bytes,
            storage_bytes: machine.storage_in_bytes,
        }
    }
}

/// A codespace (`codespace`), to the fields May reads.
#[derive(Debug, Deserialize)]
struct Codespace {
    name: String,
    state: CodespaceState,
    machine: Option<GhMachine>,
}

impl From<Codespace> for CloudMachine {
    fn from(codespace: Codespace) -> Self {
        Self {
            id: MachineId::new(codespace.name),
            state: codespace.state.into(),
            machine_type: codespace.machine.map(Into::into),
        }
    }
}

/// A codespace's `state`, every value GitHub documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
enum CodespaceState {
    Unknown,
    Created,
    Queued,
    Provisioning,
    Available,
    Awaiting,
    Unavailable,
    Deleted,
    Moved,
    Shutdown,
    Archived,
    Starting,
    ShuttingDown,
    Failed,
    Exporting,
    Updating,
    Rebuilding,
}

impl From<CodespaceState> for MachineState {
    fn from(state: CodespaceState) -> Self {
        match state {
            CodespaceState::Available => Self::Running,
            CodespaceState::Shutdown | CodespaceState::Unavailable => Self::Suspended,
            // A failed codespace is dead where it is: nothing starts it.
            CodespaceState::Deleted
            | CodespaceState::Moved
            | CodespaceState::Archived
            | CodespaceState::Failed => Self::Gone,
            CodespaceState::Unknown
            | CodespaceState::Created
            | CodespaceState::Queued
            | CodespaceState::Provisioning
            | CodespaceState::Awaiting
            | CodespaceState::Starting
            | CodespaceState::ShuttingDown
            | CodespaceState::Exporting
            | CodespaceState::Updating
            | CodespaceState::Rebuilding => Self::Provisioning,
        }
    }
}

#[cfg(test)]
#[path = "codespaces_tests.rs"]
mod tests;
