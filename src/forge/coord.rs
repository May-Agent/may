//! Forge coordinates: which forge software a host runs and where a
//! repository lives on it.
//!
//! [`ForgeRepo`] is the forge-neutral identity of one repository: its kind,
//! its host and its path. The canonical string form — what `sessions.repo`
//! and `workspace.toml` store — is `github:owner/repo` for `github.com`
//! (unchanged from before), `github:<host>/owner/repo` for other GitHub
//! hosts, and `gitlab:<host>/<namespace/...>/<repo>` for every GitLab
//! repository, host always explicit.

use serde::{Deserialize, Serialize};

/// The default host for each forge kind.
pub const GITHUB_HOST: &str = "github.com";
/// The default GitLab host.
pub const GITLAB_HOST: &str = "gitlab.com";

/// The forge software a host runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForgeKind {
    /// GitHub or GitHub Enterprise Server.
    GitHub,
    /// `GitLab.com` or a self-managed GitLab instance.
    GitLab,
}

impl ForgeKind {
    /// The kind a well-known hostname implies; `None` for hosts only the
    /// configuration can name.
    #[must_use]
    pub fn of_host(host: &str) -> Option<Self> {
        match host {
            GITHUB_HOST => Some(Self::GitHub),
            GITLAB_HOST => Some(Self::GitLab),
            _ => None,
        }
    }

    /// The kind's canonical binding prefix.
    #[must_use]
    pub const fn binding_prefix(self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::GitLab => "gitlab",
        }
    }

    /// The host the kind defaults to in shorthand bindings.
    #[must_use]
    pub const fn default_host(self) -> &'static str {
        match self {
            Self::GitHub => GITHUB_HOST,
            Self::GitLab => GITLAB_HOST,
        }
    }

    /// The HTTP Basic username git accepts for token authentication against
    /// this forge over HTTPS.
    ///
    /// GitHub reads `x-access-token:<token>`; GitLab reads `oauth2:<token>`
    /// for OAuth tokens and personal access tokens alike.
    #[must_use]
    pub const fn git_basic_username(self) -> &'static str {
        match self {
            Self::GitHub => "x-access-token",
            Self::GitLab => "oauth2",
        }
    }
}

/// Why a string is not a [`ForgeHost`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid forge host {0:?}: expected host[:port], no scheme, path or whitespace")]
pub struct ForgeHostError(String);

/// A canonical forge hostname: lowercase, `host` or `host:port`, no scheme,
/// path or whitespace.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ForgeHost(String);

impl ForgeHost {
    /// Parse `raw` (`host` or `host:port`, brackets around IPv6) into its
    /// canonical lowercase form.
    ///
    /// # Errors
    /// Returns [`ForgeHostError`] when `raw` is not a host with an optional
    /// port: a scheme, path, userinfo, query, fragment or whitespace fails.
    pub fn new(raw: &str) -> Result<Self, ForgeHostError> {
        let invalid = || ForgeHostError(raw.to_string());
        if raw.is_empty()
            || raw.bytes().any(|b| {
                b.is_ascii_whitespace() || matches!(b, b'/' | b'@' | b'?' | b'#' | b'%' | b'\\')
            })
            || raw.contains("://")
        {
            return Err(invalid());
        }
        let (name, port) = if let Some(rest) = raw.strip_prefix('[') {
            // Bracketed IPv6: [::1] or [::1]:8443. The bracket check already
            // constrained the characters, so the label rule does not apply.
            let (address, tail) = rest.split_once(']').ok_or_else(invalid)?;
            if address.is_empty()
                || !address
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() || matches!(b, b':' | b'.'))
            {
                return Err(invalid());
            }
            let port = match tail.strip_prefix(':') {
                Some(digits) => Some(parse_port(digits).ok_or_else(invalid)?),
                None if tail.is_empty() => None,
                _ => return Err(invalid()),
            };
            let address = address.to_ascii_lowercase();
            return Ok(Self(port.map_or_else(
                || format!("[{address}]"),
                |port| format!("[{address}]:{port}"),
            )));
        } else {
            match raw.split_once(':') {
                Some((name, digits)) => (
                    name.to_ascii_lowercase(),
                    Some(parse_port(digits).ok_or_else(invalid)?),
                ),
                None => (raw.to_ascii_lowercase(), None),
            }
        };
        if name.is_empty()
            || !name
                .split('.')
                .all(|label| !label.is_empty() && is_host_label(label))
        {
            return Err(invalid());
        }
        Ok(Self(match port {
            Some(port) => format!("{name}:{port}"),
            None => name,
        }))
    }

    /// The canonical `host[:port]` string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn parse_port(digits: &str) -> Option<u16> {
    if digits.is_empty() || digits.len() > 5 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let port = digits.parse::<u16>().ok()?;
    (port > 0).then_some(port)
}

