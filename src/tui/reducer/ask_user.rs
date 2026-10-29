use crossterm::event::KeyCode;
use itertools::Itertools;

use crate::ask_user::{AnswerValue, QuestionAnswer};
use crate::tui::app::{App, SendAction};
use crate::tui::state::{AppMode, AskUserQuestionState, AskUserState};

/// What a key did to the answer modal.
#[derive(Debug)]
pub enum AskUserKey {
    /// The modal stays up, possibly changed.
    Stay,
    /// The modal closes and nothing is committed: an open question stays
    /// open.
    Close,
    /// Every question has an answer; the modal closes and the answers are
    /// committed.
    Submit(Vec<QuestionAnswer>),
}

pub fn handle_ask_user_key(app: &mut App, code: KeyCode) -> Option<SendAction> {
    let AppMode::AskUser(state) = &mut app.mode else {
        return None;
    };
    match handle_key(state, code) {
        AskUserKey::Stay => None,
        AskUserKey::Close => {
            app.mode = AppMode::Chat;
            None
        }
        AskUserKey::Submit(answers) => {
            let AppMode::AskUser(state) = std::mem::replace(&mut app.mode, AppMode::Chat) else {
                unreachable!("the key was handled in the answer modal");
            };
            Some(SendAction::RespondAskUser {
                request_id: state.request_id,
                request: state.request,
                answers,
            })
        }
    }
}

/// Apply `code` to the answer modal `state`.
pub fn handle_key(state: &mut AskUserState, code: KeyCode) -> AskUserKey {
    if let Some(qs) = state.current_state_mut()
        && qs.custom_mode
    {
        match code {
            KeyCode::Esc => qs.custom_mode = false,
            KeyCode::Enter => {
                qs.custom_mode = false;
                return advance_or_submit(state);
            }
            KeyCode::Backspace => {
                qs.custom_text.pop();
            }
            KeyCode::Char(c) => {
                qs.custom_text.push(c);
            }
            _ => {}
        }
        return AskUserKey::Stay;
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
                if cursor < q.option.len() {
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
                    qs.custom_text.clear();
                }
            }
        }
        KeyCode::Enter => {
            if let (Some(q), Some(qs)) = (&question, state.current_state_mut()) {
                let cursor = qs.cursor;
                if cursor < q.option.len() {
                    if !q.multi_select {
                        qs.selections.clear();
                        qs.selections.push(cursor);
                        qs.custom_text.clear();
                    }
                    return advance_or_submit(state);
                }
                qs.custom_mode = true;
                qs.selections.clear();
            }
        }
        // An open question stays open when its modal closes; the user opens
        // it again with `/questions`. A prompt a run blocks on has no other
        // way to be answered, so its modal stays up.
        KeyCode::Esc if state.request.is_none() => return AskUserKey::Close,
        _ => {}
    }

    AskUserKey::Stay
}

/// Move on to the next question, or submit after the last. An unanswered
/// question holds the user where it is: nothing is committed for it.
fn advance_or_submit(state: &mut AskUserState) -> AskUserKey {
    if !state
        .current_state()
        .is_some_and(AskUserQuestionState::is_answered)
    {
        return AskUserKey::Stay;
    }
    if state.current_question + 1 < state.questions.len() {
        state.current_question += 1;
        return AskUserKey::Stay;
    }
    // Last question answered: submit once every question has an answer,
    // otherwise go back to the first one that has none.
    if let Some(unanswered) = state
        .question_states
        .iter()
        .position(|qs| !qs.is_answered())
    {
        state.current_question = unanswered;
        return AskUserKey::Stay;
    }
    AskUserKey::Submit(answers(state))
}

