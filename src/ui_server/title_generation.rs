use std::sync::Arc;

use crate::agent::generate_session_title;
use crate::session::SessionId;

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

    {
        let mut manager = state.session_manager.lock().await;
        if let Err(e) = manager.load_session(&session_id).await {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "failed to load session before title generation"
            );
            return;
        }
        let replaceable = is_replaceable_session_title(
            manager.current().and_then(|s| s.title.as_deref()),
            &placeholder,
        );
        if !replaceable {
            return;
        }
    }

    let config = state.config.read().await.clone();
    let Some(generated_title) =
        generate_session_title(&config, &task.user_message, &task.assistant_response).await
    else {
        return;
    };
    let generated_title = generated_title.trim().to_string();
    if generated_title.is_empty() {
        return;
    }

    let mut manager = state.session_manager.lock().await;
    if let Err(e) = manager.load_session(&session_id).await {
        tracing::warn!(
            session_id = %session_id,
            error = %e,
            "failed to reload session for title write"
        );
        return;
    }
    let replaceable = is_replaceable_session_title(
        manager.current().and_then(|s| s.title.as_deref()),
        &placeholder,
    );
    if !replaceable {
        return;
    }
    if let Err(e) = manager.set_current_title(generated_title.clone()).await {
        tracing::warn!(
            session_id = %session_id,
            error = %e,
            "failed to persist generated session title"
        );
        return;
    }
    drop(manager);

    state.notify_ui_events();
    tracing::debug!(
        session_id = %session_id,
        title = %generated_title,
        "session title generated"
    );
}