/// One hostname label: alphanumerics and inner hyphens (`localhost` and IPv4
/// literals fit the same rule).
fn is_host_label(label: &str) -> bool {
    label.len() <= 63
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && label
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && label
            .bytes()
            .last()
            .is_some_and(|b| b.is_ascii_alphanumeric())
}

impl std::fmt::Display for ForgeHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for ForgeHost {
    type Err = ForgeHostError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for ForgeHost {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ForgeHost {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::new(&value).map_err(serde::de::Error::custom)
    }
}

/// Why a string is not a [`ForgeRepo`].
#[derive(Debug, thiserror::Error)]
pub enum ForgeRepoError {
    /// The binding's syntax or path shape is wrong.
    #[error("invalid forge binding {0:?}")]
    Binding(String),
    /// A host part is not a [`ForgeHost`].
    #[error("invalid forge host {0:?}: {1}")]
    Host(String, #[source] ForgeHostError),
    /// The remote URL's syntax is wrong.
    #[error("invalid git remote URL {0:?}")]
    RemoteUrl(String),
    /// The host names no known forge kind and has no `[forge]` entry.
    #[error("forge kind of {0:?} is unknown; set kind in [forge.\"{0}\"] of config.toml")]
    UnknownKind(String),
}

/// One repository on one forge host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeRepo {
    kind: ForgeKind,
    host: ForgeHost,
    /// `owner` on GitHub; `group` or `group/sub/...` on GitLab.
    namespace: String,
    /// The project or repository slug, without a trailing `.git`.
    name: String,
}

impl ForgeRepo {
    /// Build the repository directly from parts; segment checks apply.
    ///
    /// # Errors
    /// Returns [`ForgeRepoError::Binding`] when `namespace` or `name` is not a
    /// valid path: empty, a `.`/`..` segment, whitespace or a stray slash.
    /// GitHub namespaces hold exactly one segment.
    pub fn new(
        kind: ForgeKind,
        host: ForgeHost,
        namespace: &str,
        name: &str,
    ) -> Result<Self, ForgeRepoError> {
        let repo = Self {
            kind,
            host,
            namespace: namespace.to_string(),
            name: name.to_string(),
        };
        repo.check_path().map_err(|()| {
            ForgeRepoError::Binding(format!("{}:{namespace}/{name}", kind.binding_prefix()))
        })?;
        Ok(repo)
    }

    /// Path segments are all real: non-empty, not `.`/`..`, no whitespace.
    /// GitHub's namespace is exactly the owner segment.
    fn check_path(&self) -> Result<(), ()> {
        let path = self.full_path();
        if path.is_empty()
            || path.starts_with('/')
            || path.ends_with('/')
            || path.bytes().any(|b| b.is_ascii_whitespace())
        {
            return Err(());
        }
        let segments: Vec<&str> = path.split('/').collect();
        if segments
            .iter()
            .any(|s| s.is_empty() || *s == "." || *s == "..")
        {
            return Err(());
        }
        if self.namespace.is_empty()
            || self.name.is_empty()
            || self.name != segments.last().copied().unwrap_or_default()
        {
            return Err(());
        }
        if self.kind == ForgeKind::GitHub && self.namespace.contains('/') {
            return Err(());
        }
        Ok(())
    }

