//! Debug adapter discovery, runtime auto-detection, and fail-fast unsupported-runtime checks.

use std::path::{Path, PathBuf};

/// Supported DAP debug adapter backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterKind {
    /// LLVM's native DAP adapter (`lldb-dap`, resolved via `PATH` or `xcrun -f lldb-dap` on macOS).
    LldbDap,
    /// GNU Debugger in native DAP mode (`gdb --interpreter=dap`).
    GdbDap,
    /// Go Delve debugger in DAP mode (`dlv dap`).
    DelveDap,
    /// Python `debugpy` adapter running inside the session sandbox over loopback TCP.
    Debugpy,
    /// Direct TCP connection to an already-listening DAP server.
    TcpDirect,
}

impl AdapterKind {
    /// Canonical adapter identifier passed to DAP `initialize`.
    #[must_use]
    pub const fn adapter_id(self) -> &'static str {
        match self {
            Self::LldbDap => "lldb-dap",
            Self::GdbDap => "gdb",
            Self::DelveDap => "dlv",
            Self::Debugpy => "debugpy",
            Self::TcpDirect => "dap-tcp",
        }
    }

    /// Parses an explicit `--adapter` name supplied by the caller.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::UnsupportedRuntime`] for known non-DAP runtimes
    /// (Node/CDP, JVM/JDWP) or [`AdapterError::UnknownAdapter`] for unrecognized names.
    pub fn parse_explicit(raw: &str) -> Result<Self, AdapterError> {
        let normalized = raw.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "lldb" | "lldb-dap" | "lldb-vscode" => Ok(Self::LldbDap),
            "gdb" | "gdb-dap" => Ok(Self::GdbDap),
            "dlv" | "delve" | "go" => Ok(Self::DelveDap),
            "debugpy" | "python" | "python3" | "py" => Ok(Self::Debugpy),
            "tcp" | "dap" => Ok(Self::TcpDirect),
            "node" | "nodejs" | "bun" | "deno" | "cdp" | "js" | "ts" | "javascript"
            | "typescript" => Err(AdapterError::UnsupportedRuntime {
                runtime: "Node/JavaScript (CDP)",
                guidance: "Node, Bun, and Deno speak Chrome DevTools Protocol (CDP) rather than DAP. Start a DAP bridge on loopback and use `debug connect 127.0.0.1:<port>`, or inspect via `node --inspect-brk` / `browser`.",
            }),
            "java" | "jvm" | "jdwp" | "kotlin" | "scala" => Err(AdapterError::UnsupportedRuntime {
                runtime: "JVM (JDWP)",
                guidance: "JVM runtimes speak JDWP rather than DAP. Start a DAP bridge (such as java-debug) on loopback and use `debug connect 127.0.0.1:<port>`, or use `jdb`.",
            }),
            _ => Err(AdapterError::UnknownAdapter(raw.to_owned())),
        }
    }
}

/// Host binary and arguments for spawning a native DAP adapter outside `heel`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedNativeAdapter {
    /// Selected adapter kind.
    pub kind: AdapterKind,
    /// Host path to the adapter binary.
    pub program: PathBuf,
    /// Command-line arguments launching the binary in stdio DAP mode.
    pub args: Vec<String>,
}

/// Failure selecting or resolving a debug adapter.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdapterError {
    /// Caller requested an unknown adapter name.
    #[error(
        "unknown debug adapter `{0}`; supported adapters are `lldb-dap`, `gdb`, `dlv`, and `debugpy`"
    )]
    UnknownAdapter(String),
    /// Target runtime does not speak DAP natively.
    #[error("unsupported debug runtime {runtime}: {guidance}")]
    UnsupportedRuntime {
        /// Runtime protocol label.
        runtime: &'static str,
        /// Actionable guidance for the agent.
        guidance: &'static str,
    },
    /// Required host adapter binary was not found.
    #[error("{0}")]
    MissingBinary(String),
    /// Attaching directly by PID to a Python process requires a `debugpy` listener.
    #[error(
        "Python processes do not support direct native PID attach without debugpy; launch with `debug launch <script.py>` or start `python3 -m debugpy --listen 127.0.0.1:<port> --wait-for-client <script.py>` and run `debug connect 127.0.0.1:<port>`"
    )]
    PythonAttachRequiresListener,
}

