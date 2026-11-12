//! The Codespaces provider against a recorded API: each test scripts the
//! exchanges GitHub answers, in order, with the response bodies GitHub's
//! REST API documentation publishes (`fixtures/`), and checks both what the
//! provider made of them and exactly what it sent.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::{
    CodespaceSource, Codespaces, CodespacesAccount, ENVIRONMENT_DEVCONTAINER,
    ENVIRONMENT_REPOSITORY,
};
use crate::cloud::{
    CloudError, CloudMachine, CloudProvider, MachineId, MachineState, MachineType, NewMachine,
    RunningWait, until_running,
};
use crate::issue::github::GitHubRest;
use crate::pr::PrTokenSource;
use crate::test_support::{ApiReply, ApiRequests, serve_api};
use crate::workspace::GitHubRepository;

/// `GET /repos/{owner}/{repo}/codespaces/machines`, as documented.
const MACHINES: &str = include_str!("fixtures/codespace_machines.json");
/// A codespace, as every codespace endpoint documents it.
const CODESPACE: &str = include_str!("fixtures/codespace.json");
/// `GET /repos/{owner}/{repo}` and `POST /user/repos`, as documented.
const REPOSITORY: &str = include_str!("fixtures/full_repository.json");
/// `GET /user`, as documented.
const USER: &str = include_str!("fixtures/user.json");
/// `GET /repos/{owner}/{repo}/codespaces/devcontainers`, as documented.
const DEVCONTAINERS: &str = include_str!("fixtures/codespace_devcontainers.json");
/// A file of `GET /repos/{owner}/{repo}/contents/{path}`, as documented.
const CONTENT_FILE: &str = include_str!("fixtures/content_file.json");
/// `PUT /repos/{owner}/{repo}/contents/{path}`, as documented.
const FILE_COMMIT: &str = include_str!("fixtures/file_commit.json");

/// The documented examples' account and codespace.
const LOGIN: &str = "octocat";
const CODESPACE_NAME: &str = "monalisa-octocat-hello-world-g4wpq6h95q";

/// One scripted exchange: the request expected, and GitHub's reply.
struct Exchange {
    method: &'static str,
    path: String,
    reply: ApiReply,
}

fn exchange(method: &'static str, path: impl Into<String>, reply: ApiReply) -> Exchange {
    Exchange {
        method,
        path: path.into(),
        reply,
    }
}

/// A loopback GitHub that answers `script` in order; a request off the
/// script is answered 500 and shows in [`Recorded::assert_sent`].
struct Recorded {
    api_base: String,
    requests: ApiRequests,
    expected: Vec<(&'static str, String)>,
}

impl Recorded {
    async fn serve(script: Vec<Exchange>) -> Self {
        let expected = script
            .iter()
            .map(|exchange| (exchange.method, exchange.path.clone()))
            .collect();
        let script = Arc::new(Mutex::new(VecDeque::from(script)));
        let (api_base, requests) = serve_api(move |request, _| {
            let mut script = script.lock().expect("script");
            match script.front() {
                Some(next) if next.method == request.method && next.path == request.path => {
                    script.pop_front().expect("the front exchange").reply
                }
                _ => ApiReply::json(500, json!({"message": "off the script"}).to_string()),
            }
        })
        .await;
        Self {
            api_base,
            requests,
            expected,
        }
    }

    fn rest(&self) -> GitHubRest<PrTokenSource> {
        GitHubRest::with_api_base(PrTokenSource::Fixed("test-token".into()), &self.api_base)
    }

    fn provider(&self) -> Codespaces<PrTokenSource> {
        Codespaces::new(
            self.rest(),
            CodespacesAccount {
                login: LOGIN.into(),
            },
        )
    }

    /// Every scripted exchange happened, in order, and nothing else; every
    /// request carried the account's token.
    fn assert_sent(&self) {
        let requests = self.requests.lock().expect("requests").clone();
        let sent: Vec<_> = requests
            .iter()
            .map(|request| (request.method.as_str(), request.path.clone()))
            .collect();
        assert_eq!(sent, self.expected);
        assert!(
            requests
                .iter()
                .all(|request| request.authorization == "Bearer test-token"),
            "{requests:?}"
        );
    }

