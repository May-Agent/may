//! `GET /api/github/repos`: the forge repositories a new chat can be bound
//! to, for the web UI's repository picker.
//!
//! The route's name is kept for compatibility; the list it serves spans
//! every authenticated forge host — `github.com` and each configured
//! `[forge."<host>"]` GitHub or GitLab entry — merged with the checkouts the
//! machine used recently.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use futures_util::{StreamExt, TryStreamExt, stream};
use skyzen::utils::State;
use skyzen::{Result as SkyResult, StatusCode};
use zenwave::header::LINK;
use zenwave::{Method, ResponseExt};

use super::error::{internal_error, json_ok_response, typed_error_response};
use super::state::DaemonState;
use super::types::{ApiErrorCode, ApiGithubRepo, ApiGithubRepos};
use crate::forge::{ForgeHost, ForgeKind, ForgeRepo, GITHUB_HOST};
use crate::issue::IssueError;
use crate::issue::github::{GitHubRest, GitHubTokenSource};
use crate::issue::gitlab::{ForgeGitLabAuth, GitLabRest, GitLabTokenSource};

/// GitHub's maximum `per_page`.
const PAGE_SIZE: usize = 100;
/// Pages fetched at once after the first page has revealed the count.
const PAGE_CONCURRENCY: usize = 8;
/// Forge hosts listed at once.
const HOST_CONCURRENCY: usize = 8;

/// A repository one forge host lists: what binding a session takes, how the
/// picker labels it and where it lives.
pub(super) struct RemoteRepo {
    /// The canonical session binding (`github:owner/repo`,
    /// `gitlab:<host>/<path>`).
    pub(super) binding: String,
    /// `owner/repo` on `github.com`, `<host>/<path>` elsewhere.
    pub(super) full_name: String,
    /// `github` or `gitlab`.
    pub(super) forge: &'static str,
    /// The forge host.
    pub(super) host: String,
}

impl RemoteRepo {
    fn of(repo: &ForgeRepo) -> Self {
        Self {
            binding: repo.canonical_binding(),
            full_name: repo.cache_key(),
            forge: repo.kind().binding_prefix(),
            host: repo.host().as_str().to_string(),
        }
    }
}

/// `host`'s `[forge]` credential through the live authenticator: a GitHub
/// token source for the hosts `github.com` does not cover (GitHub
/// Enterprise).
#[derive(Debug, Clone)]
struct ForgeHostTokenSource {
    auth: crate::forge::LiveForgeAuth,
    host: String,
}

impl ForgeHostTokenSource {
    const fn new(auth: crate::forge::LiveForgeAuth, host: String) -> Self {
        Self { auth, host }
    }
}

impl GitHubTokenSource for ForgeHostTokenSource {
    async fn token(&self) -> Result<String, IssueError> {
        self.auth
            .token_for(&self.host)
            .await
            .map_err(|error| IssueError::GitHubAuth(error.to_string()))
    }
}

/// What the per-host fetches produced.
enum FetchOutcome {
    /// At least one host answered; its repositories (possibly none).
    Repos(Vec<RemoteRepo>),
    /// Every configured host failed authentication.
    Unauthenticated(String),
    /// A host failed for another reason and nothing answered.
    Failed(IssueError),
}

type HostJob = Pin<Box<dyn Future<Output = Result<Vec<RemoteRepo>, IssueError>> + Send>>;

