//! A tool distribution for tests that prepares offline.
//!
//! The embedded distribution downloads and unpacks the real pinned tools
//! (about 33 MB of artifacts and 164 MB of runtimes) into the tools directory
//! of whichever home a test builds, and then runs each of them in a sandbox
//! to probe its version. Tests that only need a prepared registry pin one
//! script per managed tool instead, served from `file://` URLs under the
//! test's own directory, through the same download, digest, staging and
//! version-probe path.

use std::path::Path;

use aither_sandbox::{ManagedTool, ToolDistribution, ToolTarget};
use askama::Template;
use sha2::{Digest, Sha256};

/// Version every fixture tool is pinned to and reports.
const VERSION: &str = "1.0.0";
/// Version of the interpreter the fixture `uv` installs.
const PYTHON_VERSION: &str = "3.13.1";

/// The interpreter the fixture `uv` installs as `python3`.
#[derive(Template)]
#[template(path = "fixture_interpreter.sh", escape = "none")]
struct Interpreter {
    version: &'static str,
}

/// A `uv` that reports its version and installs the interpreter the way
/// `uv python install <request>` lays out one.
#[derive(Template)]
#[template(path = "fixture_uv.sh", escape = "none")]
struct Uv {
    version: &'static str,
    interpreter: String,
}

/// The fixture `bash`, which runs the host's.
#[derive(Template)]
#[template(path = "fixture_bash.sh", escape = "none")]
struct Bash {
    version: &'static str,
}

/// A tool reporting `<command> <version>`; a multicall tool lists its aliases
/// to `--list`.
#[derive(Template)]
#[template(path = "fixture_tool.sh", escape = "none")]
struct Tool {
    command: &'static str,
    version: &'static str,
    aliases: &'static [&'static str],
}

fn interpreter() -> String {
    Interpreter {
        version: PYTHON_VERSION,
    }
    .render()
    .expect("the interpreter template renders")
}

/// The interpreter file as the fixture `uv` writes it: the template's text
/// followed by the newline its heredoc adds.
fn installed_interpreter() -> String {
    format!("{}\n", interpreter().trim())
}

fn script_tool(tool: ManagedTool) -> String {
    match tool {
        ManagedTool::Bash => Bash { version: VERSION }.render(),
        ManagedTool::Uv => Uv {
            version: VERSION,
            interpreter: interpreter(),
        }
        .render(),
        _ => Tool {
            command: tool.command(),
            version: VERSION,
            aliases: tool.aliases(),
        }
        .render(),
    }
    .expect("the fixture tool templates render")
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// A distribution of one script per tool the native platform requires, written
/// under `dir`; keep `dir` alive while it is used.
///
/// # Panics
/// Panics when `dir` is not writable or the platform is unsupported.
pub fn fixture_distribution(dir: &Path) -> ToolDistribution {
    std::fs::create_dir_all(dir).expect("fixture directory");
    let target = ToolTarget::native().expect("supported platform");
    let target_json = serde_json::json!({"os": target.os(), "arch": target.arch()});
    let artifacts: Vec<_> = target
        .required_tools()
        .iter()
        .map(|&tool| {
            let bytes = script_tool(tool);
            let path = dir.join(tool.command());
            std::fs::write(&path, &bytes).expect("fixture tool");
            serde_json::json!({
                "tool": tool,
                "version": VERSION,
                "target": target_json,
                "url": url::Url::from_file_path(&path).expect("absolute fixture path"),
                "sha256": sha256_hex(bytes.as_bytes()),
                "encoding": {"format": "executable"},
            })
        })
        .collect();
    let python: Vec<_> = target
        .provides_python()
        .then(|| {
            serde_json::json!({
                "version": PYTHON_VERSION,
                "target": target_json,
                "interpreter_sha256": sha256_hex(installed_interpreter().as_bytes()),
            })
        })
        .into_iter()
        .collect();
    let manifest = serde_json::json!({
        "artifacts": artifacts,
        "host_programs": [],
        "python": python,
    });
    ToolDistribution::from_json(manifest.to_string().as_bytes())
        .expect("the fixture distribution validates")
}
