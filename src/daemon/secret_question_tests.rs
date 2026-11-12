//! Secret `ask_user` questions against a real SQLite database file: the value
//! lands in the secret store under the asked name and reaches the asking
//! session's terminals, and nothing the agent, the history, the database or
//! the UI reads ever holds it.

use std::sync::Arc;

use aither_core::llm::{Tool, ToolContext};
use tempfile::TempDir;

use super::state::{
    AskUserApprover, AskUserOutcome, AskUserRespondError, UiAnswerInput, UiQuestionAnswer,
};
use crate::agent::ask_user_tool::{AskUserArgs, AskUserTool, AskedQuestion, SecretSpec};
use crate::db::ChatDb;
use crate::secrets::{SecretRedactor, SecretScope, SecretStore, SessionEnv};
use crate::session::SessionRuntimeService;
use crate::ui_event_bus::UiEventBus;

const NAME: &str = "DEPLOY_TOKEN";
const VALUE: &str = "hunter2-correct-horse";

struct Fixture {
    dir: TempDir,
    runtime: Arc<SessionRuntimeService>,
    store: SecretStore,
    approver: AskUserApprover,
}

impl Fixture {
    async fn new() -> Self {
        crate::test_support::install_global_executor();
        let dir = tempfile::tempdir().expect("temp dir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("open database");
        for id in ["session", "other"] {
            db.create_session(id, Some(id), Some("test-model"))
                .await
                .expect("create session");
        }
        let runtime = Arc::new(SessionRuntimeService::new(
            db,
            UiEventBus::new(),
            crate::session::MayHomePaths::new(dir.path().join("home")),
        ));
        let store = SecretStore::in_memory(dir.path());
        let approver = AskUserApprover::new()
            .with_runtime_service(runtime.clone())
            .with_secret_store(store.clone());
        Self {
            dir,
            runtime,
            store,
            approver,
        }
    }

    /// Ask a secret question as `session` and return its request id.
    async fn ask(&self, session: &str, name: &str, scope: SecretScope) -> Result<String, String> {
        let args = AskUserArgs {
            question: None,
            question_text: None,
            option: Vec::new(),
            multi_select: false,
            secret: None,
            secret_scope: None,
            questions: vec![AskedQuestion {
                section: String::new(),
                question: "Deploy token?".to_string(),
                option: Vec::new(),
                multi_select: false,
                secret: Some(SecretSpec {
                    name: name.to_string(),
                    scope,
                }),
            }],
        };
        AskUserTool::new(self.runtime.clone(), session)
            .call(args, ToolContext::new())
            .await
            .map_err(|error| error.to_string())?;
        let open = self
            .runtime
            .get(session)
            .await
            .expect("runtime loads")
            .open_questions;
        Ok(open.last().expect("the question is open").id.clone())
    }

    async fn respond(
        &self,
        id: &str,
        section: &str,
        answer: UiAnswerInput,
    ) -> Result<Option<super::state::AskUserResolved>, AskUserRespondError> {
        self.approver
            .respond(
                id,
                vec![UiQuestionAnswer {
                    section: section.to_string(),
                    answer,
                }],
            )
            .await
    }

    fn typed(value: &str) -> UiAnswerInput {
        UiAnswerInput::Custom {
            text: value.to_string(),
        }
    }

    fn env(&self, session: &str) -> Vec<(String, String)> {
        SessionEnv::new(self.store.clone(), session)
            .env()
            .expect("the stored secrets read")
    }

    /// Every byte the database files hold.
    fn database_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        for entry in std::fs::read_dir(self.dir.path()).expect("list the database dir") {
            let path = entry.expect("dir entry").path();
            if path.is_file() {
                bytes.extend(std::fs::read(path).expect("read database file"));
            }
        }
        bytes
    }
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

/// The answer stores the value under the asked name; the next terminal
/// command of the session sees it; the agent's message, the history, the
/// runtime the UI reads, the database file and the provider request hold the
/// name and not the value.
#[tokio::test]
async fn a_secret_answer_is_stored_and_only_its_name_travels() {
    let fixture = Fixture::new().await;
    let id = fixture
        .ask("session", NAME, SecretScope::Session)
        .await
        .expect("the question is asked");

    let resolved = fixture
        .respond(&id, NAME, Fixture::typed(VALUE))
        .await
        .expect("the answer commits")
        .expect("the question was open");
    assert_eq!(resolved.outcome, AskUserOutcome::Queued);

    assert!(
        fixture
            .env("session")
            .contains(&(NAME.to_string(), VALUE.to_string())),
        "the terminal environment carries the secret"
    );

    let runtime = fixture.runtime.get("session").await.expect("runtime loads");
    let delivered = runtime
        .pending_immediate
        .clone()
        .expect("the answer is queued")
        .hidden_context_xml
        .expect("hidden context")
        .to_string();
    assert!(delivered.contains(NAME), "{delivered}");
    assert!(!delivered.contains(VALUE), "{delivered}");

    let answers = fixture
        .runtime
        .committed_ask_user_answers(&id)
        .await
        .expect("answers load")
        .expect("the answer is committed");
    let committed = format!("{answers:?}");
    assert!(
        committed.contains(NAME) && !committed.contains(VALUE),
        "{committed}"
    );

    let history = format!(
        "{:?}",
        fixture
            .runtime
            .db()
            .get_messages_by_role("session", "ask_user_response")
            .await
            .expect("history loads")
    );
    assert!(!history.contains(VALUE), "{history}");

    let ui = serde_json::to_string(&runtime).expect("runtime serializes");
    assert!(!ui.contains(VALUE), "{ui}");
    assert!(
        !contains(&fixture.database_bytes(), VALUE),
        "the database files never held the value"
    );

    // The model-bound text is what a request would carry; and a value that
    // does reach a tool result is replaced by the name for this session.
    let redactions = SecretRedactor::over(fixture.store.clone(), Some("session".to_string()))
        .current()
        .await
        .expect("the stored secrets read");
    assert_eq!(
        redactions.redact(&format!("echo {VALUE}")),
        format!("echo {NAME}")
    );
}

/// A name that is no environment variable name is refused when the question
/// is asked, and nothing is left open.
#[tokio::test]
async fn an_invalid_secret_name_is_refused_when_asked() {
    let fixture = Fixture::new().await;
    for name in ["bad-name", "9LIVES", "", "A B"] {
        let error = fixture
            .ask("session", name, SecretScope::Global)
            .await
            .expect_err("the question is refused");
        assert!(error.contains("invalid secret name"), "{error}");
    }
    let open = fixture
        .runtime
        .get("session")
        .await
        .expect("runtime loads")
        .open_questions;
    assert!(open.is_empty(), "{open:?}");
}

/// A session secret reaches its own session's terminals and not another's,
/// and is gone once its session is deleted; a global one reaches both.
#[tokio::test]
async fn a_session_secret_stays_in_its_session_and_goes_with_it() {
    let fixture = Fixture::new().await;
    let session = fixture
        .ask("session", NAME, SecretScope::Session)
        .await
        .expect("asked");
    fixture
        .respond(&session, NAME, Fixture::typed(VALUE))
        .await
        .expect("answered");
    let global = fixture
        .ask("session", "SHARED_TOKEN", SecretScope::Global)
        .await
        .expect("asked");
    fixture
        .respond(&global, "SHARED_TOKEN", Fixture::typed("shared-value-1"))
        .await
        .expect("answered");

    let own = (NAME.to_string(), VALUE.to_string());
    let shared = ("SHARED_TOKEN".to_string(), "shared-value-1".to_string());
    assert!(fixture.env("session").contains(&own));
    assert!(fixture.env("session").contains(&shared));
    assert!(!fixture.env("other").contains(&own), "not another session");
    assert!(fixture.env("other").contains(&shared), "global reaches all");

    let other_redactions = SecretRedactor::over(fixture.store.clone(), Some("other".to_string()))
        .current()
        .await
        .expect("the stored secrets read");
    assert_eq!(
        other_redactions.redact(VALUE),
        VALUE,
        "only its own session redacts it"
    );

    fixture
        .store
        .remove_session("session")
        .expect("the session's secrets are deleted");
    assert!(!fixture.env("session").contains(&own));
    assert!(fixture.env("session").contains(&shared), "global stays");
}

/// Declining stores nothing and tells the agent so.
#[tokio::test]
async fn declining_a_secret_question_stores_nothing() {
    let fixture = Fixture::new().await;
    let id = fixture
        .ask("session", NAME, SecretScope::Global)
        .await
        .expect("asked");
    fixture
        .respond(&id, NAME, UiAnswerInput::Declined)
        .await
        .expect("declined")
        .expect("open");
    assert_eq!(fixture.env("session").len(), 0);
    let delivered = fixture
        .runtime
        .get("session")
        .await
        .expect("runtime loads")
        .pending_immediate
        .expect("queued")
        .hidden_context_xml
        .expect("hidden context")
        .to_string();
    assert!(
        delivered.contains("declined") && delivered.contains(NAME),
        "{delivered}"
    );
}

/// A secret question takes its value as text, a plain question cannot be
/// declined, and a blank value stores nothing; the question stays open.
#[tokio::test]
async fn answers_that_do_not_fit_the_question_store_nothing() {
    let fixture = Fixture::new().await;
    let id = fixture
        .ask("session", NAME, SecretScope::Global)
        .await
        .expect("asked");
    let selected = UiAnswerInput::Selected {
        values: vec![VALUE.to_string()],
    };
    assert!(fixture.respond(&id, NAME, selected).await.is_err());
    assert!(
        fixture
            .respond(&id, NAME, Fixture::typed("   "))
            .await
            .is_err()
    );
    assert_eq!(fixture.env("session").len(), 0);
    assert_eq!(
        fixture
            .runtime
            .get("session")
            .await
            .expect("runtime loads")
            .open_questions
            .len(),
        1
    );
}