    /// Parse a canonical binding: `github:owner/repo`,
    /// `github:<host>/owner/repo`, `gitlab:<host>/<namespace/...>/<repo>` or
    /// the GitLab shorthand `gitlab:<group>/<repo>` (`gitlab.com`).
    ///
    /// # Errors
    /// Returns [`ForgeRepoError`] when `binding` names no repository.
    pub fn parse_binding(binding: &str) -> Result<Self, ForgeRepoError> {
        let invalid = || ForgeRepoError::Binding(binding.to_string());
        let (prefix, rest) = binding.split_once(':').ok_or_else(invalid)?;
        let kind = match prefix {
            "github" => ForgeKind::GitHub,
            "gitlab" => ForgeKind::GitLab,
            _ => return Err(invalid()),
        };
        let segments: Vec<&str> = rest.split('/').collect();
        let (host, path) = match kind {
            // GitHub paths are always owner/repo: a third segment can only
            // be an explicit host in front of them.
            ForgeKind::GitHub => match segments.as_slice() {
                [owner, repo] => (
                    ForgeHost(GITHUB_HOST.to_string()),
                    format!("{owner}/{repo}"),
                ),
                [host, owner, repo] => (
                    ForgeHost::new(host)
                        .map_err(|source| ForgeRepoError::Host((*host).to_string(), source))?,
                    format!("{owner}/{repo}"),
                ),
                _ => return Err(invalid()),
            },
            // A first segment that looks like a host (`host.name`,
            // `localhost`, `name:port`) is the host; otherwise the whole
            // path is a `gitlab.com` namespace.
            ForgeKind::GitLab => {
                let explicit_host = segments.first().is_some_and(|first| looks_like_host(first));
                if explicit_host {
                    if segments.len() < 3 {
                        return Err(invalid());
                    }
                    (
                        ForgeHost::new(segments[0]).map_err(|source| {
                            ForgeRepoError::Host(segments[0].to_string(), source)
                        })?,
                        segments[1..].join("/"),
                    )
                } else {
                    if segments.len() < 2 {
                        return Err(invalid());
                    }
                    (ForgeHost(GITLAB_HOST.to_string()), rest.to_string())
                }
            }
        };
        let (namespace, name) = path.rsplit_once('/').ok_or_else(invalid)?;
        Self::new(kind, host, namespace, name)
    }

    /// Parse a git remote URL — `https://host/ns/repo(.git)`,
    /// `ssh://git@host(:port)/ns/repo(.git)` or `git@host:ns/repo(.git)` —
    /// against `config`'s `[forge]` map for the host's kind.
    ///
    /// # Errors
    /// Returns [`ForgeRepoError::RemoteUrl`] for a URL that is not a git
    /// remote, and [`ForgeRepoError::UnknownKind`] when the host's kind is
    /// neither known nor configured.
    pub fn from_remote_url(
        remote: &str,
        config: &crate::config::MayConfig,
    ) -> Result<Self, ForgeRepoError> {
        Self::from_remote_url_with(remote, |host| config.forge_kind(host))
    }

