//! Every entry point refuses to start on an invalid configuration file and
//! starts when the file is absent. Each case drives the real `may` binary
//! with an isolated home directory.

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::time::{Instant, sleep, timeout};

/// Longest a refusal or an exiting entry point may take.
const EXIT_TIMEOUT: Duration = Duration::from_secs(60);
/// Longest a long-running entry point may take to get past startup.
const STARTED_TIMEOUT: Duration = Duration::from_secs(60);
const REFUSAL: &str = "refusing to start";

/// How a configuration file is broken.
#[derive(Clone, Copy, Debug)]
enum Fault {
    /// Not syntactically valid for the file's format.
    Parse,
    /// Well-formed, but the loader rejects its content.
    Validation,
}

/// A configuration file May validates at startup.
#[derive(Clone, Copy, Debug)]
struct ConfigFile {
    /// Path relative to `~/.may`, or to the default profile's home when
    /// `profile` is set.
    name: &'static str,
    /// The file belongs to a profile, found through the history database.
    profile: bool,
    valid: &'static str,
    parse: &'static str,
    validation: &'static str,
}

const CONFIG_TOML: ConfigFile = ConfigFile {
    name: "config.toml",
    profile: false,
    valid: "",
    parse: "this is = = not toml\n",
    // `api` is only supported for custom providers.
    validation: "[provider.openai]\napi = \"responses\"\n",
};
const MCP_JSON: ConfigFile = ConfigFile {
    name: "mcp.json",
    profile: false,
    valid: "{}\n",
    parse: "{ \"mcpServers\": ",
    // Well-formed JSON of the wrong shape.
    validation: "{ \"mcpServers\": [] }\n",
};
const WORKSPACE_TOML: ConfigFile = ConfigFile {
    name: "workspace.toml",
    profile: false,
    valid: "",
    parse: "[[workdirs\n",
    validation: "[[workdirs]]\npath = \"/tmp\"\npermission = \"Bogus\"\n",
};
const PROFILE_WORKSPACE_TOML: ConfigFile = ConfigFile {
    profile: true,
    ..WORKSPACE_TOML
};
const FILES: [ConfigFile; 4] = [
    CONFIG_TOML,
    MCP_JSON,
    WORKSPACE_TOML,
    PROFILE_WORKSPACE_TOML,
];

/// An entry point of the `may` binary.
#[derive(Clone, Copy, Debug)]
enum EntryPoint {
    Tui,
    Rest,
    Ui,
    Exec,
    OneShot,
}

const ENTRY_POINTS: [EntryPoint; 5] = [
    EntryPoint::Tui,
    EntryPoint::Rest,
    EntryPoint::Ui,
    EntryPoint::Exec,
    EntryPoint::OneShot,
];

impl EntryPoint {
    fn args(self) -> Vec<String> {
        let strings = |args: &[&str]| args.iter().map(ToString::to_string).collect();
        match self {
            Self::Tui => Vec::new(),
            Self::Rest => strings(&["rest", "--host", "127.0.0.1", "--port", &free_port()]),
            Self::Ui => strings(&["ui", "--host", "127.0.0.1", "--port", &free_port()]),
            Self::Exec => strings(&["exec", "--session", "no-such-session", "--", "true"]),
            Self::OneShot => strings(&["say hello"]),
        }
    }
}

fn free_port() -> String {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("an ephemeral port should bind")
        .local_addr()
        .expect("the listener should have a local address")
        .port()
        .to_string()
}

fn may_command(home: &Path, args: &[String]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_may"));
    command
        .args(args)
        .current_dir(home)
        .env("HOME", home)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    command
}

/// Where `file` lives under `home`. A profile file needs the profile to exist,
/// so its home's history database is seeded with the default profiles first,
/// the way May's own first run does.
async fn path_for(home: &Path, file: ConfigFile) -> PathBuf {
    let may_dir = home.join(".may");
    std::fs::create_dir_all(&may_dir).expect("the May directory should be created");
    if !file.profile {
        return may_dir.join(file.name);
    }
    let db = may_agent::ChatDb::open(&may_dir.join("history.db"))
        .await
        .expect("the history database should open");
    let profile_id = db
        .ensure_default_profiles()
        .await
        .expect("the default profiles should be created");
    may_dir.join("profiles").join(profile_id).join(file.name)
}

