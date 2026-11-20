//! The one way git runs on the host.
//!
//! Host git works on the user's checkouts, on scratch repositories and on
//! nothing the agent wrote. Where its configuration comes from is the one
//! thing a caller varies: the host's own (`inherit`), or a file standing in
//! for the user's global configuration (`from_file`), which is how tests give
//! each fixture a hermetic user with their own hooks and signing key.
//!
//! A caller may also hand git a one-shot HTTPS credential
//! ([`HostGit::with_https_auth`]). The credential travels in the command's
//! environment (`GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_n`/`GIT_CONFIG_VALUE_n`),
//! scoped to the credential's origin: it never appears on the command line
//! (`ps` shows argv), never lands in a config file, never enters the remote
//! URL, and a redirect to another host never receives it.

use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine as _;

/// A host git invocation that did not succeed.
#[derive(Debug, thiserror::Error)]
pub enum HostGitError {
    /// `git` could not run.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// `git` exited nonzero.
    #[error("git {args} failed: {stderr}")]
    Failed {
        /// The command line.
        args: String,
        /// Captured stderr.
        stderr: String,
    },
}

/// `host` is not a bare `host[:port]` an HTTPS credential can be scoped to.
#[derive(Debug, thiserror::Error)]
#[error("invalid host for an HTTPS credential {0:?}: expected host[:port]")]
pub struct InvalidAuthHost(String);

/// A one-shot HTTPS credential git carries in its environment.
///
/// The Basic-auth material is a `http.<origin>.extraHeader` entry injected
/// through `GIT_CONFIG_*` environment variables, so it is scoped to exactly
/// `https://<host>/` (port included when present) and nothing else.
#[derive(Clone)]
pub struct HttpsAuth {
    /// `https://<host>[:port]/` — the origin the header is scoped to.
    origin: String,
    /// `Authorization: Basic <base64>` value.
    header: String,
}

impl HttpsAuth {
    /// `Authorization: Basic <username>:<token>` for `host` (`host[:port]`).
    ///
    /// GitHub accepts `x-access-token` as `username` for OAuth tokens, app
    /// tokens and PATs; GitLab accepts `oauth2` for OAuth tokens and PATs
    /// alike.
    ///
    /// # Errors
    /// [`InvalidAuthHost`] when `host` is not a bare `host[:port]` — a
    /// scheme, path, userinfo or whitespace is refused. Bracketed IPv6
    /// (`[::1]:8443`) is accepted.
    pub fn new(host: &str, username: &str, token: &str) -> Result<Self, InvalidAuthHost> {
        let invalid = || InvalidAuthHost(host.to_string());
        if host.is_empty()
            || host
                .bytes()
                .any(|b| b.is_ascii_whitespace() || matches!(b, b'/' | b'@' | b'?' | b'#' | b'\\'))
            || host.contains("://")
        {
            return Err(invalid());
        }
        if let Some(rest) = host.strip_prefix('[') {
            let Some((_, tail)) = rest.split_once(']') else {
                return Err(invalid());
            };
            match tail.strip_prefix(':') {
                Some(port) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {}
                None if tail.is_empty() => {}
                _ => return Err(invalid()),
            }
        } else {
            match host.split(':').count() {
                1 => {}
                2 => {
                    let Some((_, port)) = host.rsplit_once(':') else {
                        return Err(invalid());
                    };
                    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                        // A `:` not followed by digits can only be IPv6
                        // without brackets; git's `http.<url>` matching
                        // wants the bracketed form.
                        return Err(invalid());
                    }
                }
                _ => return Err(invalid()),
            }
        }
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(format!("{username}:{token}").as_bytes());
        Ok(Self {
            origin: format!("https://{host}/"),
            header: format!("Authorization: Basic {encoded}"),
        })
    }

    /// The origin the credential is scoped to: `https://<host>/`.
    #[must_use]
    pub fn origin(&self) -> &str {
        &self.origin
    }
}

