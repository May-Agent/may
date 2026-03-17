use std::sync::Arc;

use crate::agent::generate_session_title;
use crate::session::{MayHomePaths, SessionId, SessionStore};

use super::helpers::truncate_preview;
use super::state::{TitleGenerationTask, UiServerState};

pub(super) fn enqueue(
    state: &Arc<UiServerState>,
    session_id: &SessionId,
    user_message: String,
    assistant_response: String,
) {
    if user_message.trim().is_empty() || assistant_response.trim().is_empty() {
        return;
    }

    state.title_generation_queue.enqueue(TitleGenerationTask {
        session_id: session_id.as_str().to_string(),
        user_message,
        assistant_response,
    });
}

pub(super) fn spawn_worker(
    state: Arc<UiServerState>,
    mut queue: tokio::sync::mpsc::UnboundedReceiver<TitleGenerationTask>,
) {
    tokio::spawn(async move {
        while let Some(task) = queue.recv().await {
            process_task(state.clone(), task).await;
        }
    });
}

fn is_replaceable_session_title(current: Option<&str>, placeholder: &str) -> bool {
    match current {
        None => true,
        Some(title) => {
            let trimmed = title.trim();
            trimmed.is_empty() || trimmed == "New Chat" || trimmed == placeholder
        }
    }
}

async fn process_task(state: Arc<UiServerState>, task: TitleGenerationTask) {
    let session_id = SessionId::from_string(task.session_id);
    let placeholder = truncate_preview(&task.user_message);
    let db = state.db();

    {
        let current = match db.get_session(session_id.as_str()).await {
            Ok(current) => current,
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    error = %error,
                    "failed to load session before title generation"
                );
                return;
            }
        };
        let replaceable = is_replaceable_session_title(
            current.as_ref().and_then(|s| s.title.as_deref()),
            &placeholder,
        );
        if !replaceable {
            return;
        }
    }

    let config = (*state.config_snapshot()).clone();
    let Some(generated_title) =
        generate_session_title(&config, &task.user_message, &task.assistant_response).await
    else {
        return;
    };
    let generated_title = generated_title.trim().to_string();
    if generated_title.is_empty() {
        return;
    }

    let current = match db.get_session(session_id.as_str()).await {
        Ok(current) => current,
        Err(error) => {
            tracing::warn!(
                session_id = %session_id,
                error = %error,
                "failed to reload session for title write"
            );
            return;
        }
    };
    let Some(current) = current else {
        tracing::warn!(
            session_id = %session_id,
            "session disappeared before title generation could persist"
        );
        return;
    };
    let replaceable = is_replaceable_session_title(current.title.as_deref(), &placeholder);
    if !replaceable {
        return;
    }
    if let Err(error) = db
        .update_session(session_id.as_str(), Some(generated_title.as_str()), None)
        .await
    {
        tracing::warn!(
            session_id = %session_id,
            error = %error,
            "failed to persist generated session title"
        );
        return;
    }
    let store = SessionStore::new(MayHomePaths::new(state.may_dir.clone()));
    match store
        .read_history(session_id.as_str(), current.profile_id.as_deref())
        .await
    {
        Ok(mut history) => {
            history.metadata.title = Some(generated_title.clone());
            if let Err(error) = store
                .write_history(session_id.as_str(), current.profile_id.as_deref(), &history)
                .await
            {
                tracing::warn!(
                    session_id = %session_id,
                    error = %error,
                    "failed to update session backup title"
                );
            }
        }
        Err(error) => {
            tracing::warn!(
                session_id = %session_id,
                error = %error,
                "failed to load session backup for title update"
            );
        }
    }

    state.notify_ui_events();
    tracing::debug!(
        session_id = %session_id,
        title = %generated_title,
        "session title generated"
    );
}