    /// [`Self::from_remote_url`] with the host's kind resolved by `kind`
    /// instead of a `MayConfig` — the live forge authenticator answers it
    /// for the same `[forge]` map.
    ///
    /// # Errors
    /// As [`Self::from_remote_url`].
    pub fn from_remote_url_with(
        remote: &str,
        kind: impl Fn(&str) -> Option<ForgeKind>,
    ) -> Result<Self, ForgeRepoError> {
        let invalid = || ForgeRepoError::RemoteUrl(remote.to_string());
        let remote = remote.trim();
        let (authority, path) = match remote.split_once("://") {
            Some((scheme, rest)) if matches!(scheme, "http" | "https" | "ssh") => {
                let (authority, path) = rest.split_once('/').ok_or_else(invalid)?;
                let authority = authority
                    .rsplit_once('@')
                    .map_or(authority, |(_, host)| host);
                // An SSH port is not the forge's HTTPS port; only http(s)
                // ports carry over.
                let authority = if scheme == "ssh" {
                    authority.split(':').next().unwrap_or(authority)
                } else {
                    authority
                };
                (authority.to_string(), path)
            }
            _ => {
                // scp-style: `user@host:ns/repo(.git)`.
                let (_, host_path) = remote.split_once('@').ok_or_else(invalid)?;
                let (host, path) = host_path.split_once(':').ok_or_else(invalid)?;
                (host.to_string(), path)
            }
        };
        if authority.is_empty() || path.is_empty() {
            return Err(invalid());
        }
        let host = ForgeHost::new(&authority)
            .map_err(|source| ForgeRepoError::Host(authority.clone(), source))?;
        let kind = kind(host.as_str())
            .ok_or_else(|| ForgeRepoError::UnknownKind(host.as_str().to_string()))?;
        let path = path.trim_end_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        if path.split('/').count() < 2 {
            return Err(invalid());
        }
        let (namespace, name) = path.rsplit_once('/').ok_or_else(invalid)?;
        Self::new(kind, host, namespace, name)
    }

    /// The forge kind.
    #[must_use]
    pub const fn kind(&self) -> ForgeKind {
        self.kind
    }

    /// The canonical forge host.
    #[must_use]
    pub const fn host(&self) -> &ForgeHost {
        &self.host
    }

    /// The owner or group(/subgroup/…) path.
    #[must_use]
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// The repository slug.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `owner/repo` or `group/sub/.../repo`.
    #[must_use]
    pub fn full_path(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }

    /// `github.com/owner/repo` or `gitlab.com/group/sub/repo`.
    #[must_use]
    pub fn host_and_path(&self) -> String {
        format!("{}/{}", self.host, self.full_path())
    }

    /// `namespace%2Frepo`: the project id GitLab REST calls take in
    /// `/api/v4/projects/:id`.
    #[must_use]
    pub fn gitlab_encoded_path(&self) -> String {
        percent_encoding::utf8_percent_encode(&self.full_path(), GITLAB_PATH_ENCODE).to_string()
    }

    /// `https://<host>/<namespace>/<name>.git`.
    #[must_use]
    pub fn https_clone_url(&self) -> String {
        format!("https://{}/{}.git", self.host, self.full_path())
    }

    /// The web URL of issue `number` on this forge.
    #[must_use]
    pub fn issue_web_url(&self, number: u64) -> String {
        match self.kind {
            ForgeKind::GitHub => {
                format!("https://{}/{}/issues/{number}", self.host, self.full_path())
            }
            ForgeKind::GitLab => {
                format!(
                    "https://{}/{}/-/issues/{number}",
                    self.host,
                    self.full_path()
                )
            }
        }
    }

    /// The web URL of merge request / pull request `number` on this forge.
    #[must_use]
    pub fn merge_web_url(&self, number: u64) -> String {
        match self.kind {
            ForgeKind::GitHub => {
                format!("https://{}/{}/pull/{number}", self.host, self.full_path())
            }
            ForgeKind::GitLab => {
                format!(
                    "https://{}/{}/-/merge_requests/{number}",
                    self.host,
                    self.full_path()
                )
            }
        }
    }

    /// The sigil that introduces a pull/merge request number: `#7` on
    /// GitHub, `!7` on GitLab.
    #[must_use]
    pub const fn pull_sigil(&self) -> char {
        match self.kind {
            ForgeKind::GitHub => '#',
            ForgeKind::GitLab => '!',
        }
    }

    /// The canonical stored form — `github:owner/repo`,
    /// `github:<host>/owner/repo`, `gitlab:<host>/<namespace>/<repo>`.
    #[must_use]
    pub fn canonical_binding(&self) -> String {
        match self.kind {
            ForgeKind::GitHub if self.host.as_str() == GITHUB_HOST => {
                format!("github:{}", self.full_path())
            }
            _ => format!(
                "{}:{}/{}",
                self.kind.binding_prefix(),
                self.host,
                self.full_path()
            ),
        }
    }