impl std::fmt::Debug for HttpsAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpsAuth")
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

/// The command line of `command`: program and arguments, never its
/// environment — `Command`'s `Debug` prints `env` overrides, and those can
/// carry credentials.
fn command_line(command: &Command) -> String {
    let mut line = command.get_program().to_string_lossy().into_owned();
    for arg in command.get_args() {
        line.push(' ');
        line.push_str(&arg.to_string_lossy());
    }
    line
}

/// Where the host's git configuration is read from.
#[derive(Debug, Clone, Default)]
pub struct HostGit {
    /// A file standing in for the user's global configuration, with the
    /// system one ignored; `None` reads the host's own.
    global_config: Option<PathBuf>,
    /// A one-shot HTTPS credential injected into every command.
    https_auth: Option<HttpsAuth>,
}

impl HostGit {
    /// The host's own git configuration.
    #[must_use]
    pub const fn inherit() -> Self {
        Self {
            global_config: None,
            https_auth: None,
        }
    }

    /// `config` standing in for the user's global git configuration, with no
    /// system configuration.
    #[must_use]
    pub fn from_file(config: impl Into<PathBuf>) -> Self {
        Self {
            global_config: Some(config.into()),
            https_auth: None,
        }
    }

    /// Every command carries `auth`'s credential in its environment.
    ///
    /// The token never reaches argv, the repository's config or the remote
    /// URL — only git itself reads the injected `http.<origin>.extraHeader`,
    /// and only for requests under that origin.
    #[must_use]
    pub fn with_https_auth(mut self, auth: HttpsAuth) -> Self {
        self.https_auth = Some(auth);
        self
    }