/// Selects the adapter kind for `debug launch <program>`.
///
/// # Errors
///
/// Returns [`AdapterError`] if the explicit adapter is invalid or the target is
/// an unsupported runtime (Node/JS CDP, JVM JDWP).
pub fn select_adapter_for_launch(
    program: &str,
    explicit: Option<&str>,
) -> Result<AdapterKind, AdapterError> {
    if let Some(name) = explicit {
        return AdapterKind::parse_explicit(name);
    }

    let path = Path::new(program);
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(program)
        .to_ascii_lowercase();
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);

    if matches!(ext.as_deref(), Some("py" | "pyw"))
        || matches!(file_name.as_str(), "python" | "python3")
    {
        return Ok(AdapterKind::Debugpy);
    }

    if matches!(ext.as_deref(), Some("js" | "mjs" | "cjs" | "ts" | "tsx"))
        || matches!(file_name.as_str(), "node" | "nodejs" | "bun" | "deno")
    {
        return Err(AdapterError::UnsupportedRuntime {
            runtime: "Node/JavaScript (CDP)",
            guidance: "Node, Bun, and Deno speak Chrome DevTools Protocol (CDP) rather than DAP. Start a DAP bridge on loopback and use `debug connect 127.0.0.1:<port>`, or inspect via `node --inspect-brk` / `browser`.",
        });
    }

    if matches!(ext.as_deref(), Some("jar" | "class"))
        || matches!(file_name.as_str(), "java" | "javaw")
    {
        return Err(AdapterError::UnsupportedRuntime {
            runtime: "JVM (JDWP)",
            guidance: "JVM runtimes speak JDWP rather than DAP. Start a DAP bridge (such as java-debug) on loopback and use `debug connect 127.0.0.1:<port>`, or use `jdb`.",
        });
    }

    if matches!(ext.as_deref(), Some("go")) {
        return Ok(AdapterKind::DelveDap);
    }

    Ok(default_native_adapter_kind())
}

/// Selects the adapter kind for `debug attach --pid <pid>` given the target's process name.
///
/// # Errors
///
/// Returns [`AdapterError`] if the explicit adapter is invalid or the target
/// process belongs to an unsupported runtime.
pub fn select_adapter_for_attach(
    process_comm: Option<&str>,
    explicit: Option<&str>,
) -> Result<AdapterKind, AdapterError> {
    if let Some(name) = explicit {
        return AdapterKind::parse_explicit(name);
    }

    if let Some(comm) = process_comm {
        let base = Path::new(comm)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(comm)
            .to_ascii_lowercase();
        if matches!(base.as_str(), "node" | "nodejs" | "bun" | "deno") {
            return Err(AdapterError::UnsupportedRuntime {
                runtime: "Node/JavaScript (CDP)",
                guidance: "Node, Bun, and Deno speak Chrome DevTools Protocol (CDP) rather than DAP. Start a DAP bridge on loopback and use `debug connect 127.0.0.1:<port>`, or inspect via `node --inspect-brk` / `browser`.",
            });
        }
        if matches!(base.as_str(), "java" | "javaw") {
            return Err(AdapterError::UnsupportedRuntime {
                runtime: "JVM (JDWP)",
                guidance: "JVM runtimes speak JDWP rather than DAP. Start a DAP bridge (such as java-debug) on loopback and use `debug connect 127.0.0.1:<port>`, or use `jdb`.",
            });
        }
        if base == "python" || base.starts_with("python3") {
            return Err(AdapterError::PythonAttachRequiresListener);
        }
    }

    Ok(default_native_adapter_kind())
}

