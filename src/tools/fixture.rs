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
use sha2::{Digest, Sha256};

/// Version every fixture tool is pinned to and reports.
const VERSION: &str = "1.0.0";
/// Version of the interpreter the fixture `uv` installs.
const PYTHON_VERSION: &str = "3.13.1";

/// The script the fixture `uv` installs as `python3`.
fn interpreter() -> String {
    format!("#!/bin/sh\nprintf '%s\\n' 'Python {PYTHON_VERSION}'\n")
}

/// A script that reports `<command> 1.0.0` to `--version` and, as a multicall
/// tool, lists its aliases to `--list`.
fn script_tool(tool: ManagedTool) -> String {
    match tool {
        ManagedTool::Bash => format!(
            "#!/bin/sh\ncase \"$1\" in --version) printf '%s\\n' 'GNU bash, version {VERSION}(1)-release (fixture)'; exit 0;; esac\nexec /bin/bash \"$@\"\n"
        ),
        ManagedTool::Uv => fixture_uv(),
        _ => {
            let multicall = if tool.aliases().is_empty() {
                String::new()
            } else {
                format!(
                    "case \"${{0##*/}}\" in {}) ;; *) PATH=/usr/bin:/bin exec \"${{0##*/}}\" \"$@\";; esac\ncase \"$1\" in --list) printf '%s\\n' {}; exit 0;; esac\n",
                    tool.command(),
                    tool.aliases().join(" ")
                )
            };
            format!(
                "#!/bin/sh\n{multicall}printf '%s\\n' '{} {VERSION}'\n",
                tool.command()
            )
        }
    }
}

/// A `uv` that reports its version and installs [`interpreter`] the way
/// `uv python install <request>` lays out an interpreter.
fn fixture_uv() -> String {
    format!(
        r#"#!/bin/sh
PATH=/usr/bin:/bin
case "$1" in
--version) printf '%s\n' 'uv {VERSION}'; exit 0;;
python)
    for request; do :; done
    dir="$UV_PYTHON_INSTALL_DIR/$request"
    mkdir -p "$dir/bin" "$UV_PYTHON_INSTALL_DIR/.temp"
    cat > "$dir/bin/python3.13" <<'INTERPRETER'
{}INTERPRETER
    chmod 755 "$dir/bin/python3.13"
    ln -s python3.13 "$dir/bin/python3"
    ln -s "$dir" "$UV_PYTHON_INSTALL_DIR/cpython-3.13-minor"
    : > "$UV_PYTHON_INSTALL_DIR/.lock"
    exit 0;;
esac
printf '%s\n' 'uv {VERSION}'
"#,
        interpreter()
    )
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
                "interpreter_sha256": sha256_hex(interpreter().as_bytes()),
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