async fn write_config(home: &Path, file: ConfigFile, content: &str) -> PathBuf {
    let path = path_for(home, file).await;
    std::fs::create_dir_all(path.parent().expect("a config path has a parent"))
        .expect("the config directory should be created");
    std::fs::write(&path, content).expect("the config file should be written");
    path
}

async fn run_to_exit(entry: EntryPoint, home: &Path) -> Output {
    timeout(
        EXIT_TIMEOUT,
        may_command(home, &entry.args())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .unwrap_or_else(|_| panic!("`may` ({entry:?}) did not exit within {EXIT_TIMEOUT:?}"))
    .expect("`may` should spawn")
}

async fn assert_refuses(entry: EntryPoint, file: ConfigFile, fault: Fault) {
    let home = TempDir::new().expect("a home directory should be created");
    let body = match fault {
        Fault::Parse => file.parse,
        Fault::Validation => file.validation,
    };
    let path = write_config(home.path(), file, body).await;
    let output = run_to_exit(entry, home.path()).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let context = format!("{entry:?} with {fault:?} fault in {}", file.name);
    assert!(
        !output.status.success(),
        "{context}: must exit non-zero; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(REFUSAL),
        "{context}: stderr must say `{REFUSAL}`; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(&path.display().to_string()),
        "{context}: stderr must name {}; stderr:\n{stderr}",
        path.display()
    );
    assert!(
        stderr.contains("arse") || stderr.contains("nvalid") || stderr.contains("expected"),
        "{context}: stderr must name the problem; stderr:\n{stderr}"
    );
}

async fn assert_starts_without_file(entry: EntryPoint, file: ConfigFile) {
    let home = TempDir::new().expect("a home directory should be created");
    // Every other file is present and valid; `file` is absent.
    for other in FILES
        .iter()
        .filter(|other| (other.name, other.profile) != (file.name, file.profile))
    {
        write_config(home.path(), *other, other.valid).await;
    }
    let absent = path_for(home.path(), file).await;
    assert!(!absent.exists(), "{} must be absent", absent.display());
    let context = format!("{entry:?} without {}", file.name);
    match entry {
        EntryPoint::Rest | EntryPoint::Ui => {
            // `ui` needs the `ui` cargo feature, so it ends with its own
            // error; either way the startup step must not refuse.
            if matches!(entry, EntryPoint::Rest) {
                assert_rest_answers(home.path(), &context).await;
            } else {
                let output = run_to_exit(entry, home.path()).await;
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert!(!stderr.contains(REFUSAL), "{context}: refused:\n{stderr}");
            }
        }
        EntryPoint::Tui => assert_tui_starts(home.path(), &context).await,
        EntryPoint::Exec | EntryPoint::OneShot => {
            let output = run_to_exit(entry, home.path()).await;
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(!stderr.contains(REFUSAL), "{context}: refused:\n{stderr}");
            // Past the startup step the entry point fails on its own terms.
            assert!(
                !output.status.success(),
                "{context}: expected its own post-startup error; stderr:\n{stderr}"
            );
        }
    }
}

async fn assert_rest_answers(home: &Path, context: &str) {
    let args = EntryPoint::Rest.args();
    let port = args.last().expect("rest args end with the port").clone();
    let mut child = may_command(home, &args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("`may rest` should spawn");
    let url = format!("http://127.0.0.1:{port}/api/health");
    let client = reqwest::Client::new();
    let deadline = Instant::now() + STARTED_TIMEOUT;
    let outcome = loop {
        if let Ok(Some(status)) = child.try_wait() {
            break Err(format!("exited with {status} before answering {url}"));
        }
        if client
            .get(&url)
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            break Ok(());
        }
        if Instant::now() >= deadline {
            break Err(format!("did not answer {url} within {STARTED_TIMEOUT:?}"));
        }
        sleep(Duration::from_millis(100)).await;
    };
    let stderr = stop(&mut child).await;
    if let Err(problem) = outcome {
        panic!("{context}: `may rest` {problem}; stderr:\n{stderr}");
    }
}

/// Kills `child` and returns what it wrote to stderr.
async fn stop(child: &mut Child) -> String {
    let _ = child.kill().await;
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = timeout(Duration::from_secs(5), pipe.read_to_string(&mut stderr)).await;
    }
    let _ = child.wait().await;
    stderr
}