    /// The JSON body of the `index`th request.
    fn body(&self, index: usize) -> Value {
        let body = self.requests.lock().expect("requests")[index].body.clone();
        serde_json::from_str(&body).expect("a JSON request body")
    }
}

/// The documented codespace in `state`, on machine type `machine` (one of
/// the documented catalog's).
fn codespace(state: &str, machine: &str) -> String {
    let mut codespace: Value = serde_json::from_str(CODESPACE).expect("codespace fixture");
    let catalog: Value = serde_json::from_str(MACHINES).expect("machines fixture");
    codespace["state"] = json!(state);
    codespace["machine"] = catalog["machines"]
        .as_array()
        .expect("machines")
        .iter()
        .find(|entry| entry["name"] == machine)
        .expect("a documented machine type")
        .clone();
    codespace.to_string()
}

/// The documented repository with `private` set.
fn repository(private: bool) -> String {
    let mut repository: Value = serde_json::from_str(REPOSITORY).expect("repository fixture");
    repository["private"] = json!(private);
    repository.to_string()
}

/// GitHub's `basic-error` body for a 404.
fn not_found() -> ApiReply {
    ApiReply::json(
        404,
        json!({
            "message": "Not Found",
            "documentation_url": "https://docs.github.com/rest/codespaces/codespaces",
            "status": "404",
        })
        .to_string(),
    )
}

fn project() -> GitHubRepository {
    "octocat/Hello-World".parse().expect("repository")
}

/// The project as a source, without a devcontainer.
fn project_source() -> CodespaceSource {
    CodespaceSource {
        repository: project(),
        devcontainer_path: None,
        clone: None,
    }
}

fn standard_linux() -> MachineType {
    MachineType {
        name: "standardLinux".into(),
        display_name: "4 cores, 16 GB RAM, 64 GB storage".into(),
        cpus: 4,
        memory_bytes: 17_179_869_184,
        storage_bytes: 68_719_476_736,
    }
}

fn premium_linux() -> MachineType {
    MachineType {
        name: "premiumLinux".into(),
        display_name: "8 cores, 32 GB RAM, 64 GB storage".into(),
        cpus: 8,
        memory_bytes: 34_359_738_368,
        storage_bytes: 68_719_476_736,
    }
}

fn machine(state: MachineState, machine_type: MachineType) -> CloudMachine {
    CloudMachine {
        id: MachineId::new(CODESPACE_NAME.into()),
        state,
        machine_type: Some(machine_type),
    }
}

#[tokio::test]
async fn setup_saves_the_account_whose_token_carries_the_codespace_scope() {
    let api = Recorded::serve(vec![exchange(
        "GET",
        "/user",
        ApiReply::json(200, USER).header("x-oauth-scopes", "repo, codespace, workflow"),
    )])
    .await;

    let account = Codespaces::verify(&api.rest()).await.expect("verified");

    assert_eq!(
        account,
        CodespacesAccount {
            login: LOGIN.into()
        }
    );
    api.assert_sent();
}

#[tokio::test]
async fn setup_refuses_a_token_without_the_codespace_scope() {
    let api = Recorded::serve(vec![exchange(
        "GET",
        "/user",
        ApiReply::json(200, USER).header("x-oauth-scopes", "repo, gist, workflow"),
    )])
    .await;

    let error = Codespaces::verify(&api.rest()).await.expect_err("no scope");

    assert!(
        matches!(&error, CloudError::MissingGitHubScope { login, scope: "codespace" } if login == LOGIN),
        "{error:?}"
    );
    assert!(
        error
            .to_string()
            .contains("gh auth refresh --hostname github.com --scopes codespace"),
        "{error}"
    );
    api.assert_sent();
}

#[tokio::test]
async fn the_catalog_is_the_repositorys_machine_list() {
    let api = Recorded::serve(vec![exchange(
        "GET",
        "/repos/octocat/Hello-World/codespaces/machines",
        ApiReply::json(200, MACHINES),
    )])
    .await;

    let catalog = api
        .provider()
        .catalog(&project_source())
        .await
        .expect("catalog");

    assert_eq!(catalog, [standard_linux(), premium_linux()]);
    api.assert_sent();
}

/// The documented devcontainer listing, cut to `paths`.
fn devcontainers(paths: &[&str]) -> String {
    let mut listing: Value = serde_json::from_str(DEVCONTAINERS).expect("listing fixture");
    let kept: Vec<Value> = listing["devcontainers"]
        .as_array()
        .expect("devcontainers")
        .iter()
        .filter(|entry| paths.iter().any(|path| entry["path"] == *path))
        .cloned()
        .collect();
    listing["total_count"] = json!(kept.len());
    listing["devcontainers"] = Value::Array(kept);
    listing.to_string()
}

/// The documented file of the contents API holding `text`, encoded as
/// GitHub sends it: base64 in lines of 60.
fn content_file(text: &str) -> String {
    use base64::Engine as _;

    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let lines: Vec<String> = encoded
        .as_bytes()
        .chunks(60)
        .map(|line| String::from_utf8(line.to_vec()).expect("base64 is ASCII"))
        .collect();
    let mut file: Value = serde_json::from_str(CONTENT_FILE).expect("content fixture");
    file["content"] = json!(lines.join("\n") + "\n");
    file.to_string()
}

fn environment() -> GitHubRepository {
    format!("{LOGIN}/{ENVIRONMENT_REPOSITORY}")
        .parse()
        .expect("repository")
}

/// The environment repository's devcontainer path, read and written.
fn environment_devcontainer_path() -> String {
    format!("/repos/{LOGIN}/{ENVIRONMENT_REPOSITORY}/contents/{ENVIRONMENT_DEVCONTAINER}")
}

#[tokio::test]
async fn a_project_whose_devcontainer_has_an_ssh_server_is_the_source() {
    let devcontainer = r#"{
        // The project's own image, with the SSH server feature.
        "image": "mcr.microsoft.com/devcontainers/rust:1",
        "features": {
            "ghcr.io/devcontainers/features/sshd:1": {},
        },
    }"#;
    let api = Recorded::serve(vec![
        exchange(
            "GET",
            "/repos/octocat/Hello-World/codespaces/devcontainers",
            ApiReply::json(
                200,
                devcontainers(&[
                    ".devcontainer/foobar/devcontainer.json",
                    ".devcontainer/devcontainer.json",
                ]),
            ),
        ),
        exchange(
            "GET",
            "/repos/octocat/Hello-World/contents/.devcontainer/devcontainer.json",
            ApiReply::json(200, content_file(devcontainer)),
        ),
    ])
    .await;