fn answers(state: &AskUserState) -> Vec<QuestionAnswer> {
    state
        .questions
        .iter()
        .zip(state.question_states.iter())
        .map(|(q, qs)| {
            let answer = if qs.custom_text.trim().is_empty() {
                let selected: Vec<String> = qs
                    .selections
                    .iter()
                    .sorted()
                    .filter_map(|&idx| q.option.get(idx).cloned())
                    .collect();
                AnswerValue::Selected(selected)
            } else {
                AnswerValue::Custom(qs.custom_text.trim().to_string())
            };
            QuestionAnswer {
                section: q.section.clone(),
                answer,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ask_user::{AskUserArgs, AskUserRequest, AskUserRequestPayload, Question};
    use crate::session::{PersistedAskUserQuestion, PersistedAskUserRequest};

    fn open_question(multi_select: bool) -> AskUserState {
        AskUserState::for_open_question(PersistedAskUserRequest {
            id: "open-1".to_string(),
            questions: vec![PersistedAskUserQuestion {
                section: String::new(),
                question: "Which database?".to_string(),
                option: vec!["PostgreSQL".to_string(), "SQLite".to_string()],
                multi_select,
            }],
        })
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Chose {
        Options(Vec<&'static str>),
        Text(String),
    }

    /// What a submit committed; fails the test for any other outcome.
    fn submitted(key: AskUserKey) -> Vec<Chose> {
        let AskUserKey::Submit(answers) = key else {
            panic!("expected a submit, got {key:?}");
        };
        answers
            .into_iter()
            .map(|answer| match answer.answer {
                AnswerValue::Selected(values) => Chose::Options(
                    values
                        .iter()
                        .map(|value| match value.as_str() {
                            "PostgreSQL" => "PostgreSQL",
                            "SQLite" => "SQLite",
                            other => panic!("unexpected option {other}"),
                        })
                        .collect(),
                ),
                AnswerValue::Custom(text) => Chose::Text(text),
                AnswerValue::Skipped => panic!("a skipped answer is never produced"),
            })
            .collect()
    }

    fn press(state: &mut AskUserState, codes: &[KeyCode]) -> AskUserKey {
        let mut last = AskUserKey::Stay;
        for &code in codes {
            last = handle_key(state, code);
        }
        last
    }

    /// Esc closes the modal and commits nothing: the question stays open.
    #[test]
    fn esc_closes_the_modal_and_commits_nothing() {
        let mut state = open_question(false);
        assert!(matches!(
            handle_key(&mut state, KeyCode::Esc),
            AskUserKey::Close
        ));

        // Even with a choice made, Esc commits nothing.
        let mut state = open_question(false);
        assert!(matches!(
            press(&mut state, &[KeyCode::Char(' '), KeyCode::Esc]),
            AskUserKey::Close
        ));
    }

    /// Esc cannot dismiss a prompt a run is blocked on: it has no other way
    /// to be answered.
    #[test]
    fn esc_does_not_dismiss_a_prompt_a_run_blocks_on() {
        let (tx, _rx) = async_channel::bounded(1);
        let question = Question {
            section: String::new(),
            question: "Allow SSH?".to_string(),
            option: vec!["Allow".to_string(), "Deny".to_string()],
            multi_select: false,
        };
        let request = AskUserRequest::new(
            AskUserRequestPayload {
                session_id: "session".to_string(),
                request: AskUserArgs {
                    question: Some(question.question.clone()),
                    option: question.option.clone(),
                    multi_select: false,
                    questions: Vec::new(),
                },
            },
            tx,
        );
        let mut state = AskUserState::new("blocking-1".to_string(), request, vec![question]);
        assert!(matches!(
            handle_key(&mut state, KeyCode::Esc),
            AskUserKey::Stay
        ));
    }

    /// Enter with nothing selected and no text commits nothing.
    #[test]
    fn an_empty_enter_commits_nothing() {
        // On the custom-answer row with no text.
        let mut state = open_question(false);
        let outcome = press(
            &mut state,
            &[KeyCode::Down, KeyCode::Down, KeyCode::Enter, KeyCode::Enter],
        );
        assert!(matches!(outcome, AskUserKey::Stay));

        // A multi-select with nothing toggled.
        let mut state = open_question(true);
        assert!(matches!(
            handle_key(&mut state, KeyCode::Enter),
            AskUserKey::Stay
        ));
    }

    /// There is no skip row: the list is the options and the custom answer.
    #[test]
    fn the_list_has_no_skip_row() {
        let state = open_question(false);
        assert_eq!(state.current_item_count(), 3);
    }

    /// A chosen option commits and delivers.
    #[test]
    fn an_answer_submits() {
        let mut state = open_question(false);
        let outcome = press(&mut state, &[KeyCode::Down, KeyCode::Enter]);
        assert_eq!(submitted(outcome), [Chose::Options(vec!["SQLite"])]);

        let mut state = open_question(false);
        let mut keys = vec![KeyCode::Down, KeyCode::Down, KeyCode::Enter];
        keys.extend("MIT".chars().map(KeyCode::Char));
        keys.push(KeyCode::Enter);
        assert_eq!(
            submitted(press(&mut state, &keys)),
            [Chose::Text("MIT".to_string())]
        );
    }

    /// A multi-select submits the toggled options on Enter.
    #[test]
    fn a_multi_select_submits_its_toggled_options() {
        let mut state = open_question(true);
        let outcome = press(
            &mut state,
            &[
                KeyCode::Char(' '),
                KeyCode::Down,
                KeyCode::Char(' '),
                KeyCode::Enter,
            ],
        );
        assert_eq!(
            submitted(outcome),
            [Chose::Options(vec!["PostgreSQL", "SQLite"])]
        );
    }
}
