//! `GET /api/github/repos`: the GitHub repositories a new chat can be bound
//! to, for the web UI's repository picker.

use std::path::PathBuf;
use std::sync::Arc;

use futures_util::{StreamExt, TryStreamExt, stream};
use skyzen::utils::State;
use skyzen::{Result as SkyResult, StatusCode};
use zenwave::header::LINK;
use zenwave::{Method, ResponseExt};

use super::error::{internal_error, json_ok_response, typed_error_response};
use super::state::DaemonState;
use super::types::{ApiErrorCode, ApiGithubRepo, ApiGithubRepos};
use crate::issue::IssueError;
use crate::issue::github::{GitHubRest, GitHubTokenSource};

/// GitHub's maximum `per_page`.
const PAGE_SIZE: usize = 100;
/// Pages fetched at once after the first page has revealed the count.
const PAGE_CONCURRENCY: usize = 8;

pub(super) async fn list_github_repos(
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<skyzen::Response> {
    let github = state.github_api.clone();
    let db = state.db();
    let (accessible, recent) = futures_lite::future::zip(
        // The GitHub client runs on the tokio runtime, like the other
        // daemon handlers that reach the network.
        async {
            state
                .tokio_handle
                .spawn(async move { accessible_repositories(&github).await })
                .await
                .map_err(internal_error)
        },
        async {
            let usage = db.repo_usage_since(0).await.map_err(internal_error)?;
            Ok::<_, skyzen::Error>(recent_repositories(usage).await)
        },
    )
    .await;
    let recent = recent?;
    let accessible = match accessible? {
        Ok(accessible) => accessible,
        Err(IssueError::GitHubAuth(message)) => {
            return Ok(typed_error_response(
                StatusCode::FAILED_DEPENDENCY,
                ApiErrorCode::GithubUnauthenticated,
                message,
            ));
        }
        Err(error) => return Err(internal_error(error)),
    };
    json_ok_response(&merge(recent, accessible))
}

/// `owner/repo` of the GitHub checkouts in the usage registry, latest use first.
async fn recent_repositories(mut usage: Vec<crate::db::RepoUsage>) -> Vec<String> {
    usage.sort_by_key(|entry| std::cmp::Reverse(entry.last_used_at));
    let paths: Vec<PathBuf> = usage.iter().map(crate::db::RepoUsage::path).collect();
    blocking::unblock(move || {
        paths
            .iter()
            .filter(|path| path.is_dir())
            .filter_map(|path| match crate::trust::repo_kind(path) {
                crate::trust::RepoKind::Github { owner_repo } => Some(owner_repo),
                crate::trust::RepoKind::Git => None,
            })
            .collect()
    })
    .await
}

/// Recently used repositories first, then the rest in GitHub's order, each once.
fn merge(recent: Vec<String>, accessible: Vec<String>) -> ApiGithubRepos {
    let mut seen = std::collections::HashSet::new();
    let mut repos = Vec::new();
    for (names, is_recent) in [(recent, true), (accessible, false)] {
        for full_name in names {
            if seen.insert(full_name.to_ascii_lowercase()) {
                repos.push(ApiGithubRepo {
                    full_name,
                    recent: is_recent,
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

/// Every repository the token's account owns, collaborates on, or reaches
/// through an organization.
///
/// The first page reveals the page count through its `rel="last"` link; the
/// remaining pages are then fetched [`PAGE_CONCURRENCY`] at a time. A first
/// page that links a next page but no last one leaves the count unknown, and
/// the pages are followed one after another.
async fn accessible_repositories<T: GitHubTokenSource>(
    rest: &GitHubRest<T>,
) -> Result<Vec<String>, IssueError> {
    let first = format!(
        "{}/user/repos?affiliation=owner,collaborator,organization_member&sort=pushed&per_page={PAGE_SIZE}",
        rest.api_base()
    );
    let (mut names, links) = page(rest, first.clone()).await?;
    if let Some(last) = links.last {
        let rest_pages = stream::iter(2..=last)
            .map(|number| page(rest, format!("{first}&page={number}")))
            .buffered(PAGE_CONCURRENCY)
            .map_ok(|(names, _)| names)
            .try_collect::<Vec<_>>()
            .await?;
        names.extend(rest_pages.into_iter().flatten());
    } else {
        let mut next = links.next;
        while let Some(url) = next {
            let (more, links) = page(rest, url).await?;
            names.extend(more);
            next = links.next;
        }
    }
    Ok(names)
}

#[derive(Default)]
struct PageLinks {
    next: Option<String>,
    last: Option<u32>,
}

async fn page<T: GitHubTokenSource>(
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
