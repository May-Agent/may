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

/// The devcontainer May keeps in the environment repository.
pub const ENVIRONMENT_DEVCONTAINER: &str = ".devcontainer/devcontainer.json";

/// Where a repository's devcontainer is found when no path is named, in
/// the order the dev container specification looks.
const DEFAULT_DEVCONTAINER_PATHS: [&str; 2] =
    [".devcontainer/devcontainer.json", ".devcontainer.json"];

/// The dev container feature that installs an SSH server, without its
/// version tag.
const SSHD_FEATURE: &str = "ghcr.io/devcontainers/features/sshd";

/// The image of GitHub's default codespace, which ships an SSH server; a
/// repository without a devcontainer gets it.
const UNIVERSAL_IMAGE: &str = "mcr.microsoft.com/devcontainers/universal";

/// The image of the environment repository's devcontainer.
const ENVIRONMENT_IMAGE: &str = "mcr.microsoft.com/devcontainers/base:ubuntu";

/// The SSH server feature the environment repository's devcontainer
/// installs, at its major version.
const ENVIRONMENT_SSHD_FEATURE: &str = "ghcr.io/devcontainers/features/sshd:1";

/// The commit message of the environment devcontainer's writes.
const ENVIRONMENT_COMMIT_MESSAGE: &str = "Boot May sessions";

/// What a codespace is created from: a repository, the devcontainer in it
/// to build, and the project to clone into it when the codespace is not
/// created from the project itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodespaceSource {
    /// The repository the codespace belongs to.
    pub repository: GitHubRepository,
    /// The devcontainer to build, when the repository has one.
    pub devcontainer_path: Option<String>,
    /// The session's repository, when its devcontainer has no SSH server
    /// and the codespace is created from the environment repository.
    pub clone: Option<GitHubRepository>,
}

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
        self.write_environment_devcontainer(&repository).await?;
        Ok(repository)
    }

    /// Writes May's devcontainer into the environment repository unless it
    /// is already there as May writes it: a changed one is a codespace that
    /// boots without its SSH server or daemon.
    async fn write_environment_devcontainer(
        &self,
        repository: &GitHubRepository,
    ) -> Result<(), CloudError> {
        let wanted = environment_devcontainer();
        let url = self.url(&format!(
            "/repos/{repository}/contents/{ENVIRONMENT_DEVCONTAINER}"
        ));
        let response = self.send(Method::GET, url.clone(), None).await?;
        let sha = if response.status().as_u16() == 404 {
            None
        } else {
            let file: GhContentFile = decode(succeeded(response).await?).await?;
            if file.text()? == wanted {
                return Ok(());
            }
            Some(file.sha)
        };
        let body = serde_json::to_value(PutContents {
            message: ENVIRONMENT_COMMIT_MESSAGE,
            content: base64_encode(wanted.as_bytes()),
            sha,
        })
        .expect("a contents write always serializes");
        succeeded(self.send(Method::PUT, url, Some(body)).await?)
            .await
            .map(drop)
    }

    /// The devcontainer a codespace of `project` builds by default, and its
    /// text, or `None` when the project has none and GitHub's default
    /// codespace image is used.
    async fn project_devcontainer(
        &self,
        project: &GitHubRepository,
    ) -> Result<Option<(String, String)>, CloudError> {
        let url = self.url(&format!("/repos/{project}/codespaces/devcontainers"));
        let listing: DevcontainerListing =
            decode(succeeded(self.send(Method::GET, url, None).await?).await?).await?;
        let Some(path) = DEFAULT_DEVCONTAINER_PATHS
            .iter()
            .find_map(|default| {
                listing
                    .devcontainers
                    .iter()
                    .find(|devcontainer| devcontainer.path == *default)
            })
            .or_else(|| listing.devcontainers.first())
            .map(|devcontainer| devcontainer.path.clone())
        else {
            return Ok(None);
        };
        let url = self.url(&format!("/repos/{project}/contents/{path}"));
        let file: GhContentFile =
            decode(succeeded(self.send(Method::GET, url, None).await?).await?).await?;
        Ok(Some((path, file.text()?)))
    }
}

/// Whether the devcontainer `text` gives its codespace an SSH server: the
/// SSH server feature, or GitHub's default image, which ships one.
///
/// # Errors
/// [`CloudError::Decode`] when `text` is not a devcontainer document.
fn has_ssh_server(text: &str) -> Result<bool, CloudError> {
    let devcontainer: DevcontainerSshProbe =
        jsonc_parser::parse_to_serde_value(text, &jsonc_parser::ParseOptions::default()).map_err(
            |error| CloudError::Decode {
                provider: PROVIDER,
                message: format!("the devcontainer is not readable: {error}"),
            },
        )?;
    let feature = devcontainer.features.keys().any(|feature| {
        feature
            .strip_prefix(SSHD_FEATURE)
            .is_some_and(|tag| tag.is_empty() || tag.starts_with(':'))
    });
    let image = devcontainer
        .image
        .is_some_and(|image| image.starts_with(UNIVERSAL_IMAGE));
    Ok(feature || image)
}