    /// A host `git`, started outside every repository.
    #[must_use]
    pub fn command(&self) -> Command {
        let mut command = Command::new("git");
        command.current_dir("/");
        for var in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
            command.env_remove(var);
        }
        if let Some(config) = &self.global_config {
            command
                .env("GIT_CONFIG_GLOBAL", config)
                .env("GIT_CONFIG_SYSTEM", "/dev/null");
        }
        if let Some(auth) = &self.https_auth {
            // Environment-carried config keeps the credential out of argv
            // and out of every config file; the extraHeader key scopes it
            // to the credential's own origin so a redirect elsewhere never
            // receives it. No terminal prompt may appear instead.
            command
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_CONFIG_COUNT", "1")
                .env(
                    "GIT_CONFIG_KEY_0",
                    format!("http.{}.extraHeader", auth.origin),
                )
                .env("GIT_CONFIG_VALUE_0", &auth.header);
        }
        command
    }

    /// Runs `git` (with `-C dir` when given) and returns its stdout.
    ///
    /// # Errors
    /// Fails when git cannot run or exits nonzero.
    pub fn output(&self, dir: Option<&Path>, args: &[&str]) -> Result<String, HostGitError> {
        let mut command = self.command();
        if let Some(dir) = dir {
            command.arg("-C").arg(dir);
        }
        let output = command.args(args).output()?;
        if !output.status.success() {
            return Err(HostGitError::Failed {
                args: command_line(&command),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// The boolean `key` in the host's configuration the way git reads it
    /// (`true`, `yes`, `on`, `1`, ...), `None` when unset.
    ///
    /// # Errors
    /// Fails when git cannot read its configuration or the value is not a
    /// boolean.
    pub fn config_bool(&self, key: &str) -> Result<Option<bool>, HostGitError> {
        let output = self
            .command()
            .args(["config", "--type=bool", "--get", key])
            .output()?;
        match output.status.code() {
            Some(0) => Ok(Some(
                String::from_utf8_lossy(&output.stdout).trim() == "true",
            )),
            Some(1) => Ok(None),
            _ => Err(HostGitError::Failed {
                args: format!("config --type=bool --get {key}"),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            }),
        }
    }

    /// The value of `key` in the host's configuration, `None` when unset.
    ///
    /// # Errors
    /// Fails when git cannot read its configuration.
    pub fn config_value(&self, key: &str) -> Result<Option<String>, HostGitError> {
        let output = self.command().args(["config", "--get", key]).output()?;
        match output.status.code() {
            Some(0) => Ok(Some(
                String::from_utf8_lossy(&output.stdout).trim().to_string(),
            )),
            Some(1) => Ok(None),
            _ => Err(HostGitError::Failed {
                args: format!("config --get {key}"),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "glpat-test-token-0123456789abcdef";

    #[test]
    fn https_auth_injects_environment_config_and_never_argv() {
        use base64::Engine as _;
        let expected_header = format!(
            "Authorization: Basic {}",
            base64::engine::general_purpose::STANDARD.encode(b"oauth2:glpat-x")
        );
        let auth = HttpsAuth::new("gitlab.corp.com:8443", "oauth2", "glpat-x").unwrap();
        assert_eq!(auth.origin(), "https://gitlab.corp.com:8443/");

        let command = HostGit::inherit().with_https_auth(auth).command();
        let envs: Vec<_> = command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        let get = |key: &str| {
            envs.iter()
                .find(|(k, _)| k == key)
                .and_then(|(_, v)| v.clone())
        };
        assert_eq!(get("GIT_TERMINAL_PROMPT").as_deref(), Some("0"));
        assert_eq!(get("GIT_CONFIG_COUNT").as_deref(), Some("1"));
        assert_eq!(
            get("GIT_CONFIG_KEY_0").as_deref(),
            Some("http.https://gitlab.corp.com:8443/.extraHeader")
        );
        assert_eq!(
            get("GIT_CONFIG_VALUE_0").as_deref(),
            Some(expected_header.as_str())
        );
        // No token material in argv.
        let args: Vec<_> = command.get_args().collect();
        assert!(args.is_empty(), "{args:?}");
    }

    #[test]
    fn https_auth_rejects_malformed_hosts() {
        for host in [
            "",
            "https://github.com",
            "github.com/path",
            "user@github.com",
            "git hub.com",
            "github.com:abc",
            "::1",
        ] {
            assert!(HttpsAuth::new(host, "u", "t").is_err(), "{host}");
        }
        assert!(HttpsAuth::new("github.com", "u", "t").is_ok());
        assert!(HttpsAuth::new("github.com:8443", "u", "t").is_ok());
    }

    #[test]
    fn a_failed_authenticated_command_never_leaks_the_token() {
        use base64::Engine as _;
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!("oauth2:{TOKEN}").as_bytes());
        let auth = HttpsAuth::new("gitlab.com", "oauth2", TOKEN).unwrap();
        let git = HostGit::inherit().with_https_auth(auth);
        // `frobnicate` is not a git command: it fails fast without a
        // repository or the network.
        let error = git.output(None, &["frobnicate"]).unwrap_err();
        let HostGitError::Failed { args, .. } = &error else {
            panic!("expected a failed git: {error:?}");
        };
        assert!(args.starts_with("git "), "{args}");
        for rendered in [format!("{error}"), format!("{error:?}"), args.clone()] {
            assert!(!rendered.contains(TOKEN), "{rendered}");
            assert!(!rendered.contains(&encoded), "{rendered}");
            assert!(!rendered.contains("extraHeader"), "{rendered}");
            assert!(!rendered.contains("GIT_CONFIG"), "{rendered}");
        }
    }

    #[test]
    fn https_auth_debug_shows_only_the_origin() {
        let auth = HttpsAuth::new("gitlab.com", "oauth2", TOKEN).unwrap();
        let rendered = format!("{auth:?}");
        assert!(rendered.contains("https://gitlab.com/"), "{rendered}");
        assert!(!rendered.contains(TOKEN), "{rendered}");
    }
}