    let source = api
        .provider()
        .source(Some(&project()))
        .await
        .expect("source");

    assert_eq!(
        source,
        CodespaceSource {
            repository: project(),
            devcontainer_path: Some(".devcontainer/devcontainer.json".into()),
            clone: None,
        }
    );
    api.assert_sent();
}

#[tokio::test]
async fn a_project_without_a_devcontainer_gets_githubs_default_image_which_has_ssh() {
    let api = Recorded::serve(vec![exchange(
        "GET",
        "/repos/octocat/Hello-World/codespaces/devcontainers",
        ApiReply::json(200, devcontainers(&[])),
    )])
    .await;

    let source = api
        .provider()
        .source(Some(&project()))
        .await
        .expect("source");

    assert_eq!(
        source,
        CodespaceSource {
            repository: project(),
            devcontainer_path: None,
            clone: None,
        }
    );
    api.assert_sent();
}

#[tokio::test]
async fn a_project_without_an_ssh_server_is_cloned_into_the_environment_repository() {
    let devcontainer = r#"{ "image": "mcr.microsoft.com/devcontainers/rust:1" }"#;
    let api = Recorded::serve(vec![
        exchange(
            "GET",
            "/repos/octocat/Hello-World/codespaces/devcontainers",
            ApiReply::json(200, devcontainers(&[".devcontainer.json"])),
        ),
        exchange(
            "GET",
            "/repos/octocat/Hello-World/contents/.devcontainer.json",
            ApiReply::json(200, content_file(devcontainer)),
        ),
        exchange(
            "GET",
            format!("/repos/{LOGIN}/{ENVIRONMENT_REPOSITORY}"),
            ApiReply::json(200, repository(true)),
        ),
        exchange(
            "GET",
            environment_devcontainer_path(),
            ApiReply::json(200, content_file(&super::environment_devcontainer())),
        ),
    ])
    .await;

    let source = api
        .provider()
        .source(Some(&project()))
        .await
        .expect("source");

    assert_eq!(
        source,
        CodespaceSource {
            repository: environment(),
            devcontainer_path: Some(ENVIRONMENT_DEVCONTAINER.into()),
            clone: Some(project()),
        }
    );
    api.assert_sent();
}