/// Resolves the host-side native debug adapter executable outside `heel`.
///
/// On macOS, `lldb-dap` is resolved via `PATH` first and then `xcrun -f lldb-dap`.
///
/// # Errors
///
/// Returns [`AdapterError::MissingBinary`] when the requested adapter (and its
/// native fallback, when auto-selected) is not installed on the host.
pub async fn resolve_native_adapter(
    kind: AdapterKind,
    allow_native_fallback: bool,
) -> Result<ResolvedNativeAdapter, AdapterError> {
    match kind {
        AdapterKind::LldbDap => {
            if let Some(path) = find_lldb_dap().await {
                return Ok(ResolvedNativeAdapter {
                    kind: AdapterKind::LldbDap,
                    program: path,
                    args: Vec::new(),
                });
            }
            if allow_native_fallback && let Ok(gdb) = which::which("gdb") {
                return Ok(ResolvedNativeAdapter {
                    kind: AdapterKind::GdbDap,
                    program: gdb,
                    args: vec!["--interpreter=dap".to_owned()],
                });
            }
            Err(AdapterError::MissingBinary(missing_lldb_message()))
        }
        AdapterKind::GdbDap => {
            if let Ok(gdb) = which::which("gdb") {
                return Ok(ResolvedNativeAdapter {
                    kind: AdapterKind::GdbDap,
                    program: gdb,
                    args: vec!["--interpreter=dap".to_owned()],
                });
            }
            if allow_native_fallback && let Some(lldb) = find_lldb_dap().await {
                return Ok(ResolvedNativeAdapter {
                    kind: AdapterKind::LldbDap,
                    program: lldb,
                    args: Vec::new(),
                });
            }
            Err(AdapterError::MissingBinary(
                "gdb was not found on PATH; install gdb (>= 14 with DAP support) or pass --adapter lldb-dap".to_owned(),
            ))
        }
        AdapterKind::DelveDap => {
            if let Ok(dlv) = which::which("dlv") {
                return Ok(ResolvedNativeAdapter {
                    kind: AdapterKind::DelveDap,
                    program: dlv,
                    args: vec!["dap".to_owned()],
                });
            }
            Err(AdapterError::MissingBinary(
                "dlv (Delve) was not found on PATH; install with `go install github.com/go-delve/delve/cmd/dlv@latest`".to_owned(),
            ))
        }
        AdapterKind::Debugpy | AdapterKind::TcpDirect => Err(AdapterError::MissingBinary(
            "adapter kind does not use a native host stdio binary".to_owned(),
        )),
    }
}

async fn find_lldb_dap() -> Option<PathBuf> {
    if let Ok(path) = which::which("lldb-dap") {
        return Some(path);
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = async_process::Command::new("xcrun")
            .args(["-f", "lldb-dap"])
            .output()
            .await
            && output.status.success()
        {
            let resolved = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if !resolved.is_empty() {
                let candidate = PathBuf::from(resolved);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

const fn default_native_adapter_kind() -> AdapterKind {
    #[cfg(target_os = "macos")]
    {
        AdapterKind::LldbDap
    }
    #[cfg(not(target_os = "macos"))]
    {
        AdapterKind::LldbDap
    }
}

fn missing_lldb_message() -> String {
    #[cfg(target_os = "macos")]
    {
        "lldb-dap was not found on PATH or via `xcrun -f lldb-dap`; install Xcode or Xcode Command Line Tools (`xcode-select --install`)".to_owned()
    }
    #[cfg(not(target_os = "macos"))]
    {
        "neither lldb-dap nor gdb was found on PATH; install lldb (providing `lldb-dap`) or gdb (>= 14)".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsupported_node_and_jvm_runtimes_with_guidance() {
        let node_err = select_adapter_for_launch("app.js", None).unwrap_err();
        assert!(matches!(
            node_err,
            AdapterError::UnsupportedRuntime {
                runtime: "Node/JavaScript (CDP)",
                ..
            }
        ));

        let jvm_err = select_adapter_for_launch("service.jar", None).unwrap_err();
        assert!(matches!(
            jvm_err,
            AdapterError::UnsupportedRuntime {
                runtime: "JVM (JDWP)",
                ..
            }
        ));

        let attach_node = select_adapter_for_attach(Some("node"), None).unwrap_err();
        assert!(matches!(
            attach_node,
            AdapterError::UnsupportedRuntime { .. }
        ));

        let attach_py = select_adapter_for_attach(Some("python3.13"), None).unwrap_err();
        assert!(matches!(
            attach_py,
            AdapterError::PythonAttachRequiresListener
        ));
    }

    #[test]
    fn selects_expected_adapters_for_supported_targets() {
        assert_eq!(
            select_adapter_for_launch("script.py", None).unwrap(),
            AdapterKind::Debugpy
        );
        assert_eq!(
            select_adapter_for_launch("./target/debug/app", None).unwrap(),
            AdapterKind::LldbDap
        );
        assert_eq!(
            select_adapter_for_launch("./server", Some("dlv")).unwrap(),
            AdapterKind::DelveDap
        );
        assert_eq!(
            select_adapter_for_launch("./app", Some("gdb")).unwrap(),
            AdapterKind::GdbDap
        );
    }
}