    /// The key clone registries and daemon checkouts use for this
    /// repository: `owner/repo` on `github.com` (unchanged from before),
    /// `host/path` everywhere else.
    #[must_use]
    pub fn cache_key(&self) -> String {
        match self.kind {
            ForgeKind::GitHub if self.host.as_str() == GITHUB_HOST => self.full_path(),
            _ => self.host_and_path(),
        }
    }

    /// The repository a [`Self::cache_key`] names: `owner/repo` on
    /// `github.com`, `<host>/<namespace>/<repo>` everywhere else.
    ///
    /// # Errors
    /// Returns [`ForgeRepoError::Binding`] when `key` is not a cache key of
    /// `kind`, and [`ForgeRepoError::Host`] for a bad host part.
    pub fn from_cache_key(kind: ForgeKind, key: &str) -> Result<Self, ForgeRepoError> {
        let segments: Vec<&str> = key.split('/').collect();
        let (host, path) = match kind {
            ForgeKind::GitHub if segments.len() == 2 => (GITHUB_HOST.to_string(), key.to_string()),
            _ if segments.len() >= 3 => (segments[0].to_string(), segments[1..].join("/")),
            _ => return Err(ForgeRepoError::Binding(key.to_string())),
        };
        let host =
            ForgeHost::new(&host).map_err(|source| ForgeRepoError::Host(host.clone(), source))?;
        let (namespace, name) = path
            .rsplit_once('/')
            .ok_or_else(|| ForgeRepoError::Binding(key.to_string()))?;
        Self::new(kind, host, namespace, name)
    }

    /// The binding `parse_binding` accepts, plus the bare `owner/repo`
    /// `workspace.toml` and session rows wrote before the canonical form:
    /// it means `github:owner/repo`. A bare `host/ns/repo` names the GitLab
    /// repository when it cannot be a GitHub `owner/repo`.
    ///
    /// # Errors
    /// As [`Self::parse_binding`].
    pub fn parse_stored(value: &str) -> Result<Self, ForgeRepoError> {
        Self::parse_binding(value)
            .or_else(|_| Self::parse_binding(&format!("github:{value}")))
            .or_else(|_| Self::parse_binding(&format!("gitlab:{value}")))
    }
}

impl std::str::FromStr for ForgeRepo {
    type Err = ForgeRepoError;

    /// The stored form [`Self::parse_stored`] accepts.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse_stored(value)
    }
}

impl Serialize for ForgeRepo {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.canonical_binding())
    }
}

impl<'de> Deserialize<'de> for ForgeRepo {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse_stored(&value).map_err(serde::de::Error::custom)
    }
}

/// Whether `segment` reads as a host rather than a namespace segment: it
/// carries a dot or a port, or is `localhost`.
pub fn looks_like_host(segment: &str) -> bool {
    segment == "localhost" || segment.contains('.') || segment.contains(':')
}