#[tokio::test]
async fn a_session_without_a_repository_gets_a_private_environment_repository() {
    let environment_path = format!("/repos/{LOGIN}/{ENVIRONMENT_REPOSITORY}");
    let api = Recorded::serve(vec![
        exchange("GET", environment_path, not_found()),
        exchange("POST", "/user/repos", ApiReply::json(201, repository(true))),
        exchange("GET", environment_devcontainer_path(), not_found()),
        exchange(
            "PUT",
            environment_devcontainer_path(),
            ApiReply::json(201, FILE_COMMIT),
        ),
    ])
    .await;

    let source = api.provider().source(None).await.expect("source");

    assert_eq!(
        source,
        CodespaceSource {
            repository: environment(),
            devcontainer_path: Some(ENVIRONMENT_DEVCONTAINER.into()),
            clone: None,
        }
    );
    let created = api.body(1);
    assert_eq!(created["name"], ENVIRONMENT_REPOSITORY);
    assert_eq!(created["private"], true);
    assert_eq!(created["auto_init"], true);
    let written = api.body(3);
    assert_eq!(written.get("sha"), None);
    let document = {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(written["content"].as_str().expect("content"))
            .expect("base64");
        serde_json::from_slice::<Value>(&bytes).expect("a devcontainer")
    };
    assert_eq!(
        document["features"],
        json!({"ghcr.io/devcontainers/features/sshd:1": {}})
    );
    assert_eq!(
        document["postStartCommand"],
        r#"test ! -x "$HOME/.may-cloud/bin/may" || "$HOME/.may-cloud/bin/may" daemon start"#
    );
    api.assert_sent();
}

