//! A codespace reached through the GitHub CLI.
//!
//! Commands go over `gh codespace ssh` and the daemon's API over `gh
//! codespace ports forward`, both through GitHub's relay, so the codespace
//! opens no inbound port. Each `gh` runs as the codespace's account: its
//! token is handed over in `GH_TOKEN`, whichever account `gh` has active.

use std::net::{Ipv4Addr, SocketAddr};
use std::process::Stdio;
use std::time::Duration;

use futures_lite::io::AsyncWriteExt as _;

use super::bootstrap::RemoteShell;
use super::{CloudError, MachineId};

/// The `gh` executable.
const GH: &str = "gh";

/// Lines of a failed command's error output kept in its error.
const ERROR_LINES: usize = 20;

/// `gh codespace ssh` to one codespace.
#[derive(Debug, Clone)]
pub struct CodespaceShell {
    codespace: MachineId,
    token: String,
}

impl CodespaceShell {
    /// Commands on `codespace`, as the account `token` belongs to.
    #[must_use]
    pub const fn new(codespace: MachineId, token: String) -> Self {
        Self { codespace, token }
    }

    /// The command running `script` by `sh` on the codespace, whatever its
    /// login shell: `gh codespace ssh` passes what follows `--` to `ssh`,
    /// whose first non-option word is the remote command.
    fn command(&self, script: &str) -> std::process::Command {
        let mut command = std::process::Command::new(GH);
        command
            .args([
                "codespace",
                "ssh",
                "--codespace",
                self.codespace.as_str(),
                "--",
            ])
            .arg(shell_words::join(["sh", "-c", script]))
            .env("GH_TOKEN", &self.token)
            .env("GH_PROMPT_DISABLED", "1");
        command
    }
}

impl RemoteShell for CodespaceShell {
    async fn run(
        &self,
        step: &'static str,
        script: &str,
        input: Option<Vec<u8>>,
        deadline: Duration,
    ) -> Result<String, CloudError> {
        let mut command = async_process::Command::from(self.command(script));
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let failed = |message: String| CloudError::Remote { step, message };
        let mut child = command
            .spawn()
            .map_err(|error| failed(format!("cannot run gh: {error}")))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| failed("gh has no standard input".to_owned()))?;
        let exchange = async {
            let feed = async {
                if let Some(bytes) = input {
                    stdin.write_all(&bytes).await?;
                }
                stdin.close().await
            };
            let (sent, output) = futures_lite::future::zip(feed, child.output()).await;
            let output = output.map_err(|error| failed(error.to_string()))?;
            if !output.status.success() {
                return Err(failed(error_tail(&output.stderr, output.status)));
            }
            sent.map_err(|error| failed(format!("the input was cut off: {error}")))?;
            String::from_utf8(output.stdout)
                .map_err(|error| failed(format!("the output is not text: {error}")))
        };
        futures_lite::future::or(exchange, async {
            async_io::Timer::after(deadline).await;
            Err(failed(format!(
                "it did not finish within {}s",
                deadline.as_secs()
            )))
        })
        .await
    }
}

/// The last lines of a failed command's error output.
fn error_tail(stderr: &[u8], status: std::process::ExitStatus) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let tail = lines
        .get(lines.len().saturating_sub(ERROR_LINES)..)
        .unwrap_or_default()
        .join("\n");
    if tail.is_empty() {
        format!("it exited with {status}")
    } else {
        tail
    }
}

/// `gh codespace ports forward` of one codespace port to a loopback port
/// of this machine. The forward ends when this is dropped.
#[derive(Debug)]
pub struct PortForward {
    child: async_process::Child,
    local: SocketAddr,
}

impl PortForward {
    /// Forwards `remote_port` of `codespace` to a free loopback port, as
    /// the account `token` belongs to. The forward answers once `gh` has
    /// connected the relay; [`PortForward::exited`] reports a forward that
    /// ended.
    ///
    /// # Errors
    /// [`CloudError::Remote`] when no loopback port is free or `gh` cannot
    /// be run.
    pub fn open(codespace: &MachineId, token: &str, remote_port: u16) -> Result<Self, CloudError> {
        let failed = |message: String| CloudError::Remote {
            step: "forward the May daemon's port",
            message,
        };
        // `gh` takes the local port by number: the port a loopback bind is
        // given is free, and stays free for the moment until `gh` binds it.
        let local = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .and_then(|listener| listener.local_addr())
            .map_err(|error| failed(format!("no loopback port is free: {error}")))?;
        let child = async_process::Command::new(GH)
            .args(["codespace", "ports", "forward"])
            .arg(format!("{remote_port}:{}", local.port()))
            .args(["--codespace", codespace.as_str()])
            .env("GH_TOKEN", token)
            .env("GH_PROMPT_DISABLED", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| failed(format!("cannot run gh: {error}")))?;
        Ok(Self { child, local })
    }

    /// The loopback address the codespace port answers on.
    #[must_use]
    pub const fn local(&self) -> SocketAddr {
        self.local
    }

    /// How the forward ended, once it has.
    pub fn exited(&mut self) -> Option<String> {
        self.child
            .try_status()
            .ok()
            .flatten()
            .map(|status| status.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_runs_by_sh_on_the_named_codespace_as_its_account() {
        let shell = CodespaceShell::new(
            MachineId::new("monalisa-octocat-hello-world-g4wpq6h95q".into()),
            "account-token".into(),
        );
        let command = shell.command("printf '%s' 日本語 한국어 中文");

        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(command.get_program(), GH);
        assert_eq!(
            args,
            [
                "codespace",
                "ssh",
                "--codespace",
                "monalisa-octocat-hello-world-g4wpq6h95q",
                "--",
                "sh -c 'printf '\\''%s'\\'' 日本語 한국어 中文'",
            ]
        );
        let token = command
            .get_envs()
            .find(|(name, _)| *name == "GH_TOKEN")
            .and_then(|(_, value)| value);
        assert_eq!(token, Some(std::ffi::OsStr::new("account-token")));
    }

    #[test]
    fn a_failure_keeps_the_last_lines_of_its_error_output() {
        use std::os::unix::process::ExitStatusExt as _;

        let stderr = (0..30).fold(String::new(), |mut text, line| {
            use std::fmt::Write as _;
            writeln!(text, "line {line}").expect("writing to a String cannot fail");
            text
        });
        let tail = error_tail(stderr.as_bytes(), std::process::ExitStatus::from_raw(256));
        assert!(tail.starts_with("line 10\n"), "{tail}");
        assert!(tail.ends_with("line 29"), "{tail}");
        assert_eq!(
            error_tail(b"", std::process::ExitStatus::from_raw(256)),
            "it exited with exit status: 1"
        );
    }
}