pub(super) async fn list_github_repos(
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<skyzen::Response> {
    let config = state.config_snapshot();
    let db = state.db();
    let (remote, recent) = futures_lite::future::zip(
        // The forge clients run on the tokio runtime, like the other
        // daemon handlers that reach the network.
        async {
            state
                .tokio_handle
                .spawn(fetch_forge_repositories(state.clone(), config.clone()))
                .await
                .map_err(internal_error)
        },
        async {
            let usage = db.repo_usage_since(0).await.map_err(internal_error)?;
            Ok::<_, skyzen::Error>(recent_repositories(usage, &config).await)
        },
    )
    .await;
    let recent = recent?;
    match remote? {
        FetchOutcome::Repos(accessible) => json_ok_response(&merge(recent, accessible)),
        FetchOutcome::Unauthenticated(message) => Ok(typed_error_response(
            StatusCode::FAILED_DEPENDENCY,
            ApiErrorCode::GithubUnauthenticated,
            message,
        )),
        FetchOutcome::Failed(error) => Err(internal_error(error)),
    }
}

/// The repositories every configured forge host lists: `github.com` through
/// the shared client, each `[forge."<host>"]` entry through its own —
/// GitHub Enterprise at `https://<host>/api/v3`, GitLab at
/// `https://<host>/api/v4`.
async fn fetch_forge_repositories(
    state: Arc<DaemonState>,
    config: Arc<crate::config::MayConfig>,
) -> FetchOutcome {
    let mut jobs: Vec<HostJob> = Vec::new();
    {
        let github = state.github_api.clone();
        jobs.push(Box::pin(async move {
            github_repositories(&github, GITHUB_HOST).await
        }));
    }
    let forge = state.forge_auth.clone();
    for host in config.forge.keys() {
        if host == GITHUB_HOST {
            // Covered by `state.github_api`, whose api_base tests override.
            continue;
        }
        match config.forge_kind(host) {
            Some(ForgeKind::GitHub) => {
                let rest = GitHubRest::with_api_base(
                    ForgeHostTokenSource::new(forge.clone(), host.clone()),
                    &format!("https://{host}/api/v3"),
                );
                let host = host.clone();
                jobs.push(Box::pin(
                    async move { github_repositories(&rest, &host).await },
                ));
            }
            Some(ForgeKind::GitLab) => {
                let rest = GitLabRest::new(ForgeGitLabAuth::new(forge.clone(), host.clone()), host);
                let host = host.clone();
                jobs.push(Box::pin(async move { gitlab_projects(&rest, &host).await }));
            }
            // A host of unknown kind cannot be listed.
            None => {}
        }
    }

    let mut repos = Vec::new();
    let mut succeeded = false;
    let mut auth_error = None;
    let mut other_error = None;
    let mut results = stream::iter(jobs).buffer_unordered(HOST_CONCURRENCY);
    while let Some(result) = results.next().await {
        match result {
            Ok(list) => {
                succeeded = true;
                repos.extend(list);
            }
            Err(error @ (IssueError::GitHubAuth(_) | IssueError::GitLabAuth(_))) => {
                auth_error.get_or_insert_with(|| error.to_string());
            }
            Err(error) => {
                tracing::warn!(%error, "forge repository listing failed");
                other_error.get_or_insert(error);
            }
        }
    }
    if succeeded {
        return FetchOutcome::Repos(repos);
    }
    if let Some(error) = other_error {
        return FetchOutcome::Failed(error);
    }
    FetchOutcome::Unauthenticated(
        auth_error.unwrap_or_else(|| "no forge host is authenticated".to_string()),
    )
}

/// `ForgeRepo`s of the forge checkouts in the usage registry, latest use first.
async fn recent_repositories(
    mut usage: Vec<crate::db::RepoUsage>,
    config: &Arc<crate::config::MayConfig>,
) -> Vec<RemoteRepo> {
    usage.sort_by_key(|entry| std::cmp::Reverse(entry.last_used_at));
    let paths: Vec<PathBuf> = usage.iter().map(crate::db::RepoUsage::path).collect();
    let config = config.clone();
    blocking::unblock(move || {
        paths
            .iter()
            .filter(|path| path.is_dir())
            .filter_map(
                |path| match crate::trust::repo_kind_with_config(path, &config) {
                    crate::trust::RepoKind::Forge { repo } => Some(RemoteRepo::of(&repo)),
                    crate::trust::RepoKind::Git => None,
                },
            )
            .collect()
    })
    .await
}

/// Recently used repositories first, then the rest in each host's order,
/// deduplicated on the canonical binding.
fn merge(recent: Vec<RemoteRepo>, accessible: Vec<RemoteRepo>) -> ApiGithubRepos {
    let mut seen = std::collections::HashSet::new();
    let mut repos = Vec::new();
    for (items, is_recent) in [(recent, true), (accessible, false)] {
        for item in items {
            if seen.insert(item.binding.to_ascii_lowercase()) {
                repos.push(ApiGithubRepo {
                    full_name: item.full_name,
                    recent: is_recent,
                    binding: item.binding,
                    forge: item.forge.to_string(),
                    host: item.host,
                });
            }
        }
    }
    ApiGithubRepos { repos }
}

#[derive(serde::Deserialize)]
struct GhRepoName {
    full_name: String,
}

/// The GitHub `full_name`s on `host`, as forge repository items.
fn github_item(host: &str, full_name: &str) -> Result<RemoteRepo, IssueError> {
    let repo = match full_name.split_once('/') {
        Some((owner, name)) => ForgeRepo::new(
            ForgeKind::GitHub,
            ForgeHost::new(host).map_err(|e| IssueError::GitHub(e.to_string()))?,
            owner,
            name,
        )
        .map_err(|e| IssueError::GitHub(e.to_string()))?,
        None => return Err(IssueError::GitHub(format!("bad full_name {full_name}"))),
    };
    Ok(RemoteRepo::of(&repo))
}

/// Every repository the token's account owns, collaborates on, or reaches
/// through an organization.
///
/// The first page reveals the page count through its `rel="last"` link; the
/// remaining pages are then fetched [`PAGE_CONCURRENCY`] at a time. A first
/// page that links a next page but no last one leaves the count unknown, and
/// the pages are followed one after another.
async fn github_repositories<T: GitHubTokenSource>(
    rest: &GitHubRest<T>,
    host: &str,
) -> Result<Vec<RemoteRepo>, IssueError> {
    let first = format!(
        "{}/user/repos?affiliation=owner,collaborator,organization_member&sort=pushed&per_page={PAGE_SIZE}",
        rest.api_base()
    );
    let (mut names, links) = github_page(rest, first.clone()).await?;
    if let Some(last) = links.last {
        let rest_pages = stream::iter(2..=last)
            .map(|number| github_page(rest, format!("{first}&page={number}")))
            .buffered(PAGE_CONCURRENCY)
            .map_ok(|(names, _)| names)
            .try_collect::<Vec<_>>()
            .await?;
        names.extend(rest_pages.into_iter().flatten());
    } else {
        let mut next = links.next;
        while let Some(url) = next {
            let (more, links) = github_page(rest, url).await?;
            names.extend(more);
            next = links.next;
        }
    }
    names
        .iter()
        .map(|name| github_item(host, name))
        .collect::<Result<Vec<_>, _>>()
}

#[derive(Default)]
struct PageLinks {
    next: Option<String>,
    last: Option<u32>,
}

async fn github_page<T: GitHubTokenSource>(
    rest: &GitHubRest<T>,
    url: String,
) -> Result<(Vec<String>, PageLinks), IssueError> {
    let response = rest.request(Method::GET, url, None).await?;
    let links = response
        .headers()
        .get_all(LINK)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .fold(PageLinks::default(), |mut links, value| {
            links.next = links.next.take().or_else(|| link_target(value, "next"));
            links.last = links.last.or_else(|| {
                link_target(value, "last").and_then(|url| {
                    url.split(['?', '&'])
                        .find_map(|part| part.strip_prefix("page=")?.parse().ok())
                })
            });
            links
        });
    let repos = response
        .into_json::<Vec<GhRepoName>>()
        .await
        .map_err(|error| IssueError::GitHub(error.to_string()))?;
    Ok((
        repos.into_iter().map(|repo| repo.full_name).collect(),
        links,
    ))
}

/// The target of the `rel` entry of an RFC 8288 `Link` header value.
fn link_target(link: &str, rel: &str) -> Option<String> {
    let rel = format!("rel=\"{rel}\"");
    link.split('<').skip(1).find_map(|entry| {
        let (target, params) = entry.split_once('>')?;
        params.contains(&rel).then(|| target.to_string())
    })
}

#[derive(serde::Deserialize)]
struct GlProjectPath {
    path_with_namespace: String,
}

/// Every project the token's account is a member of on the GitLab at
/// `host`, most recently active first, `x-next-page` links followed.
pub(super) async fn gitlab_projects<T: GitLabTokenSource>(
    rest: &GitLabRest<T>,
    host: &str,
) -> Result<Vec<RemoteRepo>, IssueError> {
    let mut repos = Vec::new();
    let mut page = 1_u32;
    loop {
        let url = format!(
            "{}/projects?membership=true&order_by=last_activity_at&per_page={PAGE_SIZE}&page={page}",
            rest.api_base()
        );
        let response = rest.request(Method::GET, url, None).await?;
        let next = response
            .headers()
            .get("x-next-page")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u32>().ok());
        let items = response
            .into_json::<Vec<GlProjectPath>>()
            .await
            .map_err(|error| IssueError::GitLab(error.to_string()))?;
        for item in items {
            let repo =
                ForgeRepo::parse_binding(&format!("gitlab:{host}/{}", item.path_with_namespace))
                    .map_err(|error| IssueError::GitLab(error.to_string()))?;
            repos.push(RemoteRepo::of(&repo));
        }
        match next {
            Some(next) => page = next,
            None => return Ok(repos),
        }
    }
}