#[tokio::test]
async fn an_existing_private_environment_repository_is_reused_and_its_devcontainer_restored() {
    let api = Recorded::serve(vec![
        exchange(
            "GET",
            format!("/repos/{LOGIN}/{ENVIRONMENT_REPOSITORY}"),
            ApiReply::json(200, repository(true)),
        ),
        exchange(
            "GET",
            environment_devcontainer_path(),
            ApiReply::json(200, content_file(r#"{ "image": "ubuntu" }"#)),
        ),
        exchange(
            "PUT",
            environment_devcontainer_path(),
            ApiReply::json(200, FILE_COMMIT),
        ),
    ])
    .await;

    let source = api.provider().source(None).await.expect("source");

    assert_eq!(source.repository, environment());
    let content: Value = serde_json::from_str(CONTENT_FILE).expect("content fixture");
    assert_eq!(api.body(2)["sha"], content["sha"]);
    api.assert_sent();
}

#[tokio::test]
async fn a_public_environment_repository_is_refused() {
    let api = Recorded::serve(vec![exchange(
        "GET",
        format!("/repos/{LOGIN}/{ENVIRONMENT_REPOSITORY}"),
        ApiReply::json(200, repository(false)),
    )])
    .await;

    let error = api.provider().source(None).await.expect_err("public");

    assert!(
        matches!(&error, CloudError::PublicSource(repository)
            if repository.to_string() == format!("{LOGIN}/{ENVIRONMENT_REPOSITORY}")),
        "{error:?}"
    );
    api.assert_sent();
}

#[tokio::test]
async fn a_machine_is_created_started_stopped_resized_and_deleted() {
    let path = format!("/user/codespaces/{CODESPACE_NAME}");
    let api = Recorded::serve(vec![
        exchange(
            "POST",
            "/repos/octocat/Hello-World/codespaces",
            ApiReply::json(202, codespace("Queued", "standardLinux")),
        ),
        exchange(
            "GET",
            path.clone(),
            ApiReply::json(200, codespace("Available", "standardLinux")),
        ),
        exchange(
            "POST",
            format!("{path}/stop"),
            ApiReply::json(200, codespace("ShuttingDown", "standardLinux")),
        ),
        exchange(
            "GET",
            path.clone(),
            ApiReply::json(200, codespace("Shutdown", "standardLinux")),
        ),
        exchange(
            "PATCH",
            path.clone(),
            ApiReply::json(200, codespace("Shutdown", "standardLinux")),
        ),
        exchange(
            "POST",
            format!("{path}/start"),
            ApiReply::json(200, codespace("Starting", "premiumLinux")),
        ),
        exchange(
            "GET",
            path.clone(),
            ApiReply::json(200, codespace("Available", "premiumLinux")),
        ),
        exchange("DELETE", path, ApiReply::json(202, "{}")),
    ])
    .await;
    let provider = api.provider();
    let id = MachineId::new(CODESPACE_NAME.into());

    let created = provider
        .create(
            &CodespaceSource {
                devcontainer_path: Some(".devcontainer/devcontainer.json".into()),
                ..project_source()
            },
            &NewMachine {
                machine_type: "standardLinux".into(),
                display_name: "May session".into(),
            },
        )
        .await
        .expect("create");
    assert_eq!(
        created,
        machine(MachineState::Provisioning, standard_linux())
    );
    assert_eq!(
        provider.machine(&id).await.expect("read"),
        Some(machine(MachineState::Running, standard_linux()))
    );
    assert_eq!(
        provider.stop(&id).await.expect("stop"),
        machine(MachineState::Provisioning, standard_linux())
    );
    assert_eq!(
        provider.machine(&id).await.expect("read"),
        Some(machine(MachineState::Suspended, standard_linux()))
    );
    assert_eq!(
        provider.resize(&id, "premiumLinux").await.expect("resize"),
        machine(MachineState::Suspended, standard_linux())
    );
    assert_eq!(
        provider.start(&id).await.expect("start"),
        machine(MachineState::Provisioning, premium_linux())
    );
    assert_eq!(
        provider.machine(&id).await.expect("read"),
        Some(machine(MachineState::Running, premium_linux()))
    );
    provider.delete(&id).await.expect("delete");

    assert_eq!(
        api.body(0),
        json!({
            "machine": "standardLinux",
            "display_name": "May session",
            "devcontainer_path": ".devcontainer/devcontainer.json",
        })
    );
    assert_eq!(api.body(4), json!({"machine": "premiumLinux"}));
    api.assert_sent();
}

#[tokio::test]
async fn a_machine_github_no_longer_has_is_gone() {
    let path = format!("/user/codespaces/{CODESPACE_NAME}");
    let api = Recorded::serve(vec![
        exchange("GET", path.clone(), not_found()),
        exchange("POST", format!("{path}/start"), not_found()),
        exchange("POST", format!("{path}/stop"), not_found()),
        exchange("PATCH", path.clone(), not_found()),
        exchange("DELETE", path, not_found()),
    ])
    .await;
    let provider = api.provider();
    let id = MachineId::new(CODESPACE_NAME.into());

    assert_eq!(provider.machine(&id).await.expect("read"), None);
    for error in [
        provider.start(&id).await.expect_err("start"),
        provider.stop(&id).await.expect_err("stop"),
        provider
            .resize(&id, "premiumLinux")
            .await
            .expect_err("resize"),
    ] {
        assert!(
            matches!(&error, CloudError::Gone(gone) if *gone == id),
            "{error:?}"
        );
    }
    provider.delete(&id).await.expect("already deleted");
    api.assert_sent();
}

#[tokio::test]
async fn a_refusal_carries_githubs_message() {
    let api = Recorded::serve(vec![exchange(
        "POST",
        "/repos/octocat/Hello-World/codespaces",
        ApiReply::json(
            403,
            json!({
                "message": "You must have a billing method to create a codespace",
                "documentation_url": "https://docs.github.com/rest/codespaces/codespaces",
                "status": "403",
            })
            .to_string(),
        ),
    )])
    .await;

    let error = api
        .provider()
        .create(
            &project_source(),
            &NewMachine {
                machine_type: "standardLinux".into(),
                display_name: "May session".into(),
            },
        )
        .await
        .expect_err("refused");

    assert!(
        matches!(&error, CloudError::Refused { status: 403, message, .. }
            if message == "You must have a billing method to create a codespace"),
        "{error:?}"
    );
    api.assert_sent();
}

/// A wait short enough for a test, long enough for a few polls.
const TEST_WAIT: RunningWait = RunningWait {
    deadline: Duration::from_secs(5),
    first_poll: Duration::from_millis(1),
    longest_poll: Duration::from_millis(4),
};

#[tokio::test]
async fn a_suspended_machine_is_started_once_and_waited_on_until_it_runs() {
    let path = format!("/user/codespaces/{CODESPACE_NAME}");
    let api = Recorded::serve(vec![
        exchange(
            "GET",
            path.clone(),
            ApiReply::json(200, codespace("Shutdown", "standardLinux")),
        ),
        exchange(
            "POST",
            format!("{path}/start"),
            ApiReply::json(200, codespace("Shutdown", "standardLinux")),
        ),
        // The start is not visible yet: no second start is sent.
        exchange(
            "GET",
            path.clone(),
            ApiReply::json(200, codespace("Shutdown", "standardLinux")),
        ),
        exchange(
            "GET",
            path.clone(),
            ApiReply::json(200, codespace("Starting", "standardLinux")),
        ),
        exchange(
            "GET",
            path,
            ApiReply::json(200, codespace("Available", "standardLinux")),
        ),
    ])
    .await;
    let mut seen = Vec::new();

    let running = until_running(
        &api.provider(),
        &MachineId::new(CODESPACE_NAME.into()),
        TEST_WAIT,
        |machine| seen.push(machine.state),
    )
    .await
    .expect("running");

    assert_eq!(running, machine(MachineState::Running, standard_linux()));
    assert_eq!(
        seen,
        [
            MachineState::Suspended,
            MachineState::Suspended,
            MachineState::Suspended,
            MachineState::Provisioning,
            MachineState::Running,
        ]
    );
    api.assert_sent();
}

#[tokio::test]
async fn a_machine_that_falls_back_to_suspended_is_started_again() {
    let path = format!("/user/codespaces/{CODESPACE_NAME}");
    let api = Recorded::serve(vec![
        exchange(
            "GET",
            path.clone(),
            ApiReply::json(200, codespace("Shutdown", "standardLinux")),
        ),
        exchange(
            "POST",
            format!("{path}/start"),
            ApiReply::json(200, codespace("Starting", "standardLinux")),
        ),
        exchange(
            "GET",
            path.clone(),
            ApiReply::json(200, codespace("Starting", "standardLinux")),
        ),
        exchange(
            "GET",
            path.clone(),
            ApiReply::json(200, codespace("Shutdown", "standardLinux")),
        ),
        exchange(
            "POST",
            format!("{path}/start"),
            ApiReply::json(200, codespace("Available", "standardLinux")),
        ),
    ])
    .await;

    let running = until_running(
        &api.provider(),
        &MachineId::new(CODESPACE_NAME.into()),
        TEST_WAIT,
        |_| {},
    )
    .await
    .expect("running");

    assert_eq!(running, machine(MachineState::Running, standard_linux()));
    api.assert_sent();
}

#[tokio::test]
async fn a_machine_that_dies_or_vanishes_while_waited_on_is_gone() {
    let path = format!("/user/codespaces/{CODESPACE_NAME}");
    let api = Recorded::serve(vec![
        exchange(
            "GET",
            path.clone(),
            ApiReply::json(200, codespace("Provisioning", "standardLinux")),
        ),
        exchange(
            "GET",
            path.clone(),
            ApiReply::json(200, codespace("Failed", "standardLinux")),
        ),
        exchange("GET", path, not_found()),
    ])
    .await;
    let provider = api.provider();
    let id = MachineId::new(CODESPACE_NAME.into());

    for _ in 0..2 {
        let error = until_running(&provider, &id, TEST_WAIT, |_| {})
            .await
            .expect_err("gone");
        assert!(
            matches!(&error, CloudError::Gone(gone) if *gone == id),
            "{error:?}"
        );
    }
    api.assert_sent();
}

#[tokio::test]
async fn a_machine_still_settling_at_the_deadline_is_not_running() {
    let path = format!("/user/codespaces/{CODESPACE_NAME}");
    let api = Recorded::serve(vec![exchange(
        "GET",
        path,
        ApiReply::json(200, codespace("Rebuilding", "standardLinux")),
    )])
    .await;
    let id = MachineId::new(CODESPACE_NAME.into());

    let error = until_running(
        &api.provider(),
        &id,
        RunningWait {
            deadline: Duration::ZERO,
            ..TEST_WAIT
        },
        |_| {},
    )
    .await
    .expect_err("deadline");

    assert!(
        matches!(&error, CloudError::NotRunning { id: waited, state: MachineState::Provisioning, .. } if *waited == id),
        "{error:?}"
    );
    api.assert_sent();
}

#[test]
fn every_documented_state_maps_to_a_lifecycle_state() {
    use MachineState::{Gone, Provisioning, Running, Suspended};
    let states = [
        ("Unknown", Provisioning),
        ("Created", Provisioning),
        ("Queued", Provisioning),
        ("Provisioning", Provisioning),
        ("Available", Running),
        ("Awaiting", Provisioning),
        ("Unavailable", Suspended),
        ("Deleted", Gone),
        ("Moved", Gone),
        ("Shutdown", Suspended),
        ("Archived", Gone),
        ("Starting", Provisioning),
        ("ShuttingDown", Provisioning),
        ("Failed", Gone),
        ("Exporting", Provisioning),
        ("Updating", Provisioning),
        ("Rebuilding", Provisioning),
    ];
    for (state, expected) in states {
        let codespace: super::Codespace =
            serde_json::from_str(&codespace(state, "standardLinux")).expect("codespace");
        assert_eq!(CloudMachine::from(codespace).state, expected, "{state}");
    }
}