/// Runs the TUI on a pseudo-terminal of its own and waits for it to enter the
/// alternate screen, which it only does after the startup step.
async fn assert_tui_starts(home: &Path, context: &str) {
    const ENTER_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049h";
    let binary = env!("CARGO_BIN_EXE_may");
    let mut command = Command::new("script");
    #[cfg(target_os = "macos")]
    command.args(["-q", "/dev/null", binary]);
    #[cfg(not(target_os = "macos"))]
    command.args(["-q", "-e", "-c", binary, "/dev/null"]);
    let mut child = command
        .current_dir(home)
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("`script` should spawn the TUI on a pseudo-terminal");
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let deadline = Instant::now() + STARTED_TIMEOUT;
    let mut seen = Vec::new();
    let mut buffer = [0u8; 4096];
    let outcome = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match timeout(remaining, stdout.read(&mut buffer)).await {
            Err(_) => break Err("did not enter the alternate screen within the timeout"),
            Ok(Ok(0) | Err(_)) => break Err("exited before entering the alternate screen"),
            Ok(Ok(read)) => seen.extend_from_slice(&buffer[..read]),
        }
        if seen
            .windows(ENTER_ALTERNATE_SCREEN.len())
            .any(|window| window == ENTER_ALTERNATE_SCREEN)
        {
            break Ok(());
        }
    };
    // Dropping stdin hangs the pseudo-terminal up, which ends the TUI.
    drop(child.stdin.take());
    let stderr = stop(&mut child).await;
    if let Err(problem) = outcome {
        panic!(
            "{context}: the TUI {problem}; output:\n{}\nstderr:\n{stderr}",
            String::from_utf8_lossy(&seen)
        );
    }
}

async fn refusals_for(file: ConfigFile) {
    for entry in ENTRY_POINTS {
        for fault in [Fault::Parse, Fault::Validation] {
            assert_refuses(entry, file, fault).await;
        }
    }
}

async fn absence_for(file: ConfigFile) {
    for entry in ENTRY_POINTS {
        assert_starts_without_file(entry, file).await;
    }
}

#[tokio::test]
async fn config_toml_refuses_on_every_entry_point() {
    refusals_for(CONFIG_TOML).await;
}

#[tokio::test]
async fn mcp_json_refuses_on_every_entry_point() {
    refusals_for(MCP_JSON).await;
}

#[tokio::test]
async fn workspace_toml_refuses_on_every_entry_point() {
    refusals_for(WORKSPACE_TOML).await;
}

#[tokio::test]
async fn profile_workspace_toml_refuses_on_every_entry_point() {
    refusals_for(PROFILE_WORKSPACE_TOML).await;
}

#[tokio::test]
async fn config_toml_may_be_absent_on_every_entry_point() {
    absence_for(CONFIG_TOML).await;
}

#[tokio::test]
async fn mcp_json_may_be_absent_on_every_entry_point() {
    absence_for(MCP_JSON).await;
}

#[tokio::test]
async fn workspace_toml_may_be_absent_on_every_entry_point() {
    absence_for(WORKSPACE_TOML).await;
}

#[tokio::test]
async fn profile_workspace_toml_may_be_absent_on_every_entry_point() {
    absence_for(PROFILE_WORKSPACE_TOML).await;
}