/// Percent-encode set for [`ForgeRepo::gitlab_encoded_path`]: everything
/// outside an unreserved URI character is escaped, so the `/` separators
/// become `%2F` and the whole path is one token.
const GITLAB_PATH_ENCODE: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// `value` percent-encoded as one GitLab API path segment — the set
/// [`ForgeRepo::gitlab_encoded_path`] uses, for path pieces such as a
/// branch name that may carry `/`.
#[must_use]
pub fn url_path_encode(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, GITLAB_PATH_ENCODE).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(binding: &str) -> ForgeRepo {
        ForgeRepo::parse_binding(binding).unwrap_or_else(|e| panic!("{binding}: {e}"))
    }

    fn config(toml_str: &str) -> crate::config::MayConfig {
        toml::from_str(toml_str).expect("config")
    }

    #[test]
    fn github_default_binding_round_trips() {
        let repo = repo("github:acme/app");
        assert_eq!(repo.kind(), ForgeKind::GitHub);
        assert_eq!(repo.host().as_str(), "github.com");
        assert_eq!(repo.namespace(), "acme");
        assert_eq!(repo.name(), "app");
        assert_eq!(repo.canonical_binding(), "github:acme/app");
    }

    #[test]
    fn github_explicit_host_round_trips() {
        let repo = repo("github:ghe.corp.example.com/acme/app");
        assert_eq!(repo.host().as_str(), "ghe.corp.example.com");
        assert_eq!(
            repo.canonical_binding(),
            "github:ghe.corp.example.com/acme/app"
        );
    }

    #[test]
    fn github_default_host_stays_canonical_when_named() {
        let repo = repo("github:github.com/acme/app");
        assert_eq!(repo.host().as_str(), "github.com");
        assert_eq!(repo.canonical_binding(), "github:acme/app");
    }

    #[test]
    fn gitlab_shorthand_normalizes_to_gitlab_com() {
        for (binding, canonical) in [
            ("gitlab:group/repo", "gitlab:gitlab.com/group/repo"),
            ("gitlab:group/sub/repo", "gitlab:gitlab.com/group/sub/repo"),
        ] {
            let repo = repo(binding);
            assert_eq!(repo.kind(), ForgeKind::GitLab);
            assert_eq!(repo.host().as_str(), "gitlab.com", "{binding}");
            // Internal form always carries the explicit host.
            assert_eq!(repo.canonical_binding(), canonical, "{binding}");
        }
    }

    #[test]
    fn gitlab_explicit_host_and_deep_namespace() {
        let repo = repo("gitlab:git.corp.example.com/group/sub/repo");
        assert_eq!(repo.kind(), ForgeKind::GitLab);
        assert_eq!(repo.host().as_str(), "git.corp.example.com");
        assert_eq!(repo.namespace(), "group/sub");
        assert_eq!(repo.name(), "repo");
        assert_eq!(
            repo.canonical_binding(),
            "gitlab:git.corp.example.com/group/sub/repo"
        );
    }

    #[test]
    fn gitlab_host_like_first_segments() {
        for (binding, host) in [
            ("gitlab:gitlab.com/group/repo", "gitlab.com"),
            ("gitlab:localhost:8080/group/repo", "localhost:8080"),
            ("gitlab:localhost/group/repo", "localhost"),
        ] {
            let repo = repo(binding);
            assert_eq!(repo.host().as_str(), host, "{binding}");
        }
    }

    #[test]
    fn malformed_bindings_fail() {
        for binding in [
            "github:",
            "github:acme",
            "github:a/b/c/d",
            "gitlab:",
            "gitlab:group",
            "gitlab:gitlab.com/repo",
            "gitlab:git.corp.com/repo",
            "gitea:a/b",
            "a/b",
            "github:UPPER CASE/repo/x",
        ] {
            assert!(ForgeRepo::parse_binding(binding).is_err(), "{binding}");
        }
    }

    #[test]
    fn forge_host_validation() {
        for (raw, canonical) in [
            ("github.com", "github.com"),
            ("GitHub.COM", "github.com"),
            ("git.corp.com:8443", "git.corp.com:8443"),
            ("localhost", "localhost"),
            ("localhost:3000", "localhost:3000"),
            ("127.0.0.1:8080", "127.0.0.1:8080"),
            ("[::1]:8080", "[::1]:8080"),
            ("ghe", "ghe"),
        ] {
            assert_eq!(ForgeHost::new(raw).unwrap().as_str(), canonical, "{raw}");
        }
        for raw in [
            "",
            "https://github.com",
            "github.com/",
            "git hub.com",
            "github.com:abc",
            "github.com:0",
            "github.com:99999",
            "user@github.com",
            "github.com?x=1",
            "-bad.com",
            "bad-.com",
            "double..dot.com",
            ":8080",
            "[::1",
            "::1]",
        ] {
            assert!(ForgeHost::new(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn remote_urls_parse_against_configured_kinds() {
        let config = config("[forge.\"git.corp.com\"]\nkind = \"gitlab\"\nmode = \"token\"\n");
        for (remote, binding) in [
            ("https://github.com/acme/app.git", "github:acme/app"),
            ("https://github.com/acme/app", "github:acme/app"),
            ("git@github.com:acme/app.git", "github:acme/app"),
            ("ssh://git@github.com/acme/app", "github:acme/app"),
            ("ssh://git@github.com:22/acme/app.git", "github:acme/app"),
            (
                "https://gitlab.com/group/sub/repo",
                "gitlab:gitlab.com/group/sub/repo",
            ),
            (
                "git@gitlab.com:group/sub/repo.git",
                "gitlab:gitlab.com/group/sub/repo",
            ),
            (
                "https://git.corp.com/group/sub/repo.git",
                "gitlab:git.corp.com/group/sub/repo",
            ),
            (
                "ssh://git@git.corp.com:2222/group/repo.git",
                "gitlab:git.corp.com/group/repo",
            ),
        ] {
            let repo = ForgeRepo::from_remote_url(remote, &config)
                .unwrap_or_else(|e| panic!("{remote}: {e}"));
            assert_eq!(repo.canonical_binding(), binding, "{remote}");
        }
    }

    #[test]
    fn remote_urls_on_unconfigured_hosts_fail_typed() {
        let config = config("");
        let error =
            ForgeRepo::from_remote_url("https://git.corp.com/g/r.git", &config).unwrap_err();
        assert!(matches!(error, ForgeRepoError::UnknownKind(_)), "{error}");
    }

    #[test]
    fn remote_url_rejects_wrong_path_depth() {
        let config = config("");
        for remote in [
            "https://github.com/only",
            "https://github.com",
            "https://github.com/a/b/c",
            "ftp://github.com/a/b",
            "not a url",
            "",
        ] {
            assert!(
                ForgeRepo::from_remote_url(remote, &config).is_err(),
                "{remote}"
            );
        }
    }

    #[test]
    fn helpers_render_per_kind() {
        let gh = repo("github:acme/app");
        assert_eq!(gh.full_path(), "acme/app");
        assert_eq!(gh.host_and_path(), "github.com/acme/app");
        assert_eq!(gh.gitlab_encoded_path(), "acme%2Fapp");
        assert_eq!(gh.https_clone_url(), "https://github.com/acme/app.git");
        assert_eq!(gh.issue_web_url(7), "https://github.com/acme/app/issues/7");
        assert_eq!(gh.merge_web_url(7), "https://github.com/acme/app/pull/7");
        assert_eq!(gh.pull_sigil(), '#');

        let gl = repo("gitlab:git.corp.com/group/sub/repo");
        assert_eq!(gl.full_path(), "group/sub/repo");
        assert_eq!(gl.host_and_path(), "git.corp.com/group/sub/repo");
        assert_eq!(gl.gitlab_encoded_path(), "group%2Fsub%2Frepo");
        assert_eq!(
            gl.https_clone_url(),
            "https://git.corp.com/group/sub/repo.git"
        );
        assert_eq!(
            gl.issue_web_url(7),
            "https://git.corp.com/group/sub/repo/-/issues/7"
        );
        assert_eq!(
            gl.merge_web_url(7),
            "https://git.corp.com/group/sub/repo/-/merge_requests/7"
        );
        assert_eq!(gl.pull_sigil(), '!');
    }

    #[test]
    fn namespace_and_name_checks() {
        let host = ForgeHost::new("github.com").unwrap();
        for (namespace, name) in [
            ("", "repo"),
            ("owner", ""),
            ("a//b", "repo"),
            (".", "repo"),
            ("a/b", "repo"),
        ] {
            assert!(
                ForgeRepo::new(ForgeKind::GitHub, host.clone(), namespace, name).is_err(),
                "{namespace}/{name}"
            );
        }
        // The same deep namespace is legal on GitLab.
        assert!(ForgeRepo::new(ForgeKind::GitLab, host, "a/b", "repo").is_ok());
    }
}
