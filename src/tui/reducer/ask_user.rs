use anyhow::Result;
use crossterm::event::KeyCode;
use itertools::Itertools;

use crate::ask_user::{AnswerValue, QuestionAnswer};
use crate::tui::app::{App, SendAction};
use crate::tui::state::AppMode;

pub(crate) fn handle_ask_user_key(app: &mut App, code: KeyCode) -> Result<Option<SendAction>> {
    let state = match &mut app.mode {
        AppMode::AskUser(s) => s,
        _ => return Ok(None),
    };

    if let Some(qs) = state.current_state_mut()
        && qs.custom_mode
    {
        match code {
            KeyCode::Esc => qs.custom_mode = false,
            KeyCode::Enter => {
                qs.custom_mode = false;
                ask_user_advance_or_submit(app);
            }
            KeyCode::Backspace => {
                qs.custom_text.pop();
            }
            KeyCode::Char(c) => {
                qs.custom_text.push(c);
            }
            _ => {}
        }
        return Ok(None);
    }

    let item_count = state.current_item_count();
    let question = state.current().cloned();

    match code {
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(qs) = state.current_state_mut()
                && qs.cursor > 0
            {
                qs.cursor -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(qs) = state.current_state_mut()
                && qs.cursor + 1 < item_count
            {
                qs.cursor += 1;
            }
        }
        KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
            if state.current_question + 1 < state.questions.len() {
                state.current_question += 1;
            }
        }
        KeyCode::BackTab | KeyCode::Left | KeyCode::Char('H') => {
            if state.current_question > 0 {
                state.current_question -= 1;
            }
        }
        KeyCode::Char(' ') => {
            if let (Some(q), Some(qs)) = (&question, state.current_state_mut()) {
                let cursor = qs.cursor;
                let opt_count = q.option.len();
                if cursor < opt_count {
                    if q.multi_select {
                        if qs.selections.contains(&cursor) {
                            qs.selections.retain(|&x| x != cursor);
                        } else {
                            qs.selections.push(cursor);
                        }
                    } else {
                        qs.selections.clear();
                        qs.selections.push(cursor);
                    }
                    qs.skipped = false;
                }
            }
        }
        KeyCode::Enter => {
            if let (Some(q), Some(qs)) = (&question, state.current_state_mut()) {
                let cursor = qs.cursor;
                let opt_count = q.option.len();

                if cursor < opt_count {
                    if !q.multi_select {
                        qs.selections.clear();
                        qs.selections.push(cursor);
                        qs.skipped = false;
                    }
                    ask_user_advance_or_submit(app);
                } else if cursor == opt_count {
                    if let Some(qs) = state.current_state_mut() {
                        qs.skipped = true;
                        qs.selections.clear();
                        qs.custom_text.clear();
                    }
                    ask_user_advance_or_submit(app);
                } else if cursor == opt_count + 1 {
                    if let Some(qs) = state.current_state_mut() {
                        qs.custom_mode = true;
                        qs.skipped = false;
                        qs.selections.clear();
                    }
                } else if q.multi_select {
                    ask_user_advance_or_submit(app);
                }
            }
        }
        KeyCode::Esc => ask_user_submit_all_skipped(app),
        _ => {}
    }

    Ok(None)
}

fn ask_user_advance_or_submit(app: &mut App) {
    let state = match &mut app.mode {
        AppMode::AskUser(s) => s,
        _ => return,
    };

    if state.current_question + 1 < state.questions.len() {
        state.current_question += 1;
    } else {
        ask_user_submit(app);
    }
}

fn ask_user_submit(app: &mut App) {
    let mode = std::mem::replace(&mut app.mode, AppMode::Chat);
    if let AppMode::AskUser(state) = mode {
        let answers: Vec<QuestionAnswer> = state
            .questions
            .iter()
            .zip(state.question_states.iter())
            .map(|(q, qs)| {
                let answer = if qs.skipped {
                    AnswerValue::Skipped
                } else if !qs.custom_text.is_empty() {
                    AnswerValue::Custom(qs.custom_text.clone())
                } else {
                    let selected: Vec<String> = qs
                        .selections
                        .iter()
                        .sorted()
                        .filter_map(|&idx| q.option.get(idx).cloned())
                        .collect();
                    AnswerValue::Selected(selected)
                };
                QuestionAnswer {
                    section: q.section.clone(),
                    answer,
                }
            })
            .collect();
        state
            .request
            .respond(answers)
            .unwrap_or_else(|_| panic!("{}", super::super::app::ERR_TOOL_RESPONSE_SEND.trim()));
    }
}

fn ask_user_submit_all_skipped(app: &mut App) {
    let mode = std::mem::replace(&mut app.mode, AppMode::Chat);
    if let AppMode::AskUser(state) = mode {
        let answers: Vec<QuestionAnswer> = state
            .questions
            .iter()
            .map(|q| QuestionAnswer {
                section: q.section.clone(),
                answer: AnswerValue::Skipped,
            })
            .collect();
        state
            .request
            .respond(answers)
            .unwrap_or_else(|_| panic!("{}", super::super::app::ERR_TOOL_RESPONSE_SEND.trim()));
    }
}
