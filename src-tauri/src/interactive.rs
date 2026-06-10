//! Synchronous user-question plumbing for the `ask_user` tool.
//!
//! When the agent calls `ask_user`, we emit a `user-question` event to the
//! frontend and block the tool call on a `tokio::sync::oneshot` receiver. The
//! UI collects the user's answer and calls the `answer_user_question` tauri
//! command, which fires the matching sender. A 10-minute timeout prevents
//! indefinite hangs.
//!
//! Only one question may be outstanding per session — concurrent `ask_user`
//! calls on the same session are rejected. This keeps the UI model simple
//! (one prompt at a time) and prevents deadlocks from weird tool-call plans.
//!
//! For scheduled tasks (non-interactive runs), the tool is not even exposed to
//! the model — see `llm::anthropic_tools_json` / `llm::openai_tools_json`. The
//! runtime check here is a belt-and-suspenders guard.

use serde_json::{json, Value};
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::sync::oneshot;

use crate::AppState;

const QUESTION_TIMEOUT_SECS: u64 = 600; // 10 minutes

/// Ask the user a question and block until they answer, time out, or cancel.
///
/// `choices`, if non-empty, is rendered as selectable buttons; otherwise the
/// UI shows a free-text input. The returned string is exactly what the user
/// typed or picked.
pub(crate) async fn ask_user(
    app: &AppHandle,
    state: &AppState,
    session_id: &str,
    question: &str,
    choices: Vec<String>,
) -> Result<String, String> {
    if question.trim().is_empty() {
        return Err("ask_user called with an empty question".to_string());
    }

    // Reject a second concurrent question on the same session.
    {
        let pending = state
            .pending_questions
            .lock()
            .map_err(|_| "pending_questions lock poisoned".to_string())?;
        if pending.contains_key(session_id) {
            return Err(
                "A question is already pending on this session. Wait for the user's answer \
                 to the previous question before asking another."
                    .to_string(),
            );
        }
    }

    let (tx, rx) = oneshot::channel::<String>();
    {
        let mut pending = state
            .pending_questions
            .lock()
            .map_err(|_| "pending_questions lock poisoned".to_string())?;
        pending.insert(session_id.to_string(), tx);
    }

    // Notify the UI.
    let _ = app.emit(
        "user-question",
        json!({
            "session_id": session_id,
            "question": question,
            "choices": choices,
        }),
    );

    // Race: answer vs. timeout. `stop_chat` cancels by dropping the sender,
    // which makes `rx` resolve with `Err` — handled as "cancelled".
    match tokio::time::timeout(Duration::from_secs(QUESTION_TIMEOUT_SECS), rx).await {
        Ok(Ok(answer)) => Ok(answer),
        Ok(Err(_)) => Err("question cancelled before the user answered".to_string()),
        Err(_) => {
            // Timeout — drop the orphaned sender entry if it's still there.
            if let Ok(mut pending) = state.pending_questions.lock() {
                pending.remove(session_id);
            }
            let _ = app.emit("user-question-timeout", json!({"session_id": session_id}));
            Err(format!(
                "User did not respond within {} minutes. Proceed with your best judgment \
                 based on what you already know, or defer the task.",
                QUESTION_TIMEOUT_SECS / 60
            ))
        }
    }
}

/// Called by the tauri command `answer_user_question`. Delivers the answer to
/// the waiting `ask_user` future and returns whether there was a pending
/// question to answer.
pub(crate) fn deliver_answer(state: &AppState, session_id: &str, answer: &str) -> Result<(), String> {
    let sender = {
        let mut pending = state
            .pending_questions
            .lock()
            .map_err(|_| "pending_questions lock poisoned".to_string())?;
        pending.remove(session_id)
    };
    match sender {
        Some(tx) => tx
            .send(answer.to_string())
            .map_err(|_| "the question was already cancelled or timed out".to_string()),
        None => Err("no question is pending on this session".to_string()),
    }
}

/// Cancel a pending question without delivering an answer. Dropping the sender
/// causes the receiving side to resolve with Err, which `ask_user` reports to
/// the model as "cancelled".
pub(crate) fn cancel_question(state: &AppState, session_id: &str) {
    if let Ok(mut pending) = state.pending_questions.lock() {
        pending.remove(session_id);
    }
}

/// Convenience: is there a pending question for this session? UI uses this
/// to restore the prompt after a reload.
pub(crate) fn has_pending(state: &AppState, session_id: &str) -> bool {
    state
        .pending_questions
        .lock()
        .map(|m| m.contains_key(session_id))
        .unwrap_or(false)
}

/// Describe the pending question — currently just whether one exists. Kept as
/// a function so future extension (e.g. storing the question text for rehydration)
/// is a one-line change at callsites.
#[allow(dead_code)]
pub(crate) fn pending_snapshot(state: &AppState, session_id: &str) -> Value {
    json!({
        "pending": has_pending(state, session_id),
    })
}
