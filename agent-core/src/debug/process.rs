//! Process tree resolution and loopback socket helpers for host-brokered debugging.

use std::net::TcpListener;
use std::path::Path;

/// Shell wrapper process names that may sit between `JobRegistry`'s root PID and
/// the actual debuggee child process.
const WRAPPER_PROCESS_NAMES: &[&str] = &["sh", "bash", "zsh", "dash", "fish", "env"];

/// Maximum process-tree descent depth when unwrapping shell wrappers.
const MAX_WRAPPER_DEPTH: usize = 8;

/// Allocates a currently free ephemeral loopback TCP port on `127.0.0.1`.
///
/// # Errors
///
/// Returns an I/O error if binding `127.0.0.1:0` fails.
pub fn allocate_loopback_port() -> std::io::Result<u16> {
    TcpListener::bind("127.0.0.1:0")?
        .local_addr()
        .map(|addr| addr.port())
}

/// Validates that `endpoint` is a `host:port` string and returns whether it targets loopback.
///
/// # Errors
///
/// Returns a descriptive error string when `endpoint` is empty or lacks a valid port.
pub fn parse_endpoint(endpoint: &str) -> Result<(String, u16, bool), String> {
    let trimmed = endpoint.trim();
    if trimmed.is_empty() {
        return Err("connect endpoint must not be empty".to_owned());
    }
    let (host, port_str) = if let Some(rest) = trimmed.strip_prefix('[') {
        let Some((h, after)) = rest.split_once("]:") else {
            return Err(format!(
                "invalid IPv6 endpoint `{trimmed}`; expected `[::1]:port`"
            ));
        };
        (h, after)
    } else {
        let Some((h, p)) = trimmed.rsplit_once(':') else {
            return Err(format!(
                "invalid endpoint `{trimmed}`; expected `host:port`"
            ));
        };
        (h, p)
    };

    let port = port_str
        .parse::<u16>()
        .map_err(|_| format!("invalid port `{port_str}` in endpoint `{trimmed}`"))?;
    let is_loopback = host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
        || host == "::1"
        || host.starts_with("127.");
    Ok((host.to_owned(), port, is_loopback))
}

/// Resolves `pid` by descending through shell wrapper processes (`sh`, `bash`,
/// `zsh`, `dash`, `fish`, `env`) to the underlying debuggee PID when present.
pub async fn resolve_debuggee_pid(pid: u32) -> u32 {
    let mut current = pid;
    for _ in 0..MAX_WRAPPER_DEPTH {
        let Some(comm) = inspect_process_comm(current).await else {
            break;
        };
        if !is_wrapper_comm(&comm) {
            break;
        }
        let children = direct_child_pids(current).await;
        if let Some(&child) = children.first() {
            current = child;
        } else {
            break;
        }
    }
    current
}

/// Reads the short executable name (`comm`) of `pid`, when accessible on this host.
#[cfg_attr(not(unix), allow(clippy::unused_async))]
pub async fn inspect_process_comm(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(comm) = async_fs::read_to_string(format!("/proc/{pid}/comm")).await {
            let trimmed = comm.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
    }

    #[cfg(unix)]
    {
        let output = async_process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
            .await
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        (!text.is_empty()).then_some(text)
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
        None
    }
}

#[cfg_attr(not(unix), allow(clippy::unused_async))]
async fn direct_child_pids(pid: u32) -> Vec<u32> {
    #[cfg(target_os = "linux")]
    {
        let mut children = Vec::new();
        if let Ok(mut entries) = async_fs::read_dir(format!("/proc/{pid}/task")).await {
            use futures_lite::StreamExt;
            while let Some(Ok(entry)) = entries.next().await {
                let path = entry.path().join("children");
                if let Ok(content) = async_fs::read_to_string(path).await {
                    for token in content.split_whitespace() {
                        if let Ok(child) = token.parse::<u32>() {
                            children.push(child);
                        }
                    }
                }
            }
        }
        if !children.is_empty() {
            return children;
        }
    }

    #[cfg(unix)]
    {
        if let Ok(output) = async_process::Command::new("pgrep")
            .args(["-P", &pid.to_string()])
            .output()
            .await
            && output.status.success()
        {
            return String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .filter_map(|token| token.parse::<u32>().ok())
                .collect();
        }
    }

    let _ = pid;
    Vec::new()
}

fn is_wrapper_comm(comm: &str) -> bool {
    let base = Path::new(comm)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(comm)
        .trim_start_matches('-');
    WRAPPER_PROCESS_NAMES
        .iter()
        .any(|wrapper| base.eq_ignore_ascii_case(wrapper))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_loopback_and_non_loopback_endpoints() {
        let (host, port, loopback) = parse_endpoint("127.0.0.1:5678").unwrap();
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 5678);
        assert!(loopback);

        let (host, port, loopback) = parse_endpoint("[::1]:9000").unwrap();
        assert_eq!(host, "::1");
        assert_eq!(port, 9000);
        assert!(loopback);

        let (_, _, loopback) = parse_endpoint("10.0.0.5:4000").unwrap();
        assert!(!loopback);

        assert!(parse_endpoint("no-port").is_err());
    }

    #[test]
    fn recognizes_shell_wrapper_names() {
        assert!(is_wrapper_comm("/bin/bash"));
        assert!(is_wrapper_comm("-zsh"));
        assert!(is_wrapper_comm("sh"));
        assert!(!is_wrapper_comm("my-debug-target"));
    }
}
