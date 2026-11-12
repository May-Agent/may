//! A bootstrap against a recorded machine: each test scripts what the
//! machine's commands answer and checks every command the bootstrap ran,
//! with its input, in order.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use super::{Bootstrap, BootstrapStep, MayBuild, RemoteShell};
use crate::cloud::CloudError;
use crate::cloud::home::{CloudHome, FrontingToken};
use crate::workspace::GitHubRepository;

/// One command the bootstrap ran.
#[derive(Debug, Clone)]
struct Ran {
    step: &'static str,
    script: String,
    input: Option<Vec<u8>>,
}

/// A machine answering each command with the next scripted answer.
struct RecordedShell {
    answers: Mutex<VecDeque<Result<String, CloudError>>>,
    ran: Mutex<Vec<Ran>>,
}

impl RecordedShell {
    fn new(answers: Vec<Result<String, CloudError>>) -> Self {
        Self {
            answers: Mutex::new(answers.into()),
            ran: Mutex::new(Vec::new()),
        }
    }

    fn ran(&self) -> Vec<Ran> {
        self.ran.lock().expect("log").clone()
    }
}

impl RemoteShell for RecordedShell {
    fn run(
        &self,
        step: &'static str,
        script: &str,
        input: Option<Vec<u8>>,
        _deadline: Duration,
    ) -> impl Future<Output = Result<String, CloudError>> + Send {
        self.ran.lock().expect("log").push(Ran {
            step,
            script: script.to_owned(),
            input,
        });
        let answer = self
            .answers
            .lock()
            .expect("answers")
            .pop_front()
            .unwrap_or_else(|| Ok(String::new()));
        std::future::ready(answer)
    }
}

fn home() -> CloudHome {
    CloudHome::for_token(&FrontingToken::generate(), "octocat".into())
}

#[tokio::test]
async fn a_fresh_machine_gets_the_release_the_record_and_a_running_daemon() {
    let shell = RecordedShell::new(vec![Ok(String::new())]);
    let home = home();
    let build = MayBuild::Release {
        version: "0.1.0".into(),
    };
    let mut steps = Vec::new();

    Bootstrap {
        build: &build,
        home: &home,
        clone: None,
        github_token: "account-token",
    }
    .run(&shell, |step| steps.push(step))
    .await
    .expect("bootstrapped");

    let ran = shell.ran();
    let steps_ran: Vec<_> = ran.iter().map(|ran| ran.step).collect();
    assert_eq!(
        steps_ran,
        [
            "check the installed May",
            "install May",
            "write the cloud home record",
            "start the May daemon",
        ]
    );
    assert!(
        ran[1].script.contains(
            "https://github.com/May-Agent/may/releases/download/v0.1.0/may-agent-installer.sh"
        ),
        "{}",
        ran[1].script
    );
    assert!(ran[1].script.contains("release:0.1.0"), "{}", ran[1].script);
    assert!(
        ran[1].script.contains(r#"CARGO_HOME="$HOME/.may-cloud""#),
        "{}",
        ran[1].script
    );
    let record: CloudHome =
        serde_json::from_slice(ran[2].input.as_deref().expect("the record")).expect("a record");
    assert_eq!(record, home);
    assert!(
        ran[3]
            .script
            .contains(r#""$HOME/.may-cloud/bin/may" daemon start"#)
    );
    assert_eq!(
        steps,
        [
            BootstrapStep::Checking,
            BootstrapStep::Installing,
            BootstrapStep::StartingDaemon,
        ]
    );
}

#[tokio::test]
async fn an_installed_build_is_kept_and_the_project_is_cloned_with_the_token_on_stdin() {
    let commit = "6bbeeed53be58e7e1be3ecde6a781dcc5a5c6711";
    let shell = RecordedShell::new(vec![Ok(format!("ci:{commit}"))]);
    let home = home();
    let build = MayBuild::Ci {
        commit: commit.into(),
    };
    let project: GitHubRepository = "octocat/Hello-World".parse().expect("repository");

    Bootstrap {
        build: &build,
        home: &home,
        clone: Some(&project),
        github_token: "account-token",
    }
    .run(&shell, |_| {})
    .await
    .expect("bootstrapped");

    let ran = shell.ran();
    let steps_ran: Vec<_> = ran.iter().map(|ran| ran.step).collect();
    assert_eq!(
        steps_ran,
        [
            "check the installed May",
            "clone the repository",
            "write the cloud home record",
            "start the May daemon",
        ]
    );
    let clone = &ran[1];
    assert!(
        clone
            .script
            .contains("clone https://github.com/octocat/Hello-World.git /workspaces/Hello-World"),
        "{}",
        clone.script
    );
    assert!(!clone.script.contains("account-token"), "{}", clone.script);
    assert_eq!(clone.input.as_deref(), Some(&b"account-token\n"[..]));
}

#[tokio::test]
async fn a_failed_step_stops_the_bootstrap() {
    let shell = RecordedShell::new(vec![
        Ok(String::new()),
        Err(CloudError::Remote {
            step: "install May",
            message: "curl: (6) Could not resolve host: github.com".into(),
        }),
    ]);
    let home = home();
    let build = MayBuild::Release {
        version: "0.1.0".into(),
    };

    let error = Bootstrap {
        build: &build,
        home: &home,
        clone: None,
        github_token: "account-token",
    }
    .run(&shell, |_| {})
    .await
    .expect_err("the install failed");

    assert!(
        matches!(&error, CloudError::Remote { step: "install May", message } if message.contains("github.com")),
        "{error:?}"
    );
    assert_eq!(shell.ran().len(), 2);
}

#[test]
fn the_start_command_starts_an_installed_may_only() {
    use askama::Template as _;

    assert_eq!(
        super::PostStart::default()
            .render()
            .expect("renders")
            .trim(),
        r#"test ! -x "$HOME/.may-cloud/bin/may" || "$HOME/.may-cloud/bin/may" daemon start"#
    );
}