/// The environment repository's devcontainer document: GitHub's base
/// image with the SSH server feature, and a start command that starts the
/// daemon once May is installed.
fn environment_devcontainer() -> String {
    use askama::Template as _;

    let post_start = super::bootstrap::PostStart::default()
        .render()
        .expect("the post-start command always renders");
    let document = EnvironmentDevcontainer {
        name: "May",
        image: ENVIRONMENT_IMAGE,
        features: std::iter::once((ENVIRONMENT_SSHD_FEATURE, serde_json::json!({}))).collect(),
        post_start_command: post_start.trim(),
    };
    let mut text =
        serde_json::to_string_pretty(&document).expect("a devcontainer always serializes");
    text.push('\n');
    text
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;

    base64::engine::general_purpose::STANDARD.encode(bytes)
}

impl<T: GitHubTokenSource> CloudProvider for Codespaces<T> {
    type Source = CodespaceSource;

    /// The project itself when its devcontainer gives the codespace an SSH
    /// server, which installing May and starting its daemon go through;
    /// otherwise the environment repository, with the project to clone.
    async fn source(
        &self,
        project: Option<&GitHubRepository>,
    ) -> Result<CodespaceSource, CloudError> {
        let Some(project) = project else {
            return Ok(CodespaceSource {
                repository: self.environment_repository().await?,
                devcontainer_path: Some(ENVIRONMENT_DEVCONTAINER.to_owned()),
                clone: None,
            });
        };
        let devcontainer = self.project_devcontainer(project).await?;
        let reachable = match &devcontainer {
            Some((_, text)) => has_ssh_server(text)?,
            None => true,
        };
        if reachable {
            return Ok(CodespaceSource {
                repository: project.clone(),
                devcontainer_path: devcontainer.map(|(path, _)| path),
                clone: None,
            });
        }
        Ok(CodespaceSource {
            repository: self.environment_repository().await?,
            devcontainer_path: Some(ENVIRONMENT_DEVCONTAINER.to_owned()),
            clone: Some(project.clone()),
        })
    }

    async fn catalog(&self, source: &CodespaceSource) -> Result<Vec<MachineType>, CloudError> {
        let url = self.url(&format!("/repos/{}/codespaces/machines", source.repository));
        let listing: MachineListing =
            decode(succeeded(self.send(Method::GET, url, None).await?).await?).await?;
        Ok(listing.machines.into_iter().map(Into::into).collect())
    }

    async fn create(
        &self,
        source: &CodespaceSource,
        request: &NewMachine,
    ) -> Result<CloudMachine, CloudError> {
        let body = serde_json::to_value(CreateCodespace {
            machine: &request.machine_type,
            display_name: &request.display_name,
            devcontainer_path: source.devcontainer_path.as_deref(),
        })
        .expect("a codespace request always serializes");
        let url = self.url(&format!("/repos/{}/codespaces", source.repository));
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
    #[serde(skip_serializing_if = "Option::is_none")]
    devcontainer_path: Option<&'a str>,
}

/// `GET /repos/{owner}/{repo}/codespaces/devcontainers`, to the fields May
/// reads.
#[derive(Debug, Deserialize)]
struct DevcontainerListing {
    devcontainers: Vec<GhDevcontainer>,
}

/// One devcontainer of the listing.
#[derive(Debug, Deserialize)]
struct GhDevcontainer {
    path: String,
}

/// A file of `GET /repos/{owner}/{repo}/contents/{path}` (`content-file`),
/// to the fields May reads.
#[derive(Debug, Deserialize)]
struct GhContentFile {
    sha: String,
    encoding: String,
    content: String,
}

impl GhContentFile {
    /// The file's text: GitHub sends it base64-encoded, wrapped in lines.
    fn text(&self) -> Result<String, CloudError> {
        use base64::Engine as _;

        let decode_error = |message: String| CloudError::Decode {
            provider: PROVIDER,
            message,
        };
        if self.encoding != "base64" {
            return Err(decode_error(format!(
                "a file in the {} encoding",
                self.encoding
            )));
        }
        let packed: String = self.content.split_ascii_whitespace().collect();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(packed)
            .map_err(|error| decode_error(error.to_string()))?;
        String::from_utf8(bytes).map_err(|error| decode_error(error.to_string()))
    }
}

/// The body of `PUT /repos/{owner}/{repo}/contents/{path}`.
#[derive(Debug, Serialize)]
struct PutContents<'a> {
    message: &'a str,
    content: String,
    /// The replaced file's blob, when one is replaced.
    #[serde(skip_serializing_if = "Option::is_none")]
    sha: Option<String>,
}

/// A devcontainer document, to the fields that tell whether it has an SSH
/// server.
#[derive(Debug, Deserialize)]
struct DevcontainerSshProbe {
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    features: std::collections::BTreeMap<String, serde_json::Value>,
}

/// The environment repository's devcontainer document.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct EnvironmentDevcontainer<'a> {
    name: &'a str,
    image: &'a str,
    features: std::collections::BTreeMap<&'a str, serde_json::Value>,
    post_start_command: &'a str,
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
